//! Tool catalogue — generated dynamically from `ToolSettings` so users can
//! disable tools their model shouldn't be allowed to call.

use anyhow::{anyhow, Result};
use serde_json::json;

use crate::config::{SearchEngine, ToolSettings};
use crate::db::Db;
/// The shared reminder validation. `pretty_local` lives beside it because
/// the refusal sentences render times too, and one copy beats two.
use crate::reminders::spec::{self, pretty_local};

/// Runtime context the tool layer needs that isn't in the static schema —
/// API keys, search-engine selection, plus the DB handle + peer scope for
/// memory tools that need to persist facts.
#[derive(Clone, Default)]
pub struct ToolRuntime {
    pub search_engine: SearchEngine,
    pub search_api_key: Option<String>,
    /// Base URL of the family's SearXNG; only meaningful when
    /// `search_engine` is `Searxng`.
    pub searxng_url: String,
    /// Use SearXNG when the paid engine is permanently unavailable.
    pub search_fallback_searxng: bool,
    /// Base URL of the family's transcript service; empty = the
    /// `video_transcript` tool is not offered at all.
    pub transcript_url: String,
    /// DB handle for memory tools (remember, forget). Set only when the
    /// caller wants those tools to actually persist — search-tool-only
    /// callsites (e.g. an isolated extractor pass) can leave it None.
    pub db: Option<Db>,
    /// Peer scope for memory writes. Required when `db` is set; ignored
    /// otherwise. Always `HOST_PEER` for in-app turns; for Telegram-
    /// originated turns it's the connected peer's id.
    pub peer_id: Option<String>,
    /// Source message id for traceability. When a fact is written via
    /// remember(), this is the user message that triggered the call.
    pub source_msg_id: Option<String>,
    /// The conversation this turn belongs to. A reminder set here keeps
    /// it, so "remind me about the last response" can offer a way back to
    /// the answer instead of only naming it.
    pub thread_id: Option<String>,
}

impl ToolRuntime {
    pub fn from_tool_settings(s: &ToolSettings) -> Self {
        Self {
            search_engine: s.search_engine,
            search_api_key: s.search_api_key.clone(),
            searxng_url: s.searxng_url.clone(),
            search_fallback_searxng: s.search_fallback_searxng,
            transcript_url: s.transcript_url.clone(),
            db: None,
            peer_id: None,
            source_msg_id: None,
            thread_id: None,
        }
    }

    /// Attach the DB + peer scope so memory tools can persist. Chainable:
    /// `ToolRuntime::from_tool_settings(&cfg.tools).with_memory(db, peer)`.
    pub fn with_memory(mut self, db: Db, peer_id: impl Into<String>) -> Self {
        self.db = Some(db);
        self.peer_id = Some(peer_id.into());
        self
    }

    pub fn with_source_msg(mut self, source_msg_id: impl Into<String>) -> Self {
        self.source_msg_id = Some(source_msg_id.into());
        self
    }

