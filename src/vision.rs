//! Vision routing for image attachments.
//!
//! Decides whether an image-bearing user turn goes through the active
//! chat model (when it's a known vision-capable model) or through a
//! dedicated vision endpoint (with optional failover). Mirrors CCC's
//! pattern: primary endpoint handles steady state, failover catches
//! transient cloud issues (429s, "high demand", 5xx), and an explicitly
//! vision-capable chat model bypasses both.
//!
//! The actual multipart wire format lives in `llm::stream::serialize_message`
//! — this module only owns the routing decision and the small retry loop.

use anyhow::{anyhow, Result};

use crate::config::{LlmSettings, VisionEndpoint, VisionSettings};
use crate::db::Attachment;

/// Glob-style patterns covering every model family known to natively
/// understand image inputs over an OpenAI-compatible interface. Match
/// is substring + case-insensitive — keeps the list short and forgiving
/// to vendor naming jitter ("gemini-2.5-pro", "gpt-4o-2024-08-06", etc.).
///
/// Add new families here as they ship. Order doesn't matter; first
/// substring hit wins.
const VISION_CAPABLE_FRAGMENTS: &[&str] = &[
    // Anthropic Claude (Sonnet 3.5+ and Opus 3+ are vision; Haiku 3.5 too)
    "claude-3", "claude-3-5", "claude-3-7", "claude-sonnet", "claude-opus", "claude-haiku",
    // OpenAI GPT-4o / 4-vision (4o-mini also)
    "gpt-4o", "gpt-4-vision", "gpt-4-turbo",
    // Google Gemini (Pro & Flash both vision; 2.0/2.5 etc.)
    "gemini-2", "gemini-1.5", "gemini-pro", "gemini-flash",
    // Open-source vision models commonly served via Ollama / vLLM
    "llava", "qwen2-vl", "qwen-vl", "moondream", "minicpm-v",
    "pixtral", "cogvlm", "internvl", "phi-3-vision", "phi-3.5-vision",
];

/// Returns true if the given model name is in the hardcoded vision list.
/// Substring + case-insensitive — see VISION_CAPABLE_FRAGMENTS docs for
/// the rationale.
pub fn is_vision_capable(model: &str) -> bool {
    let lc = model.to_ascii_lowercase();
    VISION_CAPABLE_FRAGMENTS.iter().any(|frag| lc.contains(frag))
}

/// Does this attachment carry an image?
pub fn is_image(att: &Attachment) -> bool {
    let mime_ok = att
        .mime
        .as_deref()
        .map(|m| m.to_ascii_lowercase().starts_with("image/"))
        .unwrap_or(false);
    mime_ok || att.kind.eq_ignore_ascii_case("image")
}

/// All image data URLs in this attachment list, in order. Used when
/// emitting OpenAI multipart content.
pub fn image_data_urls(attachments: &[Attachment]) -> Vec<String> {
    attachments
        .iter()
        .filter(|a| is_image(a))
        .filter_map(|a| a.data_url.clone())
        .filter(|u| !u.is_empty())
        .collect()
}

/// Remove image parts from a message list, replacing each stripped image
/// with a short text marker. Used before sending a turn to a non-vision
/// text model so a historical image in the thread doesn't get serialized
/// into multipart content that the backend rejects (llama.cpp without an
/// mmproj returns a 500 for any image input).
fn strip_images_from_history(
    messages: Vec<crate::context::ChatMessage>,
) -> Vec<crate::context::ChatMessage> {
    use crate::context::ChatMessage;
    messages
        .into_iter()
        .map(|m| match m {
            ChatMessage::User {
                content,
                name,
                image_data_urls,
            } if !image_data_urls.is_empty() => {
                const NOTE: &str =
                    "[an image was attached here; the current model can't view images]";
                let new_content = if content.trim().is_empty() {
                    NOTE.to_string()
                } else {
                    format!("{content}\n\n{NOTE}")
                };
                ChatMessage::User {
                    content: new_content,
                    name,
                    image_data_urls: vec![],
                }
            }
            other => other,
        })
        .collect()
}

/// Where should this turn go?
#[derive(Debug, Clone)]
pub enum Route {
    /// Use the active chat LLM directly. Either there are no images, or
    /// the chat model is vision-capable.
    Chat,
    /// Send to a dedicated vision endpoint. If `failover` is `Some`,
    /// retry there on a transient error.
    Vision {
        primary: VisionEndpoint,
        failover: Option<VisionEndpoint>,
    },
}

