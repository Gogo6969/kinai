//! Route incoming Telegram updates into KinAI's chat pipeline.
//!
//! Four cases per inbound message:
//!
//!   1. `/start <token>` — first-time pairing handshake. Redeem the
//!      pending pair → permanent link → confirmation message.
//!
//!   2. `/start` (no arg) — friendly welcome explaining how to pair
//!      from KinAI Settings.
//!
//!   3. Message from an UNpaired chat — short reply telling the user
//!      they need to pair first.
//!
//!   4. Message from a paired chat — find the dedicated Telegram
//!      thread for that peer, run the chat turn (slash commands work
//!      identically to KinAI's WS path), send the assistant reply
//!      back via sendMessage / sendPhoto.

use anyhow::Result;
use base64::Engine;
use tauri::{AppHandle, Emitter, Runtime};

use crate::db::telegram as tg_db;
use crate::db::Attachment;
use crate::network::protocol::Envelope;
use crate::SharedState;

use super::api::{BotApi, PhotoSize, TelegramMessage, TelegramUpdate};

pub async fn handle_update<R: Runtime>(
    api: &BotApi,
    state: &SharedState,
    app: &AppHandle<R>,
    update: &TelegramUpdate,
) -> Result<()> {
    let Some(msg) = &update.message else {
        return Ok(()); // non-message updates filtered upstream; defensive guard
    };
    let chat_id = msg.chat.id;
    let text_or_caption = msg
        .text
        .clone()
        .or_else(|| msg.caption.clone())
        .unwrap_or_default();

    // /start handshake
    if let Some(rest) = text_or_caption.strip_prefix("/start") {
        let arg = rest.trim();
        return handle_start(api, state, chat_id, msg, arg).await;
    }

    // Anything else routes through the chat pipeline IF paired.
    let Some(peer_id) = tg_db::peer_for_chat(&state.db.pool, &chat_id.to_string()).await? else {
        api.send_message(
            chat_id,
            "👋 Hi! I'm KinAI's family bot, but this Telegram isn't linked to a KinAI account yet.\n\n\
             To pair: open KinAI on your computer → Settings → Telegram → \"Connect Telegram\" \
             → scan the QR code shown there.",
        )
        .await?;
        return Ok(());
    };

    // Routed — run the chat turn.
    if let Err(e) = run_turn_for_peer(api, state, app, chat_id, &peer_id, &text_or_caption, msg)
        .await
    {
        tracing::warn!("telegram run_turn: {e:?}");
        let _ = api
            .send_message(
                chat_id,
                &format!("Something went wrong on KinAI's end: {e}"),
            )
            .await;
    }
    Ok(())
}

async fn handle_start(
    api: &BotApi,
    state: &SharedState,
    chat_id: i64,
    msg: &TelegramMessage,
    arg: &str,
) -> Result<()> {
    if arg.is_empty() {
        api.send_message(
            chat_id,
            "👋 Hi! I'm a family bot for KinAI — a private local AI shared between household devices.\n\n\
             To connect this Telegram to your KinAI account, open KinAI on a Mac or Windows PC, \
             go to Settings → Telegram, click \"Connect Telegram\", then scan the QR code with your \
             phone's camera.",
        )
        .await?;
        return Ok(());
    }

    let username = msg.from.as_ref().and_then(|u| u.username.as_deref());
    let first_name = msg.from.as_ref().map(|u| u.first_name.as_str()).filter(|s| !s.is_empty());

    match tg_db::redeem_pair(
        &state.db.pool,
        arg,
        &chat_id.to_string(),
        username,
        first_name,
    )
    .await?
    {
        Some(peer_id) => {
            tracing::info!("telegram: paired chat {chat_id} → peer {peer_id}");
            api.send_message(
                chat_id,
                "✅ Linked to KinAI.\n\nTry:\n• `/help` — list commands\n• `/pic a sunset over Miami` — generate an image\n• or just type any question.",
            )
            .await?;
        }
        None => {
            api.send_message(
                chat_id,
                "❌ That pairing code was invalid or expired (codes last 10 minutes). \
                 Generate a fresh QR code in KinAI Settings → Telegram and try again.",
            )
            .await?;
        }
    }
    Ok(())
}

