//! Tool catalogue — generated dynamically from `ToolSettings` so users can
//! disable tools their model shouldn't be allowed to call.

use anyhow::{anyhow, Result};
use serde_json::json;

use crate::config::{SearchEngine, ToolSettings};
use crate::db::Db;

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
        "datetime" => Ok(super::datetime::now_pretty()),
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
                "description": "Get the current local date and time (host machine's timezone).",
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
