//! Image search dispatcher.
//!
//! Returns a markdown-formatted list of image hits the LLM can drop
//! straight into its reply (the frontend renders inline `![](url)`
//! safely — see `frontend/src/lib/markdown.ts`). Two backends:
//!
//!   * `Duckduckgo` (default) — we use Wikimedia Commons as the
//!     zero-config image source. Reliable, free, returns CC-licensed
//!     images. Covers landmarks, people, history, biology, etc. very
//!     well; thinner for product/celebrity shots.
//!   * `Exa` — uses the regular Exa search with `contents.extras.
//!     imageLinks` enrichment so each result page contributes its primary
//!     image. Same API key as the rest of Exa (web_search, x_search).
//!     Falls back to Wikimedia Commons when Exa returns no images at all,
//!     so a quiet API change or exhausted credits still leaves the family
//!     with pictures rather than nothing.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::time::Duration;

use crate::config::SearchEngine;

pub async fn search(
    query: &str,
    max_results: usize,
    engine: SearchEngine,
    api_key: Option<&str>,
) -> Result<String> {
    match engine {
        // SearXNG's JSON results carry thumbnails, not the licensed
        // full-size originals this tool needs, so image search uses the
        // same Wikimedia Commons source as DuckDuckGo mode.
        SearchEngine::Duckduckgo | SearchEngine::Searxng => {
            wikimedia_commons(query, max_results).await
        }
        SearchEngine::Exa => match api_key {
            Some(key) if !key.trim().is_empty() => exa_images(query, max_results, key).await,
            _ => Err(anyhow!(
                "Exa is selected as the search engine but no API key is set. \
Open Settings → Search engine and paste your key, or pick DuckDuckGo \
(uses Wikimedia Commons under the hood for image search)."
            )),
        },
    }
}

// ---- Wikimedia Commons (zero-config) -------------------------------------

#[derive(Deserialize)]
struct CommonsResp {
    query: Option<CommonsQuery>,
}

#[derive(Deserialize)]
struct CommonsQuery {
    pages: Option<std::collections::HashMap<String, CommonsPage>>,
}

#[derive(Deserialize)]
struct CommonsPage {
    title: String,
    imageinfo: Option<Vec<CommonsImageInfo>>,
    #[serde(default)]
    index: Option<i64>,
}

#[derive(Deserialize)]
struct CommonsImageInfo {
    url: String,
    #[serde(rename = "descriptionurl")]
    description_url: Option<String>,
    mime: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
}

async fn wikimedia_commons(query: &str, max: usize) -> Result<String> {
    let url = format!(
        "https://commons.wikimedia.org/w/api.php?action=query&format=json\
&generator=search&gsrsearch={}&gsrnamespace=6&gsrlimit={}\
&prop=imageinfo&iiprop=url%7Csize%7Cmime&iiurlwidth=600",
        urlencode(query),
        max.max(1).min(15)
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()?;
    let resp = client
        .get(&url)
        .header("User-Agent", user_agent())
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "Wikimedia Commons returned {}",
            resp.status()
        ));
    }
    let parsed: CommonsResp = resp.json().await?;
    let Some(pages) = parsed.query.and_then(|q| q.pages) else {
        return Ok(format!("No images found for \"{}\".", query));
    };
    // Wikimedia returns pages keyed by id, with `index` controlling the
    // sort order from the search. Honor it so result 1 is the top hit.
    let mut pages_vec: Vec<CommonsPage> = pages.into_values().collect();
    pages_vec.sort_by_key(|p| p.index.unwrap_or(i64::MAX));

    let mut out = format!("Found these images for \"{}\":\n\n", query);
    let mut count = 0;
    for page in pages_vec.into_iter().take(max) {
        let Some(info) = page.imageinfo.as_ref().and_then(|v| v.first()) else {
            continue;
        };
        // Filter out non-image MIME types (Commons indexes PDFs, audio,
        // etc. in the File namespace too).
        let is_image = info
            .mime
            .as_deref()
            .map(|m| m.starts_with("image/"))
            .unwrap_or(false);
        if !is_image {
            continue;
        }
        count += 1;
        let alt = page.title.trim_start_matches("File:").trim_end_matches(|c: char| {
            // Strip extension for cleaner alt text
            c == '.' || c.is_ascii_alphanumeric()
        });
        let alt = if alt.is_empty() {
            page.title.clone()
        } else {
            page.title
                .trim_start_matches("File:")
                .rsplit_once('.')
                .map(|(stem, _)| stem.to_string())
                .unwrap_or_else(|| page.title.clone())
        };
        let dims = match (info.width, info.height) {
            (Some(w), Some(h)) => format!(" ({}×{})", w, h),
            _ => String::new(),
        };
        let page_link = info
            .description_url
            .clone()
            .unwrap_or_else(|| info.url.clone());
        // The format the LLM should echo verbatim — frontend renders the
        // `![](url)` lines as inline `<img>` and the `[caption](page)` as
        // a link. Caption + source on a separate line lets the model
        // explain context without breaking the image embed.
        out.push_str(&format!(
            "![{alt}]({img})\n_{alt}{dims} — [Wikimedia Commons]({page})_\n\n",
            alt = alt.replace('[', "(").replace(']', ")"),
            img = info.url,
            page = page_link,
        ));
    }
    if count == 0 {
        return Ok(format!("No images found for \"{}\".", query));
    }
    Ok(out)
}

