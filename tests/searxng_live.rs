//! SearXNG through KinAI's real dispatcher.
//!
//! the owner runs an instance locally; this checks the integration against it
//! rather than a fixture, because the two things that actually break are
//! environmental: the instance must have the JSON output format enabled
//! (SearXNG ships with it OFF and answers HTML, which would otherwise
//! surface as "no results" and invite the model to invent an answer), and
//! the result shape must match what we deserialize.
//!
//! #[ignore] — needs a reachable SearXNG.

use kinai::config::SearchEngine;
use kinai::tools::web_search;

const URL: &str = "http://127.0.0.1:8888";

#[tokio::test]
#[ignore = "needs a local SearXNG; run explicitly"]
async fn searxng_returns_usable_grounding() {
    let out = web_search::search("Olares One ethernet speed", 10, SearchEngine::Searxng, None, URL, false)
        .await
        .expect("searxng search");

    eprintln!("---- result block ----\n{out}\n----------------------");

    assert!(!out.contains("No results."), "instance returned nothing");
    // The model is given a numbered block of title / snippet / URL. If the
    // shape drifts, answers lose their citations.
    assert!(out.starts_with("1. "), "should be a numbered list:\n{out}");
    assert!(out.contains("http"), "results must carry URLs to cite");
    // The page that actually answers this question, found by Exa too.
    assert!(
        out.contains("olares.com/docs/one/spec") || out.contains("spec"),
        "expected the spec page among the results:\n{out}"
    );
}

#[tokio::test]
#[ignore = "needs a local SearXNG; run explicitly"]
async fn a_wrong_url_fails_loudly_rather_than_silently() {
    // A misconfigured instance must produce an error the tool-failure path
    // can report, NOT an empty result the model would paper over.
    let err = web_search::search("anything", 5, SearchEngine::Searxng, None, "http://127.0.0.1:9", false)
        .await
        .expect_err("unreachable instance must error");
    eprintln!("error text: {err:#}");

    let empty = web_search::search("anything", 5, SearchEngine::Searxng, None, "  ", false)
        .await
        .expect_err("blank URL must error");
    assert!(
        format!("{empty:#}").contains("no URL is configured"),
        "blank URL should tell the user what to fix: {empty:#}"
    );
}

/// The Settings "Test" button, end to end against the real instance —
/// including the two misconfigurations users actually hit.
#[tokio::test]
#[ignore = "needs a local SearXNG; run explicitly"]
async fn the_settings_test_button_reports_accurately() {
    use kinai::commands::{test_searxng, TestSearxngArgs};

    let ok = test_searxng(TestSearxngArgs { url: URL.into() }).await.unwrap();
    eprintln!("OK case: {} | {} | engines={:?}", ok.message, ok.sample, ok.engines);
    assert!(ok.ok, "live instance should pass: {}", ok.message);
    assert!(!ok.sample.is_empty(), "should prove it really searched");
    assert!(ok.latency_ms > 0);

    // No scheme — the single most common typo.
    let bare = test_searxng(TestSearxngArgs { url: "127.0.0.1:8888".into() }).await.unwrap();
    assert!(!bare.ok);
    assert!(bare.message.contains("http://"), "should suggest the fix: {}", bare.message);

    // Nothing listening.
    let dead = test_searxng(TestSearxngArgs { url: "http://127.0.0.1:9".into() }).await.unwrap();
    assert!(!dead.ok);
    assert!(
        dead.message.to_lowercase().contains("listening") || dead.message.to_lowercase().contains("reach"),
        "should say nothing is there: {}", dead.message
    );

    // Empty.
    let blank = test_searxng(TestSearxngArgs { url: "   ".into() }).await.unwrap();
    assert!(!blank.ok && blank.message.contains("Enter the address"));
}