/// Decide the route for a turn.
///
/// * `chat_model` — the host's currently selected chat model id
/// * `attachments` — what the user attached this turn
/// * `vision` — the host's vision settings
pub fn decide(chat_model: &str, attachments: &[Attachment], vision: &VisionSettings) -> Result<Route> {
    let has_image = attachments.iter().any(is_image);
    if !has_image {
        return Ok(Route::Chat);
    }
    if is_vision_capable(chat_model) {
        return Ok(Route::Chat);
    }
    if !vision.enabled || vision.primary.base_url.is_empty() || vision.primary.model.is_empty() {
        return Err(anyhow!(
            "This image needs a vision-capable model. Either pick one as your chat model \
             (Claude Sonnet/Opus, Gemini Pro/Flash, GPT-4o, llava, qwen-vl…) or configure \
             a Vision endpoint in Settings → Host."
        ));
    }
    let failover = if !vision.failover.base_url.is_empty() && !vision.failover.model.is_empty() {
        Some(vision.failover.clone())
    } else {
        None
    };
    Ok(Route::Vision {
        primary: vision.primary.clone(),
        failover,
    })
}

/// Convert a `VisionEndpoint` into the `LlmSettings` shape the existing
/// LLM client takes. Keeps the rest of the pipeline ignorant of the
/// vision-specific routing — once we know which endpoint to call, it's
/// just another OpenAI-compatible target.
pub fn endpoint_to_llm_settings(ep: &VisionEndpoint, base: &LlmSettings) -> LlmSettings {
    LlmSettings {
        // The vision endpoint type is always "openai-compat" at the wire
        // level — even Gemini and Anthropic both expose multipart through
        // their OpenAI-compat shims today.
        provider: "openai-compat".into(),
        base_url: ep.base_url.clone(),
        model: ep.model.clone(),
        api_key: ep.api_key.clone(),
        // Reuse the chat model's reasoning knobs as defaults — the user
        // hasn't been asked for per-vision tuning yet, and copying these
        // gives reasonable starting behavior.
        context_window: base.context_window,
        temperature: base.temperature,
        max_tokens: base.max_tokens,
        system_addendum: base.system_addendum.clone(),
        enabled: true,
    }
}

