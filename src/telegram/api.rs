//! Thin reqwest wrapper around the Telegram Bot API.
//!
//! We deliberately don't pull in a heavy bot framework (teloxide /
//! frankenstein) — the Bot API is plain HTTP+JSON, our usage is
//! narrow (text in/out, photo out, file-id download), and a hand-
//! rolled client keeps the dep tree light and the error surface clear.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct BotApi {
    token: String,
    http: reqwest::Client,
}

/// Hand-written so the token cannot be printed by accident.
///
/// The derived `Debug` rendered the field verbatim, so a single
/// `tracing::warn!("telegram: {api:?}")` anywhere would have put the
/// family's bot token in the log — the same way the derived `Display` on
/// a reqwest error already did. Nothing formats a `BotApi` today; this is
/// here so nothing can start to.
impl std::fmt::Debug for BotApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotApi").field("token", &"<redacted>").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BotUser {
    pub id: i64,
    pub is_bot: bool,
    pub first_name: String,
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramUser {
    pub id: i64,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramChat {
    pub id: i64,
    #[serde(default)]
    pub r#type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PhotoSize {
    pub file_id: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    #[serde(default)]
    pub from: Option<TelegramUser>,
    pub chat: TelegramChat,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
    /// Multiple sizes — the largest is the highest quality.
    #[serde(default)]
    pub photo: Vec<PhotoSize>,
    /// Present when the user sent a recorded voice message. We can't
    /// transcribe these (no STT yet) — the router answers with a hint
    /// instead of silently ignoring them. Payload contents unused.
    #[serde(default)]
    pub voice: Option<Value>,
    #[serde(default)]
    pub audio: Option<Value>,
    #[serde(default)]
    pub video_note: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramUpdate {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<TelegramMessage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramFile {
    pub file_id: String,
    pub file_path: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
}

/// Entries we feed to setMyCommands so users see slash autocomplete in
/// the phone keyboard.
#[derive(Debug, Clone, Serialize)]
pub struct BotCommand {
    pub command: String,
    pub description: String,
}

/// Turn a `reqwest` error into one that is safe to write to a log.
///
/// reqwest's `Display` appends the request URL, and every Bot API URL
/// embeds the token (`.../bot<id>:<secret>/getUpdates`). So *any* caller
/// that formats one of these errors — a `tracing::warn!("{e:?}")` in the
/// poll loop, the string `test_telegram_token` hands back to Settings —
/// writes the family's bot token out in the clear. It reached
/// `~/.kinai/logs/` that way on five days running, mostly from getUpdates.
///
/// 0.2.116 redacted the one startup path it knew about. Redacting at each
/// log site does not scale: there are ~20 of them across polling, router,
/// echo and commands, every one of them a chance to forget, and a new one
/// is added every time someone logs a send failure. So scrub here, at the
/// single boundary where a Bot API error is born, and no call site can
/// leak the token by omission.
///
/// The source chain is flattened into the message rather than kept as a
/// `source`: the sources carry the part worth reading ("Connection reset
/// by peer"), but leaving the original error attached would let `{e:?}`
/// print the unredacted URL straight back out.
fn scrub(ctx: &'static str, e: reqwest::Error) -> anyhow::Error {
    let mut msg = e.to_string();
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    anyhow::anyhow!("{ctx}: {}", super::redact_token(&msg))
}

impl BotApi {
    pub fn new(token: String) -> Self {
        // The default getUpdates long-poll timeout we ask Telegram for
        // is 30s; allow up to 60s in the HTTP client so a healthy
        // long-poll never trips the timeout-as-error path.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("reqwest client");
        Self { token, http }
    }

    fn endpoint(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{method}", self.token)
    }

    fn file_endpoint(&self, file_path: &str) -> String {
        format!("https://api.telegram.org/file/bot{}/{file_path}", self.token)
    }

    /// Unwrap Telegram's `{ok: true, result: ...}` envelope; surface
    /// `{ok: false, description: "..."}` errors with their text.
    async fn unwrap_response<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
    ) -> Result<T> {
        let value: Value = resp.json().await.map_err(|e| scrub("decode telegram json", e))?;
        let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if !ok {
            let desc = value
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("(no description)")
                .to_string();
            anyhow::bail!("telegram error: {desc}");
        }
        let result = value
            .get("result")
            .cloned()
            .unwrap_or(Value::Null);
        Ok(serde_json::from_value(result)?)
    }

    /// Identify the bot — used at startup to populate `bot_username`
    /// and to validate that the token works.
    pub async fn get_me(&self) -> Result<BotUser> {
        let resp = self
            .http
            .get(self.endpoint("getMe"))
            .send()
            .await
            .map_err(|e| scrub("getMe send", e))?;
        Self::unwrap_response(resp).await
    }

    /// Long-poll. `offset` is the next update_id we want (last+1).
    /// `timeout` is the seconds Telegram should hold the request open
    /// when there's nothing new (we ask for 30s).
    pub async fn get_updates(&self, offset: i64, timeout: u32) -> Result<Vec<TelegramUpdate>> {
        let resp = self
            .http
            .post(self.endpoint("getUpdates"))
            .json(&json!({
                "offset": offset,
                "timeout": timeout,
                // Filter out edited messages, channel posts, callbacks,
                // etc. We only care about new chat messages.
                "allowed_updates": ["message"],
            }))
            .send()
            .await
            .map_err(|e| scrub("getUpdates send", e))?;
        Self::unwrap_response(resp).await
    }

    /// Send a plain-text message. Returns the first part's `message_id`
    /// (a long message split into N parts returns the id of part 1) so the
    /// streaming reply path can later edit it. Existing callers that `?`
    /// this just drop the id.
    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<i64> {
        self.send_message_with_parse(chat_id, text, None).await
    }

    /// Send an HTML-formatted message (parse_mode=HTML). Telegram's HTML
    /// mode supports `<b>`, `<i>`, `<u>`, `<s>`, `<code>`, `<pre>`,
    /// `<a href>`, `<blockquote>`, `<tg-spoiler>` — the subset we need
    /// for Q&A echoes (blockquote on the user's question, plain text on
    /// the assistant reply). Caller is responsible for escaping `<`,
    /// `>`, `&` in any user-supplied content (`html_escape` helper).
    pub async fn send_message_html(&self, chat_id: i64, html: &str) -> Result<i64> {
        self.send_message_with_parse(chat_id, html, Some("HTML")).await
    }

    /// Edit the text of a previously-sent message. Used by the streaming
    /// reply path to live-update a placeholder as tokens arrive. Plain text
    /// only (no parse_mode) — mid-stream markdown is frequently unbalanced
    /// (an open `**` or half a code fence) and Telegram rejects unbalanced
    /// entities with a 400. "message is not modified" (identical text) is
    /// treated as success, not an error.
    pub async fn edit_message_text(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        let resp = self
            .http
            .post(self.endpoint("editMessageText"))
            .json(&json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "text": text,
                "disable_web_page_preview": true,
            }))
            .send()
            .await
            .map_err(|e| scrub("editMessageText send", e))?;
        match Self::unwrap_response::<Value>(resp).await {
            Ok(_) => Ok(()),
            Err(e) => {
                if e.to_string().contains("message is not modified") {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn send_message_with_parse(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<i64> {
        // Telegram chops messages at 4096 chars; split greedily on
        // paragraph boundaries when over. NOTE: with HTML parse mode an
        // unlucky split could land inside a tag; for our use case the
        // HTML structure is small (blockquote up front) and the body is
        // mostly plain text, so split_for_telegram is safe enough. If
        // we ever start emitting heavily-nested HTML this will need a
        // smarter splitter that re-emits open tags on each chunk.
        let mut first_id: Option<i64> = None;
        for part in split_for_telegram(text) {
            let mut body = json!({
                "chat_id": chat_id,
                "text": part,
                "disable_web_page_preview": true,
            });
            if let Some(mode) = parse_mode {
                body["parse_mode"] = Value::String(mode.to_string());
            }
            let resp = self
                .http
                .post(self.endpoint("sendMessage"))
                .json(&body)
                .send()
                .await
                .map_err(|e| scrub("sendMessage send", e))?;
            let sent: TelegramMessage = Self::unwrap_response(resp).await?;
            if first_id.is_none() {
                first_id = Some(sent.message_id);
            }
        }
        // Callers always pass non-empty text, so the loop runs ≥ once; 0 is
        // a defensive sentinel for the impossible empty case.
        Ok(first_id.unwrap_or(0))
    }

    /// Delete a message the bot previously sent (e.g. the "creating a
    /// picture…" placeholder once the photo is ready). "message to delete
    /// not found" is treated as success — the goal state (message gone)
    /// already holds.
    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<()> {
        let resp = self
            .http
            .post(self.endpoint("deleteMessage"))
            .json(&json!({ "chat_id": chat_id, "message_id": message_id }))
            .send()
            .await
            .map_err(|e| scrub("deleteMessage send", e))?;
        match Self::unwrap_response::<Value>(resp).await {
            Ok(_) => Ok(()),
            Err(e) => {
                if e.to_string().contains("message to delete not found") {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Fire-and-forget typing indicator. Shows "Bot is typing…" in the
    /// chat for ~5 seconds. Used by the KinAI→Telegram echo so phone
    /// users get a heads-up that a reply is being generated. Failures
    /// are logged upstream; the chat flow doesn't depend on this.
    pub async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<()> {
        let resp = self
            .http
            .post(self.endpoint("sendChatAction"))
            .json(&json!({ "chat_id": chat_id, "action": action }))
            .send()
            .await
            .map_err(|e| scrub("sendChatAction send", e))?;
        let _: Value = Self::unwrap_response(resp).await?;
        Ok(())
    }

    /// Send an image file (from disk) as a Telegram photo with optional
    /// caption. Used by /pic and /picHQ outbound — the ComfyUI output
    /// is already saved under `~/.kinai/pics/<uuid>.png`, so we just
    /// upload that. `caption_parse_mode` lets the caller request HTML
    /// rendering for the caption (used by the QA echo to format the
    /// user's question as a blockquote).
    pub async fn send_photo_file(
        &self,
        chat_id: i64,
        path: &std::path::Path,
        caption: Option<&str>,
        caption_parse_mode: Option<&str>,
    ) -> Result<()> {
        let file_bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("image.png")
            .to_string();
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                "photo",
                reqwest::multipart::Part::bytes(file_bytes)
                    .file_name(file_name)
                    .mime_str("image/png")
                    .unwrap_or_else(|_| reqwest::multipart::Part::bytes(vec![])),
            );
        if let Some(c) = caption {
            // Telegram caps captions at 1024 chars — trim with an
            // ellipsis if the caller exceeds. Better a partial caption
            // than a Bad Request that loses the whole photo.
            let trimmed = if c.chars().count() > 1024 {
                let truncated: String = c.chars().take(1019).collect();
                format!("{truncated}…")
            } else {
                c.to_string()
            };
            form = form.text("caption", trimmed);
            if let Some(mode) = caption_parse_mode {
                form = form.text("parse_mode", mode.to_string());
            }
        }
        let resp = self
            .http
            .post(self.endpoint("sendPhoto"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| scrub("sendPhoto send", e))?;
        let _: Value = Self::unwrap_response(resp).await?;
        Ok(())
    }

    /// Send already-in-memory bytes as a Telegram photo. Used for remote
    /// images (e.g. an `image_search` hit) that the host downloaded itself,
    /// so Telegram never has to fetch the URL (it often can't — many image
    /// hits are behind a CDN that 403s Telegram's fetcher, or are page URLs).
    pub async fn send_photo_bytes(
        &self,
        chat_id: i64,
        bytes: Vec<u8>,
        file_name: &str,
        mime: &str,
        caption: Option<&str>,
        caption_parse_mode: Option<&str>,
    ) -> Result<()> {
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                "photo",
                reqwest::multipart::Part::bytes(bytes)
                    .file_name(file_name.to_string())
                    .mime_str(mime)
                    .unwrap_or_else(|_| reqwest::multipart::Part::bytes(vec![])),
            );
        if let Some(c) = caption {
            let trimmed = if c.chars().count() > 1024 {
                let truncated: String = c.chars().take(1019).collect();
                format!("{truncated}…")
            } else {
                c.to_string()
            };
            form = form.text("caption", trimmed);
            if let Some(mode) = caption_parse_mode {
                form = form.text("parse_mode", mode.to_string());
            }
        }
        let resp = self
            .http
            .post(self.endpoint("sendPhoto"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| scrub("sendPhoto(bytes) send", e))?;
        let _: Value = Self::unwrap_response(resp).await?;
        Ok(())
    }

    /// Upload an audio file as a Telegram VOICE NOTE — the round
    /// waveform bubble. OGG/Opus is the canonical format (Telegram
    /// parses duration + waveform itself); the AAC `.m4a` fallback needs
    /// `duration` passed explicitly or clients render a dead 00:00
    /// bubble. No caption: the reply text was already delivered as a
    /// normal message right before.
    pub async fn send_voice_file(
        &self,
        chat_id: i64,
        path: &std::path::Path,
        duration_secs: Option<u32>,
    ) -> Result<()> {
        let file_bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        let is_ogg = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ogg"))
            .unwrap_or(false);
        let mime = if is_ogg { "audio/ogg" } else { "audio/mp4" };
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(if is_ogg { "voice.ogg" } else { "voice.m4a" })
            .to_string();
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                "voice",
                reqwest::multipart::Part::bytes(file_bytes)
                    .file_name(file_name)
                    .mime_str(mime)
                    .unwrap_or_else(|_| reqwest::multipart::Part::bytes(vec![])),
            );
        if let Some(d) = duration_secs {
            form = form.text("duration", d.to_string());
        }
        let resp = self
            .http
            .post(self.endpoint("sendVoice"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| scrub("sendVoice send", e))?;
        let _: Value = Self::unwrap_response(resp).await?;
        Ok(())
    }

    /// Resolve a Telegram file_id to a downloadable URL. Step 1 of the
    /// two-step Bot API file download.
    pub async fn get_file(&self, file_id: &str) -> Result<TelegramFile> {
        let resp = self
            .http
            .post(self.endpoint("getFile"))
            .json(&json!({ "file_id": file_id }))
            .send()
            .await
            .map_err(|e| scrub("getFile send", e))?;
        Self::unwrap_response(resp).await
    }

    /// Step 2 of file download — fetch the bytes from the URL getFile
    /// returned. Used for inbound photo → vision pipeline.
    pub async fn download_file(&self, file_path: &str) -> Result<Vec<u8>> {
        let url = self.file_endpoint(file_path);
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| scrub("download_file send", e))?
            .error_for_status()
            .map_err(|e| scrub("download_file status", e))?;
        Ok(resp.bytes().await?.to_vec())
    }

    pub async fn set_my_commands(&self, cmds: &[BotCommand]) -> Result<()> {
        let resp = self
            .http
            .post(self.endpoint("setMyCommands"))
            .json(&json!({ "commands": cmds }))
            .send()
            .await
            .map_err(|e| scrub("setMyCommands send", e))?;
        let _: Value = Self::unwrap_response(resp).await?;
        Ok(())
    }
}

/// Split text into Telegram-sized chunks. The 4096-char limit applies
/// per message; split on paragraph boundaries when possible to keep
/// each chunk readable.
pub(crate) fn split_for_telegram(text: &str) -> Vec<String> {
    const LIMIT: usize = 4000; // leave headroom below 4096 for safety
    if text.len() <= LIMIT {
        return vec![text.to_string()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for paragraph in text.split("\n\n") {
        // Paragraph itself too big? Hard split on chars.
        if paragraph.len() > LIMIT {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            for chunk in paragraph
                .as_bytes()
                .chunks(LIMIT)
                // Re-decode chunks. Slicing on bytes is risky for
                // multi-byte UTF-8; we lose char boundaries. For the
                // path we actually hit (giant code blocks etc.) it's
                // acceptable; Telegram tolerates the resulting ?.
            {
                parts.push(String::from_utf8_lossy(chunk).to_string());
            }
            continue;
        }
        if current.len() + paragraph.len() + 2 > LIMIT {
            parts.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(paragraph);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

#[cfg(test)]
mod redaction_tests {
    use super::{super::redact_token, scrub, BotApi};

    /// The derived `Debug` printed `token` verbatim; this keeps the
    /// hand-written one honest if someone reaches for the derive again.
    #[test]
    fn debug_formatting_a_botapi_does_not_print_the_token() {
        let token = "1234567890:AAFakeFakeFakeFakeFakeFakeFakeFakeFak";
        let shown = format!("{:?}", BotApi::new(token.to_string()));
        assert!(!shown.contains(token), "token survived Debug: {shown}");
        assert!(shown.contains("<redacted>"), "unexpected shape: {shown}");
    }

    /// Proves the assumption the whole redaction rests on: that a *real*
    /// reqwest transport error prints the request URL, and that `scrub`
    /// takes it back out of the message and every source in the chain.
    /// The string-level tests above cannot see that — they assert against
    /// a shape typed by hand.
    ///
    /// Networked (it connects to a port nothing listens on), so it is
    /// `#[ignore]`d and stays out of `cargo test --lib` and CI. Run it by
    /// hand when touching either half:
    /// `cargo test --lib -- --ignored scrub_strips`
    #[tokio::test]
    #[ignore]
    async fn scrub_strips_the_token_from_a_real_reqwest_error() {
        let token = "1234567890:AAFakeFakeFakeFakeFakeFakeFakeFakeFak";
        // Port 1 on loopback: nothing listens, so this fails to connect —
        // the same class of error as the "Connection reset by peer" that
        // wrote the token to the log on 2026-09-05.
        let url = format!("http://127.0.0.1:1/bot{token}/getUpdates");
        let err = reqwest::Client::new().get(&url).send().await.unwrap_err();
        assert!(
            err.to_string().contains(token),
            "reqwest no longer puts the URL in Display — re-check what this guards: {err}"
        );
        let scrubbed = format!("{:?}", scrub("getUpdates send", err));
        assert!(!scrubbed.contains(token), "token survived scrub: {scrubbed}");
        assert!(
            scrubbed.contains("/bot<redacted>/getUpdates"),
            "lost the useful shape: {scrubbed}"
        );
    }

    /// Guards the pairing rather than a hand-written string: the redactor
    /// runs over the URLs `BotApi` actually builds, so a change to either
    /// side has to keep them matched. Both shapes carry the token —
    /// `/bot<token>/<method>` and `/file/bot<token>/<path>`.
    #[test]
    fn every_bot_api_url_we_build_redacts_cleanly() {
        let token = "1234567890:AAFakeFakeFakeFakeFakeFakeFakeFakeFak";
        let api = BotApi::new(token.to_string());
        for url in [api.endpoint("getUpdates"), api.file_endpoint("photos/f_1.jpg")] {
            let safe = redact_token(&url);
            assert!(!safe.contains(token), "token survived in {url}: {safe}");
            assert!(safe.contains("/bot<redacted>/"), "unexpected shape: {safe}");
        }
    }
}