/// Run a chat turn for `peer_id` on the dedicated Telegram thread,
/// send the reply back via Telegram, and also persist + emit
/// the messages so any open KinAI client for that peer sees the
/// exchange in real time.
async fn run_turn_for_peer<R: Runtime>(
    api: &BotApi,
    state: &SharedState,
    app: &AppHandle<R>,
    chat_id: i64,
    peer_id: &str,
    content: &str,
    msg: &TelegramMessage,
) -> Result<()> {
    // Pull any inbound photo into a KinAI image attachment so the vision
    // pipeline can analyze it (routes to the configured Vision endpoint —
    // Gemini/Claude with failover — exactly like an in-app image paste).
    // Before this the router only looked at text/caption and silently
    // dropped `msg.photo`, so "send me an image to analyze" did nothing.
    let attachments: Vec<Attachment> = match photo_attachment(api, msg).await {
        Ok(Some(att)) => vec![att],
        Ok(None) => vec![],
        Err(e) => {
            tracing::warn!("telegram photo download failed: {e:?}");
            api.send_message(
                chat_id,
                &format!("I couldn't download that image from Telegram: {e}"),
            )
            .await?;
            return Ok(());
        }
    };
    let has_image = !attachments.is_empty();

    // Nothing actionable: no text AND no image (sticker, voice note, …).
    if content.trim().is_empty() && !has_image {
        return Ok(()); // ignore unknown payloads for v1
    }

    // A photo with no caption still needs a prompt for the model to act
    // on. Fall back to a generic "describe it" so a bare image gets a
    // useful answer instead of an empty user turn.
    let content: &str = if content.trim().is_empty() && has_image {
        "What's in this image?"
    } else {
        content
    };

    // /newchat — rotate this Telegram chat to a fresh thread so a new
    // question doesn't inherit the previous conversation's context.
    // Persistent memory + saved facts are unaffected (they're
    // peer-scoped, not thread-scoped).
    let mut content = content;
    if let Some(rest) = strip_newchat(content) {
        let fresh = state.db.create_thread(peer_id, Some("Telegram")).await?;
        let _ = tg_db::set_active_thread(&state.db.pool, peer_id, &fresh.id).await;
        if rest.is_empty() {
            api.send_message(
                chat_id,
                "🆕 Started a new chat — earlier messages won't be used as \
                 context. Your saved memory is still active.",
            )
            .await?;
            return Ok(());
        }
        // A question rode along (`/newchat <prompt>`): answer it in the
        // brand-new, empty thread.
        content = rest;
    }

    // /voice — per-chat opt-in for spoken replies. Bare `/voice`
    // toggles; `/voice on` / `/voice off` set explicitly. Gated on the
    // host's master switch so the command can explain itself instead of
    // silently doing nothing when TTS is disabled in Settings.
    if let Some(arg) = strip_voice(content) {
        if !state.config.read().tts.enabled {
            api.send_message(
                chat_id,
                "🔇 Voice replies are switched off on the host. Ask the host \
                 to enable them in KinAI → Settings → Voice replies.",
            )
            .await?;
            return Ok(());
        }
        let current = tg_db::voice_replies(&state.db.pool, peer_id)
            .await
            .unwrap_or(false);
        let new_state = match arg.to_ascii_lowercase().as_str() {
            "" => !current,
            "on" => true,
            "off" => false,
            _ => {
                api.send_message(
                    chat_id,
                    "Usage: /voice — toggle spoken replies, or /voice on / /voice off.",
                )
                .await?;
                return Ok(());
            }
        };
        tg_db::set_voice_replies(&state.db.pool, peer_id, new_state).await?;
        let confirmation = if new_state {
            "🔊 Voice replies are ON for this chat — answers now arrive as text plus a spoken voice note. Send /voice again to turn them off."
        } else {
            "🔇 Voice replies are OFF for this chat — answers arrive as text only."
        };
        api.send_message(chat_id, confirmation).await?;
        return Ok(());
    }

    // Resolve this peer's Telegram thread: the `/newchat`-rotated one if
    // set, else the deterministic default (same chat → same thread
    // across restarts; backward compatible with pre-/newchat pairings).
    let thread_id = tg_db::active_thread(&state.db.pool, peer_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| telegram_thread_id_for_peer(peer_id));
    state
        .db
        .upsert_thread(peer_id, &thread_id, "Telegram")
        .await
        .ok();

    let cfg = state.config.read().clone();

    // Bare `/fast` or `/deep` (no body) = mode switch, not a question.
    // Persist the choice on the thread, confirm via Telegram, return.
    let trimmed_lc = content.trim().to_ascii_lowercase();
    if trimmed_lc == "/fast" || trimmed_lc == "/deep" {
        let slot = if trimmed_lc == "/deep" { "deep" } else { "fast" };
        // thread_id is already resolved + upserted above — write the
        // sticky slot to the active (possibly /newchat-rotated) thread.
        let _ = state.db.set_thread_active_slot(peer_id, &thread_id, Some(slot)).await;
        let active_model = if slot == "deep" {
            &cfg.llm_deep.model
        } else {
            &cfg.llm.model
        };
        let icon = if slot == "deep" { "🧠" } else { "⚡" };
        let label = if slot == "deep" { "deep" } else { "fast" };
        let body = if active_model.trim().is_empty() {
            format!("{icon} Switched to **{label}** model — but no model is configured for this slot. Open KinAI → Settings → {} model to add one.", if slot == "deep" { "Deep" } else { "Fast" })
        } else {
            format!(
                "{icon} Switched to **{label}** model (`{active_model}`).\nAll follow-up questions in this chat go here until you type `/{}` to switch back.",
                if slot == "deep" { "fast" } else { "deep" }
            )
        };
        api.send_message(chat_id, &body).await?;
        return Ok(());
    }

    // Persist user message (with any image attachment so build_context
    // carries the image into the prompt and the desktop UI shows it too).
    let sender = "Telegram".to_string();
    let user_msg = state
        .db
        .append_message(&thread_id, "user", &sender, content, &attachments)
        .await?;
    // Fan out to whichever KinAI surface(s) belong to this peer so the
    // chat shows up live instead of only on the next thread reload.
    fan_out_message(state, app, peer_id, &user_msg).await;

    // Resolve the routing slot. Same `route_for` the in-app chat paths
    // use, so `/fast`/`/deep` prefix handling AND the per-thread sticky
    // memory both work for Telegram-originated turns. Without this, the
    // Telegram path was always pinned to `cfg.llm` and the user-typed
    // routing prefix leaked into the model's prompt.
    let route_pick =
        crate::slash::route_for(&state.db, &cfg, peer_id, &thread_id, content).await;
    let llm_route_content = route_pick.stripped_content.clone();

    // Slash commands intercept BEFORE the LLM — using the stripped
    // content so e.g. "/deep /pic …" routes through the deep slot and
    // also triggers /pic.
    //
    // Telegram-specific bypass for /help: the regular slash::handle
    // returns Markdown-flavored text, which Telegram's default
    // `sendMessage` (no parse_mode) renders as LITERAL `**asterisks**`
    // and backticks — bad UX, makes the bot look amateur. We grab the
    // HTML variant and send via `send_message_html` so section headers
    // and command names render properly. The Markdown version still
    // gets persisted to the DB so the desktop UI sees the same
    // transcript the user sent on Telegram.
    let trimmed = llm_route_content.trim();
    if trimmed.eq_ignore_ascii_case("/help") || trimmed == "?" {
        let html = crate::slash::help_html(&cfg);
        let md = crate::slash::help_markdown(&cfg);
        // Persist the markdown form so the desktop chat / fan-out shows
        // it nicely too. send_message_html below pushes the HTML form
        // to Telegram only.
        let persisted = state
            .db
            .append_message(&thread_id, "assistant", "KinAI", &md, &[])
            .await
            .ok();
        if let Some(msg) = persisted {
            fan_out_message(state, app, peer_id, &msg).await;
        }
        api.send_message_html(chat_id, &html).await?;
        return Ok(());
    }

    // Bare `/fast` / `/deep` → mode switch confirmation (instant, no LLM).
    // Otherwise run the native slash handler. `/pic` / `/picHQ` route
    // through ComfyUI and take 5-30s, during which the slash path returns
    // BEFORE the regular chat-path's typing keep-alive is set up — so the
    // Telegram user saw no activity indicator at all. Telegram's chat
    // actions only offer fixed client-rendered labels ("sending photo…"),
    // which reads wrong while the image is still being GENERATED — so we
    // post a real "creating a picture…" placeholder message instead and
    // delete it when the result is ready. The 800ms initial delay means
    // instant handlers (the /pic usage hint, the "not configured"
    // message) never flash a placeholder.
    let slash_reply = if route_pick.bare_switch {
        Some(crate::slash::switch_confirmation(&route_pick))
    } else {
        // Only /pic and /picHQ are slow enough to warrant a placeholder;
        // every other slash handler answers instantly.
        let is_pic = llm_route_content
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("/pic");
        let action_cancel = tokio_util::sync::CancellationToken::new();
        let placeholder_task = is_pic.then(|| {
            let api_clone = api.clone();
            let cancel_clone = action_cancel.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = cancel_clone.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(800)) => {}
                }
                let placeholder_id = api_clone
                    .send_message(chat_id, "🎨 Creating a picture… this can take a minute.")
                    .await
                    .ok();
                // Hold until the handler finishes, then clean up our own
                // placeholder — owning deletion here avoids any race over
                // the message id with the main task.
                cancel_clone.cancelled().await;
                if let Some(id) = placeholder_id {
                    if let Err(e) = api_clone.delete_message(chat_id, id).await {
                        tracing::debug!("telegram: placeholder delete failed: {e:?}");
                    }
                }
            })
        });
        let reply = crate::slash::handle(&cfg, &llm_route_content).await;
        action_cancel.cancel();
        // Wait for the placeholder cleanup (one bounded HTTP call) so the
        // "creating…" message is gone before the photo/reply lands.
        if let Some(task) = placeholder_task {
            let _ = task.await;
        }
        reply
    };
    if let Some(reply) = slash_reply {
        send_assistant_reply(
            api,
            state,
            app,
            peer_id,
            &thread_id,
            chat_id,
            &reply,
            None, // slash replies don't have LLM metrics
            None, // slash replies aren't streamed
        )
        .await?;
        return Ok(());
    }

    // Regular chat path — build the same prompt KinAI builds for any
    // turn (system prompt + memory recalls + recent turns + this one),
    // run the LLM pipeline, capture the final assistant content, send
    // it back to Telegram.
    //
    // Streaming tokens are dropped on the floor (Telegram doesn't have
    // a streaming wire shape we'd use here — paid users CAN edit
    // messages to simulate it, but the UX cost (tokens per second
    // shimmer vs. one clean reply) isn't worth it). We just wait for
    // the full reply.
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use crate::tools::loop_pipeline::PipelineHandlers;
    use crate::tools::registry;

    // Spawn a keep-alive task that re-sends `sendChatAction(typing)`
    // every 4s so Telegram keeps showing "KinAI is typing…" until the
    // LLM finishes. Without this the indicator drops after ~5s and the
    // phone user thinks the bot froze. Cancelled when the LLM run
    // completes (or errors).
    //
    // The 1.5-second initial delay matters more than it looks: fast-
    // model turns often finish in well under 2s. If we fire
    // sendChatAction immediately on those, the HTTP call can reach
    // Telegram AFTER our sendMessage (network race), which Telegram
    // interprets as "bot started typing AGAIN" and leaves the indicator
    // hanging for ~5 seconds after the reply already arrived. By
    // delaying the first fire past the typical fast-response window,
    // we either avoid the indicator entirely (no race possible) or
    // fire it well before the reply lands.
    let typing_cancel = CancellationToken::new();
    {
        let api_clone = api.clone();
        let cancel_clone = typing_cancel.clone();
        tokio::spawn(async move {
            // Initial delay: skip the indicator for sub-1.5s turns.
            tokio::select! {
                _ = cancel_clone.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_millis(1500)) => {}
            }
            // For longer turns, re-fire every 4s. Telegram's typing
            // indicator auto-times-out at ~5s without a re-fire.
            loop {
                let _ = api_clone.send_chat_action(chat_id, "typing").await;
                tokio::select! {
                    _ = cancel_clone.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(4)) => {}
                }
            }
        });
    }

    // Build context with the slash-stripped content so the model
    // doesn't see "/deep" in its prompt.
    let user_msg_for_llm = crate::db::Message {
        content: llm_route_content.clone(),
        ..user_msg.clone()
    };
    let messages = crate::context::builder::build_context(
        &state.db,
        &cfg,
        peer_id,
        &thread_id,
        &user_msg_for_llm,
    )
    .await?;

    // Diagnostic: log how many turns made it into the LLM prompt so the
    // "I'm not sure what 'they' refers to" context-loss bug is
    // debuggable without me trial-running each scenario by hand.
    tracing::info!(
        "telegram turn: peer={} thread={} slot={} model={} ctx_messages={} new_msg_chars={}",
        peer_id,
        thread_id,
        route_pick.slot_label,
        route_pick.settings.model,
        messages.len(),
        llm_route_content.len(),
    );

    // Snapshot the prompt for the per-message 🔍 panel in KinAI's UI —
    // same shape as the in-app chat path so Telegram-originated turns
    // get the same "see exactly what the model saw" debug surface.
    let prompt_debug = serde_json::to_string_pretty(
        &messages
            .iter()
            .map(|m| m.redacted_for_debug())
            .collect::<Vec<_>>(),
    )
    .ok();

    let tools = registry::enabled(&cfg.tools);
    let tool_runtime = registry::ToolRuntime::from_tool_settings(&cfg.tools)
        .with_memory(state.db.clone(), peer_id)
        .with_source_msg(user_msg.id.clone());
    let active_llm_settings = route_pick.settings.clone();
    let max_tokens = compute_max_tokens(&active_llm_settings, &messages);
    // Build the LLM client from the routed slot's settings, not the
    // cached state.llm which is always the fast slot.
    let llm = crate::llm::LlmClient::new(active_llm_settings.clone());
    let cancel = CancellationToken::new();

    // Live-streaming reply: accumulate visible tokens into `buf` and let a
    // throttled editor task edit one Telegram message in place as they
    // arrive (the ChatGPT-style growing bubble). on_token runs on the SSE
    // hot path, so it stays non-blocking — it only locks a std Mutex for a
    // push and pings a watch channel; all rate-limiting lives in the editor.
    // Reasoning tokens are NOT streamed (they'd be confusing and burn the
    // edit budget). The final, authoritative text is still written once by
    // send_assistant_reply (which also persists + fans out).
    let buf = Arc::new(std::sync::Mutex::new(String::new()));
    let (buf_tx, buf_rx) = tokio::sync::watch::channel::<()>(());
    let handlers = {
        let buf = buf.clone();
        let buf_tx = buf_tx.clone();
        PipelineHandlers {
            on_token: Arc::new(move |t: String| {
                if let Ok(mut g) = buf.lock() {
                    g.push_str(&t);
                }
                let _ = buf_tx.send(());
            }),
            on_reasoning: Arc::new(|_| {}),
            on_tool: Arc::new(|_| {}),
        }
    };

    // Editor task: lazily creates one message on the first ≥12 chars of
    // content, then edits it at most once per 1.2s (≈ Telegram's safe edit
    // rate). Returns the placeholder message_id (if any) so the final edit
    // happens in send_assistant_reply. Cancels the typing keep-alive once a
    // real bubble exists. A reply too short to reach 12 chars never creates
    // a bubble → None → normal single send, identical to before.
    let edit_cancel = CancellationToken::new();
    let editor = {
        let api = api.clone();
        let buf = buf.clone();
        let mut buf_rx = buf_rx;
        let cancel = edit_cancel.clone();
        let typing_cancel = typing_cancel.clone();
        tokio::spawn(async move {
            const EDIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1200);
            const MIN_FIRST_CHARS: usize = 12;
            let mut placeholder_id: Option<i64> = None;
            let mut last_edit = std::time::Instant::now()
                .checked_sub(EDIT_INTERVAL)
                .unwrap_or_else(std::time::Instant::now);
            let mut last_sent = String::new();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    changed = buf_rx.changed() => {
                        if changed.is_err() { break; }
                    }
                }
                let snapshot = buf.lock().map(|g| g.clone()).unwrap_or_default();
                if placeholder_id.is_none() {
                    if snapshot.chars().count() < MIN_FIRST_CHARS {
                        continue;
                    }
                    match api.send_message(chat_id, &truncate_for_tg(&snapshot)).await {
                        Ok(id) => {
                            placeholder_id = Some(id);
                            last_sent = snapshot.clone();
                            last_edit = std::time::Instant::now();
                            typing_cancel.cancel(); // live bubble replaces the dots
                        }
                        Err(e) => tracing::warn!("tg stream: first send failed: {e:?}"),
                    }
                    continue;
                }
                if snapshot == last_sent {
                    continue;
                }
                let since = last_edit.elapsed();
                if since < EDIT_INTERVAL {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(EDIT_INTERVAL - since) => {}
                    }
                }
                // Re-read after the throttle sleep so we edit with the freshest text.
                let snapshot = buf.lock().map(|g| g.clone()).unwrap_or_default();
                if snapshot == last_sent {
                    continue;
                }
                let id = placeholder_id.unwrap();
                match api.edit_message_text(chat_id, id, &truncate_for_tg(&snapshot)).await {
                    Ok(()) => {
                        last_sent = snapshot;
                        last_edit = std::time::Instant::now();
                    }
                    Err(e) => {
                        if e.to_string().contains("Too Many Requests") {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        } else {
                            tracing::warn!("tg stream edit: {e:?}");
                        }
                    }
                }
            }
            placeholder_id
        })
    };

    let route = crate::vision::decide(&active_llm_settings.model, &attachments, &cfg.vision)?;
    let started = std::time::Instant::now();
    let result = crate::vision::run_with_route(
        route,
        llm,
        &active_llm_settings,
        messages,
        tools,
        tool_runtime,
        max_tokens,
        handlers,
        cancel,
    )
    .await;
    // Stop both keep-alives, then join the editor to recover the placeholder
    // id for the final authoritative edit.
    typing_cancel.cancel();
    edit_cancel.cancel();
    let stream_msg_id = editor.await.ok().flatten();
    let result = result?;
    let total_ms = started.elapsed().as_millis() as u64;
    let output_tokens =
        crate::context::token_guard::count_tokens(&result.final_content) as u64;
    // tts metric in Telegram is end-to-end (no per-token first-token
    // hook here since we discard the stream); set ttft = total_ms and
    // tps as a coarse average to keep the schema consistent.
    let tps = if total_ms < 200 || output_tokens == 0 {
        0.0
    } else {
        (output_tokens as f64) * 1000.0 / (total_ms as f64)
    };
    let metrics = crate::network::protocol::TurnMetricsWire {
        first_token_ms: total_ms,
        total_ms,
        output_tokens,
        tps,
        model: active_llm_settings.model.clone(),
        slot: route_pick.slot_label.to_string(),
    };

    send_assistant_reply(
        api,
        state,
        app,
        peer_id,
        &thread_id,
        chat_id,
        &result.final_content,
        Some((metrics, prompt_debug)),
        stream_msg_id,
    )
    .await?;

    if let Err(e) =
        crate::context::memory::maybe_summarize(&state.db, peer_id, &thread_id).await
    {
        tracing::warn!("telegram summarizer: {e:?}");
    }
    Ok(())
}

