//! Attachment ingestion — for now: PDF → text. Image bytes pass through
//! untouched (to be consumed by a future vision-routing pipeline).
//!
//! The frontend sends file bytes as `data:<mime>;base64,<payload>` URLs in
//! the `data_url` field of each Attachment. We decode them here, run the
//! type-specific extractor, and return text the chat-turn code prepends
//! to the user's prompt.

use anyhow::{anyhow, Result};
use base64::Engine;

use crate::db::Attachment;

/// Hard cap on a single attachment's decoded size. PDFs above this are
/// rejected with a clear error rather than ballooning RAM during text
/// extraction. 25 MB covers nearly every personal document while still
/// fitting comfortably in memory on consumer hardware.
const MAX_BYTES: usize = 25 * 1024 * 1024;

/// Pull out any text the model can read from these attachments, ready to
/// be appended to the user's prompt. Returns an empty string if nothing
/// extracts (e.g. attachments are all images). Errors propagate so the
/// caller can surface them to the UI rather than silently dropping the
/// file.
pub fn extract_text(attachments: &[Attachment]) -> Result<String> {
    let mut out = String::new();
    for (i, att) in attachments.iter().enumerate() {
        if !is_pdf(att) {
            continue;
        }
        let label = att
            .name
            .clone()
            .unwrap_or_else(|| format!("attachment_{}.pdf", i + 1));
        match extract_pdf_text_from_attachment(att) {
            Ok(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&format!(
                    "--- PDF attachment: {} ---\n{}\n--- end of {} ---",
                    label, trimmed, label
                ));
            }
            Err(e) => {
                tracing::warn!("pdf extraction failed for {label}: {e:?}");
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&format!(
                    "--- PDF attachment: {} ---\n[Couldn't read this PDF: {}]\n--- end of {} ---",
                    label, e, label
                ));
            }
        }
    }
    Ok(out)
}

fn is_pdf(att: &Attachment) -> bool {
    let mime_is_pdf = att
        .mime
        .as_deref()
        .map(|m| m.eq_ignore_ascii_case("application/pdf"))
        .unwrap_or(false);
    let name_is_pdf = att
        .name
        .as_deref()
        .map(|n| n.to_ascii_lowercase().ends_with(".pdf"))
        .unwrap_or(false);
    mime_is_pdf || name_is_pdf
}

fn extract_pdf_text_from_attachment(att: &Attachment) -> Result<String> {
    let bytes = decode_data_url(att.data_url.as_deref().unwrap_or_default())?;
    if bytes.is_empty() {
        return Err(anyhow!("empty PDF"));
    }
    if bytes.len() > MAX_BYTES {
        return Err(anyhow!(
            "PDF is too large ({} MB; max 25 MB)",
            bytes.len() / (1024 * 1024)
        ));
    }
    // pdf-extract panics on malformed input on some PDF versions — wrap
    // in catch_unwind so a single bad PDF doesn't take down the chat
    // turn (and through it, the whole WS connection).
    let extracted = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&bytes))
        .map_err(|_| anyhow!("PDF parser crashed on this file"))??;
    Ok(extracted)
}

fn decode_data_url(s: &str) -> Result<Vec<u8>> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("attachment has no data"));
    }
    let comma = trimmed
        .find(',')
        .ok_or_else(|| anyhow!("attachment isn't a data URL"))?;
    let header = &trimmed[..comma];
    let payload = &trimmed[comma + 1..];
    if !header.contains(";base64") {
        return Err(anyhow!("only base64-encoded data URLs supported"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| anyhow!("base64 decode failed: {e}"))?;
    Ok(bytes)
}
