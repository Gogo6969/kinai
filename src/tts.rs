//! Local text-to-speech for voice replies.
//!
//! macOS-only engine built on the system `say` command — fully local,
//! zero dependencies, fits KinAI's privacy stance. Two consumers:
//!   * Telegram voice notes: synthesize to AAC-in-m4a (`say -o x.m4a
//!     --data-format=aac`) and upload via `sendVoice` — since Bot API
//!     7.2, .m4a renders as a real voice-note waveform bubble, so no
//!     OGG/Opus encoder (ffmpeg) is needed.
//!   * Desktop "speak" button + Settings previews: `say` straight to
//!     the host's speakers (no output file).
//!
//! The host is always a Mac today (see README); on other platforms the
//! synth functions return a clear error and the feature stays hidden.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// One installed system voice, parsed from `say -v '?'`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsVoice {
    pub name: String,
    /// BCP-47-ish locale as printed by `say` (e.g. "en_US", "de_DE").
    pub lang: String,
}

/// Joke/effect voices that ship with macOS but would be ridiculous as
/// an assistant voice — filtered out of the Settings dropdowns.
const NOVELTY_VOICES: &[&str] = &[
    "Albert", "Bad News", "Bahh", "Bells", "Boing", "Bubbles", "Cellos",
    "Good News", "Jester", "Junior", "Organ", "Ralph", "Superstar",
    "Trinoids", "Whisper", "Wobble", "Zarvox",
];

/// Strip markdown down to text worth speaking aloud.
///
/// * fenced code blocks are dropped entirely (listening to code is
///   useless; the text version is right above the voice note anyway)
/// * images vanish, links keep their visible text
/// * emphasis/heading/table syntax characters are removed
/// * whitespace collapses, length is capped so a long essay doesn't
///   become a ten-minute voice note
pub fn speakable_text(markdown: &str) -> String {
    const MAX_CHARS: usize = 3500;

    let mut out = String::with_capacity(markdown.len());
    let mut in_fence = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }

    // Images first (so the link pass doesn't keep their alt text with a
    // dangling URL), then links → visible text.
    let no_images = regex::Regex::new(r"!\[[^\]]*\]\([^)]*\)")
        .unwrap()
        .replace_all(&out, "");
    let no_links = regex::Regex::new(r"\[([^\]]+)\]\([^)]*\)")
        .unwrap()
        .replace_all(&no_images, "$1");

    // Drop markdown syntax characters; '|' becomes a pause-ish comma so
    // table rows read as lists rather than running together.
    let mut cleaned = String::with_capacity(no_links.len());
    for c in no_links.chars() {
        match c {
            '*' | '_' | '#' | '`' | '>' | '~' => {}
            '|' => cleaned.push(','),
            _ => cleaned.push(c),
        }
    }

    // Collapse whitespace runs (incl. the newlines we kept) to single
    // spaces — `say` handles its own pausing from punctuation.
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");

    if collapsed.chars().count() > MAX_CHARS {
        let mut s: String = collapsed.chars().take(MAX_CHARS).collect();
        s.push_str(" …");
        s
    } else {
        collapsed
    }
}

/// Cheap German-vs-English detection so replies get a native-sounding
/// voice. Umlauts/ß are a strong signal; otherwise count stopwords.
/// Ties and unknowns fall back to English.
pub fn detect_lang(text: &str) -> &'static str {
    if text.chars().any(|c| "äöüÄÖÜß".contains(c)) {
        return "de";
    }
    const DE: &[&str] = &[
        "der", "die", "das", "und", "ist", "nicht", "ein", "eine", "ich",
        "du", "wir", "sie", "mit", "für", "auf", "von", "zu", "im", "den",
        "auch", "aber", "oder", "wenn", "kann", "sind", "wird", "noch",
    ];
    const EN: &[&str] = &[
        "the", "and", "is", "are", "you", "for", "with", "that", "this",
        "of", "to", "in", "it", "on", "be", "can", "will", "your", "have",
        "not", "but", "if", "or",
    ];
    let mut de = 0usize;
    let mut en = 0usize;
    for word in text.split_whitespace().take(120) {
        let w: String = word
            .chars()
            .filter(|c| c.is_alphabetic())
            .collect::<String>()
            .to_lowercase();
        if DE.contains(&w.as_str()) {
            de += 1;
        }
        if EN.contains(&w.as_str()) {
            en += 1;
        }
    }
    if de > en {
        "de"
    } else {
        "en"
    }
}

