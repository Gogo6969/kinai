//! Independent fact-check of a single assistant reply.
//!
//! Runs on the dedicated ONLINE checker slot (Settings → Fact check,
//! `AppConfig::llm_factcheck`) — deliberately NOT part of the
//! fast/balanced/deep chat routing or its failover: the whole point is
//! an independent second opinion from a different provider, invoked
//! only by the per-message button.
//!
//! Evidence-first design (0.2.83): the checker model NEVER drives tool
//! calling. Online providers speak different tool-call dialects —
//! DeepSeek V4's OpenAI-compat layer leaked raw DSML markup into the
//! content channel (0.2.82 field bug), which our OpenAI-style pipeline
//! can't see. Instead: (1) one plain completion proposes search
//! queries, (2) KINAI runs the web searches itself through the tool
//! registry, (3) a second completion judges the answer against that
//! evidence. Works identically with any OpenAI-compatible API.
//!
//! Results are ephemeral UI panels; never persisted into the thread,
//! so a fact-check can't leak into future model context.

use tokio_util::sync::CancellationToken;

use crate::config::{AppConfig, LlmSettings};
use crate::context::ChatMessage;
use crate::tools::registry::{self, ToolRuntime};

/// The button (and the WS request) only exist when the checker can
/// actually be called: base URL + model + a non-empty API key. Wolf's
/// rule: "only shows when there are credentials in the fourth model
/// settings" — an online API without a key can't be it.
pub fn is_configured(s: &LlmSettings) -> bool {
    !s.base_url.trim().is_empty()
        && !s.model.trim().is_empty()
        && s.api_key
            .as_deref()
            .map(|k| !k.trim().is_empty())
            .unwrap_or(false)
}

/// Keep the checker's output bounded — it's a verdict card, not an essay,
/// and online APIs bill per token.
const FACT_CHECK_MAX_TOKENS: usize = 2048;
/// At most this many verification searches per check (metered engines).
const MAX_QUERIES: usize = 3;