/// Per-slot budget calculator. Takes the routed slot's settings
/// directly (rather than always `cfg.llm`) so /deep turns honour the
/// deep slot's context_window + max_tokens instead of inheriting the
/// fast slot's caps.
fn compute_max_tokens(
    llm: &crate::config::LlmSettings,
    messages: &[crate::context::ChatMessage],
) -> Option<usize> {
    if llm.max_tokens == 0 {
        return None;
    }
    let used = crate::context::token_guard::estimate_messages(messages);
    let remaining = llm.context_window.saturating_sub(used);
    Some(llm.max_tokens.min(remaining))
}

/// Send the assistant reply back to Telegram. For slash-command
/// replies that contain a `![alt](url)` image reference, we ALSO
/// upload the underlying file as a Telegram photo so users see the
/// image inline in their chat rather than just a clickable link.
///
/// `metrics_with_debug`: when the reply came from a full LLM turn,
/// passes (metrics, optional prompt-debug JSON) so the persisted
/// assistant message gets the same metadata in-app chats have —
/// without it the KinAI UI rendered Telegram replies with just
/// "KinAI · time" and no model badge / latency / 🔍 prompt button.
/// Slash-command replies (no LLM involved) pass None.
async fn send_assistant_reply<R: Runtime>(
    api: &BotApi,
    state: &SharedState,
    app: &AppHandle<R>,
    peer_id: &str,
    thread_id: &str,
    chat_id: i64,
    reply: &str,
    metrics_with_debug: Option<(crate::network::protocol::TurnMetricsWire, Option<String>)>,
    // When the reply was live-streamed (the regular LLM path), this is the
    // message_id of the placeholder bubble the editor task created. We
    // finalize it with one authoritative edit instead of sending a new
    // message. `None` for slash replies and for streamed turns too short to
    // have created a bubble — those fall through to a normal send.
    stream_msg_id: Option<i64>,
) -> Result<()> {
    // Persist the assistant message so it shows up in KinAI clients
    // viewing this peer's Telegram thread.
    let persisted = state
        .db
        .append_message(thread_id, "assistant", "KinAI", reply, &[])
        .await
        .ok();
    if let Some(mut msg) = persisted.clone() {
        // Attach metrics + emit prompt_debug so the KinAI UI surfaces
        // the same model badge / latency / 🔍 panel it does for in-app
        // turns. The DB row is updated separately; the fan-out copy
        // carries the metrics inline so listeners don't have to re-read
        // the row before rendering.
        if let Some((metrics, prompt)) = metrics_with_debug.as_ref() {
            let metrics_json =
                serde_json::to_value(metrics).unwrap_or(serde_json::Value::Null);
            let _ = state
                .db
                .set_message_metrics(&msg.id, &metrics_json)
                .await;
            msg.metrics = Some(metrics_json);
            if let Some(p) = prompt {
                let _ = app.emit(
                    "kinai://prompt-debug",
                    serde_json::json!({
                        "assistant_msg_id": msg.id,
                        "prompt": p,
                    }),
                );
            }
        }
        fan_out_message(state, app, peer_id, &msg).await;
    }

    // --- Text delivery: three mutually exclusive paths. Kept as an
    // if/else chain (no early returns) so the voice-note hook below
    // runs after the text has landed, whichever path delivered it.
    let mut sent_photo = false;
    if let Some(id) = stream_msg_id {
        // Streaming path: finalize the live bubble with the complete
        // text. (A streamed turn is always the LLM path, never /pic.)
        if reply.chars().count() <= 4096 {
            if let Err(e) = api.edit_message_text(chat_id, id, reply).await {
                // One retry on a transient rate-limit so we don't lose the
                // final authoritative text.
                if e.to_string().contains("Too Many Requests") {
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    api.edit_message_text(chat_id, id, reply).await?;
                } else {
                    return Err(e);
                }
            }
        } else {
            // Too long for one bubble: edit the placeholder to the first
            // chunk, send the remaining chunks as new messages.
            let parts = super::api::split_for_telegram(reply);
            if let Some(first) = parts.first() {
                let _ = api.edit_message_text(chat_id, id, first).await;
            }
            for part in parts.iter().skip(1) {
                api.send_message(chat_id, part).await?;
            }
        }
    } else if let Some(local_path) = extract_local_pic_path(state, reply) {
        // `![alt](http://host/v1/pic/<uuid>.png)` from ComfyUI output:
        // upload the local file as a photo with the rest as caption.
        let caption = strip_inline_image_markdown(reply);
        let caption_opt = if caption.trim().is_empty() { None } else { Some(caption.as_str()) };
        // Plain-text caption (no parse_mode) — the user typed this on
        // Telegram, so we don't need the blockquote-formatted Q&A echo.
        api.send_photo_file(chat_id, &local_path, caption_opt, None).await?;
        sent_photo = true;
    } else {
        api.send_message(chat_id, reply).await?;
    }

    // --- Voice note (opt-in per chat via /voice, host master switch in
    // Settings). Spawned so synthesis/upload never delays the turn;
    // failures only log. Photo replies are skipped — reading an image
    // caption aloud is noise.
    if !sent_photo {
        maybe_send_voice_note(api, state, peer_id, chat_id, reply).await;
    }
    Ok(())
}

