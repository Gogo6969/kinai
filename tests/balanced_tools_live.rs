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
