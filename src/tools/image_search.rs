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
//!   * `Exa` — uses the regular Exa search with `contents.images`
//!     enrichment so each result page contributes its primary image.
//!     Same API key as the rest of Exa (web_search, x_search).

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
        SearchEngine::Duckduckgo => wikimedia_commons(query, max_results).await,
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

async fn exa_images(query: &str, max: usize, api_key: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let body = serde_json::json!({
        "query": query,
        "numResults": max.max(1).min(15),
        "contents": {
            // We don't need the page text, just the headline image.
            "text": false,
            "images": 1
        }
    });
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
        return Ok(format!("No images found for \"{}\".", query));
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