/// If this peer opted in (/voice) and the host has TTS enabled,
/// synthesize `reply` and send it as a Telegram voice note in a
/// background task. Best-effort: any failure is logged and the chat
/// flow is unaffected.
async fn maybe_send_voice_note(
    api: &BotApi,
    state: &SharedState,
    peer_id: &str,
    chat_id: i64,
    reply: &str,
) {
    let tts_cfg = state.config.read().tts.clone();
    if !tts_cfg.enabled {
        return;
    }
    let opted_in = tg_db::voice_replies(&state.db.pool, peer_id)
        .await
        .unwrap_or(false);
    if !opted_in {
        return;
    }
    let text = crate::tts::speakable_text(reply);
    if text.is_empty() {
        return; // nothing speakable (e.g. pure code answer)
    }
    let voice = crate::tts::voice_for_text(&tts_cfg, &text);
    let api = api.clone();
    tokio::spawn(async move {
        match crate::tts::synthesize_m4a(&text, &voice).await {
            Ok(path) => {
                if let Err(e) = api.send_voice_file(chat_id, &path).await {
                    tracing::warn!("telegram: voice note upload failed: {e:?}");
                }
                let _ = tokio::fs::remove_file(&path).await;
            }
            Err(e) => tracing::warn!("telegram: voice synthesis failed: {e:?}"),
        }
    });
}

