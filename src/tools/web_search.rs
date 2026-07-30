//! Web search dispatcher.
//!
//! The user picks the engine in Settings:
//!   * `DuckDuckGo` (default) — free HTML scrape with a Wikipedia fallback
//!     when DDG throttles. Zero config.
//!   * `Exa` — high-quality semantic search aimed at AI agents
//!     (https://exa.ai). Requires an API key, and is metered.
//!   * `SearXNG` — the family's own metasearch instance. Free, no key, and
//!     the only option that keeps a family member's query on their own
//!     hardware, which is the promise the rest of the product makes.
//!     Needs `formats: [json]` enabled on the instance.
//!
//! Exa additionally falls back to SearXNG when it is permanently
//! unavailable (credits exhausted, key rejected) rather than letting the
//! whole family lose search — see `search`.
//!
//! The engines have very different result shapes; we normalize to a
//! single text block formatted as `N. <title>\n   <snippet>\n   <url>` so
//! the LLM sees the same surface regardless of backend.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::time::Duration;

use crate::config::SearchEngine;

pub async fn search(
    query: &str,
    max_results: usize,
    engine: SearchEngine,
    api_key: Option<&str>,
    searxng_url: &str,
    fallback_to_searxng: bool,
) -> Result<String> {
    match engine {
        SearchEngine::Duckduckgo => duckduckgo_with_fallback(query, max_results).await,
        SearchEngine::Searxng => searxng_search(query, max_results, searxng_url).await,
        SearchEngine::Exa => {
            let primary = match api_key {
                Some(key) if !key.trim().is_empty() => exa_search(query, max_results, key).await,
                _ => Err(anyhow!(
                    "Exa is selected as the search engine but no API key is configured. \
Open Settings → Search engine and paste your key from https://exa.ai, or pick \
DuckDuckGo to skip the key requirement."
                )),
            };
            match primary {
                Ok(out) => Ok(out),
                // Out of credits, or a key Exa refuses. Retrying Exa cannot
                // help — but the family's own instance costs nothing and is
                // already on the LAN, so the turn need not go dark. Only
                // permanent failures land here: timeouts and 5xx are retried
                // inside `exa_search` and never reach this point.
                Err(e) if fallback_to_searxng && is_permanently_unavailable(&e) => {
                    if searxng_url.trim().is_empty() {
                        return Err(e);
                    }
                    tracing::warn!(
                        "Exa unavailable ({e:#}); falling back to SearXNG at {}",
                        searxng_url.trim()
                    );
                    match searxng_search(query, max_results, searxng_url).await {
                        // Say which engine answered. Without this the model
                        // cannot tell the user that its paid search is down,
                        // and the host never learns to top it up.
                        Ok(out) => Ok(format!(
                            "(Exa is unavailable — {}. These results come from your own \
SearXNG instead.)\n{out}",
                            short_reason(&e)
                        )),
                        // Report the ORIGINAL failure too: "SearXNG refused"
                        // alone would send the host looking at the wrong box.
                        Err(fb) => Err(anyhow!(
                            "Exa failed ({e:#}) and the SearXNG fallback also failed ({fb:#})"
                        )),
                    }
                }
                Err(e) => Err(e),
            }
        }
    }
}

/// Errors that will not resolve by trying the same engine again: billing,
/// authentication, and missing configuration.
fn is_permanently_unavailable(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}").to_lowercase();
    ["(401", "(402", "(403", "no_more_credits", "credits limit", "payment required",
     "no api key is configured", "invalid api key"]
        .iter()
        .any(|m| msg.contains(m))
}

/// One short clause for the note shown above fallback results.
fn short_reason(e: &anyhow::Error) -> &'static str {
    let msg = format!("{e:#}").to_lowercase();
    if msg.contains("no_more_credits") || msg.contains("credits limit") || msg.contains("(402") {
        "its credits are used up"
    } else if msg.contains("(401") || msg.contains("invalid api key") {
        "its API key was rejected"
    } else if msg.contains("no api key is configured") {
        "no API key is configured"
    } else {
        "it is unavailable"
    }
}

