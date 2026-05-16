//! Tool catalogue — generated dynamically from `ToolSettings` so users can
//! disable tools their model shouldn't be allowed to call.

use anyhow::{anyhow, Result};
use serde_json::json;

use crate::config::{SearchEngine, ToolSettings};

/// Runtime context the tool layer needs that isn't in the static schema —
/// API keys, search-engine selection, etc.
#[derive(Debug, Clone, Default)]
pub struct ToolRuntime {
    pub search_engine: SearchEngine,
    pub search_api_key: Option<String>,
}

impl ToolRuntime {
    pub fn from_tool_settings(s: &ToolSettings) -> Self {
        Self {
            search_engine: s.search_engine,
            search_api_key: s.search_api_key.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

pub fn enabled(settings: &ToolSettings) -> Vec<ToolDef> {
    let mut out = Vec::new();
    if settings.web_search {
        out.push(web_search_def());
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
                5,
                runtime.search_engine,
                runtime.search_api_key.as_deref(),
            )
            .await
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
    ToolDef {
        name: "web_search".into(),
        description: "Search the internet and return up-to-date results.".into(),
        schema: json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the internet and return up-to-date results.",
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