/// Surface a persisted Telegram-originated message to whichever KinAI
/// surface(s) belong to `peer_id`:
///   * Host's own thread (`peer_id == HOST_PEER`) → Tauri `kinai://message`
///     event, picked up by the chat UI store's `pushMessage` listener.
///   * Client peer's thread → walk `net.peers` for sessions whose
///     `invite_id` matches `peer_id` (recall: `invite_id` IS the stable
///     peer_id used by storage; the HashMap key is a per-WS UUID) and
///     send `Envelope::Message` over their writer channel.
///
/// Best-effort: failures (no listeners, dropped tx) are logged but the
/// chat flow already committed the message to the DB so the user sees
/// it on their next reload either way.
async fn fan_out_message<R: Runtime>(
    state: &SharedState,
    app: &AppHandle<R>,
    peer_id: &str,
    message: &crate::db::Message,
) {
    if peer_id == crate::db::HOST_PEER {
        if let Err(e) = app.emit("kinai://message", message) {
            tracing::warn!("telegram fan-out: app.emit failed: {e:?}");
        }
        return;
    }

    // Client peer path — find any connected WS sessions for this peer
    // and push them the envelope. There can be more than one (rare,
    // e.g. mid-reconnect) — send to all and let the client dedupe by
    // message id (`pushMessage` already does this).
    let net = state.net.lock().await;
    let mut sent = 0usize;
    for info in net.peers.values() {
        if info.invite_id == peer_id {
            let _ = info.tx.send(Envelope::Message {
                message: message.clone(),
            });
            sent += 1;
        }
    }
    if sent == 0 {
        // Not an error — peer might just be offline. Their next reload
        // will read from the DB.
        tracing::debug!(
            "telegram fan-out: peer {peer_id} has no live WS sessions, message stored in DB only"
        );
    }
}