// ---- SearXNG (self-hosted metasearch) ------------------------------------

#[derive(Debug, Deserialize)]
struct SearxResponse {
    #[serde(default)]
    results: Vec<SearxResult>,
}

#[derive(Debug, Deserialize)]
struct SearxResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    /// The snippet. SearXNG passes through whatever the upstream engine
    /// gave it, so this is short (150–400 chars observed) and can be
    /// stale — it is a cached engine snippet, not a fetch of the page.
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default, rename = "publishedDate")]
    published_date: Option<String>,
}

/// Query a family-run SearXNG.
///
/// Needs `formats: [json]` in the instance's `settings.yml`; SearXNG
/// disables JSON by default and answers HTML instead, which is why a
/// parse failure here says so explicitly rather than reporting "no
/// results" and letting the model invent an answer.
async fn searxng_search(query: &str, max_results: usize, base_url: &str) -> Result<String> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        anyhow::bail!(
            "SearXNG is selected as the search engine but no URL is configured. \
Open Settings → Search engine and enter your instance's address (e.g. http://127.0.0.1:8888)."
        );
    }
    let url = format!("{base}/search?q={}&format=json", urlencode(query));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let resp = client
        .get(&url)
        .header("User-Agent", user_agent())
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!(
            "SearXNG at {base} responded {status}. If this is 403, the instance is \
refusing API requests — check `search.formats` includes `json` in settings.yml."
        );
    }
    let body = resp.text().await?;
    let parsed: SearxResponse = serde_json::from_str(&body).map_err(|e| {
        anyhow!(
            "SearXNG at {base} did not return JSON ({e}). Enable the JSON format in \
its settings.yml (`search: formats: [html, json]`) and restart it."
        )
    })?;
    if parsed.results.is_empty() {
        return Ok("No results.".into());
    }
    let out: Vec<String> = parsed
        .results
        .into_iter()
        .take(max_results)
        .enumerate()
        .map(|(i, r)| {
            let title = r.title.unwrap_or_else(|| "(untitled)".into());
            let url = r.url.unwrap_or_default();
            let snippet = r.content.unwrap_or_default();
            let date = r
                .published_date
                .as_deref()
                .map(|d| format!(" · {}", &d[..d.len().min(10)]))
                .unwrap_or_default();
            let via = r
                .engine
                .as_deref()
                .filter(|e| !e.is_empty())
                .map(|e| format!(" [{e}]"))
                .unwrap_or_default();
            format!("{}. {title}{date}{via}\n   {snippet}\n   {url}", i + 1)
        })
        .collect();
    Ok(out.join("\n"))
}

// ---- DuckDuckGo (free, scrape) -------------------------------------------

async fn duckduckgo_with_fallback(query: &str, max_results: usize) -> Result<String> {
    if let Ok(text) = duckduckgo(query, max_results).await {
        if !text.trim().is_empty() && !text.contains("No results found") {
            return Ok(text);
        }
    }
    wikipedia(query, max_results).await
}

async fn duckduckgo(query: &str, max: usize) -> Result<String> {
    let url = format!("https://duckduckgo.com/html/?q={}", urlencode(query));
    let resp = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", user_agent())
        .send()
        .await?
        .text()
        .await?;
    Ok(extract_ddg(&resp, max))
}

/// Build a User-Agent string from cargo manifest metadata so it tracks the
/// project's actual version + repo URL — no hardcoded placeholders.
fn user_agent() -> String {
    format!(
        "Mozilla/5.0 (compatible; KinAI/{} +{})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY"),
    )
}

