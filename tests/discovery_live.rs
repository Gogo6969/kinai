//! Live LAN discovery probe — runs KinAI's REAL `scan_local_network()`
//! against whatever network the test machine is on and prints what it
//! finds. This is the legitimate discovery mechanism (the same code the
//! "Scan network" button drives), used here to confirm the range-based
//! port fix actually surfaces a backend the old sparse list missed.
//!
//! Ignored by default (needs a live network + real LLM servers). Run with:
//!   cargo test --test discovery_live -- --ignored --nocapture

#[tokio::test]
#[ignore]
async fn scan_local_network_prints_backends() {
    let backends = kinai::llm::detect::scan_local_network().await;
    eprintln!("\n=== KinAI discovered {} backend(s) ===", backends.len());
    for b in &backends {
        eprintln!(
            "  {:<16} {:<28} models: {}",
            b.provider,
            b.base_url,
            b.models.join(", ")
        );
    }
    eprintln!("=== end ===\n");
    // Not asserting a count — the machine's LAN is whatever it is. The
    // point is the printed inventory: it must include every LLM server
    // that's actually up, including ones on the dense-range ports.
}
