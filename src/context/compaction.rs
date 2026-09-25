//! Conversation compaction: fold the messages that are about to scroll out
//! of the model's view into a running per-thread summary, the *digest*.
//!
//! Why this shape — measured 2026-09-25 on the family's llama.cpp servers:
//! a server skips re-reading a prompt only up to the first token that
//! differs from the prompt it read last. A turn that merely appended a
//! question and an answer re-read 275 of ~4.3k tokens (0.5 s); a turn whose
//! oldest exchange had been dropped re-read all of them (3.3–4.3 s, about
//! a second per 1,000–1,200 tokens). So history stays append-only, and
//! when it fills up a large chunk is folded in ONE step: a thread pays one
//! re-read per fold instead of one per turn, and what leaves the window
//! survives as a summary instead of vanishing. (Until 0.2.136 the oldest
//! messages were cut in blocks of up to eight with nothing kept, and a
//! long thread could fall to four remembered messages in one turn.)
//!
//! Runs after the reply is delivered, in the background, on the model that
//! just answered — a local conversation is summarized locally. The builder
//! (`builder::build_context`) shows the digest with the first kept history
//! message and loads only the messages after it.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

use crate::config::LlmSettings;
use crate::db::{Db, Message};
use crate::llm::LlmClient;

use super::builder::{history_token_cap, prompt_budget, sent_costs};
use super::{token_guard, ChatMessage};

/// Fold once the unfolded history passes this share of the history cap…
const FOLD_AT_PERCENT: usize = 90;
/// …keeping this share of the cap verbatim. The gap between the two is how
/// much a thread grows between folds — and so between full re-reads.
const KEEP_PERCENT: usize = 40;
/// The newest messages always stay verbatim, however large they are.
const MIN_KEEP_MESSAGES: usize = 4;
/// Most history one summary call reads. A thread that grew for weeks
/// before compaction existed has only its newest part summarized; the
/// long-term memory notes (`memory::maybe_summarize`) cover the rest.
const FOLD_INPUT_MAX_TOKENS: usize = 24_000;
/// Longest single message quoted into the summary request, in characters.
const QUOTE_MAX_CHARS: usize = 6_000;
/// Output ceiling for the summary call. Thinking is switched off, so this
/// is the summary alone (~350 words asked for).
const SUMMARY_MAX_TOKENS: usize = 2_000;
/// Stored digest ceiling, in characters.
const DIGEST_MAX_CHARS: usize = 5_000;
/// After a failed fold, leave the thread alone this long — a broken model
/// must not cost one extra call per turn.
const RETRY_AFTER: Duration = Duration::from_secs(600);
/// Candidate rows loaded when planning a fold.
const LOAD_LIMIT: i64 = 400;

const INSTRUCTIONS: &str = "You keep the running summary of one conversation between a family \
member and KinAI, their AI assistant. The oldest messages are about to scroll out of KinAI's \
view, and your summary is all KinAI will still know about them.

Update the summary with the new messages. Keep what KinAI may need later: facts, names, numbers, \
dates and places the person shared; their preferences and plans; decisions and results; questions \
still open; what KinAI recommended or promised; links, files and images that came up. Drop \
greetings and small talk. When something changed, keep only the latest state. Keep the previous \
summary's points unless the new messages replace them.

Write in the language the conversation mostly uses. Plain text, one point per line, each starting \
with \"- \". At most 350 words. Never add anything that is not in the conversation. Output only the \
summary.";

/// What one fold did — logged, never the content.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Folded {
    pub messages: usize,
    pub tokens: usize,
    /// Older unfolded rows left to long-term memory (legacy backlog only).
    pub skipped: usize,
    pub digest_tokens: usize,
}

/// Fold what is about to scroll out of view — in the background, so the
/// reply that was just delivered is never held up by it. `llm` is the slot
/// that answered the turn.
pub fn spawn(db: Db, llm: LlmSettings, peer_id: String, thread_id: String) {
    tokio::spawn(async move {
        maybe_compact(&db, &llm, &peer_id, &thread_id).await;
    });
}

