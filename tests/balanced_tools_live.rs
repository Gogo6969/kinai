//! Live diagnosis: run KinAI's REAL pipeline (system prompt + tools +
//! tool runtime) against the balanced slot to see whether tools fire.
//! #[ignore] — needs the live Laguna server + real config.
use kinai::config::AppConfig;
use kinai::context::system_prompt;
use kinai::llm::LlmClient;
use kinai::tools::loop_pipeline::{run_pipeline, PipelineHandlers, ToolEvent};
use kinai::tools::registry::{self, ToolRuntime};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
#[ignore = "live server + real config; run explicitly"]
async fn balanced_turn_uses_tools() {
    let cfg = AppConfig::load_or_default();
    assert!(cfg.llm_balanced.is_active(), "balanced slot must be configured");
    let tools = registry::enabled(&cfg.tools);
    eprintln!("tools sent: {:?}", tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>());
    let runtime = ToolRuntime::from_tool_settings(&cfg.tools);
    let client = LlmClient::new(cfg.llm_balanced.clone());
    let messages = vec![
        system_prompt(&cfg.host.family_name, &cfg.llm_balanced.system_addendum),
        kinai::context::ChatMessage::User {
            content: "Who won the final of the soccer World Championship?".into(),
            name: Some("Wolf".into()),
            image_data_urls: vec![],
        },
    ];
    let handlers = PipelineHandlers {
        on_token: Arc::new(|_t| {}),
        on_reasoning: Arc::new(|_r| {}),
        on_tool: Arc::new(|e: ToolEvent| match e {
            ToolEvent::Started { name, args } => eprintln!(">>> TOOL CALLED: {name} args={}", &args.chars().take(120).collect::<String>()),
            ToolEvent::Finished { name, ok, result } => eprintln!(">>> TOOL DONE: {name} ok={ok} result[0..160]={}", &result.chars().take(160).collect::<String>()),
        }),
    };
    let out = run_pipeline(client, messages, tools, Some(8192), runtime, handlers, CancellationToken::new())
        .await
        .expect("pipeline");
    eprintln!("FINAL[0..400]: {}", out.final_content.chars().take(400).collect::<String>());
}

/// The exact regression from the field: a follow-up asking for NEW facts
/// about a current event must trigger a FRESH search — the old prompt's
/// follow-up exemption made models answer from imagination instead.
async fn followup_scenario(llm: kinai::config::LlmSettings, label: &str) {
    let cfg = AppConfig::load_or_default();
    let tools = registry::enabled(&cfg.tools);
    let runtime = ToolRuntime::from_tool_settings(&cfg.tools);
    let client = LlmClient::new(llm);
    let messages = vec![
        system_prompt(&cfg.host.family_name, ""),
        kinai::context::ChatMessage::User {
            content: "Who won the final of the soccer World Championship?".into(),
            name: Some("Wolf".into()),
            image_data_urls: vec![],
        },
        kinai::context::ChatMessage::Assistant {
            content: "Spain won the 2026 FIFA World Cup final, defeating Argentina 1-0 after extra time (Ferran Torres, 106th minute).".into(),
            tool_calls: vec![],
        },
        kinai::context::ChatMessage::User {
            content: "Did the players argue with each other after the final whistle of the game?".into(),
            name: Some("Wolf".into()),
            image_data_urls: vec![],
        },
    ];
    let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let called2 = called.clone();
    let handlers = PipelineHandlers {
        on_token: Arc::new(|_| {}),
        on_reasoning: Arc::new(|_| {}),
        on_tool: Arc::new(move |e| {
            if let ToolEvent::Started { name, args } = e {
                called2.store(true, std::sync::atomic::Ordering::SeqCst);
                eprintln!(">>> [{}] TOOL: {name} {}", std::thread::current().name().unwrap_or("t"), &args.chars().take(100).collect::<String>());
            }
        }),
    };
    let out = run_pipeline(client, messages, tools, Some(8192), runtime, handlers, CancellationToken::new())
        .await
        .expect("pipeline");
    eprintln!("[{label}] searched: {} | FINAL[0..200]: {}", called.load(std::sync::atomic::Ordering::SeqCst), out.final_content.chars().take(200).collect::<String>());
}