/// If `reply` contains `![…](http://<our-host>/v1/pic/<uuid>.<ext>)`,
/// return the absolute filesystem path to that image. None if the
/// reply has no such reference.
pub(super) fn extract_local_pic_path(state: &SharedState, reply: &str) -> Option<std::path::PathBuf> {
    static RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"!\[[^\]]*\]\(http[s]?://[^/]+/v1/pic/([A-Za-z0-9_\-]+\.[a-z]+)\)").unwrap()
    });
    let caps = RE.captures(reply)?;
    let filename = caps.get(1)?.as_str();
    let dir = crate::comfyui::pics_dir();
    let path = dir.join(filename);
    if path.exists() {
        let _ = state; // unused; future: lookup peer-specific dir
        Some(path)
    } else {
        None
    }
}

pub(super) fn strip_inline_image_markdown(reply: &str) -> String {
    static RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"!\[[^\]]*\]\(http[s]?://[^)]+\)\s*\n*").unwrap()
    });
    RE.replace_all(reply, "").trim().to_string()
}

/// Cap an in-progress streamed snapshot at ~4000 chars for INTERMEDIATE
/// edits (a single Telegram message can't exceed 4096). Walks back to a
/// UTF-8 char boundary before slicing so a multi-byte char at the cut
/// can't panic. The final, complete reply is handled separately (split
/// into multiple messages if needed) in send_assistant_reply.
fn truncate_for_tg(s: &str) -> String {
    const CAP: usize = 4000;
    if s.len() <= CAP {
        return s.to_string();
    }
    let mut end = CAP;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Pick the highest-resolution entry Telegram offered for an inbound
/// photo. Telegram sends the SAME image at several escalating sizes in
/// one `photo` array; the vision model deserves the sharpest one, so we
/// take the largest by pixel area rather than blindly grabbing `[0]`
/// (which is the thumbnail).
fn largest_photo(sizes: &[PhotoSize]) -> Option<&PhotoSize> {
    sizes
        .iter()
        .max_by_key(|p| (p.width as u64) * (p.height as u64))
}

/// Download the highest-resolution photo from an inbound Telegram
/// message and wrap it as a KinAI image `Attachment` (base64 data URL),
/// so the vision pipeline can analyze it exactly like an in-app paste.
///
/// Returns `Ok(None)` when the message carries no photo (the common
/// text-only case). Telegram photos are always JPEG, so we hardcode that
/// mime — the two-step Bot API download is getFile (file_id → file_path)
/// then download_file (file_path → bytes).
async fn photo_attachment(api: &BotApi, msg: &TelegramMessage) -> Result<Option<Attachment>> {
    let Some(largest) = largest_photo(&msg.photo) else {
        return Ok(None);
    };
    let file = api.get_file(&largest.file_id).await?;
    let file_path = file
        .file_path
        .ok_or_else(|| anyhow::anyhow!("telegram getFile returned no file_path"))?;
    let bytes = api.download_file(&file_path).await?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let data_url = format!("data:image/jpeg;base64,{b64}");
    Ok(Some(Attachment {
        kind: "image".into(),
        mime: Some("image/jpeg".into()),
        name: Some("telegram-photo.jpg".into()),
        data_url: Some(data_url),
    }))
}

#[cfg(test)]
mod photo_tests {
    use super::*;

    fn size(w: u32, h: u32, id: &str) -> PhotoSize {
        PhotoSize {
            file_id: id.into(),
            width: w,
            height: h,
            file_size: None,
        }
    }

    #[test]
    fn largest_photo_picks_highest_resolution() {
        // Telegram lists thumbnails first; the sharpest is usually last,
        // but we must not assume ordering — pick by pixel area.
        let sizes = vec![
            size(90, 90, "thumb"),
            size(1280, 720, "full"),
            size(320, 240, "mid"),
        ];
        let pick = largest_photo(&sizes).expect("non-empty");
        assert_eq!(pick.file_id, "full", "must select the largest by area");
    }

    #[test]
    fn largest_photo_none_when_empty() {
        assert!(largest_photo(&[]).is_none());
    }
}

/// If `content` is the `/newchat` command, return the remainder (the
/// optional question typed after it, trimmed). `None` if it isn't
/// `/newchat`. Case-insensitive; requires a whitespace/end boundary so
/// `/newchattery` isn't mistaken for the command.
fn strip_newchat(content: &str) -> Option<&str> {
    const CMD: &str = "/newchat";
    let t = content.trim_start();
    if t.len() < CMD.len() {
        return None;
    }
    let (head, rest) = t.split_at(CMD.len());
    if !head.eq_ignore_ascii_case(CMD) {
        return None;
    }
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim_start())
    } else {
        None
    }
}