/// One fold at a time per thread, and none for a while after a failure.
pub async fn maybe_compact(db: &Db, llm: &LlmSettings, peer_id: &str, thread_id: &str) {
    let Some(_claim) = Claim::take(thread_id) else { return };
    if failed_recently(thread_id) {
        return;
    }
    let started = Instant::now();
    let client = LlmClient::new(llm.clone());
    let outcome = compact_with(db, llm, peer_id, thread_id, |request| async move {
        let r = client
            .complete_without_thinking(&request, &[], Some(SUMMARY_MAX_TOKENS))
            .await?;
        if r.truncated {
            bail!("the summary hit the {SUMMARY_MAX_TOKENS}-token output ceiling");
        }
        Ok(r.content)
    })
    .await;
    match outcome {
        Ok(Some(f)) => tracing::info!(
            thread = %short(thread_id),
            model = %llm.model,
            folded = f.messages,
            folded_tokens = f.tokens,
            skipped = f.skipped,
            digest_tokens = f.digest_tokens,
            ms = started.elapsed().as_millis() as u64,
            "compaction: folded history into the thread digest"
        ),
        Ok(None) => {}
        Err(e) => {
            mark_failed(thread_id);
            tracing::warn!(
                thread = %short(thread_id),
                model = %llm.model,
                "compaction failed, retrying in {} min: {e:#}",
                RETRY_AFTER.as_secs() / 60
            );
        }
    }
}

/// The fold itself, with the model call injected (tests pass a fake).
/// `Ok(None)` = nothing to fold, or another fold of this thread won.
pub(crate) async fn compact_with<F, Fut>(
    db: &Db,
    llm: &LlmSettings,
    peer_id: &str,
    thread_id: &str,
    summarize: F,
) -> Result<Option<Folded>>
where
    F: FnOnce(Vec<ChatMessage>) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let digest = db.thread_digest(peer_id, thread_id).await?;
    let covered = digest.as_ref().map(|d| d.through.clone());
    let msgs = db
        .load_messages_for_context(peer_id, thread_id, LOAD_LIMIT, None, covered.as_deref())
        .await?;
    let costs = sent_costs(&msgs);
    let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
    let cap = history_token_cap(prompt_budget(llm.context_window, llm.max_tokens));
    let Some((from, to)) = plan_fold(&costs, &roles, cap) else {
        return Ok(None);
    };
    let request = summary_request(digest.as_ref().map(|d| d.text.as_str()), &msgs[from..to]);
    let raw = summarize(request).await?;
    let text = clean_summary(&raw).ok_or_else(|| anyhow!("the model returned an empty summary"))?;
    let through = &msgs[to - 1].created_at;
    if !db
        .set_thread_digest(peer_id, thread_id, &text, through, covered.as_deref())
        .await?
    {
        return Ok(None);
    }
    Ok(Some(Folded {
        messages: to - from,
        tokens: costs[from..to].iter().sum(),
        skipped: from,
        digest_tokens: token_guard::count_tokens(&text),
    }))
}

/// Which messages to fold, given each unfolded message's prompt cost and
/// role, oldest first. `Some((from, to))` folds `[from, to)` and keeps
/// `[to, ..)` verbatim; `[.., from)` exists only on a long legacy backlog
/// and is left to long-term memory. `None`: the history still fits.
///
/// The kept part starts at a user turn and holds at least
/// [`MIN_KEEP_MESSAGES`], so right after a fold the model still sees the
/// last exchanges word for word.
pub(crate) fn plan_fold(costs: &[usize], roles: &[&str], cap: usize) -> Option<(usize, usize)> {
    debug_assert_eq!(costs.len(), roles.len());
    let n = costs.len();
    let total: usize = costs.iter().sum();
    if n <= MIN_KEEP_MESSAGES || total * 100 <= cap * FOLD_AT_PERCENT {
        return None;
    }
    // Newest-backwards: keep what fits the keep share of the cap…
    let keep_budget = cap * KEEP_PERCENT / 100;
    let mut kept = 0usize;
    let mut to = n;
    while to > 0 && kept + costs[to - 1] <= keep_budget {
        kept += costs[to - 1];
        to -= 1;
    }
    // …but never fewer than the newest few, and starting at a user turn
    // (moving back keeps a little more rather than splitting an exchange).
    to = to.min(n - MIN_KEEP_MESSAGES);
    while to > 0 && roles[to] != "user" {
        to -= 1;
    }
    if to == 0 {
        return None;
    }
    // Bound what one summary call reads.
    let mut from = to;
    let mut input = 0usize;
    while from > 0 && input + costs[from - 1] <= FOLD_INPUT_MAX_TOKENS {
        input += costs[from - 1];
        from -= 1;
    }
    if from == to {
        // A single message larger than the whole input bound: fold it
        // anyway — it is quoted truncated.
        from = to - 1;
    }
    Some((from, to))
}