#[tokio::test]
#[ignore = "live"]
async fn followup_balanced() {
    let cfg = AppConfig::load_or_default();
    followup_scenario(cfg.llm_balanced.clone(), "balanced").await;
}

#[tokio::test]
#[ignore = "live"]
async fn followup_deep() {
    let cfg = AppConfig::load_or_default();
    followup_scenario(cfg.llm_deep.clone(), "deep").await;
}

/// Slot failover, end-to-end through the REAL wrapper: the routed slot
/// points at a dead port, balanced is the live Laguna server. Expect a
/// visible failover notice prepended AND a real answer served by
/// balanced.
#[tokio::test]
#[ignore = "live server + real config; run explicitly"]
async fn dead_fast_slot_fails_over_to_balanced() {
    let mut cfg = AppConfig::load_or_default();
    assert!(cfg.llm_balanced.is_active(), "balanced slot must be configured");
    // Sabotage fast: configured (active) but nothing listens there.
    cfg.llm.base_url = "http://127.0.0.1:9".into();
    cfg.llm.model = "dead-model".into();
    cfg.llm.enabled = true;

    let tools = registry::enabled(&cfg.tools);
    let runtime = ToolRuntime::from_tool_settings(&cfg.tools);
    let messages = vec![
        system_prompt(&cfg.host.family_name, ""),
        kinai::context::ChatMessage::User {
            content: "In one short sentence: what is the capital of Spain?".into(),
            name: Some("Wolf".into()),
            image_data_urls: vec![],
        },
    ];
    let notice_seen = Arc::new(std::sync::Mutex::new(String::new()));
    let notice_clone = notice_seen.clone();
    let handlers = PipelineHandlers {
        on_token: Arc::new(move |t| {
            notice_clone.lock().unwrap().push_str(&t);
        }),
        on_reasoning: Arc::new(|_| {}),
        on_tool: Arc::new(|_| {}),
    };
    let state = kinai::AppState {
        handle: parking_lot::RwLock::new(None),
        config: parking_lot::RwLock::new(cfg.clone()),
        db: kinai::db::Db::open(
            tempfile::tempdir().expect("tmpdir").path().join("t.db"),
        )
        .await
        .expect("db"),
        llm: tokio::sync::Mutex::new(LlmClient::new(cfg.llm.clone())),
        net: Arc::new(tokio::sync::Mutex::new(kinai::network::NetState::default())),
        stats: parking_lot::RwLock::new(kinai::RuntimeStats::default()),
        telegram: Arc::new(kinai::telegram::TelegramSupervisor::default()),
        pending_turns: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        slot_health: parking_lot::Mutex::new(std::collections::HashMap::new()),
        fact_checks_running: parking_lot::Mutex::new(std::collections::HashSet::new()),
        tts_child: parking_lot::Mutex::new(None),
    };
    let served = kinai::slash::run_turn_with_slot_failover(
        kinai::vision::Route::Chat,
        &state,
        &cfg,
        "fast",
        messages,
        tools,
        runtime,
        handlers,
        CancellationToken::new(),
        |_, _| Some(2048),
    )
    .await
    .expect("failover must produce an answer");
    eprintln!("served by slot: {}", served.slot_label);
    eprintln!("FINAL[0..400]: {}", served.result.final_content.chars().take(400).collect::<String>());
    assert_eq!(served.slot_label, "balanced", "must have failed over to balanced");
    assert!(
        served.result.final_content.contains("isn't responding"),
        "failover notice must be in the final content"
    );
    assert!(
        served.result.final_content.to_lowercase().contains("madrid"),
        "the failover slot must actually answer"
    );
}

/// Fact-check plumbing end-to-end: the checker slot pointed at the live
/// LAN server (standing in for DeepSeek — same OpenAI-compatible API).
/// Proves factcheck::run builds the prompt, runs the tool pipeline, and
/// returns a verdict-shaped report.
#[tokio::test]
#[ignore = "live server + real config; run explicitly"]
async fn factcheck_pipeline_against_live_server() {
    let mut cfg = AppConfig::load_or_default();
    assert!(cfg.llm_balanced.is_active(), "balanced slot must be configured");
    cfg.llm_factcheck = cfg.llm_balanced.clone();
    cfg.llm_factcheck.api_key = Some("local-test".into());
    cfg.llm_factcheck.enabled = true;

    let report = kinai::factcheck::run(
        &cfg,
        "When did the Eiffel Tower open?",
        "The Eiffel Tower opened in 1887 and is 330 m tall.",
        CancellationToken::new(),
    )
    .await
    .expect("fact check must produce a report");
    eprintln!("REPORT:\n{report}");
    assert!(!report.trim().is_empty());
    assert!(
        report.contains('✅') || report.contains('⚠') || report.contains('❌'),
        "report should start with a verdict line: {report}"
    );
}