// ---- Exa (paid, image-aware) ---------------------------------------------

#[derive(Deserialize)]
struct ExaResp {
    results: Vec<ExaResult>,
}

#[derive(Deserialize)]
struct ExaResult {
    title: Option<String>,
    url: String,
    image: Option<String>,
    #[serde(default)]
    author: Option<String>,
}

/// The request body Exa actually honours for images.
///
/// Extracted so a test can pin its shape: this is exactly what broke on
/// 2026-09-05 and it broke SILENTLY — Exa kept returning 200 with normal
/// results and simply omitted each result's `image`, so every picture
/// request became "No images found" with a successful tool call in the
/// log. Nothing surfaced until a family member asked for a photo.
fn exa_image_request(query: &str, max: usize) -> serde_json::Value {
    serde_json::json!({
        "query": query,
        "numResults": max.clamp(1, 15),
        "contents": {
            // We don't need the page text, just the headline image.
            "text": false,
            // `extras.imageLinks` is the switch that makes Exa populate
            // the top-level `image` we filter on below. The older
            // `"images": 1` is ignored — verified against the live API:
            // old body -> keys [id,title,url]; this one -> [extras,id,
            // image,title,url].
            "extras": { "imageLinks": 1 }
        }
    })
}

async fn exa_images(query: &str, max: usize, api_key: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let body = exa_image_request(query, max);
    let resp = client
        .post("https://api.exa.ai/search")
        .header("x-api-key", api_key)
        .header("Content-Type", "application/json")
        .header("User-Agent", user_agent())
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Exa returned {}: {}", status, body));
    }
    let parsed: ExaResp = resp.json().await?;
    let with_images: Vec<&ExaResult> = parsed
        .results
        .iter()
        .filter(|r| r.image.as_deref().map(|s| !s.is_empty()).unwrap_or(false))
        .collect();
    if with_images.is_empty() {
        // Don't dead-end on the paid engine. Wikimedia Commons needs no
        // key and is already this tool's source in DuckDuckGo/SearXNG
        // mode, so a family asking for a picture still gets one when Exa
        // returns nothing — whether that is a quiet API change (as in
        // 2026-09-05), an empty result set, or exhausted credits.
        tracing::warn!(
            query,
            "Exa returned no images; falling back to Wikimedia Commons"
        );
        return wikimedia_commons(query, max).await;
    }
    let mut out = format!("Found these images for \"{}\":\n\n", query);
    for r in with_images.iter().take(max) {
        let title = r
            .title
            .clone()
            .unwrap_or_else(|| "Untitled".into())
            .replace('[', "(")
            .replace(']', ")");
        let author = r
            .author
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|a| format!(" — {a}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "![{alt}]({img})\n_[{alt}]({page}){author}_\n\n",
            alt = title,
            img = r.image.as_deref().unwrap_or(""),
            page = r.url,
        ));
    }
    Ok(out)
}

// ---- Shared helpers -------------------------------------------------------

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

fn user_agent() -> String {
    format!(
        "Mozilla/5.0 (compatible; KinAI/{} +{})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY"),
    )
}

#[cfg(test)]
mod exa_request_tests {
    use super::exa_image_request;

    /// Guards the 2026-09-05 silent breakage. If someone reverts to
    /// `contents.images`, Exa answers 200 with no `image` field and every
    /// picture request quietly becomes "No images found" — the failure
    /// has no error, no log line and no test to catch it but this one.
    #[test]
    fn the_request_asks_for_images_the_way_exa_still_honours() {
        let body = exa_image_request("Ada Lovelace", 5);
        let contents = &body["contents"];
        assert_eq!(
            contents["extras"]["imageLinks"], 1,
            "extras.imageLinks is what populates each result's `image`"
        );
        assert!(
            contents.get("images").is_none(),
            "`contents.images` is the deprecated form Exa silently ignores"
        );
        assert_eq!(body["query"], "Ada Lovelace");
    }

    /// The fallback only runs when Exa is broken, so it would otherwise
    /// ship having never executed. Ignored by default (it hits the
    /// network); run with `cargo test -- --ignored wikimedia`.
    #[tokio::test]
    #[ignore = "hits the live Wikimedia Commons API"]
    async fn the_fallback_actually_returns_pictures() {
        let out = super::wikimedia_commons("Ada Lovelace", 3)
            .await
            .expect("Commons lookup should succeed");
        assert!(
            out.contains("http"),
            "fallback produced no image URL: {out}"
        );
        assert!(
            !out.starts_with("No images found"),
            "fallback found nothing for a well-known person: {out}"
        );
    }

    #[test]
    fn the_result_count_stays_inside_exas_bounds() {
        assert_eq!(exa_image_request("q", 0)["numResults"], 1, "never ask for zero");
        assert_eq!(exa_image_request("q", 99)["numResults"], 15, "capped at 15");
        assert_eq!(exa_image_request("q", 5)["numResults"], 5);
    }
}
