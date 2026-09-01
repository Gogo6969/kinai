//! Human-sounding status lines shown while a tool runs.
//!
//! A 30-second research turn used to look identical to a hung app: the
//! phone showed only "typing…" and the desktop showed the raw tool id
//! ("web search"). Wolf asked for varied, human phrasing so a waiting
//! family member can see that something is actually happening.
//!
//! Two rules the phrasing must keep:
//!
//! 1. **Honest.** The line describes the tool that is genuinely running.
//!    Saying "Reading the document…" while the model is doing arithmetic
//!    would be a small lie that teaches the family to distrust the
//!    bigger statements.
//! 2. **Varied but not random-feeling.** A shared rotation counter walks
//!    each set, so consecutive calls — including the parallel batch of
//!    searches one turn fires — read differently instead of repeating
//!    one line four times.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Advanced once per phrase, shared across tools, so a turn that calls
/// three tools shows three different lines.
static ROTATION: AtomicUsize = AtomicUsize::new(0);

const SEARCHING: &[&str] = &[
    "Looking into it…",
    "Searching the web…",
    "Checking a few sources…",
    "Research started…",
    "Digging into that…",
    "Let me look that up…",
    "Chasing that down…",
    "Going through the results…",
];

const READING: &[&str] = &[
    "Opening the page…",
    "Reading the document…",
    "Fetching that link…",
    "Working through the text…",
];

const SOCIAL: &[&str] = &[
    "Checking what people are posting…",
    "Reading the discussion…",
    "Looking through recent posts…",
];

const PICTURES: &[&str] = &["Looking for pictures…", "Finding an image…"];

const CALCULATING: &[&str] = &["Working it out…", "Crunching the numbers…"];

const REMEMBERING: &[&str] = &["Saving that…", "Noting that down…"];

const FORGETTING: &[&str] = &["Updating what I remember…", "Forgetting that…"];

const CLOCK: &[&str] = &["Checking the date…"];

const GENERIC: &[&str] = &["Working on it…", "On it…", "One moment…"];

fn set_for(tool: &str) -> &'static [&'static str] {
    match tool {
        "web_search" => SEARCHING,
        "fetch_page" => READING,
        "x_search" => SOCIAL,
        "image_search" => PICTURES,
        "calculator" => CALCULATING,
        "remember" => REMEMBERING,
        "forget" => FORGETTING,
        "datetime" => CLOCK,
        // An unknown tool is a real possibility (the catalogue grows);
        // a vague-but-true line beats naming the wrong activity.
        _ => GENERIC,
    }
}

/// The line to show while `tool` runs. Never empty.
pub fn phrase_for(tool: &str) -> &'static str {
    let set = set_for(tool);
    let n = ROTATION.fetch_add(1, Ordering::Relaxed);
    set[n % set.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_tool_has_its_own_voice() {
        // The registry's full catalogue must be covered — a tool falling
        // through to GENERIC is a miss, not a crash, so assert directly.
        for t in ["web_search", "fetch_page", "x_search", "image_search",
                  "calculator", "remember", "forget", "datetime"] {
            assert!(!std::ptr::eq(set_for(t), GENERIC), "{t} has no phrases of its own");
            assert!(!phrase_for(t).is_empty());
        }
    }

    #[test]
    fn an_unknown_tool_still_says_something_true() {
        let p = phrase_for("some_future_tool");
        assert!(GENERIC.contains(&p), "unknown tool must use the vague-but-true set");
    }

    #[test]
    fn repeated_calls_do_not_repeat_the_same_line() {
        // The parallel-search case: one turn fires several web_searches
        // at once and must not print one line four times.
        let a = phrase_for("web_search");
        let b = phrase_for("web_search");
        let c = phrase_for("web_search");
        assert_ne!(a, b);
        assert_ne!(b, c);
    }

    #[test]
    fn phrases_describe_the_tool_that_is_running() {
        // Guards the honesty rule against a careless edit: a search must
        // never claim to be reading a document, and vice versa.
        for p in SEARCHING {
            let l = p.to_lowercase();
            assert!(!l.contains("page") && !l.contains("document"), "search phrase claims reading: {p}");
        }
        for p in READING {
            let l = p.to_lowercase();
            assert!(!l.contains("search"), "reading phrase claims searching: {p}");
        }
        for p in CALCULATING {
            let l = p.to_lowercase();
            assert!(!l.contains("search") && !l.contains("read"), "calc phrase misleads: {p}");
        }
    }
}
