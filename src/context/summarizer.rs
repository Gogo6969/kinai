//! Deterministic extractive summarizer.
//!
//! No second LLM round-trip — we extract first sentences + frequency keywords.
//! v0.2 swaps this for an abstractive model behind the same interface.

use std::collections::BTreeMap;

use crate::db::Message;

pub fn summarize_window(msgs: &[Message]) -> (String, String) {
    if msgs.is_empty() {
        return (String::new(), String::new());
    }

    let first_at = msgs.first().map(|m| m.created_at.as_str()).unwrap_or("");
    let last_at = msgs.last().map(|m| m.created_at.as_str()).unwrap_or("");

    let participants: Vec<String> = {
        let mut seen = std::collections::BTreeSet::new();
        for m in msgs {
            if m.role == "user" {
                seen.insert(m.sender.clone());
            }
        }
        seen.into_iter().collect()
    };

    let mut bullets = Vec::new();
    for m in msgs {
        let prefix = match m.role.as_str() {
            "user" => format!("- {} asked", m.sender),
            "assistant" => "- KinAI replied".to_string(),
            _ => continue,
        };
        let snippet = first_sentence(&m.content, 200);
        if !snippet.is_empty() {
            bullets.push(format!("{}: {}", prefix, snippet));
        }
        if bullets.len() >= 20 {
            break;
        }
    }

    let summary = format!(
        "Conversation between {participants} ({first_at} → {last_at}):\n{body}",
        participants = if participants.is_empty() {
            "the user".into()
        } else {
            participants.join(", ")
        },
        body = bullets.join("\n"),
    );

    let keywords = top_keywords(msgs, 30);
    (summary, keywords)
}

fn first_sentence(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    let end = trimmed
        .find(|c: char| c == '.' || c == '?' || c == '!' || c == '\n')
        .map(|i| i + 1)
        .unwrap_or(trimmed.len());
    let mut out = trimmed[..end].trim().to_string();
    if out.chars().count() > max {
        out = out.chars().take(max).collect::<String>();
        out.push('…');
    }
    out
}

fn top_keywords(msgs: &[Message], n: usize) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for m in msgs {
        for word in m.content.split_whitespace() {
            let w: String = word
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase();
            if w.len() < 4 || STOPWORDS.contains(&w.as_str()) {
                continue;
            }
            *counts.entry(w).or_default() += 1;
        }
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.into_iter()
        .take(n)
        .map(|(k, _)| k)
        .collect::<Vec<_>>()
        .join(" ")
}

const STOPWORDS: &[&str] = &[
    "this", "that", "with", "from", "have", "your", "they", "them", "what",
    "when", "where", "would", "could", "about", "into", "more", "some", "than",
    "then", "their", "there", "which", "while", "been", "were", "will", "want",
    "just", "like", "much", "such", "very", "really", "going", "doing", "make",
    "made", "kind", "thing", "things",
];