/// Parse the output of `say -v '?'`. Each line looks like:
/// `Zoe (Premium)       en_US    # Hello! My name is Zoe.`
/// Name may contain spaces/parens; locale is the last token before `#`.
pub fn parse_voice_listing(listing: &str) -> Vec<TtsVoice> {
    let mut voices = Vec::new();
    for line in listing.lines() {
        let meta = line.split('#').next().unwrap_or("").trim_end();
        let Some(lang) = meta.split_whitespace().last() else {
            continue;
        };
        if !lang.contains('_') {
            continue; // not a locale column → malformed line
        }
        let name = meta[..meta.rfind(lang).unwrap_or(0)].trim();
        if name.is_empty() {
            continue;
        }
        if NOVELTY_VOICES.contains(&name) {
            continue;
        }
        voices.push(TtsVoice {
            name: name.to_string(),
            lang: lang.to_string(),
        });
    }
    voices
}

/// List installed system voices (filtered of novelty voices).
#[cfg(target_os = "macos")]
pub async fn list_voices() -> Result<Vec<TtsVoice>> {
    let out = tokio::process::Command::new("say")
        .args(["-v", "?"])
        .output()
        .await
        .context("spawn `say -v ?`")?;
    Ok(parse_voice_listing(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(not(target_os = "macos"))]
pub async fn list_voices() -> Result<Vec<TtsVoice>> {
    Ok(Vec::new())
}

/// Synthesize `text` with `voice` into an AAC `.m4a` file and return its
/// path (caller deletes it after use). The text is passed via a temp
/// file (`say -f`) so argument-length limits and leading-dash content
/// can't bite. Falls back to the system default voice if the configured
/// one isn't installed.
#[cfg(target_os = "macos")]
pub async fn synthesize_m4a(text: &str, voice: &str) -> Result<std::path::PathBuf> {
    let id = uuid::Uuid::new_v4();
    let dir = std::env::temp_dir();
    let txt_path = dir.join(format!("kinai-tts-{id}.txt"));
    let out_path = dir.join(format!("kinai-tts-{id}.m4a"));
    tokio::fs::write(&txt_path, text).await.context("write tts text")?;

    let run = |use_voice: bool| {
        let mut cmd = tokio::process::Command::new("say");
        if use_voice && !voice.is_empty() {
            cmd.args(["-v", voice]);
        }
        cmd.arg("-f").arg(&txt_path);
        cmd.arg("-o").arg(&out_path);
        cmd.arg("--data-format=aac");
        cmd.output()
    };

    let mut result = run(true).await.context("spawn say")?;
    if !result.status.success() {
        // Most likely "Voice ... not found" — retry with the default
        // voice so a missing premium download degrades, not breaks.
        tracing::warn!(
            "tts: voice {voice:?} failed ({}), retrying with default voice",
            String::from_utf8_lossy(&result.stderr).trim()
        );
        result = run(false).await.context("spawn say (fallback)")?;
    }
    let _ = tokio::fs::remove_file(&txt_path).await;
    if !result.status.success() {
        let _ = tokio::fs::remove_file(&out_path).await;
        return Err(anyhow!(
            "say failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    // `say` can exit 0 yet write an empty file for broken formats; treat
    // a tiny file as failure so we never upload a corrupt voice note.
    let size = tokio::fs::metadata(&out_path).await.map(|m| m.len()).unwrap_or(0);
    if size < 1024 {
        let _ = tokio::fs::remove_file(&out_path).await;
        return Err(anyhow!("say produced an empty audio file"));
    }
    Ok(out_path)
}

#[cfg(not(target_os = "macos"))]
pub async fn synthesize_m4a(_text: &str, _voice: &str) -> Result<std::path::PathBuf> {
    Err(anyhow!("voice replies require a macOS host"))
}

/// Speak `text` straight to the host's speakers (no output file) and
/// return the child process so the caller can stop playback. Uses one
/// fixed temp file for the text, so repeated calls never accumulate
/// files. Caller is responsible for killing any previous child first.
#[cfg(target_os = "macos")]
pub async fn speak_live(text: &str, voice: &str) -> Result<tokio::process::Child> {
    let txt = std::env::temp_dir().join("kinai-tts-live.txt");
    tokio::fs::write(&txt, text).await.context("write tts text")?;
    let mut cmd = tokio::process::Command::new("say");
    if !voice.is_empty() {
        cmd.args(["-v", voice]);
    }
    cmd.arg("-f").arg(&txt);
    cmd.spawn().context("spawn say")
}

#[cfg(not(target_os = "macos"))]
pub async fn speak_live(_text: &str, _voice: &str) -> Result<tokio::process::Child> {
    Err(anyhow!("voice playback requires a macOS host"))
}

/// Pick the configured voice for the detected language of `text`.
pub fn voice_for_text(cfg: &crate::config::TtsConfig, text: &str) -> String {
    match detect_lang(text) {
        "de" => cfg.voice_de.clone(),
        _ => cfg.voice_en.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_code_blocks_links_and_syntax() {
        let md = "Here is **bold** and a [link](https://x.com).\n\
                  ```rust\nfn main() {}\n```\n\
                  ![img](http://x/y.png)\n\
                  | a | b |\n#### Heading";
        let s = speakable_text(md);
        assert!(s.contains("Here is bold and a link."), "got: {s}");
        assert!(!s.contains("fn main"), "code must be dropped: {s}");
        assert!(!s.contains("http"), "URLs must be dropped: {s}");
        assert!(!s.contains('#') && !s.contains('*') && !s.contains('['));
    }

    #[test]
    fn caps_length_on_char_boundary() {
        let long = "wörd ".repeat(2000);
        let s = speakable_text(&long);
        assert!(s.chars().count() <= 3502);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn empty_when_only_code() {
        assert_eq!(speakable_text("```\nlet x = 1;\n```"), "");
    }

    #[test]
    fn detects_german_and_english() {
        assert_eq!(detect_lang("Morgen wird es sonnig und warm, schön!"), "de");
        assert_eq!(detect_lang("Die Katze ist nicht auf dem Tisch und das ist gut"), "de");
        assert_eq!(detect_lang("Tomorrow will be sunny and warm for you"), "en");
        assert_eq!(detect_lang(""), "en");
    }

    #[test]
    fn parses_say_voice_listing() {
        let listing = "Zoe (Premium)       en_US    # Hello! My name is Zoe.\n\
                       Anna (Premium)      de_DE    # Hallo! Ich heiße Anna.\n\
                       Bubbles             en_US    # Hello! My name is Bubbles.\n\
                       Eddy (German (Germany)) de_DE    # Hallo! Ich heiße Eddy.\n";
        let v = parse_voice_listing(listing);
        let names: Vec<&str> = v.iter().map(|x| x.name.as_str()).collect();
        assert!(names.contains(&"Zoe (Premium)"));
        assert!(names.contains(&"Anna (Premium)"));
        assert!(names.contains(&"Eddy (German (Germany))"));
        assert!(!names.contains(&"Bubbles"), "novelty voices filtered");
        assert_eq!(v[0].lang, "en_US");
        assert_eq!(v[1].lang, "de_DE");
    }
}