    /// The thread this turn is happening in — carried onto any reminder
    /// set during it, so the member can get back to what it was about.
    pub fn with_thread(mut self, thread_id: impl Into<String>) -> Self {
        self.thread_id = Some(thread_id.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

/// The fact-checker's fixed toolset. Deliberately NOT derived from the
/// per-tool chat toggles: clicking "fact check" is an explicit request
/// to verify against the web — a host who turned web_search off for
/// CHAT still expects the checker to search (that's its whole job).
pub(crate) fn fact_check_defs() -> Vec<ToolDef> {
    vec![web_search_def(), datetime_def()]
}

pub fn enabled(settings: &ToolSettings) -> Vec<ToolDef> {
    let mut out = Vec::new();
    if settings.web_search {
        out.push(web_search_def());
        // Same toggle, same privacy surface: fetching a page the user
        // linked is no more of a network disclosure than searching for it.
        out.push(fetch_page_def());
    }
    // Only offered when the household actually runs the service. A tool
    // the model can call but that can never succeed is worse than none.
    if !settings.transcript_url.trim().is_empty() {
        out.push(video_transcript_def());
    }
    if settings.x_search {
        out.push(x_search_def());
    }
    if settings.calculator {
        out.push(calculator_def());
    }
    if settings.datetime {
        out.push(datetime_def());
    }
    if settings.image_search {
        out.push(image_search_def());
    }
    // Memory tools are always-on for now. The user can still purge any
    // fact via Settings → Memory; gating the tool itself would only
    // prevent the model from REMEMBERING new things, which isn't a
    // privacy property anyone actually wants — they want CONTROL over
    // what's stored, which the Settings UI provides.
    out.push(remember_def());
    out.push(forget_def());
    // Reminders are always-on for the same reason: the member controls
    // them through the Calendar, so gating the tool would only stop the
    // model from setting what they asked for.
    out.push(set_reminder_def());
    out.push(list_reminders_def());
    out.push(cancel_reminder_def());
    out
}

pub fn all_definitions() -> Vec<ToolDef> {
    enabled(&ToolSettings::default())
}

pub async fn execute(name: &str, args_json: &str, runtime: &ToolRuntime) -> Result<String> {
    let args: serde_json::Value = serde_json::from_str(args_json).unwrap_or(serde_json::json!({}));
    match name {
        "web_search" => {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("missing query"))?;
            super::web_search::search(
                query,
                // More results = more real URLs/snippets for the model to
                // ground in, fewer gaps it's tempted to fill by inventing.
                10,
                runtime.search_engine,
                runtime.search_api_key.as_deref(),
                &runtime.searxng_url,
                runtime.search_fallback_searxng,
            )
            .await
        }
        "fetch_page" => {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("missing url"))?;
            super::fetch_page::fetch(url).await
        }
        "video_transcript" => {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("missing url"))?;
            super::video_transcript::fetch(&runtime.transcript_url, url).await
        }
        "x_search" => {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("missing query"))?;
            let kind = args
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("keyword");
            super::x_search::search(
                query,
                kind,
                5,
                runtime.search_engine,
                runtime.search_api_key.as_deref(),
            )
            .await
        }
        "calculator" => {
            let expr = args
                .get("expression")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("missing expression"))?;
            let value = super::calculator::eval(expr)?;
            Ok(format!("{} = {}", expr, value))
        }
        "datetime" => {
            // In the member's zone when the turn knows who is asking.
            if let (Some(db), Some(peer)) = (runtime.db.as_ref(), runtime.peer_id.as_deref()) {
                let facts = db.user_facts_for_prompt(peer).await.unwrap_or_default();
                let tz = super::datetime::resolve_peer_tz(db, peer, &facts).await;
                Ok(super::datetime::now_pretty_in(tz))
            } else {
                Ok(super::datetime::now_pretty())
            }
        }
        "set_reminder" => {
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("missing text"))?;
            let db = runtime
                .db
                .as_ref()
                .ok_or_else(|| anyhow!("reminder tools require a DB; tool runtime has none"))?;
            let peer = runtime
                .peer_id
                .as_deref()
                .ok_or_else(|| anyhow!("reminder tools require peer_id"))?;
            let facts = db.user_facts_for_prompt(peer).await.unwrap_or_default();
            let tz = super::datetime::resolve_peer_tz(db, peer, &facts).await;
            let tz_name = tz
                .map(|z| z.name().to_string())
                .unwrap_or_else(super::datetime::host_tz_name);
            // Text is checked BEFORE the "when", because that is the order
            // 0.2.123 answered in: a long note with no time is told it is
            // too long, not sent back for a time that would then be
            // refused for the length anyway. `plan` re-checks it — this is
            // about which sentence comes first, not a second rule.
            if let Err(rejected) = spec::check_text(text) {
                return Ok(rejected.prose());
            }
            // "You gave me neither" stays here rather than in the spec:
            // `When` makes it unrepresentable there, and each surface
            // words it for its own caller — this sentence to a model, a
            // 422 to a program.
            let when = if let Some(m) = args.get("in_minutes").and_then(|v| v.as_i64()) {
                spec::When::InMinutes(m)
            } else if let Some(local) = args.get("due_local").and_then(|v| v.as_str()) {
                spec::When::DueLocal(local)
            } else {
                return Ok("Tell me when: pass due_local (YYYY-MM-DDTHH:MM in the user's \
                           timezone) or in_minutes. Nothing was set."
                    .into());
            };
            // Every user-level outcome is Ok(text): the loop treats an Err
            // as a failed lookup and tells the family their web search
            // broke. Only real infrastructure failures are errors — which
            // is also why the length check lives in `plan` and never
            // reaches the DB's `bail!`.
            // Unknown repeat words are refused, not silently dropped: a
            // member who asked for "every other Tuesday" and got a one-off
            // would only discover it by not being reminded.
            let repeat_raw = args.get("repeat").and_then(|v| v.as_str()).unwrap_or("");
            let Some(repeat) = spec::Repeat::parse(repeat_raw) else {
                return Ok(format!(
                    "I can repeat a reminder daily, on weekdays, weekly or monthly — not \
                     {repeat_raw:?}. Nothing was set."
                ));
            };
            let planned = match spec::plan(text, when, repeat, tz, &tz_name, chrono::Utc::now()) {
                Ok(p) => p,
                Err(rejected) => return Ok(rejected.prose()),
            };
            let r = db
                .create_reminder(
                    peer,
                    runtime.thread_id.as_deref(),
                    &planned.text,
                    planned.due_at,
                    &planned.tz_name,
                    runtime.source_msg_id.as_deref(),
                    planned.repeat.as_str(),
                    &planned.occurrence_local,
                )
                .await?;
            Ok(format!(
                "Reminder set for {}{} ({}): {}. It will pop up in KinAI on your devices, and on \
                 Telegram if you're paired.{} [id {}]",
                pretty_local(&r.due_local),
                if repeat.repeats() { format!(", {}", repeat.human()) } else { String::new() },
                r.tz,
                r.text,
                if repeat.repeats() {
                    " It keeps coming back until you stop it."
                } else {
                    ""
                },
                short_id(&r.id)
            ))
        }
        "list_reminders" => {
            let db = runtime
                .db
                .as_ref()
                .ok_or_else(|| anyhow!("reminder tools require a DB; tool runtime has none"))?;
            let peer = runtime
                .peer_id
                .as_deref()
                .ok_or_else(|| anyhow!("reminder tools require peer_id"))?;
            // Live reminders only, and bounded: finished ones are history
            // for the Calendar, and an unbounded oldest-first list would
            // push the upcoming ones past the tool-result cap after a few
            // months of daily use.
            const LIST_LIMIT: i64 = 25;
            let items = db.list_live_reminders(peer, LIST_LIMIT).await?;
            if items.is_empty() {
                return Ok("No reminders are set.".into());
            }
            let total = db.count_live_reminders(peer).await.unwrap_or(items.len() as i64);
            let mut out = String::from("Reminders (soonest first):\n");
            for (i, r) in items.iter().enumerate() {
                let state = match r.status.as_str() {
                    "fired" | "firing" => " — due now, not yet acknowledged",
                    _ => "",
                };
                out.push_str(&format!(
                    "{}. [id {}] {} ({}): {}{}\n",
                    i + 1,
                    short_id(&r.id),
                    pretty_local(&r.due_local),
                    r.tz,
                    r.text,
                    state
                ));
            }
            if total > items.len() as i64 {
                out.push_str(&format!(
                    "(showing the {} soonest of {} — the rest are in the Calendar)\n",
                    items.len(),
                    total
                ));
            }
            Ok(out)
        }
        "cancel_reminder" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("missing id"))?;
            let db = runtime
                .db
                .as_ref()
                .ok_or_else(|| anyhow!("reminder tools require a DB; tool runtime has none"))?;
            let peer = runtime
                .peer_id
                .as_deref()
                .ok_or_else(|| anyhow!("reminder tools require peer_id"))?;
            let hits = match db.find_reminders_by_prefix(peer, id).await {
                Ok(h) => h,
                Err(e) => return Ok(format!("{e}. Call list_reminders to see the ids.")),
            };
            match hits.as_slice() {
                [] => {
                    // Distinguish "already dealt with" from "no such thing":
                    // the second reads as a bug when the member is looking
                    // at the reminder in their Calendar.
                    let finished = db
                        .find_finished_reminders_by_prefix(peer, id)
                        .await
                        .unwrap_or_default();
                    match finished.first() {
                        Some(r) if r.status == "done" => Ok(format!(
                            "That one is already done: {}. Nothing to cancel.",
                            r.text
                        )),
                        Some(r) => Ok(format!("That one was already cancelled: {}.", r.text)),
                        None => Ok(
                            "No live reminder matches that id — call list_reminders to see them."
                                .into(),
                        ),
                    }
                }
                [one] => {
                    db.cancel_reminder(peer, &one.id).await?;
                    Ok(format!(
                        "Cancelled: {} ({}).",
                        one.text,
                        pretty_local(&one.due_local)
                    ))
                }
                many => Ok(format!(
                    "Several reminders match — give more of the id: {}",
                    many.iter()
                        .map(|r| format!("[{}] {}", short_id(&r.id), r.text))
                        .collect::<Vec<_>>()
                        .join("; ")
                )),
            }
        }
        "remember" => {
            let key = args
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("missing key"))?;
            let value = args
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("missing value"))?;
            let db = runtime
                .db
                .as_ref()
                .ok_or_else(|| anyhow!("memory tools require a DB; tool runtime has none"))?;
            let peer = runtime
                .peer_id
                .as_deref()
                .ok_or_else(|| anyhow!("memory tools require peer_id"))?;
            let fact = db
                .save_user_fact(peer, key, value, "tool", runtime.source_msg_id.as_deref())
                .await?;
            Ok(format!(
                "OK — I'll remember that {} is {}.",
                fact.key, fact.value
            ))
        }
        "forget" => {
            let key = args
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("missing key"))?;
            let db = runtime
                .db
                .as_ref()
                .ok_or_else(|| anyhow!("memory tools require a DB; tool runtime has none"))?;
            let peer = runtime
                .peer_id
                .as_deref()
                .ok_or_else(|| anyhow!("memory tools require peer_id"))?;
            let deleted = db.delete_user_fact_by_key(peer, key).await?;
            if deleted == 0 {
                Ok(format!("I had nothing stored under '{}'.", key))
            } else {
                Ok(format!("Forgotten: {}.", key))
            }
        }
        "image_search" => {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("missing query"))?;
            super::image_search::search(
                query,
                5,
                runtime.search_engine,
                runtime.search_api_key.as_deref(),
            )
            .await
        }
        _ => Err(anyhow!("unknown tool: {name}")),
    }
}