/// The summary call: instructions, then the previous digest and the
/// messages to fold, oldest first.
pub(crate) fn summary_request(previous: Option<&str>, msgs: &[Message]) -> Vec<ChatMessage> {
    let previous = previous.map(str::trim).filter(|p| !p.is_empty()).unwrap_or("(none yet)");
    vec![
        ChatMessage::System {
            content: INSTRUCTIONS.to_string(),
        },
        ChatMessage::User {
            content: format!(
                "Previous summary:\n{previous}\n\nNew messages, oldest first:\n\n{}",
                transcript(msgs)
            ),
            name: None,
            image_data_urls: vec![],
        },
    ]
}

/// The messages as the summarizer reads them: day, speaker, text, and a
/// marker for each attachment (never its payload).
pub(crate) fn transcript(msgs: &[Message]) -> String {
    let mut out = String::new();
    for m in msgs {
        let who = match m.role.as_str() {
            "user" => m.sender.trim(),
            "assistant" => "KinAI",
            _ => continue,
        };
        let day = m.created_at.get(..10).unwrap_or("");
        let mut text = quote(m.content.trim());
        for a in &m.attachments {
            let name = a.name.as_deref().unwrap_or("unnamed");
            text.push_str(&format!(" [attached {}: {name}]", a.kind));
        }
        out.push_str(&format!("[{day}] {who}: {text}\n\n"));
    }
    out
}

fn quote(s: &str) -> String {
    if s.chars().count() <= QUOTE_MAX_CHARS {
        return s.to_string();
    }
    let mut t: String = s.chars().take(QUOTE_MAX_CHARS).collect();
    t.push_str(" […]");
    t
}

/// The model's reply as a storable digest: an inline `<think>` block some
/// servers leave in the content is dropped, whitespace trimmed, length
/// capped at a line break. `None` when nothing usable is left.
pub(crate) fn clean_summary(raw: &str) -> Option<String> {
    let body = match raw.rfind("</think>") {
        Some(i) => &raw[i + "</think>".len()..],
        None => raw,
    };
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    if body.chars().count() <= DIGEST_MAX_CHARS {
        return Some(body.to_string());
    }
    let cut: String = body.chars().take(DIGEST_MAX_CHARS).collect();
    let cut = match cut.rfind('\n') {
        Some(i) if i > 0 => cut[..i].trim_end().to_string(),
        _ => cut,
    };
    Some(cut)
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn in_flight() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

fn failures() -> &'static Mutex<HashMap<String, Instant>> {
    static MAP: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    MAP.get_or_init(Default::default)
}

fn failed_recently(thread_id: &str) -> bool {
    let map = failures().lock().unwrap_or_else(|e| e.into_inner());
    map.get(thread_id).is_some_and(|t| t.elapsed() < RETRY_AFTER)
}

fn mark_failed(thread_id: &str) {
    let mut map = failures().lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, t| t.elapsed() < RETRY_AFTER);
    map.insert(thread_id.to_string(), Instant::now());
}

/// This thread's fold slot, released on drop.
struct Claim(String);

impl Claim {
    fn take(thread_id: &str) -> Option<Claim> {
        let mut set = in_flight().lock().unwrap_or_else(|e| e.into_inner());
        set.insert(thread_id.to_string()).then(|| Claim(thread_id.to_string()))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        in_flight()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    fn roles_alternating(n: usize) -> Vec<&'static str> {
        (0..n).map(|i| if i % 2 == 0 { "user" } else { "assistant" }).collect()
    }

    #[test]
    fn history_that_fits_is_left_alone() {
        let costs = vec![100; 10];
        assert_eq!(plan_fold(&costs, &roles_alternating(10), 2_000), None);
        // Just under the fold line (90% of 2,000 = 1,800).
        let costs = vec![180; 10];
        assert_eq!(plan_fold(&costs, &roles_alternating(10), 2_000), None);
    }