/// Run the chat pipeline with vision-aware routing. If the active chat
/// model is vision-capable (or there are no images), defers to the
/// caller-supplied `default_client`. Otherwise spins up a one-shot
/// LlmClient pointed at the primary vision endpoint, and falls over to
/// the failover endpoint exactly once on a transient error.
///
/// Tools are intentionally disabled on the vision path. Most vision
/// providers (Gemini, Anthropic via OpenAI-compat, vLLM/Ollama llava)
/// either don't support function calling on multimodal turns, or do so
/// inconsistently. The user-facing intent of "ask about this image" is
/// also rarely a research session — keeping it single-shot avoids tool
/// loops misfiring on multipart content.
pub async fn run_with_route(
    route: Route,
    default_client: crate::llm::LlmClient,
    chat_cfg: &LlmSettings,
    messages: Vec<crate::context::ChatMessage>,
    tools: Vec<crate::tools::registry::ToolDef>,
    tool_runtime: crate::tools::registry::ToolRuntime,
    max_tokens: Option<usize>,
    handlers: crate::tools::loop_pipeline::PipelineHandlers,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<crate::tools::loop_pipeline::PipelineResult> {
    use crate::tools::loop_pipeline::run_pipeline;
    match route {
        Route::Chat => {
            // Strip image parts from the messages when the chat model
            // can't see images. This is the fix for the "/deep fails in
            // a thread that earlier had an image" bug: decide() only
            // inspects the CURRENT message's attachments, so a text
            // follow-up routes here (Route::Chat) — but build_context
            // still carries any IMAGE from the thread history, which
            // serialize_message turns into OpenAI multipart content. A
            // non-vision backend (notably llama.cpp without an mmproj
            // projector — exactly the deep slot's Qwen3) then rejects
            // the WHOLE request with "500: image input is not supported".
            // The text model couldn't use the image anyway; replace it
            // with a short text marker so the turn still succeeds and the
            // conversation still references that an image was there.
            let messages = if is_vision_capable(&default_client.settings.model) {
                messages
            } else {
                strip_images_from_history(messages)
            };
            run_pipeline(
                default_client,
                messages,
                tools,
                max_tokens,
                tool_runtime,
                handlers,
                cancel,
            )
            .await
            .map_err(Into::into)
        }
        Route::Vision { primary, failover } => {
            let primary_client =
                crate::llm::LlmClient::new(endpoint_to_llm_settings(&primary, chat_cfg));
            // Vision turns skip tools — see function doc comment for why.
            let no_tools: Vec<crate::tools::registry::ToolDef> = vec![];
            let no_runtime = crate::tools::registry::ToolRuntime::default();
            let attempt = run_pipeline(
                primary_client,
                messages.clone(),
                no_tools.clone(),
                max_tokens,
                no_runtime.clone(),
                handlers.clone(),
                cancel.clone(),
            )
            .await;
            match attempt {
                Ok(r) => Ok(r),
                Err(e) => {
                    let msg = e.to_string();
                    let should_fail_over = failover.is_some() && is_transient_failure(&msg);
                    if !should_fail_over {
                        return Err(e.into());
                    }
                    let fo = failover.unwrap();
                    tracing::warn!(
                        "vision primary ({}) failed transiently: {msg} — failing over to {}",
                        primary.label,
                        fo.label
                    );
                    let fo_client =
                        crate::llm::LlmClient::new(endpoint_to_llm_settings(&fo, chat_cfg));
                    run_pipeline(
                        fo_client,
                        messages,
                        no_tools,
                        max_tokens,
                        no_runtime,
                        handlers,
                        cancel,
                    )
                    .await
                    .map_err(Into::into)
                }
            }
        }
    }
}

/// CCC-style transient-error heuristic. Returns true when the failover
/// chain should fire. We deliberately overshoot a little (any 5xx is
/// transient) rather than miss a Gemini overload — the failover only
/// runs once so a false positive costs one extra API call, never a loop.
pub fn is_transient_failure(err_msg: &str) -> bool {
    let lc = err_msg.to_ascii_lowercase();
    if lc.contains("status: 5") {
        return true;
    }
    let needles = [
        "429",
        "rate limit",
        "rate_limit",
        "high demand",
        "resource_exhausted",
        "resource exhausted",
        "overloaded",
        "overload",
        "timeout",
        "timed out",
        "service unavailable",
        "temporarily unavailable",
        "internal error",
    ];
    needles.iter().any(|n| lc.contains(n))
}

#[cfg(test)]
mod image_strip_tests {
    use super::*;
    use crate::context::ChatMessage;

    #[test]
    fn strips_image_from_history_user_message() {
        let msgs = vec![
            ChatMessage::System { content: "sys".into() },
            ChatMessage::User {
                content: "look at this".into(),
                name: Some("Wolf".into()),
                image_data_urls: vec!["data:image/png;base64,AAAA".into()],
            },
            ChatMessage::Assistant { content: "nice pic".into(), tool_calls: vec![] },
            ChatMessage::User {
                content: "/deep test".into(),
                name: Some("Wolf".into()),
                image_data_urls: vec![],
            },
        ];
        let out = strip_images_from_history(msgs);
        // The image-bearing message must now carry zero image URLs...
        if let ChatMessage::User { image_data_urls, content, .. } = &out[1] {
            assert!(image_data_urls.is_empty(), "image url must be stripped");
            assert!(content.contains("look at this"), "original text preserved");
            assert!(content.contains("can't view images"), "marker appended");
        } else {
            panic!("expected User at index 1");
        }
        // ...and the text-only messages are untouched.
        if let ChatMessage::User { content, .. } = &out[3] {
            assert_eq!(content, "/deep test");
        } else {
            panic!("expected User at index 3");
        }
    }

    #[test]
    fn empty_caption_image_becomes_marker_only() {
        let msgs = vec![ChatMessage::User {
            content: "".into(),
            name: None,
            image_data_urls: vec!["data:image/png;base64,AAAA".into()],
        }];
        let out = strip_images_from_history(msgs);
        if let ChatMessage::User { content, image_data_urls, .. } = &out[0] {
            assert!(image_data_urls.is_empty());
            assert!(content.contains("can't view images"));
        } else {
            panic!("expected User");
        }
    }
}
