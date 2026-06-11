//! Local speech-to-text for inbound Telegram voice messages.
//!
//! Engine: whisper.cpp embedded via whisper-rs (Metal-accelerated on
//! Apple Silicon), macOS-only like the TTS side — the host is always a
//! Mac. The model is the single artifact that can't ship inside the
//! app binary; Settings offers a one-click download to
//! `~/.kinai/models/`. Audio decode (Telegram's OGG/Opus → 16 kHz mono
//! WAV) uses the system `afconvert`, so end users install nothing.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A downloadable Whisper model option. Both are multilingual (German +
/// English auto-detected per message).
#[derive(Debug, Clone, Serialize)]
pub struct SttModel {
    pub id: &'static str,
    pub label: &'static str,
    pub filename: &'static str,
    pub url: &'static str,
    pub size_mb: u32,
}

pub const MODELS: &[SttModel] = &[
    SttModel {
        id: "small-q5_1",
        label: "Standard (good accuracy, fast)",
        filename: "ggml-small-q5_1.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small-q5_1.bin",
        size_mb: 190,
    },
    SttModel {
        id: "large-v3-turbo-q5_0",
        label: "High accuracy (bigger, slower)",
        filename: "ggml-large-v3-turbo-q5_0.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin",
        size_mb: 574,
    },
];

pub fn model_by_id(id: &str) -> Option<&'static SttModel> {
    MODELS.iter().find(|m| m.id == id)
}

/// `~/.kinai/models` — created on demand.
pub fn models_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kinai")
        .join("models")
}

pub fn model_path(id: &str) -> Option<PathBuf> {
    model_by_id(id).map(|m| models_dir().join(m.filename))
}

pub fn model_downloaded(id: &str) -> bool {
    // Guard against truncated downloads: the smallest offered model is
    // ~190 MB, so anything under 10 MB is junk from an aborted fetch.
    model_path(id)
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len() > 10 * 1024 * 1024)
        .unwrap_or(false)
}

/// Voice input is usable: switched on AND the configured model exists.
pub fn is_ready(cfg: &crate::config::SttConfig) -> bool {
    cfg.enabled && model_downloaded(&cfg.model)
}

// ---- Transcription -------------------------------------------------------

#[cfg(target_os = "macos")]
mod engine {
    use super::*;
    use std::sync::Mutex;
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    /// One loaded model, cached across calls — loading takes seconds,
    /// transcribing a short voice note takes well under one. Reloaded
    /// only when the configured model changes.
    static CONTEXT: Mutex<Option<(PathBuf, std::sync::Arc<WhisperContext>)>> = Mutex::new(None);

    fn context_for(path: &PathBuf) -> Result<std::sync::Arc<WhisperContext>> {
        let mut guard = CONTEXT.lock().unwrap();
        if let Some((cached_path, ctx)) = guard.as_ref() {
            if cached_path == path {
                return Ok(ctx.clone());
            }
        }
        let ctx = WhisperContext::new_with_params(
            path.to_str().ok_or_else(|| anyhow!("non-UTF8 model path"))?,
            WhisperContextParameters::default(),
        )
        .map_err(|e| anyhow!("loading whisper model: {e}"))?;
        let ctx = std::sync::Arc::new(ctx);
        *guard = Some((path.clone(), ctx.clone()));
        Ok(ctx)
    }

    /// Transcribe 16 kHz mono f32 PCM. Blocking + CPU/GPU heavy — call
    /// via `spawn_blocking`.
    pub fn transcribe_pcm(model: &PathBuf, pcm: &[f32]) -> Result<String> {
        let ctx = context_for(model)?;
        let mut state = ctx
            .create_state()
            .map_err(|e| anyhow!("whisper state: {e}"))?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("auto"));
        params.set_translate(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        state
            .full(params, pcm)
            .map_err(|e| anyhow!("whisper transcription: {e}"))?;
        let mut text = String::new();
        for i in 0..state.full_n_segments() {
            if let Some(seg) = state.get_segment(i) {
                if let Ok(s) = seg.to_str() {
                    text.push_str(s);
                }
            }
        }
        Ok(text.trim().to_string())
    }
}

