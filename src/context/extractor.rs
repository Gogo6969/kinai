//! Passive fact extraction.
//!
//! Runs as a fire-and-forget background task after each user turn:
//! calls the fast LLM with a tight "extract facts from this message"
//! prompt, parses JSON, saves any new facts to `user_facts` with
//! `source = "extractor"`.
//!
//! Design choices to keep this from misbehaving:
//!   * **Cheap.** Triggered only when the user message looks like it
//!     might contain a fact: first-person pronoun present, length
//!     above a threshold. Short greetings ("hi", "thanks") are skipped.
//!   * **Conservative.** Only ADDS facts whose key doesn't already
//!     exist. If the user explicitly called `remember(city, Berlin)`
//!     in an earlier turn, the extractor won't silently overwrite it
//!     with a paraphrase or a confidence-weakened version. Only the
//!     explicit `remember` tool overwrites.
//!   * **Traceable.** Facts written here get `source = "extractor"`,
//!     so the Settings → Memory page can show a "learned passively"
//!     chip next to them — the user knows what they explicitly said
//!     vs what the model inferred.
//!   * **Non-blocking.** Spawned with `tokio::spawn`; failure to
//!     extract never affects the user's reply.
//!
//! Why we can't reuse the tool path inside the main turn: a single
//! turn can call `remember` multiple times in tool-loop iterations
//! already (the model does this when it picks up a fact mid-reply),
//! but that's volatile — it depends on the model deciding to make
//! the call. The extractor is the safety net for when the model
//! doesn't.

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::config::LlmSettings;
use crate::db::Db;

const MIN_LEN: usize = 25;
const MAX_FACTS_PER_MESSAGE: usize = 3;
const MAX_OUTPUT_TOKENS: usize = 256;

