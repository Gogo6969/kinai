//! Social / discussion search.
//!
//! Despite the legacy name `x_search` (kept for prompt + config stability),
//! this tool dispatches on the user-configured `SearchEngine` exactly like
//! `web_search` — so the engine choice in Settings applies to every search
//! tool, not just one.
//!
//! Routes:
//!   * **Exa** + API key → `POST /search` with `category: "tweet"`. Returns
//!     real X / Twitter posts indexed by Exa, plus high-quality re-ranking.
//!   * **DuckDuckGo** (no key required) → falls back to **Hacker News
//!     Algolia** (`https://hn.algolia.com/api/v1/search`). Narrower than
//!     Twitter but the only large, open, no-auth, no-WAF source of
//!     technical / news discussion.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::time::Duration;

use crate::config::SearchEngine;

const HN_SEARCH: &str = "https://hn.algolia.com/api/v1/search";
const HN_SEARCH_BY_DATE: &str = "https://hn.algolia.com/api/v1/search_by_date";

pub async fn search(
    query: &str,
    mode: &str,
    max_results: usize,
    engine: SearchEngine,
    api_key: Option<&str>,
) -> Result<String> {
    match engine {
        SearchEngine::Exa => match api_key {
            Some(k) if !k.trim().is_empty() => exa_social(query, max_results, k).await,
            _ => Err(anyhow!(
                "Exa is selected but no API key is configured. Open Settings → \
Search engine and paste your key, or pick DuckDuckGo to fall back to HN."
            )),
        },
        SearchEngine::Duckduckgo => hn_search(query, mode, max_results).await,
    }
}

// ---- Exa (category=tweet) ------------------------------------------------

#[derive(Debug, Deserialize)]
struct ExaResponse {
    results: Vec<ExaResult>,
}

#[derive(Debug, Deserialize)]
struct ExaResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default, rename = "publishedDate")]
    published_date: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    summary: Option<String>,
}

async fn exa_social(query: &str, max_results: usize, api_key: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    // Exa's `category: "tweet"` returns 400 on Free / lower tiers. Domain
    // allowlisting works on every tier and is closer to what users mean by
    // "social search" anyway — it pulls from the major social/discussion
    // platforms regardless of Exa's category taxonomy.
    let body = serde_json::json!({
        "query": query,
        "numResults": max_results.clamp(1, 25),
        "type": "auto",
        "includeDomains": [
            "x.com",
            "twitter.com",
            "bsky.app",
            "mastodon.social",
            "reddit.com",
            "news.ycombinator.com",
            "lobste.rs"
        ],
        "contents": {
            "text": { "maxCharacters": 400 }
        }
    });
    let resp = client
        .post("https://api.exa.ai/search")
        .header("x-api-key", api_key)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "Exa social search failed (HTTP {status}). Response body: {body_text}"
        );
    }
    let parsed: ExaResponse = resp.json().await?;
    if parsed.results.is_empty() {
        return Ok("No matching posts on Exa for that query.".into());
    }
    let out: Vec<String> = parsed
        .results
        .into_iter()
        .take(max_results)
        .enumerate()
        .map(|(i, r)| {
            let author = r
                .author
                .as_deref()
                .filter(|a| !a.trim().is_empty())
                .map(|a| format!("@{a} ").to_string())
                .unwrap_or_default();
            let title = r
                .title
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .map(|t| t.trim().to_string())
                .unwrap_or_default();
            let body = r
                .summary
                .filter(|s| !s.trim().is_empty())
                .or(r.text)
                .map(|s| truncate(&s, 300))
                .unwrap_or_default();
            let date = r
                .published_date
                .as_deref()
                .map(|d| format!(" · {}", &d[..d.len().min(10)]))
                .unwrap_or_default();
            let url = r.url.unwrap_or_default();
            format!(
                "{}. {}{}{}\n   {}\n   {}",
                i + 1,
                author,
                title,
                date,
                body,
                url
            )
        })
        .collect();
    Ok(out.join("\n\n"))
}

// ---- Hacker News (fallback) ---------------------------------------------