async fn wikipedia(query: &str, max: usize) -> Result<String> {
    let url = format!(
        "https://en.wikipedia.org/w/api.php?action=query&format=json&list=search&srsearch={}&srlimit={}",
        urlencode(query),
        max
    );
    let resp = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", user_agent())
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    let mut out = Vec::new();
    if let Some(items) = resp["query"]["search"].as_array() {
        for (i, item) in items.iter().take(max).enumerate() {
            let title = item["title"].as_str().unwrap_or("");
            let snippet = item["snippet"].as_str().unwrap_or("");
            let clean = strip_tags(snippet);
            out.push(format!(
                "{}. {} — {}\n   https://en.wikipedia.org/wiki/{}",
                i + 1,
                title,
                clean,
                title.replace(' ', "_")
            ));
        }
    }
    Ok(if out.is_empty() {
        "No results found.".into()
    } else {
        out.join("\n")
    })
}

fn extract_ddg(html: &str, max: usize) -> String {
    const NEEDLE: &str = "result__snippet";
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(idx) = html[cursor..].find(NEEDLE) {
        cursor += idx + NEEDLE.len();
        if let Some(gt) = html[cursor..].find('>') {
            cursor += gt + 1;
            if let Some(end) = html[cursor..].find("</a>") {
                let snippet = strip_tags(&html[cursor..cursor + end]);
                if !snippet.trim().is_empty() {
                    out.push(snippet.trim().to_string());
                    if out.len() >= max {
                        break;
                    }
                }
                cursor += end + 4;
            }
        }
    }
    if out.is_empty() {
        "No results found.".into()
    } else {
        out.into_iter()
            .enumerate()
            .map(|(i, s)| format!("{}. {}", i + 1, s))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ---- Exa (https://exa.ai) ------------------------------------------------

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
    #[serde(default)]
    score: Option<f64>,
}

/// One retry for transient failures, then give up.
///
/// A search that fails costs the whole turn: the model gets a TOOL FAILED
/// payload and the user gets "the lookup failed" for a question the engine
/// could have answered a second later. Measured Exa latency for KinAI's
/// payload (which requests per-result summaries) ranges from 0.1s to ~7s
/// against a 15s ceiling, so a slow moment or a dropped connection lands
/// close enough to the edge to be worth one more attempt.
///
/// Deliberately NOT retried: auth and request errors (401/403/400/404).
/// Those fail identically on the second try — retrying only doubles the
/// wait before the user is told their API key is wrong. 429 IS retried,
/// after a longer pause, since it clears on its own.
async fn exa_search(query: &str, max_results: usize, api_key: &str) -> Result<String> {
    match exa_search_once(query, max_results, api_key).await {
        Ok(r) => Ok(r),
        Err(e) if is_transient(&e) => {
            let pause = if is_rate_limited(&e) { 2000 } else { 400 };
            tracing::warn!("exa search failed ({e:#}); retrying once in {pause}ms");
            tokio::time::sleep(Duration::from_millis(pause)).await;
            exa_search_once(query, max_results, api_key).await
        }
        Err(e) => Err(e),
    }
}

/// Timeouts, connection drops, 5xx and 429 are worth another attempt;
/// anything else is a stable "no".
fn is_transient(e: &anyhow::Error) -> bool {
    if is_rate_limited(e) {
        return true;
    }
    if let Some(re) = e.downcast_ref::<reqwest::Error>() {
        if re.is_timeout() || re.is_connect() || re.is_request() {
            return true;
        }
    }
    let msg = format!("{e:#}");
    msg.contains("(500")
        || msg.contains("(502")
        || msg.contains("(503")
        || msg.contains("(504")
        || msg.contains("timed out")
        || msg.contains("connection")
}

fn is_rate_limited(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("(429")
}

async fn exa_search_once(query: &str, max_results: usize, api_key: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let body = serde_json::json!({
        "query": query,
        "numResults": max_results.clamp(1, 25),
        "type": "auto",
        "contents": {
            "text": { "maxCharacters": 400 },
            "summary": { "query": query }
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
        let err_body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Exa search failed ({status}): {err_body}");
    }
    let parsed: ExaResponse = resp.json().await?;
    if parsed.results.is_empty() {
        return Ok("No results.".into());
    }
    let out: Vec<String> = parsed
        .results
        .into_iter()
        .take(max_results)
        .enumerate()
        .map(|(i, r)| {
            let title = r.title.unwrap_or_else(|| "(untitled)".into());
            let url = r.url.unwrap_or_default();
            let date = r
                .published_date
                .as_deref()
                .map(|d| format!(" · {}", &d[..d.len().min(10)]))
                .unwrap_or_default();
            let author = r
                .author
                .as_deref()
                .filter(|a| !a.trim().is_empty())
                .map(|a| format!(" · {a}"))
                .unwrap_or_default();
            let body = r
                .summary
                .filter(|s| !s.trim().is_empty())
                .or(r.text)
                .map(|s| truncate(&s, 350))
                .unwrap_or_default();
            let score = r
                .score
                .map(|s| format!(" · score {s:.2}"))
                .unwrap_or_default();
            format!(
                "{}. {}{}{}{}\n   {}\n   {}",
                i + 1,
                title.trim(),
                date,
                author,
                score,
                body,
                url
            )
        })
        .collect();
    Ok(out.join("\n\n"))
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

fn strip_tags(s: &str) -> String {
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
}

#[cfg(test)]
mod retry_tests {
    use super::{is_rate_limited, is_transient};

    #[test]
    fn transient_failures_are_retried() {
        for msg in [
            "Exa search failed (500): upstream error",
            "Exa search failed (502): bad gateway",
            "Exa search failed (503): unavailable",
            "Exa search failed (504): gateway timeout",
            "Exa search failed (429): slow down",
            "operation timed out",
            "error sending request: connection closed",
        ] {
            assert!(
                is_transient(&anyhow::anyhow!("{msg}")),
                "should retry: {msg}"
            );
        }
    }

    #[test]
    fn stable_failures_are_not_retried() {
        // Retrying these only doubles the wait before the user learns
        // their key is wrong — the second attempt fails identically.
        for msg in [
            "Exa search failed (401): invalid API key",
            "Exa search failed (403): forbidden",
            "Exa search failed (400): bad request",
            "Exa search failed (404): not found",
            "Exa is selected as the search engine but no API key is configured.",
        ] {
            assert!(
                !is_transient(&anyhow::anyhow!("{msg}")),
                "should NOT retry: {msg}"
            );
        }
    }

    #[test]
    fn rate_limit_is_detected_for_the_longer_backoff() {
        assert!(is_rate_limited(&anyhow::anyhow!("Exa search failed (429): x")));
        assert!(!is_rate_limited(&anyhow::anyhow!("Exa search failed (500): x")));
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::{is_permanently_unavailable, short_reason};

    #[test]
    fn credit_and_auth_failures_trigger_the_fallback() {
        for (msg, reason) in [
            // The exact 2026-07-29 outage.
            ("Exa search failed (402 Payment Required): {\"error\":\"You have exceeded your \
credits limit\",\"tag\":\"NO_MORE_CREDITS\"}", "its credits are used up"),
            ("Exa search failed (401): invalid API key", "its API key was rejected"),
            ("Exa is selected as the search engine but no API key is configured.",
             "no API key is configured"),
        ] {
            let e = anyhow::anyhow!("{msg}");
            assert!(is_permanently_unavailable(&e), "should fall back: {msg}");
            assert_eq!(short_reason(&e), reason);
        }
    }

    #[test]
    fn transient_failures_do_not_fall_back() {
        // These are retried on Exa itself; silently switching engines would
        // hide a blip and make results inconsistent for no reason.
        for msg in [
            "Exa search failed (500): upstream error",
            "Exa search failed (503): unavailable",
            "Exa search failed (429): slow down",
            "operation timed out",
            "error sending request: connection closed",
        ] {
            assert!(
                !is_permanently_unavailable(&anyhow::anyhow!("{msg}")),
                "must stay on Exa: {msg}"
            );
        }
    }
}
