//! Read what a video actually says, via the family's own transcript service.
//!
//! KinAI does not run `yt-dlp` itself. A small service on the household's
//! own hardware wraps it and exposes one endpoint; this tool calls that.
//! Nothing leaves the house and KinAI never executes third-party code.
//!
//! **Why the host allowlist below is duplicated here.** `fetch_page`
//! refuses LAN and loopback addresses precisely so a web page can never
//! talk the model into probing the family's servers. The transcript
//! service lives AT a LAN address, so it is a door around that guard.
//! The service enforces its own allowlist; this is the second lock, on
//! our side of the door, because the model chooses this tool's argument
//! and a hostile page can influence what the model asks for. A bug in
//! either layer alone must not be enough.

use anyhow::{anyhow, Result};
use std::time::Duration;

/// Video hosts the tool will pass on. Matched against the parsed host,
/// so `youtube.com.evil.tld` does not slip through and neither does a
/// bare IP address.
const ALLOWED_HOSTS: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
    "youtu.be",
    "www.youtu.be",
];

/// yt-dlp has to talk to YouTube, and a cold fetch of a long video is
/// slow; the service caps itself at 120s, so allow a little more.
const TIMEOUT: Duration = Duration::from_secs(150);
/// Charged against the turn's tool budget like any other result. A
/// three-hour podcast would otherwise swamp the conversation.
const MAX_CHARS: usize = 40_000;

fn host_allowed(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .is_some_and(|h| ALLOWED_HOSTS.contains(&h.as_str()))
}

/// Fetch a video's transcript. `service` is the host-configured base URL.
pub async fn fetch(service: &str, url: &str) -> Result<String> {
    if service.trim().is_empty() {
        return Err(anyhow!(
            "no transcript service is configured — the host can set one under Settings → Tools"
        ));
    }
    if !host_allowed(url) {
        // Permanent for this URL: the pipeline marks URL_REFUSED errors
        // as "don't retry", so the model stops offering to try again.
        return Err(anyhow!(
            "{}: I can only read captions from YouTube links",
            crate::tools::fetch_page::URL_REFUSED
        ));
    }

    let endpoint = format!("{}/transcript", service.trim_end_matches('/'));
    let resp = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()?
        .get(&endpoint)
        .query(&[("url", url)])
        .send()
        .await
        .map_err(|e| {
            anyhow!("the transcript service did not respond ({e}). It runs on the family's own hardware — the host can check it is up.")
        })?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);

    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("the transcript service could not read that video");
        // Each of these becomes user-visible text, so each is also
        // registered in `force_search::OUTAGE_CLAIMS` — otherwise the
        // sentence survives in the thread and KinAI keeps repeating
        // "I can't get transcripts" long after the service recovers.
        return Err(match body.get("kind").and_then(|v| v.as_str()) {
            Some("no_captions") => anyhow!("that video has no captions to read: {msg}"),
            Some("rate_limited") => anyhow!("{msg}"),
            Some("unsupported") => anyhow!(
                "{}: {msg}",
                crate::tools::fetch_page::URL_REFUSED
            ),
            _ => anyhow!("the transcript service could not read that video: {msg}"),
        });
    }

    let text = body.get("text").and_then(|v| v.as_str()).unwrap_or_default();
    if text.trim().is_empty() {
        return Err(anyhow!("that video has no captions to read"));
    }
    let title = body.get("title").and_then(|v| v.as_str()).unwrap_or_default();
    let secs = body.get("duration").and_then(|v| v.as_u64()).unwrap_or(0);

    let mut head = String::new();
    if !title.is_empty() {
        head.push_str(&format!("Video: {title}\n"));
    }
    if secs > 0 {
        head.push_str(&format!("Length: {}m{:02}s\n", secs / 60, secs % 60));
    }
    // The header leads, because every consumer downstream truncates the
    // head of the string — a trailing note is the first thing cut.
    head.push_str("Transcript (auto-generated captions, so wording may be imperfect):\n\n");

    let total = text.chars().count();
    if total > MAX_CHARS {
        let cut: String = text.chars().take(MAX_CHARS).collect();
        Ok(format!(
            "{head}{cut}\n\n[transcript truncated — {total} characters total. Say so if the answer depends on the rest.]"
        ))
    } else {
        Ok(format!("{head}{text}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_video_hosts_are_passed_to_the_service() {
        for ok in [
            "https://www.youtube.com/shorts/y_kCeT0bKqQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ&t=30",
        ] {
            assert!(host_allowed(ok), "{ok} should be allowed");
        }
    }

    /// The second lock. The service has its own allowlist; if this one
    /// ever regressed, the transcript tool would become an SSRF proxy
    /// around `fetch_page`'s guard — reachable because the model, not a
    /// human, picks the argument.
    #[test]
    fn the_lan_and_spoofed_hosts_never_reach_the_service() {
        for bad in [
            "http://192.168.1.210:8081/v1/models",
            "http://127.0.0.1:4847/info",
            "http://localhost:8888/search",
            "http://[::ffff:192.168.1.210]:8081/",
            "https://youtube.com.evil.tld/watch?v=abc12345",
            "https://evil.tld/watch?v=abc12345",
            "file:///etc/passwd",
            "ftp://youtube.com/x",
            "not a url",
        ] {
            assert!(!host_allowed(bad), "{bad} must be refused");
        }
    }

    #[tokio::test]
    async fn an_unconfigured_service_says_so_instead_of_failing_obscurely() {
        let e = fetch("", "https://youtu.be/dQw4w9WgXcQ").await.unwrap_err();
        assert!(e.to_string().contains("no transcript service is configured"), "got: {e}");
    }

    #[tokio::test]
    async fn a_refused_host_is_marked_permanent_for_that_url() {
        // No network: the host check happens before any request.
        let e = fetch("http://example.invalid:8099", "https://evil.tld/watch?v=x")
            .await
            .unwrap_err();
        assert!(
            e.to_string().starts_with(crate::tools::fetch_page::URL_REFUSED),
            "the pipeline keys off this marker to stop retry offers; got: {e}"
        );
    }
}
