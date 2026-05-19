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

    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<()> {
        // Telegram chops messages at 4096 chars; split greedily on
        // paragraph boundaries when over.
        for part in split_for_telegram(text) {
            let resp = self
                .http
                .post(self.endpoint("sendMessage"))
                .json(&json!({
                    "chat_id": chat_id,
                    "text": part,
                    // Markdown-V2 would be nicer but it requires
                    // strict escaping of dozens of chars; plain text
                    // avoids "Bad Request: can't parse entities".
                    "disable_web_page_preview": true,
                }))
                .send()
                .await
                .context("sendMessage send")?;
            let _: Value = Self::unwrap_response(resp).await?;
        }
        Ok(())
    }

    /// Send an image file (from disk) as a Telegram photo with optional
    /// caption. Used by /pic and /picHQ outbound — the ComfyUI output
    /// is already saved under `~/.kinai/pics/<uuid>.png`, so we just
    /// upload that.
    pub async fn send_photo_file(
        &self,
        chat_id: i64,
        path: &std::path::Path,
        caption: Option<&str>,
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
            form = form.text("caption", c.to_string());
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
fn split_for_telegram(text: &str) -> Vec<String> {
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
