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

#[derive(Debug, Clone)]
pub struct BotApi {
    token: String,
    http: reqwest::Client,
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
        let value: Value = resp.json().await.context("decode telegram json")?;
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
            .context("getMe send")?;
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
            .context("getUpdates send")?;
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
            .context("editMessageText send")?;
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
                .context("sendMessage send")?;
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
            .context("deleteMessage send")?;
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
            .context("sendChatAction send")?;
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
            .context("sendPhoto send")?;
        let _: Value = Self::unwrap_response(resp).await?;
        Ok(())
    }

    /// Upload an audio file (AAC `.m4a` from the TTS engine) as a
    /// Telegram VOICE NOTE — the round waveform bubble. Since Bot API
    /// 7.2 sendVoice accepts .m4a/.mp3 in addition to OGG/Opus, so the
    /// macOS-native AAC output works directly. No caption: the reply
    /// text was already delivered as a normal message right before.
    pub async fn send_voice_file(&self, chat_id: i64, path: &std::path::Path) -> Result<()> {
        let file_bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("voice.m4a")
            .to_string();
        let form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                "voice",
                reqwest::multipart::Part::bytes(file_bytes)
                    .file_name(file_name)
                    .mime_str("audio/mp4")
                    .unwrap_or_else(|_| reqwest::multipart::Part::bytes(vec![])),
            );
        let resp = self
            .http
            .post(self.endpoint("sendVoice"))
            .multipart(form)
            .send()
            .await
            .context("sendVoice send")?;
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
            .context("getFile send")?;
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
            .context("download_file send")?
            .error_for_status()
            .context("download_file status")?;
        Ok(resp.bytes().await?.to_vec())
    }

    pub async fn set_my_commands(&self, cmds: &[BotCommand]) -> Result<()> {
        let resp = self
            .http
            .post(self.endpoint("setMyCommands"))
            .json(&json!({ "commands": cmds }))
            .send()
            .await
            .context("setMyCommands send")?;
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
