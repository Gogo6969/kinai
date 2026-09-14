//! What an error may look like by the time it reaches the host's log.
//!
//! The rule for `~/.kinai/logs` is that it never accumulates the family's
//! own words. 0.2.126 applied that to tool ARGUMENTS. The error text was
//! the gap: a failed `fetch_page` wrote the whole address a member asked
//! for, and reqwest's `Display` appends the request URL to every transport
//! error it reports — so a search that timed out wrote the member's
//! question into the log, percent-encoded, via a bare `?` that no grep for
//! `format!` would ever find. Response bodies from upstream services can
//! echo the request back too.
//!
//! Two defences, and this module is the second. The first is at the source:
//! every tool that sends a request strips the URL from the error before it
//! propagates (`reqwest::Error::without_url`), which is where the
//! Telegram token leak of 0.2.116 was also fixed — at the boundary, once,
//! so no log site can leak by omission. This module is what the log sites
//! themselves call, for whatever still gets through: any URL becomes
//! `<url>`, and nothing longer than a few lines is written at all.
//!
//! What survives is the part a person reads the line for — the status
//! code, "operation timed out", "Connection reset by peer", which
//! fallback failed. Diagnosis needs the class of failure, never the
//! content that was being fetched.

/// The longest error the log will carry. An upstream body that echoes a
/// page back is cut long before it becomes a transcript of anything.
const MAX_CHARS: usize = 300;

/// Make an error chain safe to log: URLs replaced, length bounded.
pub fn error(chain: &str) -> String {
    let mut out = String::with_capacity(chain.len().min(MAX_CHARS + 16));
    let mut rest = chain;
    while let Some(start) = url_start(rest) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '"' | '\'' | '>' | ',' | ';'))
            .unwrap_or(tail.len());
        out.push_str("<url>");
        rest = &tail[end..];
    }
    out.push_str(rest);
    clip(&out, MAX_CHARS)
}

/// The first `http://` or `https://`, whichever comes first.
fn url_start(s: &str) -> Option<usize> {
    match (s.find("http://"), s.find("https://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// The first `max` characters, with a marker saying how much there was.
/// Counted in characters, never bytes — cutting a multibyte character in
/// half is exactly the mistake a length cap invites.
pub fn clip(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…({n} chars)")
}

#[cfg(test)]
mod tests {
    use super::{clip, error};

    /// The shape reqwest produces for a DuckDuckGo search that timed out —
    /// the question is right there in the query string.
    #[test]
    fn a_search_that_failed_does_not_log_the_question() {
        let chain = "error sending request for url \
                     (https://duckduckgo.com/html/?q=is+forty+milligrams+too+much+for+a+child): \
                     operation timed out";
        let safe = error(chain);
        assert!(!safe.contains("milligrams"), "{safe}");
        assert!(!safe.contains("duckduckgo"), "{safe}");
        assert!(safe.contains("for url (<url>)"), "{safe}");
        assert!(safe.contains("operation timed out"), "lost the cause: {safe}");
    }

    /// The whole fallback chain carries the question once per engine.
    /// Every one goes, and the markers the pipeline classifies on stay.
    #[test]
    fn every_url_in_a_fallback_chain_is_stripped() {
        let chain = "Exa failed (Exa search failed (402): {\"error\":\"no_more_credits\"}); \
                     the SearXNG fallback also failed (error sending request for url \
                     (http://searx.example:8888/search?q=cheapest+flight+in+may&format=json)); \
                     the DuckDuckGo fallback also failed (error sending request for url \
                     (https://duckduckgo.com/html/?q=cheapest+flight+in+may))";
        let safe = error(chain);
        assert!(!safe.contains("cheapest"), "{safe}");
        assert!(!safe.contains("searx.example"), "{safe}");
        assert_eq!(safe.matches("<url>").count(), 2, "{safe}");
        assert!(safe.contains("(402"), "{safe}");
        assert!(safe.contains("no_more_credits"), "{safe}");
        assert!(safe.contains("fallback also failed"), "{safe}");
    }

    /// A URL at the very end of a sentence, and one followed by a comma.
    #[test]
    fn urls_at_the_edges_of_a_sentence_are_found() {
        assert_eq!(error("too many redirects fetching https://a.example/x/y"), "too many redirects fetching <url>");
        assert_eq!(error("tried https://a.example/p, then gave up"), "tried <url>, then gave up");
    }

    #[test]
    fn ordinary_errors_are_left_exactly_alone() {
        for msg in [
            "the server answered 403 Forbidden",
            "connection reset by peer (os error 54)",
            "Exa search failed (401): invalid api key",
            "",
        ] {
            assert_eq!(error(msg), msg);
        }
    }

    /// An upstream body that echoes a whole page back stops being a log
    /// line and starts being a transcript. Cut it, and say so.
    #[test]
    fn a_long_body_is_cut_and_the_cut_is_marked() {
        let body = "LLM error 400: ".to_string() + &"the prompt you sent was ".repeat(40);
        let safe = error(&body);
        assert!(safe.chars().count() < 330, "{}", safe.chars().count());
        assert!(safe.ends_with(" chars)"), "{safe}");
        assert!(safe.starts_with("LLM error 400"), "{safe}");
    }

    #[test]
    fn clipping_counts_characters_not_bytes() {
        let s = "ü".repeat(10);
        assert_eq!(clip(&s, 10), s);
        let cut = clip(&s, 4);
        assert!(cut.starts_with("üüüü…"), "{cut}");
        assert!(cut.ends_with("(10 chars)"), "{cut}");
    }
}