fn web_search_def() -> ToolDef {
    // The year is baked into the tool description because models anchor
    // "recent" to their TRAINING years when composing queries — a July-2026
    // host watched its model search "World Cup winner 2022 2023 2024 2025"
    // and confidently report 2022 as the latest. The description is rebuilt
    // per turn, so it always carries the real current year.
    let year = crate::tools::datetime::current_year();
    let desc = format!(
        "Search the internet and return up-to-date results. The current year is {year} — \
for time-sensitive questions (news, sports, prices, who holds a role) include {year} in \
the query, not the years you remember as recent."
    );
    ToolDef {
        name: "web_search".into(),
        description: desc.clone(),
        schema: json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": desc,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "The search query." }
                    },
                    "required": ["query"]
                }
            }
        }),
    }
}

fn fetch_page_def() -> ToolDef {
    ToolDef {
        name: "fetch_page".into(),
        description: "Fetch a specific URL and return its full text — works for web pages, online PDFs (papers, reports), and plain-text files.".into(),
        schema: json!({
            "type": "function",
            "function": {
                "name": "fetch_page",
                "description": "Download a specific URL and return its readable text. Handles web pages (HTML stripped to prose), online PDF documents (papers, reports, manuals — full text extracted), XML/RSS feeds, and plain-text files. Use this whenever the user gives you a link and wants you to read, summarize, or answer questions about what's behind it, or when web_search snippets aren't enough and you need a result page's actual content. IMPORTANT: many 'live list' pages — trending topics, rankings, leaderboards, live scores — are JavaScript dashboards whose numbers are NOT in the page source, so both search snippets and a plain fetch of the page return only navigation text. When a site publishes an RSS, XML or JSON feed of the same data, fetch that instead: it is static and contains the actual entries (for example Google Trends' trending list is at https://trends.google.com/trending/rss?geo=US, with geo= set to the country you need). Very long documents are truncated with a note. Only public http/https URLs work — local or private-network addresses are refused.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The full http(s) URL to fetch, exactly as given or as found in search results."
                        }
                    },
                    "required": ["url"]
                }
            }
        }),
    }
}

