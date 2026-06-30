//! Recover hallucinated image URLs in assistant replies.
//!
//! Small chat models (e.g. gpt-oss-20b) often answer "show me a picture of X"
//! by fabricating a plausible image URL from memory instead of calling the
//! `image_search` tool — the URL 404s and the user sees a broken image, in the
//! app and on Telegram alike. This post-processes a finished reply: each
//! embedded remote image is verified, and when one is dead we recover a REAL
//! one via `image_search` on the image's alt text. Images that can't be
//! recovered are dropped (a broken `![]()` is worse than none). Our own
//! `/v1/pic/` images (ComfyUI `/pic` output) are left untouched.
//!
//! Best-effort throughout: any network/parse failure leaves the original text.

use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::tools::registry::ToolRuntime;

static IMG_MD: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"!\[([^\]]*)\]\((https?://[^)\s]+)\)").unwrap());

fn http() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        // Browser-ish UA — many image CDNs 403 a blank/default agent.
        .user_agent(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
             AppleWebKit/537.36 (KHTML, like Gecko) Safari/537.36",
        )
        .timeout(Duration::from_secs(15))
        .build()
        .ok()
}

/// True if `url` actually returns an image right now.
async fn is_live_image(client: &reqwest::Client, url: &str) -> bool {
    let Ok(resp) = client.get(url).send().await else {
        return false;
    };
    if !resp.status().is_success() {
        return false;
    }
    resp.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|t| t.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .map(|t| t.starts_with("image/"))
        .unwrap_or(false)
}

/// Run `image_search(query)` and return the first hit that resolves to a real
/// image.
async fn first_real_image(
    client: &reqwest::Client,
    query: &str,
    runtime: &ToolRuntime,
) -> Option<String> {
    if query.trim().is_empty() {
        return None;
    }
    let md = crate::tools::image_search::search(
        query,
        5,
        runtime.search_engine,
        runtime.search_api_key.as_deref(),
    )
    .await
    .ok()?;
    for cap in IMG_MD.captures_iter(&md) {
        if let Some(u) = cap.get(2) {
            let url = u.as_str();
            if is_live_image(client, url).await {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// Verify every remote image the model embedded; replace dead/fabricated URLs
/// with a real `image_search` hit on the alt text, and drop the markdown for
/// any image that can't be recovered. Returns the (possibly rewritten) reply.
pub async fn recover_reply_images(reply: &str, runtime: &ToolRuntime) -> String {
    let hits: Vec<(String, String, String)> = IMG_MD
        .captures_iter(reply)
        .filter_map(|c| {
            let full = c.get(0)?.as_str().to_string();
            let alt = c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            let url = c.get(2)?.as_str().to_string();
            // Our own host-served pics are already real — leave them.
            if url.contains("/v1/pic/") {
                return None;
            }
            Some((full, alt, url))
        })
        .collect();
    if hits.is_empty() {
        return reply.to_string();
    }
    let Some(client) = http() else {
        return reply.to_string();
    };

    let mut out = reply.to_string();
    for (full, alt, url) in hits {
        if is_live_image(&client, &url).await {
            continue; // the model's URL actually works — keep it
        }
        let replacement = match first_real_image(&client, &alt, runtime).await {
            Some(real) => {
                tracing::info!("image recover: replaced dead image for {alt:?}");
                format!("![{alt}]({real})")
            }
            None => {
                tracing::info!("image recover: dropped unrecoverable image for {alt:?}");
                String::new()
            }
        };
        out = out.replace(&full, &replacement);
    }
    out
}