/// Decode an OGG/Opus (or any CoreAudio-readable) audio file to 16 kHz
/// mono i16 WAV via the system `afconvert`, then load the samples.
#[cfg(target_os = "macos")]
async fn decode_to_pcm(audio_path: &std::path::Path) -> Result<Vec<f32>> {
    let wav_path = audio_path.with_extension("wav");
    let out = tokio::process::Command::new("afconvert")
        .args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"])
        .arg(audio_path)
        .arg(&wav_path)
        .output()
        .await
        .context("spawn afconvert")?;
    if !out.status.success() {
        return Err(anyhow!(
            "afconvert failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let wav_path_owned = wav_path.clone();
    let pcm = tokio::task::spawn_blocking(move || -> Result<Vec<f32>> {
        let mut reader = hound::WavReader::open(&wav_path_owned).context("open decoded wav")?;
        let samples: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<std::result::Result<_, _>>()
            .context("read wav samples")?;
        Ok(samples)
    })
    .await??;
    let _ = tokio::fs::remove_file(&wav_path).await;
    if pcm.len() < 1600 {
        return Err(anyhow!("voice clip too short to transcribe"));
    }
    Ok(pcm)
}

/// Transcribe raw OGG/Opus bytes (a downloaded Telegram voice note).
/// Returns the trimmed transcript ("" possible for silence).
#[cfg(target_os = "macos")]
pub async fn transcribe_ogg(cfg: &crate::config::SttConfig, ogg_bytes: &[u8]) -> Result<String> {
    let model = model_path(&cfg.model).ok_or_else(|| anyhow!("unknown model {:?}", cfg.model))?;
    if !model_downloaded(&cfg.model) {
        return Err(anyhow!("speech model not downloaded"));
    }
    let id = uuid::Uuid::new_v4();
    let ogg_path = std::env::temp_dir().join(format!("kinai-stt-{id}.oga"));
    tokio::fs::write(&ogg_path, ogg_bytes).await.context("write voice file")?;
    let decoded = decode_to_pcm(&ogg_path).await;
    let _ = tokio::fs::remove_file(&ogg_path).await;
    let pcm = decoded?;
    tokio::task::spawn_blocking(move || engine::transcribe_pcm(&model, &pcm)).await?
}

#[cfg(not(target_os = "macos"))]
pub async fn transcribe_ogg(_cfg: &crate::config::SttConfig, _ogg_bytes: &[u8]) -> Result<String> {
    Err(anyhow!("voice input requires a macOS host"))
}

/// Status snapshot for the Settings UI.
#[derive(Debug, Serialize)]
pub struct SttStatus {
    pub enabled: bool,
    pub model: String,
    pub ready: bool,
    pub models: Vec<SttModelStatus>,
}

#[derive(Debug, Serialize)]
pub struct SttModelStatus {
    pub id: &'static str,
    pub label: &'static str,
    pub size_mb: u32,
    pub downloaded: bool,
}

pub fn status(cfg: &crate::config::SttConfig) -> SttStatus {
    SttStatus {
        enabled: cfg.enabled,
        model: cfg.model.clone(),
        ready: is_ready(cfg),
        models: MODELS
            .iter()
            .map(|m| SttModelStatus {
                id: m.id,
                label: m.label,
                size_mb: m.size_mb,
                downloaded: model_downloaded(m.id),
            })
            .collect(),
    }
}

// Unused-import hygiene for non-macOS builds.
#[cfg(not(target_os = "macos"))]
#[allow(unused_imports)]
use Deserialize as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_registry_is_consistent() {
        assert!(model_by_id("small-q5_1").is_some());
        assert!(model_by_id("large-v3-turbo-q5_0").is_some());
        assert!(model_by_id("nope").is_none());
        for m in MODELS {
            assert!(m.url.starts_with("https://huggingface.co/"));
            assert!(m.filename.ends_with(".bin"));
            assert!(m.size_mb > 10);
        }
        assert!(model_path("small-q5_1").unwrap().ends_with("ggml-small-q5_1.bin"));
    }

    /// End-to-end engine test against a real spoken clip. Ignored by
    /// default (needs the downloaded model + an audio fixture); run
    /// explicitly with:
    ///   cargo test --release stt_end_to_end -- --ignored --nocapture
    /// Fixture: any OGG/Opus speech clip at /tmp/tts-test.ogg.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore]
    async fn stt_end_to_end() {
        let cfg = crate::config::SttConfig {
            enabled: true,
            model: "small-q5_1".into(),
        };
        let bytes = std::fs::read("/tmp/tts-test.ogg").expect("fixture /tmp/tts-test.ogg");
        let text = transcribe_ogg(&cfg, &bytes).await.expect("transcribe");
        println!("TRANSCRIPT: {text:?}");
        assert!(!text.is_empty());
    }
}
