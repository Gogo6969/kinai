//! The fact-check button, through KinAI's own code path.
//!
//! Field report 2026-08-08: two fact checks in a row returned "the checker
//! model returned an empty report — try again". Probing the DeepSeek
//! endpoint directly with a hand-built payload could NOT reproduce it —
//! four calls all produced proper verdicts. So the difference lives
//! somewhere in `factcheck::run` itself (query step, the searches it runs,
//! the prompt it assembles), which is exactly why this test drives the
//! real function instead of curl.
//!
//! #[ignore] — needs the configured online checker and a working search
//! engine; costs a few API calls.

use kinai::config::AppConfig;
use tokio_util::sync::CancellationToken;

/// Verbatim from the reply whose fact check failed (2026-08-08 15:32).
const ANSWER: &str = "Dean Potter is remembered not just for his athleticism, but for his \
philosophical approach to life. He often spoke about the connection between climbing, \
meditation, and the natural world. His life and death spark debates about risk, \
responsibility, and the limits of human endeavour. The crash sparked widespread discussion \
in the climbing and BASE-jumping community about the increasing risks of wingsuit flying \
and the dangers of flying in prohibited areas of national parks. (Note: His wife, Steph \
Davis, is also a legendary climber and BASE jumper; they were married from 2005 until \
Potter's death in 2015.)";

#[tokio::test]
#[ignore = "live: online checker + search engine; run explicitly"]
async fn the_failing_fact_check_now_produces_a_verdict() {
    let cfg = AppConfig::load_or_default();
    assert!(
        kinai::factcheck::is_configured(&cfg.llm_factcheck),
        "the fact-check slot must be configured for this test"
    );
    eprintln!(
        "checker: {} @ {}",
        cfg.llm_factcheck.model, cfg.llm_factcheck.base_url
    );

    let out = kinai::factcheck::run(&cfg, "Tell me about Dean Potter", ANSWER, CancellationToken::new()).await;

    match &out {
        Ok(report) => {
            eprintln!("---- REPORT ({} chars) ----\n{report}\n----", report.len());
            assert!(!report.trim().is_empty());
            // The format the panel renders: a verdict line first.
            assert!(
                report.contains('✅') || report.contains("⚠") || report.contains('❌'),
                "report must open with a verdict line:\n{report}"
            );
        }
        Err(e) => {
            let msg = format!("{e:#}");
            eprintln!("---- FAILED ----\n{msg}\n----");
            // The old message said only "empty report — try again", which
            // told neither the user nor us anything. Whatever happens now,
            // it must at least name a cause.
            assert!(
                !msg.contains("returned an empty report — try again"),
                "the uninformative old error is back: {msg}"
            );
            panic!("fact check failed: {msg}");
        }
    }
}

/// Repeat it: the field failure happened twice, so a single green run
/// proves little. Any empty report among these is a real reproduction.
#[tokio::test]
#[ignore = "live: makes several checker calls; run explicitly"]
async fn fact_check_is_repeatable() {
    let cfg = AppConfig::load_or_default();
    let mut ok = 0;
    let mut failures: Vec<String> = Vec::new();
    for i in 0..6 {
        match kinai::factcheck::run(&cfg, "Tell me about Dean Potter", ANSWER, CancellationToken::new()).await {
            Ok(r) if !r.trim().is_empty() => ok += 1,
            Ok(_) => failures.push(format!("run {i}: empty Ok")),
            Err(e) => failures.push(format!("run {i}: {e:#}")),
        }
    }
    eprintln!("{ok}/6 produced a verdict");
    for f in &failures {
        eprintln!("  {f}");
    }
    // Pre-fix this scored 1/6-equivalent (1 of 4). The whole point of the
    // change is that it stops being a coin toss, so accept nothing less
    // than every run producing a verdict.
    assert_eq!(ok, 6, "only {ok}/6 verdicts — still flaky: {failures:?}");
}