    /// Over the line: keep ≤ 40% of the cap, starting at a user turn,
    /// fold everything before it.
    #[test]
    fn a_full_history_folds_down_to_the_keep_share() {
        let costs = vec![200; 12]; // 2,400 > 1,800
        let roles = roles_alternating(12);
        let (from, to) = plan_fold(&costs, &roles, 2_000).unwrap();
        assert_eq!(from, 0);
        let kept: usize = costs[to..].iter().sum();
        assert!(kept <= 800 + 200, "kept {kept} tokens");
        assert_eq!(roles[to], "user", "kept history must start at a user turn");
        assert!(12 - to >= MIN_KEEP_MESSAGES);
    }

    /// Huge newest messages: the newest few stay verbatim anyway.
    #[test]
    fn the_newest_messages_always_stay() {
        let costs = vec![100, 100, 100, 100, 3_000, 3_000, 3_000, 3_000];
        let roles = roles_alternating(8);
        let (_, to) = plan_fold(&costs, &roles, 2_000).unwrap();
        assert_eq!(to, 4, "the four newest stay verbatim");
    }

    /// A long legacy backlog: one summary call reads at most the input
    /// bound, newest part first; the rest is left to long-term memory.
    #[test]
    fn one_fold_reads_a_bounded_amount() {
        let costs = vec![1_000; 100];
        let roles = roles_alternating(100);
        let (from, to) = plan_fold(&costs, &roles, 12_000).unwrap();
        let read: usize = costs[from..to].iter().sum();
        assert!(read <= FOLD_INPUT_MAX_TOKENS, "read {read}");
        assert!(from > 0, "the oldest backlog is skipped");
    }

    /// Nothing before the newest few: nothing to fold.
    #[test]
    fn too_few_messages_never_fold() {
        let costs = vec![5_000; 4];
        assert_eq!(plan_fold(&costs, &roles_alternating(4), 2_000), None);
    }

    #[test]
    fn summaries_are_cleaned_and_capped() {
        assert_eq!(clean_summary("  \n "), None);
        assert_eq!(clean_summary("<think>hmm</think>\n  - a\n- b "), Some("- a\n- b".into()));
        let long = "- point\n".repeat(2_000);
        let c = clean_summary(&long).unwrap();
        assert!(c.chars().count() <= DIGEST_MAX_CHARS);
        assert!(c.ends_with("- point"), "cut at a line break");
    }

    #[test]
    fn claims_are_exclusive_per_thread() {
        let a = Claim::take("t-claim").expect("first claim");
        assert!(Claim::take("t-claim").is_none(), "second claim must wait");
        assert!(Claim::take("t-other").is_some());
        drop(a);
        assert!(Claim::take("t-claim").is_some(), "released on drop");
    }

    async fn fresh_db() -> Db {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        crate::db::migrate::run(&pool).await.expect("migrations");
        Db { pool }
    }

    /// An 8k window: 4k prompt budget, 2k history cap.
    fn small_slot() -> LlmSettings {
        LlmSettings {
            context_window: 8_192,
            ..LlmSettings::default()
        }
    }

    async fn long_thread(db: &Db, turns: usize) -> String {
        let t = db.create_thread("host", Some("t")).await.unwrap();
        for i in 0..turns {
            db.append_message(&t.id, "user", "Alex", &format!("question {i} {}", "word ".repeat(60)), &[])
                .await
                .unwrap();
            db.append_message(&t.id, "assistant", "KinAI", &format!("answer {i} {}", "word ".repeat(120)), &[])
                .await
                .unwrap();
        }
        t.id
    }

