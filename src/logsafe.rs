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
///
/// Only the log reads this string, so its cut marker may carry a number.
/// `clip` below feeds error MESSAGES, and must not.
pub fn error(chain: &str) -> String {
    let mut out = String::with_capacity(chain.len().min(MAX_CHARS + 16));
    let mut rest = chain;
    while let Some(start) = url_start(rest) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        // The URL runs to the next whitespace. Then hand back whatever
        // punctuation closes the sentence around it, so reqwest's
        // `for url (<url>): operation timed out` still reads. Stopping at
        // the first `)` or `,` INSIDE the URL instead — a wiki title with
        // brackets, an image path with size parameters — left everything
        // after it on the line.
        let token = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let span = tail[..token].trim_end_matches(|c: char| {
            matches!(c, ')' | ']' | '"' | '\'' | '>' | ',' | ';' | ':' | '.')
        });
        out.push_str("<url>");
        rest = &tail[span.len()..];
    }
    out.push_str(rest);
    let n = out.chars().count();
    if n <= MAX_CHARS {
        out
    } else {
        format!("{}…({n} chars)", head(&out, MAX_CHARS))
    }
}

/// Where the next URL begins: the scheme before a `://`, any letter case.
/// One pass over the string, whatever it contains.
fn url_start(s: &str) -> Option<usize> {
    s.match_indices("://").find_map(|(i, _)| {
        let before = s.as_bytes().get(..i)?;
        ["https", "http", "wss", "ws"].iter().find_map(|scheme| {
            let n = scheme.len();
            (before.len() >= n && before[before.len() - n..].eq_ignore_ascii_case(scheme.as_bytes()))
                .then_some(i - n)
        })
    })
}

/// The first `max` characters of an upstream body, for an error MESSAGE.
///
/// The marker carries no digits on purpose. This text is read by
/// `is_permanent_tool_failure` and its siblings, which look for `(402`
/// and the like — a marker saying `(402 chars)` after a 503 would have
/// retired the search tool for the rest of the turn.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…[cut]", head(s, max))
}

/// Counted in characters, never bytes — cutting a multibyte character in
/// half is exactly the mistake a length cap invites.
fn head(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
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
        assert!(safe.contains("for url (<url>): operation timed out"), "{safe}");
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

    /// Brackets and commas are legal inside a URL. A wiki title with a
    /// disambiguator, an image path with size parameters: the first cut
    /// of this function stopped at the `)` and the `,` and wrote the rest
    /// of the address out.
    #[test]
    fn punctuation_inside_a_url_does_not_end_the_scrub_early() {
        let wiki = error(
            "error sending request for url \
             (https://de.wikipedia.org/wiki/Krebs_(Medizin)#Therapie): operation timed out",
        );
        assert!(!wiki.contains("Medizin"), "{wiki}");
        assert!(!wiki.contains("Therapie"), "{wiki}");
        assert_eq!(wiki, "error sending request for url (<url>): operation timed out");

        let image = error(
            "LLM error 400: Error while downloading \
             https://img.example/fam/upload/w_300,h_200/scan-12w.jpg.",
        );
        assert!(!image.contains("scan-12w"), "{image}");
        assert!(!image.contains("h_200"), "{image}");
        assert_eq!(image, "LLM error 400: Error while downloading <url>.");
    }

    /// Debug output and upstream bodies are not normalised by the url
    /// crate, so the scheme can arrive in any case, and a websocket
    /// address is an address too.
    #[test]
    fn schemes_are_matched_in_any_case() {
        assert_eq!(error("downloading HTTPS://Example.org/q?text=private+thing failed"), "downloading <url> failed");
        assert_eq!(error("dial wss://host.example/kin?token=abc: refused"), "dial <url>: refused");
        assert_eq!(error("xhttp://a.example/b"), "x<url>");
    }

    /// A URL at the very end of a sentence, and one followed by a comma.
    #[test]
    fn urls_at_the_edges_of_a_sentence_are_found() {
        assert_eq!(error("too many redirects fetching https://a.example/x/y"), "too many redirects fetching <url>");
        assert_eq!(error("tried https://a.example/p, then gave up"), "tried <url>, then gave up");
        assert_eq!(error("https://only.example/x"), "<url>");
    }

    #[test]
    fn ordinary_errors_are_left_exactly_alone() {
        for msg in [
            "the server answered 403 Forbidden",
            "connection reset by peer (os error 54)",
            "Exa search failed (401): invalid api key",
            "a ratio like 3://4 is not a scheme",
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

    /// `clip` output is embedded in error MESSAGES the classifiers read.
    /// Its marker must never introduce a number that reads as a status.
    #[test]
    fn clip_marker_carries_no_digits() {
        let cut = clip(&"x".repeat(402), 200);
        assert!(cut.starts_with(&"x".repeat(200)), "{cut}");
        assert!(!cut.chars().any(|c| c.is_ascii_digit()), "{cut}");
        assert!(cut.ends_with("…[cut]"), "{cut}");
    }

    #[test]
    fn clipping_counts_characters_not_bytes() {
        let s = "ü".repeat(10);
        assert_eq!(clip(&s, 10), s);
        assert_eq!(clip(&s, 4), "üüüü…[cut]");
    }
}