async fn hn_search(query: &str, mode: &str, max_results: usize) -> Result<String> {
    let user_agent = format!(
        "Mozilla/5.0 (compatible; KinAI/{} +{})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY"),
    );
    let client = reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(8))
        .build()?;
    let endpoint = if mode == "semantic" {
        HN_SEARCH_BY_DATE
    } else {
        HN_SEARCH
    };
    let url = format!(
        "{}?query={}&hitsPerPage={}&tags=(story,comment)",
        endpoint,
        urlencode(query),
        max_results.clamp(1, 25),
    );
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("hacker news search responded {}", resp.status());
    }
    let value: serde_json::Value = resp.json().await?;
    let mut posts = parse_hits(&value);
    if mode == "semantic" {
        posts = rerank(query, posts);
    }
    Ok(format_hn(posts, max_results))
}

#[derive(Debug, Clone)]
struct HnPost {
    title: String,
    author: String,
    points: i64,
    comments: i64,
    text: String,
    url: String,
    hn_url: String,
    created_at: String,
}

fn parse_hits(value: &serde_json::Value) -> Vec<HnPost> {
    let Some(hits) = value.get("hits").and_then(|p| p.as_array()) else {
        return Vec::new();
    };
    hits.iter()
        .filter_map(|h| {
            let id = h.get("objectID").and_then(|v| v.as_str()).unwrap_or("");
            let title = h
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let author = h
                .get("author")
                .and_then(|v| v.as_str())
                .unwrap_or("anon")
                .to_string();
            let points = h.get("points").and_then(|v| v.as_i64()).unwrap_or(0);
            let comments = h.get("num_comments").and_then(|v| v.as_i64()).unwrap_or(0);
            let url = h
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let raw_text = h
                .get("comment_text")
                .or_else(|| h.get("story_text"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let text = strip_html(raw_text);
            let created_at = h
                .get("created_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let hn_url = format!("https://news.ycombinator.com/item?id={}", id);
            if title.is_empty() && text.trim().is_empty() {
                return None;
            }
            Some(HnPost {
                title,
                author,
                points,
                comments,
                text,
                url,
                hn_url,
                created_at,
            })
        })
        .collect()
}

fn rerank(query: &str, mut posts: Vec<HnPost>) -> Vec<HnPost> {
    let q_terms: HashSet<String> = query
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() > 2)
        .collect();
    posts.sort_by(|a, b| {
        let a_s = overlap(&a.title, &q_terms) * 2 + overlap(&a.text, &q_terms);
        let b_s = overlap(&b.title, &q_terms) * 2 + overlap(&b.text, &q_terms);
        b_s.cmp(&a_s)
    });
    posts
}

fn overlap(text: &str, q_terms: &HashSet<String>) -> usize {
    text.split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| q_terms.contains(w))
        .count()
}

fn format_hn(posts: Vec<HnPost>, max: usize) -> String {
    if posts.is_empty() {
        return "No matching discussions on Hacker News.".into();
    }
    posts
        .into_iter()
        .take(max)
        .enumerate()
        .map(|(i, p)| {
            let title = if p.title.is_empty() {
                format!("(comment) {}", first_sentence(&p.text, 140))
            } else {
                p.title.clone()
            };
            let snippet = if p.text.trim().is_empty() {
                String::new()
            } else {
                format!("\n   {}", first_sentence(&p.text, 200))
            };
            let link = if !p.url.is_empty() {
                p.url.clone()
            } else {
                p.hn_url.clone()
            };
            format!(
                "{}. {} — by @{} · {} pts · {} comments · {}\n   {}{}\n   discussion: {}",
                i + 1,
                title,
                p.author,
                p.points,
                p.comments,
                &p.created_at[..p.created_at.len().min(10)],
                link,
                snippet,
                p.hn_url
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

// ---- Shared utils --------------------------------------------------------

fn truncate(s: &str, max_chars: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max_chars {
        return t.replace('\n', " ");
    }
    let mut out: String = t.chars().take(max_chars).collect();
    out = out.replace('\n', " ");
    out.push('…');
    out
}

fn first_sentence(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    let end = trimmed
        .find(|c: char| c == '.' || c == '?' || c == '!' || c == '\n')
        .map(|i| i + 1)
        .unwrap_or(trimmed.len());
    let mut out = trimmed[..end].trim().to_string();
    if out.chars().count() > max {
        out = out.chars().take(max).collect::<String>();
        out.push('…');
    }
    out
}

fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}