    /// The whole fold against a real database: the digest is stored,
    /// covers exactly the folded messages, feeds the next fold, and the
    /// builder then shows it instead of those messages.
    #[tokio::test]
    async fn a_fold_stores_the_digest_and_the_builder_uses_it() {
        let db = fresh_db().await;
        let llm = small_slot();
        let tid = long_thread(&db, 12).await; // ~2.7k tokens > 1.8k line

        let folded = compact_with(&db, &llm, "host", &tid, |req| async move {
            let body = req[1].content().to_string();
            assert!(body.contains("(none yet)") && body.contains("question 0"), "{body}");
            Ok("- Alex asked twelve questions".to_string())
        })
        .await
        .unwrap()
        .expect("a fold");
        assert!(folded.messages >= 2 && folded.skipped == 0, "{folded:?}");

        let d = db.thread_digest("host", &tid).await.unwrap().unwrap();
        assert_eq!(d.text, "- Alex asked twelve questions");
        let rest = db
            .load_messages_for_context("host", &tid, 100, None, Some(&d.through))
            .await
            .unwrap();
        assert_eq!(rest.len(), 24 - folded.messages, "exactly the folded rows are covered");
        assert_eq!(rest[0].role, "user");

        // Nothing new: the history fits, no second call.
        let again = compact_with(&db, &llm, "host", &tid, |_| async { panic!("must not summarize") })
            .await
            .unwrap();
        assert_eq!(again, None);

        // The builder shows the digest and not the folded messages.
        let cfg = crate::config::AppConfig::default();
        let m = db.append_message(&tid, "user", "Alex", "what did I ask first?", &[]).await.unwrap();
        let ctx = crate::context::builder::build_context(&db, &cfg, &llm, "host", &tid, &m)
            .await
            .unwrap();
        let text: String = ctx.iter().map(|c| c.content()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("Alex asked twelve questions"));
        assert!(!text.contains("question 0 "), "a folded message leaked into the prompt");

        // The next fold builds on the previous digest.
        for i in 12..20 {
            db.append_message(&tid, "user", "Alex", &format!("question {i} {}", "word ".repeat(60)), &[])
                .await
                .unwrap();
            db.append_message(&tid, "assistant", "KinAI", &format!("answer {i} {}", "word ".repeat(120)), &[])
                .await
                .unwrap();
        }
        compact_with(&db, &llm, "host", &tid, |req| async move {
            assert!(req[1].content().contains("Alex asked twelve questions"), "previous digest not passed on");
            Ok("- Alex asked twenty questions".to_string())
        })
        .await
        .unwrap()
        .expect("a second fold");
        assert_eq!(db.thread_digest("host", &tid).await.unwrap().unwrap().text, "- Alex asked twenty questions");
    }

    /// Two folds racing: the one that finishes second was built on a stale
    /// base and must not overwrite the winner.
    #[tokio::test]
    async fn a_fold_that_lost_the_race_writes_nothing() {
        let db = fresh_db().await;
        let llm = small_slot();
        let tid = long_thread(&db, 12).await;
        let db2 = db.clone();
        let tid2 = tid.clone();
        let out = compact_with(&db, &llm, "host", &tid, |_| async move {
            // Another fold lands while this one is summarizing.
            db2.set_thread_digest("host", &tid2, "- the winner", "2000-01-01T00:00:00+00:00", None)
                .await
                .unwrap();
            Ok("- the loser".to_string())
        })
        .await
        .unwrap();
        assert_eq!(out, None);
        assert_eq!(db.thread_digest("host", &tid).await.unwrap().unwrap().text, "- the winner");
    }

    /// A failed summary stores nothing and is reported as an error.
    #[tokio::test]
    async fn an_empty_summary_is_an_error_and_stores_nothing() {
        let db = fresh_db().await;
        let tid = long_thread(&db, 12).await;
        let err = compact_with(&db, &small_slot(), "host", &tid, |_| async { Ok("  ".to_string()) })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty summary"));
        assert_eq!(db.thread_digest("host", &tid).await.unwrap(), None);
    }

    #[test]
    fn the_transcript_marks_attachments_and_trims_long_messages() {
        let m = |role: &str, sender: &str, content: &str| Message {
            id: "x".into(),
            thread_id: "t".into(),
            role: role.into(),
            sender: sender.into(),
            content: content.into(),
            attachments: vec![],
            created_at: "2026-09-25T10:00:00+00:00".into(),
            summarized_into: None,
            metrics: None,
        };
        let mut photo = m("user", "Alex", "look");
        photo.attachments.push(crate::db::Attachment {
            kind: "image".into(),
            mime: Some("image/png".into()),
            name: Some("boat.png".into()),
            data_url: Some("data:image/png;base64,AAAA".into()),
        });
        let long = m("assistant", "KinAI", &"x".repeat(QUOTE_MAX_CHARS + 50));
        let t = transcript(&[photo, long, m("system", "KinAI", "hidden")]);
        assert!(t.contains("[2026-09-25] Alex: look [attached image: boat.png]"));
        assert!(!t.contains("base64"), "attachment payload leaked into the transcript");
        assert!(t.contains("KinAI: xxx") && t.contains("[…]"));
        assert!(!t.contains("hidden"));
    }
}