fn video_transcript_def() -> ToolDef {
    ToolDef {
        name: "video_transcript".into(),
        description: "Read what is actually said in a YouTube video, from its captions.".into(),
        schema: json!({
            "type": "function",
            "function": {
                "name": "video_transcript",
                "description": "Read a YouTube video's spoken content from its captions, on the family's own hardware. Use this whenever someone shares a YouTube link and wants to know what the video says — \"watch this\", \"what is this about\", \"summarise this video\". Returns the title, length and full transcript. Captions are auto-generated, so wording can be imperfect and speaker names are absent. Not every video has captions; if it doesn't, say so rather than guessing from the title. YouTube links only.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The full YouTube URL, exactly as the user gave it."
                        }
                    },
                    "required": ["url"]
                }
            }
        }),
    }
}

fn x_search_def() -> ToolDef {
    ToolDef {
        name: "x_search".into(),
        description: "Search social / discussion posts (X tweets via Exa, or Hacker News when Exa isn't configured).".into(),
        schema: json!({
            "type": "function",
            "function": {
                "name": "x_search",
                "description": "Search current social-media posts and online discussions. Routes through the user's configured search engine: Exa with category=tweet returns real X/Twitter posts; the no-key fallback queries Hacker News. Use mode=keyword for relevance ranking or mode=semantic for recency-with-query-overlap. Returns post text, author, source URL, and timestamp.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "mode": { "type": "string", "enum": ["keyword", "semantic"], "default": "keyword" }
                    },
                    "required": ["query"]
                }
            }
        }),
    }
}