#[derive(Debug, Deserialize)]
struct ExtractedFact {
    key: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct ExtractorResponse {
    #[serde(default)]
    facts: Vec<ExtractedFact>,
}

/// Quick gate: should we even bother running the LLM? Skip messages
/// that obviously can't contain a fact (too short, no self-reference)
/// to save a round-trip on every "thanks" and "?" the user sends.
pub fn looks_fact_shaped(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.len() < MIN_LEN {
        return false;
    }
    let lower = trimmed.to_lowercase();
    // First-person reference required — facts about the user only
    // surface when they talk about themselves. Multilingual hint: most
    // family-bot users write in English, German, French, or Spanish;
    // covering the high-frequency self-pronouns from each catches the
    // common cases without a real NLP pass.
    [
        " i ", " i'", "i am ", "i'm ", "my ", "mine ", "me ",
        // German
        "ich ", "mir ", "mein", "meine",
        // French
        "je ", "j'", "mon ", "ma ", "mes ",
        // Spanish
        "yo ", "mi ", "soy ",
    ]
    .iter()
    .any(|needle| lower.contains(needle) || lower.starts_with(needle.trim_start()))
}

pub async fn extract_and_save(
    db: &Db,
    llm_settings: &LlmSettings,
    peer_id: &str,
    source_msg_id: &str,
    user_message: &str,
) -> Result<usize> {
    if !looks_fact_shaped(user_message) {
        return Ok(0);
    }

    // Build a tight, JSON-only prompt. No tools, no streaming wrapper —
    // we want a single short completion. We pass the raw HTTP request
    // ourselves to bypass the streaming pipeline (which would emit
    // tokens to UI listeners that haven't subscribed).
    let prompt = format!(
        "You analyze a SINGLE user message and extract persistent facts about the user.

Output strictly valid JSON of the form:
{{\"facts\": [{{\"key\": \"<snake_case_key>\", \"value\": \"<concise value>\"}}, ...]}}

Rules:
- Include ONLY stable, persistent facts about the user themselves \
(name, location, family, work, preferences, diet, language, timezone, etc.).
- DO NOT include transient state (mood, current activity), one-off questions, \
general knowledge, or content NOT about the user.
- Use short snake_case keys (e.g. \"city\", \"wife_name\", \"diet\", \"timezone\").

PLAUSIBILITY GATE (this matters — you are NOT talking to the user, you are a silent \
background extractor and cannot ask follow-up questions; if you save junk it stays):

- REJECT physically implausible numeric values for human attributes:
  * height outside ~50 cm to 230 cm (1'8\" to 7'6\")
  * age outside 0 to 120 years
  * weight outside ~2 kg to 350 kg
  * family sizes outside ~0 to 20 (no \"I have 500 children\")
- REJECT hyperbole and jokes: \"I sleep 30 hours a day\", \"I have a million dogs\", \"I am 10 \
feet tall\", \"I have been alive for 4000 years\". Real conversational lift doesn't store as fact.
- REJECT fictional or supernatural identity claims: \"I am Batman\", \"I'm a time traveler\", \
\"I am made of pure energy\".
- REJECT vague self-description that isn't a fact: \"I am amazing\", \"I am the best\". These \
aren't memory material.
- WHEN IN DOUBT, exclude the fact. False negatives (missing a real fact) are recoverable — the \
user can re-state it, or call `remember` directly through the chat. False positives (storing \
junk) corrupt the user's memory page and require manual cleanup.

- Cap at {MAX_FACTS_PER_MESSAGE} facts. If nothing qualifies, respond exactly: {{\"facts\": []}}
- Respond with ONLY the JSON object — no preamble, no markdown fences.

User message:
\"\"\"
{user_message}
\"\"\"
",
        MAX_FACTS_PER_MESSAGE = MAX_FACTS_PER_MESSAGE,
        user_message = user_message
    );

    let response_text = simple_completion(llm_settings, &prompt).await?;
    let parsed = parse_extractor_json(&response_text)?;

    // Fetch existing keys ONCE, not once per candidate fact (this loop
    // ran up to MAX_FACTS_PER_MESSAGE full-table SELECTs over the same
    // rows). Keys we write below are added to the set so duplicates within
    // the same batch are also skipped.
    let mut existing_keys: std::collections::HashSet<String> = db
        .list_user_facts(peer_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|f| f.key)
        .collect();

    let mut written = 0;
    for fact in parsed.facts.into_iter().take(MAX_FACTS_PER_MESSAGE) {
        let key = normalize_key(&fact.key);
        let value = fact.value.trim().to_string();
        if key.is_empty() || value.is_empty() {
            continue;
        }
        // Conservative: only add if absent. Explicit remember() calls
        // (source="tool") and user-typed entries (source="manual") win
        // every time over passive extraction.
        if existing_keys.contains(&key) {
            continue;
        }
        if let Err(e) = db
            .save_user_fact(peer_id, &key, &value, "extractor", Some(source_msg_id))
            .await
        {
            tracing::warn!("extractor save failed for {key}: {e}");
            continue;
        }
        existing_keys.insert(key);
        written += 1;
    }
    Ok(written)
}

fn normalize_key(raw: &str) -> String {
    raw.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .chars()
        .collect::<String>()
        // Collapse runs of underscores. snake_case with single _.
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

fn parse_extractor_json(text: &str) -> Result<ExtractorResponse> {
    // Models sometimes wrap JSON in ```json ... ``` fences even when
    // told not to. Strip fences before parsing.
    let stripped = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    // Find the outermost { } in case the model added text outside.
    let start = stripped.find('{').ok_or_else(|| anyhow!("no JSON object in extractor output"))?;
    let end = stripped.rfind('}').ok_or_else(|| anyhow!("no JSON object end"))?;
    if end <= start {
        return Err(anyhow!("malformed JSON range"));
    }
    let json_slice = &stripped[start..=end];
    let parsed: ExtractorResponse = serde_json::from_str(json_slice)
        .map_err(|e| anyhow!("extractor JSON parse: {e} :: {json_slice}"))?;
    Ok(parsed)
}

/// Single-shot, non-streaming completion against the OpenAI-compatible
/// /v1/chat/completions endpoint. The main pipeline streams SSE through
/// `llm::stream::pump` for the chat path; the extractor doesn't need
/// any of that machinery, so we just POST + JSON-decode the body.
async fn simple_completion(settings: &LlmSettings, prompt: &str) -> Result<String> {
    let url = format!("{}/v1/chat/completions", settings.base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": settings.model,
        "messages": [
            {"role": "system", "content": "You output strictly valid JSON. No prose."},
            {"role": "user", "content": prompt}
        ],
        "temperature": 0.0,
        "max_tokens": MAX_OUTPUT_TOKENS,
        "stream": false,
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut req = client.post(&url).json(&body);
    if let Some(key) = &settings.api_key {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("extractor LLM returned {}", resp.status());
    }
    let value: serde_json::Value = resp.json().await?;
    let content = value
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_skips_short_messages() {
        assert!(!looks_fact_shaped("hi"));
        assert!(!looks_fact_shaped("thanks!"));
    }

    #[test]
    fn gate_skips_no_self_reference() {
        assert!(!looks_fact_shaped(
            "what is the tallest mountain in the world today"
        ));
    }

    #[test]
    fn gate_accepts_self_reference() {
        assert!(looks_fact_shaped("I live in Berlin and work in Munich."));
        assert!(looks_fact_shaped("My wife is vegetarian, can you suggest"));
    }

    #[test]
    fn normalize_key_handles_spaces_and_punctuation() {
        assert_eq!(normalize_key("Wife's birthday"), "wife_s_birthday");
        assert_eq!(normalize_key("CITY"), "city");
        assert_eq!(normalize_key("  multi   word  "), "multi_word");
    }

    #[test]
    fn parse_handles_fenced_json() {
        let text = "```json\n{\"facts\": [{\"key\":\"city\",\"value\":\"Berlin\"}]}\n```";
        let parsed = parse_extractor_json(text).unwrap();
        assert_eq!(parsed.facts.len(), 1);
        assert_eq!(parsed.facts[0].key, "city");
    }

    #[test]
    fn parse_handles_bare_json() {
        let text = r#"{"facts":[]}"#;
        let parsed = parse_extractor_json(text).unwrap();
        assert!(parsed.facts.is_empty());
    }
}
