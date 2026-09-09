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
    // Open-source vision models commonly served via Ollama / vLLM /
    // llama.cpp (GGUF + mmproj). The Qwen-VL line is a popular fully-local
    // choice — note "qwen2.5-vl" is NOT a superstring of "qwen2-vl" (the
    // ".5" breaks the substring), so each generation needs its own fragment.
    "llava", "qwen-vl", "qwen2-vl", "qwen2.5-vl", "qwen3-vl",
    "moondream", "minicpm-v", "pixtral", "cogvlm", "internvl",
    "phi-3-vision", "phi-3.5-vision",
];

/// Returns true if the given model name is in the hardcoded vision list.
/// Substring + case-insensitive — see VISION_CAPABLE_FRAGMENTS docs for
/// the rationale.
pub fn is_vision_capable(model: &str) -> bool {
    let lc = model.to_ascii_lowercase();
    VISION_CAPABLE_FRAGMENTS.iter().any(|frag| lc.contains(frag))
}

/// The per-slot image_recognition setting, resolved to a yes/no. Pure so
/// the whole decision table is unit-testable; the async probing lives in
/// `chat_handles_images`.
///
/// `probed` is the server's own answer when we have one (llama.cpp
/// `/props` modalities). It outranks the name list because the name
/// list can be wrong in BOTH directions for llama.cpp: a "qwen-vl"
/// GGUF loaded without its mmproj projector cannot see, and a plainly
/// named model with a projector can.
fn resolve_native_vision(setting: &str, probed: Option<bool>, model: &str) -> bool {
    match setting {
        "native" => true,
        "external" => false,
        // "auto" and anything unrecognized.
        _ => probed.unwrap_or_else(|| is_vision_capable(model)),
    }
}

/// Ask a llama.cpp server whether it has a multimodal projector loaded.
/// `None` when the server doesn't answer or doesn't report modalities
/// (old builds) — the caller falls back to the name list.
///
/// Cached per base_url for a short window: the answer changes only when
/// the server is relaunched with different flags, but every image turn
/// and every text follow-up in an image thread asks.
async fn probe_llamacpp_vision(base_url: &str, api_key: Option<&str>) -> Option<bool> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<HashMap<String, (Option<bool>, Instant)>>> = Mutex::new(None);
    const TTL: Duration = Duration::from_secs(120);

    let key = base_url.trim_end_matches('/').to_string();
    if let Some((hit, at)) = CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .get(&key)
        .copied()
    {
        if at.elapsed() < TTL {
            return hit;
        }
    }
    let probed = async {
        let mut req = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .ok()?
            .get(format!("{key}/props"));
        // llama.cpp started with --api-key guards /props too on recent
        // builds — an unauthenticated probe would silently disable the
        // feature on exactly the servers configured most carefully.
        if let Some(k) = api_key.filter(|k| !k.trim().is_empty()) {
            req = req.bearer_auth(k.trim());
        }
        let resp = req.send().await.ok()?;
        let body: serde_json::Value = resp.json().await.ok()?;
        body.get("modalities")?.get("vision")?.as_bool()
    }
    .await;
    CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(key, (probed, Instant::now()));
    probed
}