pub async fn run(
    cfg: &AppConfig,
    question: &str,
    answer: &str,
    cancel: CancellationToken,
) -> anyhow::Result<String> {
    let settings = &cfg.llm_factcheck;
    anyhow::ensure!(
        is_configured(settings),
        "The fact-check model isn't configured — add its API details in Settings → Models."
    );

    let llm = crate::llm::LlmClient::new(settings.clone());
    let year = crate::tools::datetime::current_year();

    // ---- Step 1: derive verification queries (plain completion). ----
    let query_prompt = format!(
        "List the web search queries (1 to {MAX_QUERIES}) that would best verify the \
factual claims in the ANSWER below. The current year is {year} — queries about \
time-sensitive facts should include a year. Reply with ONLY a JSON array of \
strings, nothing else.\n\nQUESTION:\n{question}\n\nANSWER:\n{answer}"
    );
    let qmsgs = vec![
        ChatMessage::System {
            content: "You write web search queries. Reply with ONLY a JSON array of strings."
                .into(),
        },
        ChatMessage::User {
            content: query_prompt,
            name: None,
            image_data_urls: vec![],
        },
    ];
    let queries = match llm.complete(&qmsgs, &[], Some(300)).await {
        Ok(r) => parse_query_array(&r.content),
        Err(e) => {
            tracing::warn!("fact check query step failed ({e:#}); using fallback query");
            Vec::new()
        }
    };
    let queries = if queries.is_empty() {
        // Fallback: verify the answer's opening claim directly.
        let stub: String = answer.chars().take(120).collect();
        vec![stub.split_whitespace().collect::<Vec<_>>().join(" ")]
    } else {
        queries
    };

    // ---- Step 2: KinAI runs the searches (no model tool-calling). ----
    let runtime = ToolRuntime::from_tool_settings(&cfg.tools);
    let mut evidence = String::new();
    let mut searches_ok = 0usize;
    for (i, q) in queries.iter().take(MAX_QUERIES).enumerate() {
        if cancel.is_cancelled() {
            anyhow::bail!("cancelled");
        }
        let args = serde_json::json!({ "query": q }).to_string();
        match registry::execute("web_search", &args, &runtime).await {
            Ok(r) => {
                searches_ok += 1;
                evidence.push_str(&format!("### Search {}: {q}\n{r}\n\n", i + 1));
            }
            Err(e) => {
                tracing::warn!("fact check search '{q}' failed: {e:#}");
                evidence.push_str(&format!(
                    "### Search {}: {q}\nSEARCH FAILED — no results from this query.\n\n",
                    i + 1
                ));
            }
        }
    }

    // ---- Step 3: verdict from the evidence. ----
    let no_evidence_note = if searches_ok == 0 {
        "\nIMPORTANT: EVERY search failed — you have NO fresh evidence. Say plainly \
that the claims could not be verified right now and suggest trying again. Do not \
judge from memory alone."
    } else {
        ""
    };
    let system = format!(
        "You are a strict, independent fact-checker. You receive a QUESTION, an \
ANSWER given by another AI, and EVIDENCE gathered from fresh web searches.\n\
RULES:\n\
- The current year is {year}.\n\
- Judge the ANSWER's factual claims against the EVIDENCE. Prefer the evidence \
over your memory; where the evidence is silent, you may cautiously use well-known \
established facts, marking them as unverified-by-search.\n\
- Judge only FACTS, not style or opinions.\n\
FORMAT (markdown, concise):\n\
Start with exactly one verdict line: `✅ Accurate`, `⚠️ Partly accurate`, or \
`❌ Contains errors` (add a ≤10-word reason).\n\
Then at most 5 bullets — only for claims that are wrong, dubious, or \
unverifiable: quote the claim briefly, state what's actually correct, name the \
source. No bullets if everything checks out.{no_evidence_note}"
    );
    let user = format!(
        "QUESTION:\n{question}\n\nANSWER TO CHECK:\n{answer}\n\nEVIDENCE (web search \
results gathered just now):\n{evidence}"
    );
    let vmsgs = vec![
        ChatMessage::System { content: system },
        ChatMessage::User {
            content: user,
            name: None,
            image_data_urls: vec![],
        },
    ];
    let result = llm
        .complete(&vmsgs, &[], Some(FACT_CHECK_MAX_TOKENS))
        .await?;
    let report = result.content.trim().to_string();
    anyhow::ensure!(
        !report.is_empty(),
        "the checker model returned an empty report — try again"
    );
    Ok(report)
}

/// Tolerant JSON-array-of-strings parser: strips code fences and grabs
/// the first `[...]` block (models love wrapping JSON in prose).
fn parse_query_array(raw: &str) -> Vec<String> {
    let cleaned = raw.replace("```json", "").replace("```", "");
    let start = cleaned.find('[');
    let end = cleaned.rfind(']');
    let (Some(s), Some(e)) = (start, end) else {
        return Vec::new();
    };
    if e <= s {
        return Vec::new();
    }
    serde_json::from_str::<Vec<String>>(&cleaned[s..=e])
        .map(|v| {
            v.into_iter()
                .map(|q| q.trim().to_string())
                .filter(|q| !q.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_requires_all_three_credentials() {
        let mut s = LlmSettings::default_empty();
        assert!(!is_configured(&s), "empty slot is not configured");
        s.base_url = "https://api.deepseek.com".into();
        s.model = "deepseek-v4-flash".into();
        assert!(!is_configured(&s), "no api key -> no button");
        s.api_key = Some("  ".into());
        assert!(!is_configured(&s), "blank api key -> no button");
        s.api_key = Some("sk-test".into());
        assert!(is_configured(&s));
    }

    #[test]
    fn query_array_parsing_is_tolerant() {
        assert_eq!(
            parse_query_array(r#"["a", "b"]"#),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            parse_query_array("Here you go:\n```json\n[\"adam ries born 1492\"]\n```"),
            vec!["adam ries born 1492".to_string()]
        );
        assert!(parse_query_array("no json here").is_empty());
        assert!(parse_query_array("[]").is_empty());
        // Model returned objects instead of strings -> graceful empty.
        assert!(parse_query_array(r#"[{"q":"x"}]"#).is_empty());
    }
}