/// The reworked evidence-first fact check against the REAL configured
/// checker slot (DeepSeek) — the exact field case from 0.2.82: raw DSML
/// tool markup leaked because the model was asked to drive tools. Now
/// KinAI runs the searches; the model only judges.
#[tokio::test]
#[ignore = "live: uses the configured online checker (billed)"]
async fn factcheck_against_configured_online_slot() {
    let cfg = AppConfig::load_or_default();
    if !kinai::factcheck::is_configured(&cfg.llm_factcheck) {
        eprintln!("SKIP: fact-check slot not configured");
        return;
    }
    let report = kinai::factcheck::run(
        &cfg,
        "Tell me about Adam Ries, the German arithmetic teacher",
        "Adam Ries was born 24 July 1492 in the town of Mühlburg, near Heilbronn. \
His most famous works were published in 1528, 1538, and 1544. He died 17 April 1559 in Stuttgart.",
        CancellationToken::new(),
    )
    .await
    .expect("fact check must produce a report");
    eprintln!("REPORT:\n{report}");
    assert!(report.contains('✅') || report.contains('⚠') || report.contains('❌'));
    assert!(
        !report.contains("DSML") && !report.contains("tool_calls"),
        "no raw tool markup may leak: {report}"
    );
}

/// Field bug 0.2.83: balanced answered "Who was Adam Riese?" with
/// "I don't have information — would you like me to search?" instead of
/// searching. The prompt now mandates: unknown factual question →
/// search immediately, never ask permission.
async fn unknown_fact_scenario(llm: kinai::config::LlmSettings, label: &str) {
    let cfg = AppConfig::load_or_default();
    let tools = registry::enabled(&cfg.tools);
    let runtime = ToolRuntime::from_tool_settings(&cfg.tools);
    let client = LlmClient::new(llm);
    let messages = vec![
        system_prompt(&cfg.host.family_name, ""),
        kinai::context::ChatMessage::User {
            content: "Who was Adam Riese?".into(),
            name: Some("Wolf".into()),
            image_data_urls: vec![],
        },
    ];
    let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let called2 = called.clone();
    let handlers = PipelineHandlers {
        on_token: Arc::new(|_| {}),
        on_reasoning: Arc::new(|_| {}),
        on_tool: Arc::new(move |e| {
            if let ToolEvent::Started { name, args } = e {
                called2.store(true, std::sync::atomic::Ordering::SeqCst);
                eprintln!(">>> TOOL: {name} {}", &args.chars().take(100).collect::<String>());
            }
        }),
    };
    let out = run_pipeline(client, messages, tools, Some(8192), runtime, handlers, CancellationToken::new())
        .await
        .expect("pipeline");
    let searched = called.load(std::sync::atomic::Ordering::SeqCst);
    eprintln!(
        "[{label}] searched: {searched} | FINAL[0..200]: {}",
        out.final_content.chars().take(200).collect::<String>()
    );
    assert!(searched, "[{label}] must search an unknown factual question, not ask permission");
    assert!(
        !out.final_content.to_lowercase().contains("would you like me to search"),
        "[{label}] must not ask permission: {}",
        out.final_content
    );
}

#[tokio::test]
#[ignore = "live"]
async fn unknown_fact_balanced() {
    let cfg = AppConfig::load_or_default();
    unknown_fact_scenario(cfg.llm_balanced.clone(), "balanced").await;
}

#[tokio::test]
#[ignore = "live"]
async fn unknown_fact_fast() {
    let cfg = AppConfig::load_or_default();
    unknown_fact_scenario(cfg.llm.clone(), "fast").await;
}

#[tokio::test]
#[ignore = "live"]
async fn unknown_fact_deep() {
    let cfg = AppConfig::load_or_default();
    unknown_fact_scenario(cfg.llm_deep.clone(), "deep").await;
}