fn calculator_def() -> ToolDef {
    ToolDef {
        name: "calculator".into(),
        description: "Evaluate an arithmetic expression.".into(),
        schema: json!({
            "type": "function",
            "function": {
                "name": "calculator",
                "description": "Evaluate an arithmetic expression. Supports + - * / ^ and parentheses.",
                "parameters": {
                    "type": "object",
                    "properties": { "expression": { "type": "string" } },
                    "required": ["expression"]
                }
            }
        }),
    }
}

fn datetime_def() -> ToolDef {
    ToolDef {
        name: "datetime".into(),
        description: "Get the current local date and time.".into(),
        schema: json!({
            "type": "function",
            "function": {
                "name": "datetime",
                "description": "Get the current local date and time (in the user's timezone when known).",
                "parameters": { "type": "object", "properties": {} }
            }
        }),
    }
}

fn remember_def() -> ToolDef {
    ToolDef {
        name: "remember".into(),
        description: "Save a fact about the user for future conversations.".into(),
        schema: json!({
            "type": "function",
            "function": {
                "name": "remember",
                "description": "Save a persistent fact about the user that should survive across chats and sessions. \
                                Use this WHENEVER the user states a stable piece of information about themselves, their life, \
                                their preferences, or their context — e.g. \"I live in Berlin\", \"my wife's name is Anna\", \
                                \"I prefer metric units\", \"I'm allergic to peanuts\", \"my work uses TypeScript\". \
                                Do NOT use for ephemeral things (\"I'm feeling tired today\") or content of the current message. \
                                Pick a short, lowercase, semantic `key` (e.g. \"city\", \"wife_name\", \"diet\", \"work_stack\") \
                                and a concise `value`. Calling remember twice with the same key OVERWRITES — that's how a user \
                                tells the model 'I moved' (just state the new value). The user can review and edit anything \
                                stored via Settings → Memory.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Short, lowercase, semantic identifier. snake_case preferred. Examples: \"city\", \"wife_name\", \"diet\", \"work_stack\", \"timezone\"."
                        },
                        "value": {
                            "type": "string",
                            "description": "The fact itself. One sentence or less. Examples: \"Berlin, Germany\", \"Anna\", \"vegetarian\", \"TypeScript and Rust\", \"Europe/Berlin\"."
                        }
                    },
                    "required": ["key", "value"]
                }
            }
        }),
    }
}

fn forget_def() -> ToolDef {
    ToolDef {
        name: "forget".into(),
        description: "Delete a previously-stored fact about the user.".into(),
        schema: json!({
            "type": "function",
            "function": {
                "name": "forget",
                "description": "Delete a previously-saved fact identified by its `key`. Use when the user explicitly asks you to forget something they told you before. If you're unsure which key to forget, ask the user — listing keys you can see in your system context is fine.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "key": { "type": "string", "description": "The exact key the fact was saved under." }
                    },
                    "required": ["key"]
                }
            }
        }),
    }
}

fn image_search_def() -> ToolDef {
    ToolDef {
        name: "image_search".into(),
        description: "Find pictures on the web for a query.".into(),
        schema: json!({
            "type": "function",
            "function": {
                "name": "image_search",
                "description": "Search the web for pictures matching a query. Returns a markdown-formatted list of images the assistant can echo directly to the user — each entry is `![alt](image_url)` followed by a caption with the source page. Use this whenever the user asks to see a photo, picture, or visual of something (e.g. \"show me a photo of the Eiffel Tower\", \"what does a quokka look like\"). The frontend renders the markdown image inline.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The subject to find images of. Be specific — 'Eiffel Tower at night' beats 'tower'."
                        }
                    },
                    "required": ["query"]
                }
            }
        }),
    }
}

// ---- Reminders ----