/// If `content` is the `/voice` command, return the remainder (e.g.
/// "on", "off", or "" for the bare toggle), trimmed. Same boundary
/// rules as `strip_newchat` so `/voicemail` isn't mistaken for it.
fn strip_voice(content: &str) -> Option<&str> {
    const CMD: &str = "/voice";
    let t = content.trim_start();
    if t.len() < CMD.len() {
        return None;
    }
    let (head, rest) = t.split_at(CMD.len());
    if !head.eq_ignore_ascii_case(CMD) {
        return None;
    }
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

#[cfg(test)]
mod voice_cmd_tests {
    use super::strip_voice;

    #[test]
    fn matches_bare_on_off_and_boundaries() {
        assert_eq!(strip_voice("/voice"), Some(""));
        assert_eq!(strip_voice("/VOICE on"), Some("on"));
        assert_eq!(strip_voice("  /voice off  "), Some("off"));
        assert_eq!(strip_voice("/voicemail"), None);
        assert_eq!(strip_voice("voice"), None);
        assert_eq!(strip_voice("tell me about /voice"), None);
    }
}

#[cfg(test)]
mod newchat_tests {
    use super::strip_newchat;

    #[test]
    fn matches_bare_and_with_prompt() {
        assert_eq!(strip_newchat("/newchat"), Some(""));
        assert_eq!(strip_newchat("/newchat   "), Some(""));
        assert_eq!(strip_newchat("/newchat what is rust?"), Some("what is rust?"));
        assert_eq!(strip_newchat("  /NewChat hi"), Some("hi"));
    }

    #[test]
    fn ignores_non_command() {
        assert_eq!(strip_newchat("/newchattery"), None);
        assert_eq!(strip_newchat("tell me about /newchat"), None);
        assert_eq!(strip_newchat("/new"), None);
    }
}

/// Deterministic thread id for `<peer>`'s Telegram conversation. UUID
/// shape so the host's threads table accepts it; first segment encodes
/// the peer so it's easy to debug (`grep telegram-` against the DB).
pub fn telegram_thread_id_for_peer(peer_id: &str) -> String {
    // We can't put `peer_id` directly into a UUID slot, so fold it
    // into a stable hash and format as a UUID-shaped string.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    "telegram".hash(&mut h);
    peer_id.hash(&mut h);
    let lo = h.finish();
    "telegram".hash(&mut h);
    peer_id.hash(&mut h);
    let hi = h.finish();
    format!(
        "telegram-{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (hi >> 32) as u32,
        ((hi >> 16) & 0xffff) as u16,
        (hi & 0xffff) as u16,
        ((lo >> 48) & 0xffff) as u16,
        lo & 0xffff_ffff_ffff
    )
}
