//! Independent fact-check of a single assistant reply.
//!
//! Runs on the dedicated ONLINE checker slot (Settings → Fact check,
//! `AppConfig::llm_factcheck`) — deliberately NOT part of the
//! fast/balanced/deep chat routing or its failover: the whole point is
//! an independent second opinion from a different provider, invoked
//! only by the per-message button.
//!
//! The checker gets the `web_search` + `datetime` tools so it verifies
//! claims against fresh sources instead of another model's memory —
//! the same anti-fabrication stance as the chat pipeline (0.2.79).
//! Results are ephemeral UI panels; they are never persisted into the
//! thread, so a fact-check can't leak into future model context.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::config::{AppConfig, LlmSettings};
use crate::context::ChatMessage;
use crate::tools::loop_pipeline::{run_pipeline, PipelineHandlers};
use crate::tools::registry::{self, ToolRuntime};

/// The button (and the WS request) only exist when the checker can
/// actually be called: base URL + model + a non-empty API key. Wolf's
/// rule: "only shows when there are credentials in the fourth model
/// settings" — an online API without a key can't be it.
pub fn is_configured(s: &LlmSettings) -> bool {
    !s.base_url.trim().is_empty()
        && !s.model.trim().is_empty()
        && s.api_key
            .as_deref()
            .map(|k| !k.trim().is_empty())
            .unwrap_or(false)
}

/// Keep the checker's output bounded — it's a verdict card, not an essay,
/// and online APIs bill per token.
const FACT_CHECK_MAX_TOKENS: usize = 2048;

pub async fn run(
    cfg: &AppConfig,
    question: &str,
    answer: &str,
    cancel: CancellationToken,
) -> anyhow::Result<String> {
    let settings = &cfg.llm_factcheck;
    anyhow::ensure!(
        is_configured(settings),
        "The fact-check model isn't configured — add its API details in Settings → Models."
    );

    let year = crate::tools::datetime::current_year();
    let system = format!(
        "You are a strict, independent fact-checker. You receive a QUESTION and an \
ANSWER given by another AI. Verify the ANSWER's factual claims.\n\
RULES:\n\
- The current year is {year}. For anything time-sensitive or that you are not \
certain of, call web_search — include {year} in queries about current events. \
Never verify current events from memory alone.\n\
- If a search fails or you cannot verify a claim, say so plainly — never guess.\n\
- Judge only FACTS, not style or opinions.\n\
FORMAT (markdown, concise):\n\
Start with exactly one verdict line: `✅ Accurate`, `⚠️ Partly accurate`, or \
`❌ Contains errors` (add a ≤10-word reason).\n\
Then at most 5 bullets — only for claims that are wrong, dubious, or unverifiable: \
quote the claim briefly, state what's actually correct, name the source you \
checked. No bullets if everything checks out."
    );
    let user = format!("QUESTION:\n{question}\n\nANSWER TO CHECK:\n{answer}");
    let messages = vec![
        ChatMessage::System { content: system },
        ChatMessage::User {
            content: user,
            name: None,
            image_data_urls: vec![],
        },
    ];

    // Verification tools only — no image search / calculator noise.
    let tools: Vec<registry::ToolDef> = registry::enabled(&cfg.tools)
        .into_iter()
        .filter(|t| t.name == "web_search" || t.name == "datetime")
        .collect();
    let runtime = ToolRuntime::from_tool_settings(&cfg.tools);

    let llm = crate::llm::LlmClient::new(settings.clone());
    let handlers = PipelineHandlers {
        on_token: Arc::new(|_| {}),
        on_reasoning: Arc::new(|_| {}),
        on_tool: Arc::new(|_| {}),
    };
    let result = run_pipeline(
        llm,
        messages,
        tools,
        Some(FACT_CHECK_MAX_TOKENS),
        runtime,
        handlers,
        cancel,
    )
    .await?;
    Ok(result.final_content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_requires_all_three_credentials() {
        let mut s = LlmSettings::default_empty();
        assert!(!is_configured(&s), "empty slot is not configured");
        s.base_url = "https://api.deepseek.com/v1".into();
        s.model = "deepseek-chat".into();
        assert!(!is_configured(&s), "no api key -> no button");
        s.api_key = Some("  ".into());
        assert!(!is_configured(&s), "blank api key -> no button");
        s.api_key = Some("sk-test".into());
        assert!(is_configured(&s));
    }
}