/// The first eight characters of a reminder id — what the model and the
/// member see; `cancel_reminder` resolves them back through a prefix
/// lookup (six or more characters are enough).
fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn set_reminder_def() -> ToolDef {
    ToolDef {
        name: "set_reminder".into(),
        description: "Set a reminder that pops up in KinAI at a time the user chooses.".into(),
        schema: json!({
            "type": "function",
            "function": {
                "name": "set_reminder",
                "description": "Schedule a reminder for the user. It pops up in KinAI on their own devices \
                                (and on Telegram if they paired it) at the given time, with acknowledge and \
                                snooze buttons. Call it when the user asks to be reminded of something \
                                (\"remind me at 9 tomorrow to …\", \"in 20 minutes remind me …\"). \
                                `text` must be concrete: resolve \"that\" / \"it\" into what was actually \
                                discussed. Give EXACTLY ONE of `due_local` (a clock time, YYYY-MM-DDTHH:MM in \
                                the user's own timezone — compute it from the \"Current time\" line at the \
                                end of the user's message) or `in_minutes` (relative). The tool answers with \
                                the confirmation to relay, including the resolved time and zone; it refuses \
                                times in the past or more than a year away, and then NOTHING is set — say so. \
                                Pass `repeat` when the user wants it to come back; a repeating reminder keeps \
                                going until they stop it, so only set one when they actually asked for it.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "What to remind the user of, in the user's own terms (up to 600 characters — a sentence or two is ideal, but keep any detail they asked to be reminded OF, including a URL). When they point at something in the conversation (\"remind me about that\", \"about your last answer\"), write what it actually SAID — the substance, in a sentence or two — not a label like \"the last response\". The conversation is linked to the reminder automatically, so the user can reopen it; your job is to make the reminder make sense on its own when it pops up. Examples: \"return the library book\", \"call the dentist about moving the appointment, the number is on the fridge\", \"Gauff won her US Open quarterfinal; the updated odds put her second favourite behind Sabalenka\"."
                        },
                        "due_local": {
                            "type": "string",
                            "description": "Clock time in the user's timezone, format YYYY-MM-DDTHH:MM, e.g. \"2026-09-10T09:00\". Use this for \"at 9\", \"tomorrow at 7\", \"on Friday at noon\"."
                        },
                        "in_minutes": {
                            "type": "integer",
                            "description": "Minutes from now, e.g. 30 for \"in half an hour\". Use this for relative phrasing instead of due_local."
                        },
                        "repeat": {
                            "type": "string",
                            "enum": ["daily", "weekdays", "weekly", "monthly"],
                            "description": "Omit for a one-off, which is the normal case. Set it when the user says the reminder should come back: \"every day\" → daily, \"every weekday\" / \"on work days\" → weekdays, \"every week\" / \"every Tuesday\" → weekly, \"every month\" / \"on the 1st\" → monthly. The time of day comes from due_local or in_minutes as usual, and a repeating reminder keeps that clock time even across a daylight-saving change. If the user asks for something this cannot express (\"every other Tuesday\", \"twice a day\"), do NOT approximate it — set nothing and say what is possible."
                        }
                    },
                    "required": ["text"]
                }
            }
        }),
    }
}

fn list_reminders_def() -> ToolDef {
    ToolDef {
        name: "list_reminders".into(),
        description: "List the user's reminders.".into(),
        schema: json!({
            "type": "function",
            "function": {
                "name": "list_reminders",
                "description": "List the user's reminders, soonest first, with their short ids and local times. \
                                Call it when the user asks what reminders they have, or before cancelling one \
                                when you don't know its id.",
                "parameters": { "type": "object", "properties": {} }
            }
        }),
    }
}

fn cancel_reminder_def() -> ToolDef {
    ToolDef {
        name: "cancel_reminder".into(),
        description: "Cancel one of the user's reminders.".into(),
        schema: json!({
            "type": "function",
            "function": {
                "name": "cancel_reminder",
                "description": "Cancel a reminder by its id (the short id from list_reminders or from the \
                                set_reminder confirmation; at least 6 characters). Use when the user asks to \
                                cancel, drop or forget a reminder. If you don't know the id, call \
                                list_reminders first.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "The reminder id or its first 6+ characters." }
                    },
                    "required": ["id"]
                }
            }
        }),
    }
}