/// Can this chat slot answer image turns itself? The single source of
/// truth for both the routing decision (`decide`) and the strip-images
/// guard in `run_with_route` — they must agree, or a turn routed to the
/// chat model would have its images stripped before the model saw them.
///
/// The probe runs ONLY under "auto": a slot pinned to "native" or
/// "external" has already answered the question, and paying a network
/// round-trip for a value the resolver discards would punish exactly
/// the user who pinned the setting to avoid probing.
pub async fn chat_handles_images(settings: &LlmSettings) -> bool {
    let setting = settings.image_recognition.as_str();
    let needs_probe = !matches!(setting, "native" | "external");
    let probed = if needs_probe
        && settings.provider == "llamacpp"
        && !settings.base_url.trim().is_empty()
    {
        probe_llamacpp_vision(settings.base_url.trim(), settings.api_key.as_deref()).await
    } else {
        None
    };
    resolve_native_vision(setting, probed, &settings.model)
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
/// Replaces a stripped image in a user message. Shared with
/// `should_force_search`, which must treat a marker-bearing caption like an
/// image-bearing one — the caption is still about a picture even after a
/// failover to a non-vision slot emptied `image_data_urls`.
pub(crate) const IMAGE_STRIP_MARKER: &str =
    "[an image was attached here; the current model can't view images]";

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
                let new_content = if content.trim().is_empty() {
                    IMAGE_STRIP_MARKER.to_string()
                } else {
                    format!("{content}\n\n{IMAGE_STRIP_MARKER}")
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

/// For a dedicated vision call, keep ONLY the current turn's image(s) and
/// drop images from earlier turns (replaced with a short text marker).
///
/// Sending the whole thread's image history on every vision turn balloons
/// the request: it 413s hosted providers (Groq caps request size) and, on a
/// local model, forces a multi-image prefill that can stall for minutes with
/// no output (tripping the stream inactivity guard) — the exact "Groq 413 →
/// fail over to local → local goes silent for 5 min → nothing" failure. A
/// vision Q&A only needs the image the user just attached, so we send that
/// one and summarize the rest away. The CURRENT turn (the last user message
/// bearing an image) keeps all of its images, so multi-image single-turn
/// questions ("compare these") still work.
fn keep_only_current_turn_images(
    messages: Vec<crate::context::ChatMessage>,
) -> Vec<crate::context::ChatMessage> {
    use crate::context::ChatMessage;
    let last_img_idx = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| {
            matches!(m, ChatMessage::User { image_data_urls, .. } if !image_data_urls.is_empty())
        })
        .map(|(i, _)| i);
    messages
        .into_iter()
        .enumerate()
        .map(|(i, m)| match m {
            ChatMessage::User {
                content,
                name,
                image_data_urls,
            } if !image_data_urls.is_empty() && Some(i) != last_img_idx => {
                const NOTE: &str =
                    "[an earlier image was attached here; omitted to keep this request small]";
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
/// * `chat` — the settings of the slot serving this turn (its
///   image_recognition preference and server decide the native path)
/// * `attachments` — what the user attached THIS turn
/// * `vision` — the host's vision settings
/// * `history_has_image` — whether the built context still carries an
///   image from a recent turn (see `history_has_image`). A text follow-up
///   about the photo just sent must reach a model that can see it, or
///   "Continue" after a photo answers blind.
pub async fn decide(
    chat: &LlmSettings,
    attachments: &[Attachment],
    vision: &VisionSettings,
    history_has_image: bool,
) -> Result<Route> {
    let fresh_image = attachments.iter().any(is_image);
    if !fresh_image && !history_has_image {
        return Ok(Route::Chat);
    }
    let turn = if fresh_image { "image turn" } else { "follow-up about a recent image" };
    if chat_handles_images(chat).await {
        tracing::info!(model = %chat.model, "{}: chat model answers natively", turn);
        return Ok(Route::Chat);
    }
    if !vision.enabled || vision.primary.base_url.is_empty() || vision.primary.model.is_empty() {
        if !fresh_image {
            // Nothing can see the carried image, but the user typed plain
            // text: run the turn on the chat model (the history image
            // becomes a text marker there) rather than failing a text turn.
            tracing::info!(
                model = %chat.model,
                "{}: no vision endpoint, chat model answers without the image",
                turn
            );
            return Ok(Route::Chat);
        }
        // Branch on WHY this slot can't take the image, or the advice
        // sends people to fixes that can't work (a slot pinned to
        // "external" ignores the chat model entirely; a probed "no"
        // outranks a vision-sounding model name).
        return Err(match chat.image_recognition.as_str() {
            "external" => anyhow!(
                "This model's Image recognition setting routes images to the Vision \
                 endpoint, but no Vision endpoint is configured. Set one up under \
                 Settings → Vision, or change the model's Image recognition setting."
            ),
            _ => anyhow!(
                "This image needs a model that can see. Give this model's server a \
                 vision projector (llama.cpp --mmproj) or pick a vision-capable chat \
                 model (Claude Sonnet/Opus, Gemini, GPT-4o, llava, qwen-vl…) — or \
                 configure a Vision endpoint under Settings → Vision."
            ),
        });
    }
    tracing::info!(model = %chat.model, "{}: routing to the vision endpoint", turn);
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

/// Does the built context still carry an image payload? True for the
/// current turn's own attachments and for the newest recent image the
/// context load carries forward (`db::messages::IMAGE_CARRY_ROWS`); older
/// images are stripped at the SQL layer and never reach here.
pub fn history_has_image(messages: &[crate::context::ChatMessage]) -> bool {
    messages.iter().any(|m| {
        matches!(
            m,
            crate::context::ChatMessage::User { image_data_urls, .. } if !image_data_urls.is_empty()
        )
    })
}

/// Should a vision turn on this endpoint keep the chat toolset? Yes when
/// the endpoint is the same server AND model as an active chat slot: that
/// pair already runs tool-enabled image turns on the chat path (verified
/// on the fast slot), so the "arbitrary endpoint" caution that keeps other
/// vision endpoints tool-free does not apply. Without this, a photo whose
/// caption asks for a lookup ends after the model's announcement — it
/// writes a tool call the pipeline then has to strip.
pub fn endpoint_keeps_tools(ep: &VisionEndpoint, cfg: &crate::config::AppConfig) -> bool {
    fn norm(url: &str) -> String {
        url.trim().trim_end_matches('/').to_ascii_lowercase()
    }
    let (url, model) = (norm(&ep.base_url), ep.model.trim());
    !url.is_empty()
        && !model.is_empty()
        && crate::slash::SLOTS.iter().any(|s| {
            let slot = crate::slash::slot_settings(cfg, s);
            slot.is_active() && norm(&slot.base_url) == url && slot.model.trim() == model
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
        // This client IS the vision model — nothing downstream re-routes
        // on the field, but "native" is the truthful value.
        image_recognition: "native".into(),
    }
}

/// Run the chat pipeline with vision-aware routing. If the active chat
/// model is vision-capable (or there are no images), defers to the
/// caller-supplied `default_client`. Otherwise spins up a one-shot
/// LlmClient pointed at the primary vision endpoint, and falls over to
/// the failover endpoint exactly once on a transient error.
///
/// Tools stay off on the vision path for arbitrary endpoints — most hosted
/// vision providers (Gemini, Anthropic via OpenAI-compat, vLLM/Ollama
/// llava) handle function calling on multimodal turns inconsistently —
/// EXCEPT when the endpoint is one of the household's own chat slots
/// (`endpoint_keeps_tools`), which already run tool-enabled image turns
/// natively.
pub async fn run_with_route(
    route: Route,
    cfg: &crate::config::AppConfig,
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
            use crate::context::ChatMessage;
            let has_any_image = messages.iter().any(|m| {
                matches!(m, ChatMessage::User { image_data_urls, .. } if !image_data_urls.is_empty())
            });
            if !has_any_image {
                // Pure text turn in an image-free window — there is no
                // capability question to answer, so no probe either.
                return run_pipeline(
                    default_client, messages, tools, max_tokens, tool_runtime, handlers, cancel,
                )
                .await
                .map_err(Into::into);
            }
            // Resolved ONCE per attempt and reused for every decision
            // below — the slot failover re-enters this function with the
            // failover slot's settings, so each attempt answers for the
            // model that will actually serve it.
            let native = chat_handles_images(&default_client.settings).await;
            if !native {
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
                let messages = strip_images_from_history(messages);
                return run_pipeline(
                    default_client, messages, tools, max_tokens, tool_runtime, handlers, cancel,
                )
                .await
                .map_err(Into::into);
            }
            let current_turn_has_image = matches!(
                messages.iter().rev().find(|m| matches!(m, ChatMessage::User { .. })),
                Some(ChatMessage::User { image_data_urls, .. }) if !image_data_urls.is_empty()
            );
            if current_turn_has_image {
                // The model can see and the user just attached an image.
                // Older images become text markers (multi-image prefill
                // stalls local servers), but the toolset STAYS: this model
                // makes clean structured tool calls on multipart turns
                // (verified against the fast slot), and a family image turn
                // is often "here's a screenshot — is this real?", which
                // needs a search. Without tools, the model wrote tool-call
                // syntax into its visible reply and then told the user it
                // had no web access. Only FORCED search stays off on image
                // turns — see should_force_search — so a question-shaped
                // caption can't hijack the turn into a mandatory search
                // round before the model has looked at the picture.
                let messages = keep_only_current_turn_images(messages);
                return run_pipeline(
                    default_client,
                    messages,
                    tools,
                    max_tokens,
                    tool_runtime,
                    handlers,
                    cancel,
                )
                .await
                .map_err(Into::into);
            }
            // Text follow-up in a thread whose window still holds an
            // image, on a model that can see it: a normal tool-enabled
            // turn, history image intact.
            run_pipeline(
                default_client, messages, tools, max_tokens, tool_runtime, handlers, cancel,
            )
            .await
            .map_err(Into::into)
        }
        Route::Vision { primary, failover } => {
            // Hosted vision endpoints have their own (often small) OUTPUT
            // ceiling — e.g. Groq's Llama-4 vision rejects `max_tokens` over
            // 8192 — whereas the chat-derived `max_tokens` here can be tens of
            // thousands (from a 32k local chat context window). An image Q&A
            // reply never needs that much, so cap it to a value every provider
            // accepts. (`None` already means "let the server decide".)
            const VISION_MAX_TOKENS: usize = 4096;
            let vision_max_tokens = max_tokens.map(|m| m.min(VISION_MAX_TOKENS));
            // Send only the newest image — the one just attached, or the
            // recent one the context load carried forward for a follow-up —
            // not every image ever sent in this thread: the accumulated
            // history 413s hosted providers and stalls local models in
            // multi-image prefill.
            let messages = keep_only_current_turn_images(messages);
            let attempt = run_vision_endpoint(
                &primary,
                cfg,
                chat_cfg,
                messages.clone(),
                &tools,
                &tool_runtime,
                vision_max_tokens,
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
                        return Err(e);
                    }
                    let fo = failover.unwrap();
                    tracing::warn!(
                        "vision primary ({}) failed transiently: {msg} — failing over to {}",
                        primary.label,
                        fo.label
                    );
                    run_vision_endpoint(
                        &fo,
                        cfg,
                        chat_cfg,
                        messages,
                        &tools,
                        &tool_runtime,
                        vision_max_tokens,
                        handlers,
                        cancel,
                    )
                    .await
                }
            }
        }
    }
}

/// One vision-endpoint attempt. Tools ride along only when the endpoint is
/// a household chat slot (`endpoint_keeps_tools`); otherwise the turn is
/// tool-free and the system prompt SAYS so — a tool-trained model asked
/// "is this real?" otherwise writes tool-call syntax into its visible reply
/// and then claims it has no web access at all.
#[allow(clippy::too_many_arguments)]
async fn run_vision_endpoint(
    ep: &VisionEndpoint,
    cfg: &crate::config::AppConfig,
    chat_cfg: &LlmSettings,
    mut messages: Vec<crate::context::ChatMessage>,
    tools: &[crate::tools::registry::ToolDef],
    tool_runtime: &crate::tools::registry::ToolRuntime,
    max_tokens: Option<usize>,
    handlers: crate::tools::loop_pipeline::PipelineHandlers,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<crate::tools::loop_pipeline::PipelineResult> {
    let with_tools = !tools.is_empty() && endpoint_keeps_tools(ep, cfg);
    tracing::info!(endpoint = %ep.label, model = %ep.model, tools = with_tools, "vision turn");
    let (tools, runtime) = if with_tools {
        (tools.to_vec(), tool_runtime.clone())
    } else {
        // Folded into the existing leading system message when there is
        // one: strict templates reject a second system message anywhere
        // after position 0.
        const NO_TOOLS_NOTE: &str = "You are answering a single question about the \
attached image, and no tools are available on this turn. Answer from the image itself. \
If the question also needs live information (verifying claims, current prices, news), \
describe what you see and suggest asking again as a plain text message, where KinAI's \
normal tools apply. Never write tool-call syntax into your reply, and make no claims \
about what KinAI can or cannot access — this turn's limits are not KinAI's limits.";
        match messages.first_mut() {
            Some(crate::context::ChatMessage::System { content }) => {
                content.push_str("\n\n");
                content.push_str(NO_TOOLS_NOTE);
            }
            _ => messages.insert(
                0,
                crate::context::ChatMessage::System {
                    content: NO_TOOLS_NOTE.into(),
                },
            ),
        }
        (Vec::new(), crate::tools::registry::ToolRuntime::default())
    };
    let client = crate::llm::LlmClient::new(endpoint_to_llm_settings(ep, chat_cfg));
    crate::tools::loop_pipeline::run_pipeline(
        client, messages, tools, max_tokens, runtime, handlers, cancel,
    )
    .await
    .map_err(Into::into)
}

/// Returns true when a primary-endpoint failure warrants trying the
/// failover. Covers two classes:
///
///   1. TRANSIENT cloud hiccups (any 5xx, 429, overload, timeout) — the
///      original CCC-style heuristic. We overshoot a little rather than
///      miss a Gemini overload; the failover runs once, so a false
///      positive costs one extra call, never a loop.
///   2. PRIMARY-CONFIG failures a *different* endpoint can rescue: a model
///      that's been retired (decommissioned / not found) or a request the
///      provider rejects as too large. These aren't transient, but the
///      failover (e.g. a local model with no size cap, or a live model id)
///      may well succeed — exactly the case where a cloud vendor pulls a
///      model out from under a working config. (Downscaling now keeps us
///      under size caps, so "too large" is belt-and-suspenders.)
pub fn is_transient_failure(err_msg: &str) -> bool {
    let lc = err_msg.to_ascii_lowercase();
    if lc.contains("status: 5") {
        return true;
    }
    let needles = [
        // Transient
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
        // Retired / wrong model — a different endpoint may have a live one
        "decommission",
        "model_not_found",
        "model not found",
        "does not exist",
        "no longer available",
        // Request rejected as too large — a local endpoint may have no cap
        "413",
        "request_too_large",
        "request entity too large",
        "too large",
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
                name: Some("Alex".into()),
                image_data_urls: vec!["data:image/png;base64,AAAA".into()],
            },
            ChatMessage::Assistant { content: "nice pic".into(), tool_calls: vec![] },
            ChatMessage::User {
                content: "/deep test".into(),
                name: Some("Alex".into()),
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

#[cfg(test)]
mod vision_capable_tests {
    use super::is_vision_capable;

    #[test]
    fn per_slot_setting_resolves_the_whole_decision_table() {
        use super::resolve_native_vision as resolve;
        // Explicit overrides win over everything.
        assert!(resolve("native", Some(false), "qwen3-32b"));
        assert!(resolve("native", None, "qwen3-32b"));
        assert!(!resolve("external", Some(true), "Qwen2.5-VL-32B"));
        // Auto: the server's own answer outranks the name list — a plainly
        // named model with an mmproj CAN see (Qwen3.8-27B), and a VL-named
        // GGUF loaded without its projector CANNOT.
        assert!(resolve("auto", Some(true), "Qwen3.8-27B-Q4_K_M"));
        assert!(!resolve("auto", Some(false), "Qwen2.5-VL-32B-Instruct"));
        // Auto with no probe answer (cloud provider, old llama.cpp build,
        // server down): the name list decides, exactly as before 0.2.103.
        assert!(resolve("auto", None, "gpt-4o"));
        assert!(!resolve("auto", None, "qwen3-32b"));
        // Unknown values behave like auto, not like a crash.
        assert!(resolve("", Some(true), "qwen3-32b"));
        assert!(!resolve("typo", None, "qwen3-32b"));
    }

    #[test]
    fn recognises_qwen_vl_generations_as_chat_vision() {
        // Real-world served ids (llama.cpp aliases, GGUF names, LM Studio).
        for m in [
            "Qwen2.5-VL-32B-Instruct",
            "qwen2.5-vl-7b-instruct-q4_k_m",
            "Qwen2-VL-7B",
            "qwen3-vl-8b",
        ] {
            assert!(is_vision_capable(m), "{m} should be vision-capable");
        }
    }

    #[test]
    fn text_only_models_are_not_vision() {
        // These must route to the dedicated Vision endpoint, not inline —
        // sending multipart to a text-only backend 500s.
        for m in ["qwen3-32b", "qwen2.5-coder-7b", "llama3.1:8b", "gpt-oss-120b"] {
            assert!(!is_vision_capable(m), "{m} must NOT be treated as vision");
        }
    }
}

#[cfg(test)]
mod failover_trigger_tests {
    use super::is_transient_failure;

    #[test]
    fn fails_over_on_transient_and_recoverable_errors() {
        for msg in [
            "LLM error 503 Service Unavailable",
            "status: 500",
            "429 Too Many Requests",
            "model has been decommissioned",
            "invalid model: model_not_found",
            "LLM error 413: {\"code\":\"request_too_large\"}",
            "Request Entity Too Large",
        ] {
            assert!(is_transient_failure(msg), "should fail over: {msg}");
        }
    }

    #[test]
    fn does_not_fail_over_on_plain_client_errors() {
        // A 401/400 that a different endpoint can't fix shouldn't burn a
        // failover call.
        for msg in [
            "LLM error 401 Unauthorized: invalid api key",
            "LLM error 400 Bad Request: malformed json",
        ] {
            assert!(!is_transient_failure(msg), "should NOT fail over: {msg}");
        }
    }
}

#[cfg(test)]
mod vision_history_tests {
    use super::keep_only_current_turn_images;
    use crate::context::ChatMessage;

    #[test]
    fn keeps_only_the_last_turns_image() {
        let msgs = vec![
            ChatMessage::System { content: "sys".into() },
            ChatMessage::User {
                content: "first pic".into(),
                name: Some("Alex".into()),
                image_data_urls: vec!["data:image/png;base64,OLD".into()],
            },
            ChatMessage::Assistant { content: "ok".into(), tool_calls: vec![] },
            ChatMessage::User {
                content: "what do you see?".into(),
                name: Some("Alex".into()),
                image_data_urls: vec!["data:image/png;base64,CURRENT".into()],
            },
        ];
        let out = keep_only_current_turn_images(msgs);
        // Old image stripped, marker added, text preserved.
        if let ChatMessage::User { image_data_urls, content, .. } = &out[1] {
            assert!(image_data_urls.is_empty(), "history image must be dropped");
            assert!(content.contains("first pic"));
            assert!(content.contains("omitted"));
        } else {
            panic!("expected User at 1");
        }
        // Current (last) image kept intact.
        if let ChatMessage::User { image_data_urls, .. } = &out[3] {
            assert_eq!(image_data_urls, &vec!["data:image/png;base64,CURRENT".to_string()]);
        } else {
            panic!("expected User at 3");
        }
    }

    #[test]
    fn preserves_multiple_images_in_the_current_turn() {
        let msgs = vec![ChatMessage::User {
            content: "compare these".into(),
            name: None,
            image_data_urls: vec![
                "data:image/png;base64,A".into(),
                "data:image/png;base64,B".into(),
            ],
        }];
        let out = keep_only_current_turn_images(msgs);
        if let ChatMessage::User { image_data_urls, .. } = &out[0] {
            assert_eq!(image_data_urls.len(), 2, "single-turn multi-image kept");
        } else {
            panic!("expected User");
        }
    }
}

#[cfg(test)]
mod carried_image_route_tests {
    use super::*;
    use crate::config::{AppConfig, VisionEndpoint, VisionSettings};
    use crate::context::ChatMessage;

    /// An openai-compat slot: no llama.cpp probe runs, and the model name
    /// is not on the vision list — the slot cannot see.
    fn blind_chat() -> LlmSettings {
        let mut s = LlmSettings::default();
        s.provider = "openai-compat".into();
        s.base_url = "https://api.example.test".into();
        s.model = "text-only-model".into();
        s.image_recognition = "auto".into();
        s
    }

    fn vision_on() -> VisionSettings {
        VisionSettings {
            enabled: true,
            primary: VisionEndpoint {
                label: "Local".into(),
                base_url: "http://10.0.0.1:8081".into(),
                model: "see-model".into(),
                api_key: None,
            },
            failover: VisionEndpoint::default(),
        }
    }

    fn img() -> Attachment {
        Attachment {
            kind: "image".into(),
            mime: Some("image/jpeg".into()),
            name: Some("photo.jpg".into()),
            data_url: Some("data:image/jpeg;base64,QUJD".into()),
        }
    }

    #[tokio::test]
    async fn text_follow_up_about_a_carried_image_routes_to_vision() {
        let route = decide(&blind_chat(), &[], &vision_on(), true).await.unwrap();
        assert!(matches!(route, Route::Vision { .. }), "carried image must reach a model that sees");
    }

    #[tokio::test]
    async fn plain_text_turn_stays_on_chat() {
        let route = decide(&blind_chat(), &[], &vision_on(), false).await.unwrap();
        assert!(matches!(route, Route::Chat));
    }

    #[tokio::test]
    async fn carried_image_without_a_vision_endpoint_still_answers() {
        let route = decide(&blind_chat(), &[], &VisionSettings::default(), true)
            .await
            .unwrap();
        assert!(matches!(route, Route::Chat), "a text turn must not fail over a stale image");
        // A FRESH image with nowhere to go is still an error the user can fix.
        assert!(decide(&blind_chat(), &[img()], &VisionSettings::default(), false)
            .await
            .is_err());
    }

    #[test]
    fn history_has_image_sees_any_user_image() {
        let msgs = vec![
            ChatMessage::User {
                content: "photo".into(),
                name: None,
                image_data_urls: vec!["data:image/png;base64,A".into()],
            },
            ChatMessage::Assistant { content: "ok".into(), tool_calls: vec![] },
            ChatMessage::User {
                content: "Continue".into(),
                name: None,
                image_data_urls: vec![],
            },
        ];
        assert!(history_has_image(&msgs));
        assert!(!history_has_image(&msgs[1..]));
    }

    #[test]
    fn tools_stay_on_only_for_a_household_chat_slot() {
        let mut cfg = AppConfig::default();
        cfg.llm.enabled = true;
        cfg.llm.base_url = "http://10.0.0.1:8081/".into();
        cfg.llm.model = "see-model".into();
        let ep = vision_on().primary;
        assert!(endpoint_keeps_tools(&ep, &cfg), "same server + model as the fast slot");

        let mut other = ep.clone();
        other.model = "other-model".into();
        assert!(!endpoint_keeps_tools(&other, &cfg), "same server, different model");

        let mut hosted = ep.clone();
        hosted.base_url = "https://vision.example.test".into();
        assert!(!endpoint_keeps_tools(&hosted, &cfg), "a hosted endpoint stays tool-free");

        cfg.llm.enabled = false;
        assert!(!endpoint_keeps_tools(&ep, &cfg), "an inactive slot is no evidence");
    }
}
