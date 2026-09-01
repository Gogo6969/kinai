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

    // Voice message + voice input enabled → transcribe locally and run
    // it as a normal chat turn. The transcript (🎙-prefixed) flows
    // through the standard pipeline, so it lands in the KinAI thread,
    // slash commands spoken aloud work, and the reply comes back as
    // text (+ voice note if /voice is on).
    let mut text_or_caption = text_or_caption;
    if text_or_caption.trim().is_empty() && msg.voice.is_some() {
        let stt_cfg = state.config.read().stt.clone();
        if crate::stt::is_ready(&stt_cfg) {
            // Show activity immediately — transcription takes a moment.
            let _ = api.send_chat_action(chat_id, "typing").await;
            match transcribe_voice_message(api, &stt_cfg, msg).await {
                Ok(t) if !t.trim().is_empty() => {
                    let transcript = t.trim().to_string();
                    // Echo what was understood — builds trust in the
                    // transcription and explains an off answer.
                    let _ = api.send_message(chat_id, &format!("🎙 «{transcript}»")).await;
                    // Feed the model the PLAIN transcript — NOT prefixed with
                    // the 🎙 emoji. A small chat model (e.g. gpt-oss-20b) reads
                    // the mic glyph as "this is audio I can't process" and
                    // refuses ("I can't interpret voice messages") even though
                    // it has the text. The Telegram echo above already shows
                    // the message came from voice.
                    text_or_caption = transcript;
                }
                Ok(_) => {
                    api.send_message(
                        chat_id,
                        "🎙 I couldn't hear any words in that voice message — please try again.",
                    )
                    .await?;
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!("telegram stt failed: {e:?}");
                    api.send_message(
                        chat_id,
                        "🎙 I couldn't transcribe that voice message — please try again or type it.",
                    )
                    .await?;
                    return Ok(());
                }
            }
        }
    }

    // No readable content? Say so instead of silently dropping the
    // update — a voice message especially feels broken when the bot
    // (which SPEAKS via /voice!) doesn't even react to it. The exchange
    // is mirrored into the peer's KinAI thread like any other turn, so
    // the app shows the same conversation Telegram does.
    if text_or_caption.trim().is_empty() && msg.photo.is_empty() {
        let (marker, reply) = if msg.voice.is_some() || msg.audio.is_some() || msg.video_note.is_some() {
            (
                "🎙 (voice message)",
                "🎙 I can't listen to voice messages yet — please type your question. \
                 (The host can enable voice input in KinAI → Settings → Voice input, \
                 then download a voice model. I can already talk back: send /voice and \
                 my answers arrive as voice notes.)",
            )
        } else {
            (
                "(unsupported content)",
                "I can only read text and photos for now.",
            )
        };
        let thread_id = tg_db::active_thread(&state.db.pool, &peer_id)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| telegram_thread_id_for_peer(&peer_id));
        state.db.upsert_thread(&peer_id, &thread_id, "Telegram").await.ok();
        if let Ok(m) = state
            .db
            .append_message(&thread_id, "user", "Telegram", marker, &[])
            .await
        {
            fan_out_message(state, app, &peer_id, &m).await;
        }
        if let Ok(m) = state
            .db
            .append_message(&thread_id, "assistant", "KinAI", reply, &[])
            .await
        {
            fan_out_message(state, app, &peer_id, &m).await;
        }
        api.send_message(chat_id, &truncate_for_tg(reply)).await?;
        return Ok(());
    }

    // Routed — run the chat turn.
    if let Err(e) = run_turn_for_peer(api, state, app, chat_id, &peer_id, &text_or_caption, msg)
        .await
    {
        tracing::warn!("telegram run_turn: {e:?}");
        let _ = api
            .send_message(chat_id, &humanize_turn_error(&e.to_string()))
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

    // /voice — per-chat opt-in for spoken replies. Shared logic with the
    // in-app chat paths (see voice_command_reply); the exchange is
    // mirrored into the thread so the KinAI app shows it too.
    if let Some(arg) = strip_voice(content) {
        let outcome = voice_command_outcome(state, peer_id, arg).await;
        if let Ok(m) = state
            .db
            .append_message(&thread_id, "user", "Telegram", content, &[])
            .await
        {
            fan_out_message(state, app, peer_id, &m).await;
        }
        if let Ok(m) = state
            .db
            .append_message(&thread_id, "assistant", "KinAI", &outcome.reply, &[])
            .await
        {
            fan_out_message(state, app, peer_id, &m).await;
        }
        api.send_message(chat_id, &outcome.reply).await?;
        // Turning voice ON: speak the confirmation itself — instant
        // audible proof the pipeline works. (The pref is already ON, so
        // maybe_send_voice_note passes its own opt-in check.)
        if outcome.enabled_now == Some(true) {
            maybe_send_voice_note(api, state, peer_id, chat_id, &outcome.reply).await;
        }
        return Ok(());
    }

    let cfg = state.config.read().clone();

    // Bare `/fast`, `/balanced` or `/deep` (no body) = mode switch, not a
    // question. Persist the choice on the thread, confirm, return.
    let trimmed_lc = content.trim().to_ascii_lowercase();
    if let Some(slot) = crate::slash::SLOTS
        .iter()
        .find(|s| trimmed_lc == format!("/{s}"))
        .copied()
    {
        // thread_id is already resolved + upserted above — write the
        // sticky slot to the active (possibly /newchat-rotated) thread.
        let _ = state.db.set_thread_active_slot(peer_id, &thread_id, Some(slot)).await;
        let active_model = &crate::slash::slot_settings(&cfg, slot).model;
        let icon = match slot {
            "deep" => "🧠",
            "balanced" => "⚖️",
            "online" => "☁️",
            _ => "⚡",
        };
        let others: Vec<String> = crate::slash::SLOTS
            .iter()
            .filter(|s| **s != slot && crate::slash::slot_settings(&cfg, s).is_active())
            .map(|s| format!("`/{s}`"))
            .collect();
        let mut body = if active_model.trim().is_empty() {
            format!("{icon} Switched to **{slot}** model — but no model is configured for this slot. Open KinAI → Settings to add one.")
        } else if others.is_empty() {
            format!("{icon} Switched to **{slot}** model (`{active_model}`).")
        } else {
            format!(
                "{icon} Switched to **{slot}** model (`{active_model}`).\nAll follow-up questions in this chat go here until you type {} to switch.",
                others.join(" or ")
            )
        };
        // Same heads-up as the app's switch_confirmation: warn NOW when
        // the wanted slot's server is down, not after the next message.
        //
        // The automatic-failover promise must be gated on FAILOVER_SLOTS,
        // not `others`: `others` lists every slot the user can SWITCH to
        // (including `online`), but failover only ever substitutes from
        // FAILOVER_SLOTS. With fast+online configured and fast down, the
        // old `others`-based branch promised a failover that the next
        // message could not deliver.
        if !active_model.trim().is_empty() && !crate::slash::slot_alive_cached(state, slot).await {
            if crate::slash::failover_available(&cfg, slot) {
                body.push_str(
                    "\n\n⚠️ Heads-up: this model's server isn't responding right now. \
Your messages will automatically use another available model until it's back.",
                );
            } else {
                body.push_str(
                    "\n\n⚠️ Heads-up: this model's server isn't responding right now, \
and no other model can take over automatically — messages will fail until it's \
back, unless you switch model yourself.",
                );
            }
        }
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
        let alive = crate::slash::slot_alive_cached(state, route_pick.slot_label).await;
        Some(crate::slash::switch_confirmation(&cfg, &route_pick, Some(alive)))
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
        route_pick.settings,
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
    // Route from the routed slot's settings, not the cached state.llm
    // which is always the fast slot. The LLM client itself is built per
    // attempt inside run_turn_with_slot_failover.
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
    // Latest human status line ("Looking into it…") from the tool loop.
    // A research turn can spend 30s before its first token; without this
    // the phone shows only the typing dots and reads as a hung app.
    let status = Arc::new(std::sync::Mutex::new(String::new()));
    let (buf_tx, buf_rx) = tokio::sync::watch::channel::<()>(());
    let handlers = {
        let buf = buf.clone();
        let buf_tx = buf_tx.clone();
        // Cloned before the struct literal: `on_token` takes ownership of
        // `buf_tx`, so `on_tool` needs its own handle up front.
        let tool_tx = buf_tx.clone();
        PipelineHandlers {
            on_token: Arc::new(move |t: String| {
                if let Ok(mut g) = buf.lock() {
                    g.push_str(&t);
                }
                let _ = buf_tx.send(());
            }),
            on_reasoning: Arc::new(|_| {}),
            on_tool: {
                let status = status.clone();
                let tx = tool_tx;
                Arc::new(move |e: crate::tools::loop_pipeline::ToolEvent| {
                    if let crate::tools::loop_pipeline::ToolEvent::Started { note, .. } = e {
                        if let Ok(mut g) = status.lock() {
                            *g = note;
                        }
                        let _ = tx.send(());
                    }
                })
            },
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
        let status = status.clone();
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
            // True while the bubble holds a status line rather than answer
            // text; the first real content overwrites it.
            let mut showing_status = false;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    changed = buf_rx.changed() => {
                        if changed.is_err() { break; }
                    }
                }
                let snapshot = buf.lock().map(|g| g.clone()).unwrap_or_default();
                let status_line = status.lock().map(|g| g.clone()).unwrap_or_default();
                // Before any real content: put the tool's status line in
                // the bubble. The SAME bubble is then edited into the
                // answer, so the user never sees a stray status message
                // left behind next to the reply.
                if snapshot.chars().count() < MIN_FIRST_CHARS && !status_line.is_empty() {
                    if status_line == last_sent {
                        continue;
                    }
                    let now = std::time::Instant::now();
                    match placeholder_id {
                        None => match api.send_message(chat_id, &status_line).await {
                            Ok(id) => {
                                placeholder_id = Some(id);
                                last_sent = status_line;
                                last_edit = now;
                                showing_status = true;
                                // Typing dots stay on: the bubble is not
                                // streaming yet, and dots + status together
                                // read as "working", not "finished".
                            }
                            Err(e) => tracing::warn!("tg status: first send failed: {e:?}"),
                        },
                        Some(id) if showing_status => {
                            if now.duration_since(last_edit) < EDIT_INTERVAL {
                                continue;
                            }
                            match api.edit_message_text(chat_id, id, &status_line).await {
                                Ok(()) => {
                                    last_sent = status_line;
                                    last_edit = now;
                                }
                                Err(e) => tracing::debug!("tg status edit: {e:?}"),
                            }
                        }
                        Some(_) => {}
                    }
                    continue;
                }
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
                if showing_status {
                    // Real content has arrived; the bubble stops being a
                    // status line and the dots hand over to streaming.
                    showing_status = false;
                    typing_cancel.cancel();
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

    let route = crate::vision::decide(&active_llm_settings, &attachments, &cfg.vision).await?;
    let started = std::time::Instant::now();
    // Runtime copy for post-turn image recovery (run_with_route consumes it).
    let recover_runtime = tool_runtime.clone();
    let served = crate::slash::run_turn_with_slot_failover(
        route,
        state,
        &cfg,
        route_pick.slot_label,
        messages,
        tools,
        tool_runtime,
        handlers,
        cancel,
        |s, msgs| compute_max_tokens(s, msgs),
    )
    .await;
    // Stop both keep-alives, then join the editor to recover the placeholder
    // id for the final authoritative edit.
    typing_cancel.cancel();
    edit_cancel.cancel();
    let stream_msg_id = editor.await.ok().flatten();
    let served = match served {
        Ok(s) => s,
        Err(e) => {
            // A failover notice may already sit in the streamed placeholder
            // ("answering with X instead") — if every slot then died, that
            // bubble would forever promise an answer that never came. Edit
            // it into the honest error instead of leaving it + sending a
            // second message.
            if let Some(msg_id) = stream_msg_id {
                let _ = api
                    .edit_message_text(chat_id, msg_id, &humanize_turn_error(&e.to_string()))
                    .await;
                return Ok(());
            }
            return Err(e);
        }
    };
    let mut result = served.result;
    // Verify + recover any fabricated image URLs the model embedded before we
    // send/store the reply (same as the app path).
    result.final_content =
        crate::tools::image_recover::recover_reply_images(&result.final_content, &recover_runtime)
            .await;
    // Don't forward an empty completion verbatim: Telegram rejects an empty
    // message ("Bad Request: message text is empty") and the app shows a blank
    // bubble. A reasoning model (the deep slot) can spend its whole budget
    // "thinking" and emit no visible content, or a backend can return an empty
    // body — surface that as an actionable note instead of a cryptic error.
    if result.final_content.trim().is_empty() {
        result.final_content = crate::tools::loop_pipeline::EMPTY_REPLY_NOTE.to_string();
    }
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
        // The slot/model that ACTUALLY answered (differs from
        // route_pick after a failover).
        model: served.settings.model.clone(),
        slot: served.slot_label.clone(),
        question_msg_id: Some(user_msg.id.clone()),
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
// Shared with every other chat surface — crate::context::builder. The
// Telegram copy this replaces omitted max_tokens for ALL auto slots;
// the shared version keeps that for cloud endpoints and sends the
// explicit remaining window to local ones, matching the app.
use crate::context::builder::compute_max_tokens;

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
        // Streaming path: finalize the live bubble with the complete text.
        // Flatten markdown the SAME way the intermediate edits did (they go
        // through truncate_for_tg) — otherwise a table would snap from clean
        // bullet lists back to raw `|` pipes on THIS final, authoritative
        // edit. (A streamed turn is always the LLM path, never /pic.)
        let final_text = super::format::markdown_to_telegram(reply);
        if final_text.chars().count() <= 4096 {
            if let Err(e) = api.edit_message_text(chat_id, id, &final_text).await {
                // One retry on a transient rate-limit so we don't lose the
                // final authoritative text.
                if e.to_string().contains("Too Many Requests") {
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    api.edit_message_text(chat_id, id, &final_text).await?;
                } else {
                    return Err(e);
                }
            }
        } else {
            // Too long for one bubble: edit the placeholder to the first
            // chunk, send the remaining chunks as new messages.
            let parts = super::api::split_for_telegram(&final_text);
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
        api.send_message(chat_id, &truncate_for_tg(reply)).await?;
    }

    // If the reply embedded a REMOTE image (an `image_search` hit), deliver
    // it as a real photo. These reach here via the text paths above —
    // markdown_to_telegram strips `![](url)`, so "show me a picture of X"
    // would otherwise be text-only on Telegram even though the app shows the
    // image inline. The caption/source text is already on screen, so the
    // photo follows without a caption. Best-effort: failures leave the text.
    if !sent_photo && send_reply_remote_image(api, reply, chat_id).await {
        sent_photo = true;
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
        match crate::tts::synthesize_voice_note(&text, &voice).await {
            Ok(note) => {
                if let Err(e) = api
                    .send_voice_file(chat_id, &note.path, note.duration_secs)
                    .await
                {
                    tracing::warn!("telegram: voice note upload failed: {e:?}");
                }
                let _ = tokio::fs::remove_file(&note.path).await;
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

/// Remote image URLs the LLM embedded as `![alt](http(s)://…)`, EXCLUDING our
/// own `/v1/pic/` local paths (those are handled by extract_local_pic_path).
fn extract_remote_image_urls(reply: &str) -> Vec<String> {
    static RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"!\[[^\]]*\]\((https?://[^)\s]+)\)").unwrap()
    });
    RE.captures_iter(reply)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .filter(|u| !u.contains("/v1/pic/"))
        .collect()
}

/// Download the first reply-embedded remote image that is genuinely an image
/// and send it as a Telegram photo. Tries a few candidates — the LLM
/// sometimes lists page URLs alongside direct image URLs. Returns true once
/// one photo lands. Best-effort: any failure just moves to the next URL, and
/// an empty/all-failed list leaves the already-sent text untouched.
async fn send_reply_remote_image(api: &BotApi, reply: &str, chat_id: i64) -> bool {
    let urls = extract_remote_image_urls(reply);
    if urls.is_empty() {
        return false;
    }
    // Browser-ish UA: many image CDNs (Cloudflare et al.) 403 a blank/default
    // agent — the same class of block we hit while diagnosing Groq.
    let client = match reqwest::Client::builder()
        .user_agent(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
             AppleWebKit/537.36 (KHTML, like Gecko) Safari/537.36",
        )
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    const MAX_TRIES: usize = 4;
    const MAX_BYTES: usize = 10 * 1024 * 1024; // Telegram photo-upload cap
    for url in urls.into_iter().take(MAX_TRIES) {
        let resp = match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => continue,
        };
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !mime.starts_with("image/") {
            continue; // a page URL or non-image hit — skip it
        }
        let bytes = match resp.bytes().await {
            Ok(b) => b.to_vec(),
            Err(_) => continue,
        };
        if bytes.is_empty() || bytes.len() > MAX_BYTES {
            continue;
        }
        let ext = mime.split('/').nth(1).unwrap_or("jpg");
        let fname = format!("image.{ext}");
        match api
            .send_photo_bytes(chat_id, bytes, &fname, &mime, None, None)
            .await
        {
            Ok(()) => return true,
            Err(e) => {
                tracing::warn!("telegram remote-image send failed for {url}: {e:?}");
                continue;
            }
        }
    }
    false
}

/// Turn a raw turn error into a calm, actionable Telegram message. The
/// common failures are all "the model isn't ready yet" — the local model
/// server (especially the big /deep model) is starting up, mid-load, or
/// briefly offline. Those get a plain-language line telling the user to
/// retry, instead of a scary "Something went wrong" plus a raw stack string.
/// Anything unrecognized falls back to the generic message so we never hide
/// a real bug.
fn humanize_turn_error(e: &str) -> String {
    // The slot-failover already writes user-facing errors — don't wrap
    // them in "Something went wrong on KinAI's end:" boilerplate.
    if e.contains("No model server is answering") || e.contains("stopped responding partway") {
        return e.to_string();
    }

    let lc = e.to_ascii_lowercase();
    // Server up but the model is still loading into memory (llama.cpp returns
    // HTTP 503 "Loading model" until the weights are resident).
    if lc.contains("loading model")
        || lc.contains("503")
        || lc.contains("service unavailable")
        || lc.contains("unavailable_error")
    {
        return "⏳ The model is still loading — a big model can take a minute to warm up. \
                Give it a few seconds and send your message again."
            .to_string();
    }
    // Couldn't even open a connection — the model server is offline or starting.
    if lc.contains("error sending request")
        || lc.contains("connection refused")
        || lc.contains("connect error")
        || lc.contains("tcp connect")
        || lc.contains("dns error")
        || lc.contains("no route")
    {
        return "🔌 I couldn't reach the model server — it may be starting up or offline. \
                Check that it's running, then try again."
            .to_string();
    }
    // Connected but no response in time (loading, or busy on a long turn).
    if lc.contains("timed out") || lc.contains("timeout") || lc.contains("went silent") {
        return "⏱ The model didn't respond in time — it may be loading or busy. \
                Try again in a moment."
            .to_string();
    }
    format!("Something went wrong on KinAI's end: {e}")
}

/// Cap an in-progress streamed snapshot at ~4000 chars for INTERMEDIATE
/// edits (a single Telegram message can't exceed 4096). Walks back to a
/// UTF-8 char boundary before slicing so a multi-byte char at the cut
/// can't panic. The final, complete reply is handled separately (split
/// into multiple messages if needed) in send_assistant_reply.
fn truncate_for_tg(s: &str) -> String {
    const CAP: usize = 4000;
    // Flatten markdown (tables → bullet lists, drop heading/bold/etc. markers)
    // first — Telegram renders no markdown on a parse_mode-less send, so raw
    // tables show up as unreadable `|`/`-` soup. Then cap the length.
    let s = super::format::markdown_to_telegram(s);
    if s.len() <= CAP {
        return s;
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

/// Outcome of a `/voice` command: the reply text plus the resulting
/// state (`Some(new_state)` on a successful toggle, `None` for the
/// error/usage/not-paired replies). Callers use `enabled_now` to decide
/// whether the confirmation itself should be SPOKEN: turning voice ON
/// speaks the confirmation (instant audible proof it works); turning it
/// OFF — or any meta reply — must stay silent, because a voice that
/// answers "voice is off" out loud is absurd.
pub(crate) struct VoiceCommandOutcome {
    pub reply: String,
    pub enabled_now: Option<bool>,
}

/// Resolve a `/voice` command for `peer_id` (bare toggle / "on" / "off").
/// Shared by the Telegram router and BOTH in-app chat paths (host
/// desktop + client WS), so the toggle works — and answers identically —
/// on every surface. The pref controls voice notes in the peer's
/// TELEGRAM chat; desktop playback is the separate host-side
/// speak-button / auto-speak setting.
pub(crate) async fn voice_command_outcome(
    state: &SharedState,
    peer_id: &str,
    arg: &str,
) -> VoiceCommandOutcome {
    let meta = |reply: &str| VoiceCommandOutcome {
        reply: reply.into(),
        enabled_now: None,
    };
    if !state.config.read().tts.enabled {
        return meta(
            "🔇 Voice replies are switched off on the host. Ask the host to \
             enable them in KinAI → Settings → Voice replies.",
        );
    }
    let paired = tg_db::chat_for_peer(&state.db.pool, peer_id)
        .await
        .ok()
        .flatten()
        .is_some();
    if !paired {
        return meta(
            "🔇 Voice notes arrive in Telegram, but you haven't connected a \
             Telegram chat yet — open KinAI → Settings → Telegram to pair.",
        );
    }
    let current = tg_db::voice_replies(&state.db.pool, peer_id)
        .await
        .unwrap_or(false);
    let new_state = match arg.to_ascii_lowercase().as_str() {
        "" => !current,
        "on" => true,
        "off" => false,
        _ => return meta("Usage: /voice — toggle spoken replies, or /voice on / /voice off."),
    };
    if let Err(e) = tg_db::set_voice_replies(&state.db.pool, peer_id, new_state).await {
        return meta(&format!("Couldn't update the voice setting: {e}"));
    }
    if new_state {
        VoiceCommandOutcome {
            reply: "🔊 Voice replies are ON for your Telegram chat — answers there now \
                    arrive as text plus a spoken voice note. Send /voice again to turn \
                    them off."
                .into(),
            enabled_now: Some(true),
        }
    } else {
        VoiceCommandOutcome {
            reply: "🔇 Voice replies are OFF for your Telegram chat — answers arrive as \
                    text only."
                .into(),
            enabled_now: Some(false),
        }
    }
}

/// Download a Telegram voice note (OGG/Opus) and transcribe it with the
/// local Whisper model. Mirrors the photo download flow: getFile →
/// download by file_path.
async fn transcribe_voice_message(
    api: &BotApi,
    stt_cfg: &crate::config::SttConfig,
    msg: &TelegramMessage,
) -> Result<String> {
    let file_id = msg
        .voice
        .as_ref()
        .and_then(|v| v.get("file_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("voice message without file_id"))?;
    let file = api.get_file(file_id).await?;
    let file_path = file
        .file_path
        .ok_or_else(|| anyhow::anyhow!("telegram getFile returned no file_path"))?;
    let bytes = api.download_file(&file_path).await?;
    crate::stt::transcribe_ogg(stt_cfg, &bytes).await
}

/// If `content` is the `/voice` command, return the remainder (e.g.
/// "on", "off", or "" for the bare toggle), trimmed. Same boundary
/// rules as `strip_newchat` so `/voicemail` isn't mistaken for it.
pub(crate) fn strip_voice(content: &str) -> Option<&str> {
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

#[cfg(test)]
mod humanize_error_tests {
    use super::humanize_turn_error;

    #[test]
    fn maps_model_not_ready_states_to_friendly_text() {
        let loading = humanize_turn_error(
            "LLM error 503 Service Unavailable: {\"message\":\"Loading model\"}",
        );
        assert!(loading.contains("still loading"), "got: {loading}");

        let unreachable =
            humanize_turn_error("error sending request for url (http://127.0.0.1:8081/v1/chat/completions)");
        assert!(unreachable.contains("couldn't reach"), "got: {unreachable}");

        let timeout = humanize_turn_error("the model server went silent for 5 minutes");
        assert!(timeout.contains("didn't respond"), "got: {timeout}");
    }

    #[test]
    fn unknown_errors_keep_the_generic_message() {
        let other = humanize_turn_error("sqlite: database is locked");
        assert!(other.contains("Something went wrong on KinAI's end"), "got: {other}");
    }
}

#[cfg(test)]
mod remote_image_tests {
    use super::extract_remote_image_urls;

    #[test]
    fn extracts_external_image_urls_with_query_strings() {
        let reply = "Here's a portrait:\n\n\
            ![Demis](https://media.licdn.com/dms/image/v2/abc/photo?e=2147483647&v=beta&t=xyz)\n\n\
            *Source: LinkedIn*";
        let urls = extract_remote_image_urls(reply);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].ends_with("t=xyz"), "full query string captured: {}", urls[0]);
    }

    #[test]
    fn skips_our_local_pic_paths() {
        // /v1/pic local images are handled by extract_local_pic_path, not here.
        let reply = "![clown](http://127.0.0.1:4847/v1/pic/abc123.png)";
        assert!(extract_remote_image_urls(reply).is_empty());
    }

    #[test]
    fn collects_multiple_and_ignores_plain_links() {
        let reply = "![a](https://x.test/a.jpg) and a [link](https://y.test/page) and ![b](https://x.test/b.png)";
        let urls = extract_remote_image_urls(reply);
        assert_eq!(urls, vec!["https://x.test/a.jpg", "https://x.test/b.png"]);
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