#[cfg(test)]
mod reminder_tool_tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    /// A ToolRuntime backed by a real, migrated in-memory DB — the same
    /// shape every chat surface hands the tools.
    async fn runtime() -> ToolRuntime {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        crate::db::migrate::run(&pool).await.expect("apply migrations");
        ToolRuntime::from_tool_settings(&ToolSettings::default())
            .with_memory(Db { pool }, "ALICE")
            .with_source_msg("msg-1")
    }

    #[tokio::test]
    async fn relative_reminder_is_stored_and_confirmed() {
        let rt = runtime().await;
        let out = execute("set_reminder", r#"{"text":"water the plants","in_minutes":30}"#, &rt)
            .await
            .unwrap();
        assert!(out.starts_with("Reminder set for"), "{out}");
        assert!(out.contains("water the plants"));
        assert!(out.contains("[id "));
        let list = execute("list_reminders", "{}", &rt).await.unwrap();
        assert!(list.contains("water the plants"), "{list}");
        // Another member sees nothing.
        let other = ToolRuntime::from_tool_settings(&ToolSettings::default())
            .with_memory(rt.db.clone().unwrap(), "BOB");
        assert_eq!(execute("list_reminders", "{}", &other).await.unwrap(), "No reminders are set.");
    }

    #[tokio::test]
    async fn user_level_refusals_are_ok_text_not_errors() {
        // The loop treats Err as a broken web lookup and tells the family
        // their search failed — so a past time, a bad time, a missing time
        // and an unknown id must all come back as Ok(text) saying nothing
        // was set.
        let rt = runtime().await;
        for (args, needle) in [
            (r#"{"text":"too late","due_local":"2001-01-01T09:00"}"#, "in the past"),
            (r#"{"text":"far","in_minutes":999999}"#, "more than a year"),
            (r#"{"text":"soon","in_minutes":0}"#, "at least one minute"),
            (r#"{"text":"when?"}"#, "Tell me when"),
            (r#"{"text":"garbled","due_local":"nine-ish"}"#, "couldn't read that time"),
        ] {
            let out = execute("set_reminder", args, &rt).await.unwrap();
            assert!(out.contains(needle), "{args} → {out}");
            assert!(out.contains("Nothing was set") || out.contains("nothing was set"), "{out}");
        }
        assert_eq!(execute("list_reminders", "{}", &rt).await.unwrap(), "No reminders are set.");
        let out = execute("cancel_reminder", r#"{"id":"abcdef01"}"#, &rt).await.unwrap();
        assert!(out.contains("No live reminder"), "{out}");
        let out = execute("cancel_reminder", r#"{"id":"abc"}"#, &rt).await.unwrap();
        assert!(out.contains("at least 6 characters"), "{out}");
        // Real infrastructure failure stays an Err: no DB attached.
        let bare = ToolRuntime::from_tool_settings(&ToolSettings::default());
        assert!(execute("set_reminder", r#"{"text":"x","in_minutes":5}"#, &bare).await.is_err());
    }

    #[tokio::test]
    async fn an_absurd_in_minutes_is_refused_not_a_panic() {
        // chrono's `now + Duration::minutes(m)` PANICS on overflow, which
        // would take down the member's whole turn — no reply at all — so
        // the bound must be checked before the arithmetic.
        let rt = runtime().await;
        for m in [366i64 * 24 * 60 + 1, 525_600_000_000, 160_000_000_000_000, i64::MAX] {
            let out = execute("set_reminder", &format!(r#"{{"text":"far off","in_minutes":{m}}}"#), &rt)
                .await
                .expect("must be a refusal, never an Err or a panic");
            assert!(out.contains("more than a year"), "in_minutes={m} → {out}");
        }
        assert_eq!(execute("list_reminders", "{}", &rt).await.unwrap(), "No reminders are set.");
    }

    #[tokio::test]
    async fn over_long_text_is_a_refusal_not_a_failed_lookup() {
        // The DB bails above the cap; if that bail reached the loop as an
        // Err the family would be told every web lookup failed.
        let rt = runtime().await;
        let long = "x".repeat(crate::db::reminders::MAX_TEXT_CHARS + 1);
        let out = execute("set_reminder", &format!(r#"{{"text":"{long}","in_minutes":30}}"#), &rt)
            .await
            .expect("must be Ok(text), not Err");
        assert!(out.contains("too long"), "{out}");
        assert!(out.contains("nothing was set") || out.contains("Nothing was set"), "{out}");
        // Exactly at the cap still works.
        let ok = "y".repeat(crate::db::reminders::MAX_TEXT_CHARS);
        let out = execute("set_reminder", &format!(r#"{{"text":"{ok}","in_minutes":30}}"#), &rt)
            .await
            .unwrap();
        assert!(out.starts_with("Reminder set for"), "{out}");
    }

    #[tokio::test]
    async fn a_long_text_with_no_time_complains_about_the_length_first() {
        // Two problems at once: too long AND no time. Which sentence comes
        // back decides whether the model fixes the real problem or spends
        // a round trip adding a time that is about to be refused anyway.
        // Moving the guards into `reminders::spec` in 0.2.124 briefly
        // swapped this order; nothing else covered the combination.
        let rt = runtime().await;
        let long = "x".repeat(crate::db::reminders::MAX_TEXT_CHARS + 1);
        let out = execute("set_reminder", &format!(r#"{{"text":"{long}"}}"#), &rt).await.unwrap();
        assert!(out.contains("too long"), "{out}");
        assert!(!out.contains("Tell me when"), "{out}");
        // A blank text is still an infrastructure Err from argument
        // extraction, never the spec's Empty refusal.
        for blank in [r#"{"text":"   ","in_minutes":5}"#, r#"{"in_minutes":5}"#] {
            assert!(execute("set_reminder", blank, &rt).await.is_err(), "{blank}");
        }
    }

    #[tokio::test]
    async fn the_list_hides_history_and_cancel_explains_itself() {
        let rt = runtime().await;
        let db = rt.db.clone().unwrap();
        let out = execute("set_reminder", r#"{"text":"pick up the parcel","in_minutes":60}"#, &rt)
            .await
            .unwrap();
        let short = out.rsplit("[id ").next().unwrap().trim_end_matches(']').trim().to_string();
        // Acknowledge it: it leaves the model's list but stays in the Calendar.
        let full = db.list_reminders("ALICE").await.unwrap()[0].id.clone();
        assert!(db.apply_reminder_action("ALICE", &full, "ack", 0).await.unwrap().is_some());
        assert_eq!(execute("list_reminders", "{}", &rt).await.unwrap(), "No reminders are set.");
        assert_eq!(db.list_reminders("ALICE").await.unwrap().len(), 1, "Calendar keeps it");
        // Cancelling it now says so honestly instead of "no such reminder".
        let out = execute("cancel_reminder", &format!(r#"{{"id":"{short}"}}"#), &rt).await.unwrap();
        assert!(out.contains("already done"), "{out}");
        assert!(out.contains("pick up the parcel"), "{out}");
    }

    #[tokio::test]
    async fn a_wildcard_id_cannot_cancel_an_arbitrary_reminder() {
        let rt = runtime().await;
        execute("set_reminder", r#"{"text":"keep me","in_minutes":30}"#, &rt).await.unwrap();
        for probe in ["______", "%%%%%%", "%"] {
            let out = execute("cancel_reminder", &format!(r#"{{"id":"{probe}"}}"#), &rt).await.unwrap();
            assert!(out.contains("at least 6 characters"), "{probe} → {out}");
        }
        assert!(execute("list_reminders", "{}", &rt).await.unwrap().contains("keep me"));
    }

    #[tokio::test]
    async fn cancel_by_short_id_from_the_confirmation() {
        let rt = runtime().await;
        let out = execute("set_reminder", r#"{"text":"call the dentist","in_minutes":90}"#, &rt)
            .await
            .unwrap();
        let short = out.rsplit("[id ").next().unwrap().trim_end_matches(']').trim().to_string();
        assert_eq!(short.len(), 8, "confirmation carries the short id: {out}");
        let out = execute("cancel_reminder", &format!(r#"{{"id":"{short}"}}"#), &rt)
            .await
            .unwrap();
        assert!(out.starts_with("Cancelled: call the dentist"), "{out}");
        assert_eq!(execute("list_reminders", "{}", &rt).await.unwrap(), "No reminders are set.");
    }

    #[tokio::test]
    async fn a_reminder_keeps_the_conversation_it_came_from() {
        // "remind me about your last answer" is useless if the reminder
        // cannot get the member back to the answer. The turn's thread and
        // the message that prompted it ride along on the row.
        let rt = runtime().await.with_thread("thread-42");
        execute("set_reminder", r#"{"text":"the gist of what was said","in_minutes":30}"#, &rt)
            .await
            .unwrap();
        let r = &rt.db.clone().unwrap().list_reminders("ALICE").await.unwrap()[0];
        assert_eq!(r.thread_id.as_deref(), Some("thread-42"));
        assert_eq!(r.source_msg_id.as_deref(), Some("msg-1"));

        // A runtime with no thread (an isolated pass) still works.
        let bare = ToolRuntime::from_tool_settings(&ToolSettings::default())
            .with_memory(rt.db.clone().unwrap(), "BOB");
        execute("set_reminder", r#"{"text":"no thread here","in_minutes":30}"#, &bare)
            .await
            .unwrap();
        let b = &bare.db.clone().unwrap().list_reminders("BOB").await.unwrap()[0];
        assert!(b.thread_id.is_none());
    }

    #[tokio::test]
    async fn absolute_time_uses_the_members_zone() {
        let rt = runtime().await;
        let db = rt.db.clone().unwrap();
        // The member's saved fact decides the zone (no device zone in a test).
        db.save_user_fact("ALICE", "timezone", "Asia/Tokyo", "manual", None)
            .await
            .unwrap();
        let next_year = { use chrono::Datelike; chrono::Utc::now().date_naive().year() + 1 };
        let args = format!(r#"{{"text":"new year plan","due_local":"{next_year}-01-02T09:00"}}"#);
        let out = execute("set_reminder", &args, &rt).await.unwrap();
        assert!(out.contains("(Asia/Tokyo)"), "{out}");
        let r = &db.list_reminders("ALICE").await.unwrap()[0];
        assert_eq!(r.due_local, format!("{next_year}-01-02T09:00"));
        assert_eq!(r.tz, "Asia/Tokyo");
        assert!(r.due_at.contains(&format!("{next_year}-01-02T00:00")), "09:00 Tokyo = 00:00 UTC: {}", r.due_at);
    }
}
