//! ComfyUI integration — powers the `/pic` and `/picHQ` chat commands.
//!
//! With a picture attached, the same two commands CHANGE that picture
//! ("give the man a birthday crown") instead of making a new one: see
//! `edit`. That path uploads the picture to the server, runs Qwen Image
//! 2.1 Turbo's edit workflow on it, and blanks the uploaded copy again.
//!
//! Ported from CCC chat's `comfyui-pic.js`. Same exact workflows (Z-Image
//! Turbo for `/pic`, Z-Image Base for `/picHQ`) so the user gets identical
//! output here as they do in CCC.
//!
//! Flow:
//!   1. POST {prompt: workflow_json, client_id} to {base_url}/prompt
//!   2. Poll {base_url}/history/<prompt_id> until outputs appear
//!   3. GET {base_url}/view?filename=...&type=output&subfolder=... → bytes
//!   4. Save bytes to ~/.kinai/pics/<uuid>.png, return relative URL
//!
//! The host's `/v1/pic/:filename` route serves saved images back to
//! clients on the LAN. Clients render them inline via the existing
//! markdown image renderer (`![alt](url)` → `<img>`).
//!
//! Requires `comfyui.base_url` in config; empty value means
//! `is_configured()` returns false and slash commands are not advertised.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use uuid::Uuid;

/// Which ComfyUI workflow to drive. `Turbo` ≈ 8 steps, ~5s; `Hq` ≈ 25 steps, ~30s.
#[derive(Debug, Clone, Copy)]
pub enum Model {
    Turbo,
    Hq,
}

impl Model {
    pub fn label(self) -> &'static str {
        match self {
            Model::Turbo => "Z-Image Turbo",
            Model::Hq => "Z-Image Base HQ",
        }
    }

    /// Slug used in saved filenames + the slash-command name visible to users.
    pub fn slug(self) -> &'static str {
        match self {
            Model::Turbo => "pic",
            Model::Hq => "picHQ",
        }
    }
}

pub struct GeneratedImage {
    /// Relative URL the client should fetch the image from.
    /// Form: `/v1/pic/<uuid>.png`.
    pub url_path: String,
    /// Absolute path on the host's disk.
    pub local_path: PathBuf,
    pub bytes: usize,
    pub elapsed_secs: f64,
}

pub fn is_configured(base_url: &str) -> bool {
    !base_url.trim().is_empty()
}

/// Where saved images live on disk. The host serves these via the
/// `/v1/pic/:filename` Axum route.
pub fn pics_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kinai")
        .join("pics")
}

/// Build the workflow JSON for the chosen model. Mirrors CCC's
/// comfyui-pic.js exactly — same node graph, same parameters. Inject
/// prompt, dimensions, and a random seed.
fn build_workflow(model: Model, prompt: &str, width: u32, height: u32, seed: u64) -> Value {
    // CLIP truncate to a reasonable length so giant prompts can't make the
    // request body inflate by 100×.
    let safe_prompt: String = prompt.chars().take(4000).collect();
    match model {
        Model::Turbo => json!({
            "9":     { "class_type": "SaveImage",             "inputs": { "filename_prefix": "kinai_pic", "images": ["57:8", 0] } },
            "57:30": { "class_type": "CLIPLoader",            "inputs": { "clip_name": "qwen_3_4b.safetensors", "type": "lumina2", "device": "default" } },
            "57:29": { "class_type": "VAELoader",             "inputs": { "vae_name": "ae.safetensors" } },
            "57:33": { "class_type": "ConditioningZeroOut",   "inputs": { "conditioning": ["57:27", 0] } },
            "57:8":  { "class_type": "VAEDecode",             "inputs": { "samples": ["57:3", 0], "vae": ["57:29", 0] } },
            "57:28": { "class_type": "UNETLoader",            "inputs": { "unet_name": "z_image_turbo_bf16.safetensors", "weight_dtype": "default" } },
            "57:27": { "class_type": "CLIPTextEncode",        "inputs": { "text": safe_prompt, "clip": ["57:30", 0] } },
            "57:13": { "class_type": "EmptySD3LatentImage",   "inputs": { "width": width, "height": height, "batch_size": 1 } },
            "57:11": { "class_type": "ModelSamplingAuraFlow", "inputs": { "shift": 3, "model": ["57:28", 0] } },
            "57:3":  { "class_type": "KSampler",              "inputs": { "seed": seed, "steps": 8, "cfg": 1, "sampler_name": "res_multistep", "scheduler": "simple", "denoise": 1, "model": ["57:11", 0], "positive": ["57:27", 0], "negative": ["57:33", 0], "latent_image": ["57:13", 0] } },
        }),
        Model::Hq => json!({
            "9":     { "class_type": "SaveImage",             "inputs": { "filename_prefix": "kinai_pichq", "images": ["57:8", 0] } },
            "57:30": { "class_type": "CLIPLoader",            "inputs": { "clip_name": "qwen_3_4b.safetensors", "type": "lumina2", "device": "default" } },
            "57:29": { "class_type": "VAELoader",             "inputs": { "vae_name": "ae.safetensors" } },
            "57:33": { "class_type": "CLIPTextEncode",        "inputs": { "text": "", "clip": ["57:30", 0] } },
            "57:8":  { "class_type": "VAEDecode",             "inputs": { "samples": ["57:3", 0], "vae": ["57:29", 0] } },
            "57:28": { "class_type": "UNETLoader",            "inputs": { "unet_name": "z_image_bf16.safetensors", "weight_dtype": "default" } },
            "57:27": { "class_type": "CLIPTextEncode",        "inputs": { "text": safe_prompt, "clip": ["57:30", 0] } },
            "57:13": { "class_type": "EmptySD3LatentImage",   "inputs": { "width": width, "height": height, "batch_size": 1 } },
            "57:11": { "class_type": "ModelSamplingAuraFlow", "inputs": { "shift": 3, "model": ["57:28", 0] } },
            "57:3":  { "class_type": "KSampler",              "inputs": { "seed": seed, "steps": 25, "cfg": 4.0, "sampler_name": "res_multistep", "scheduler": "simple", "denoise": 1, "model": ["57:11", 0], "positive": ["57:27", 0], "negative": ["57:33", 0], "latent_image": ["57:13", 0] } },
        }),
    }
}

/// Submit a generation job, poll for completion, fetch the image, save
/// it to disk, return the relative URL clients can use to load it.
pub async fn generate(
    base_url: &str,
    model: Model,
    prompt: &str,
    width: u32,
    height: u32,
) -> Result<GeneratedImage> {
    if !is_configured(base_url) {
        bail!("ComfyUI base_url is not configured (set it in Settings → Image generation)");
    }
    let base = base_url.trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .context("build http client")?;

    let seed: u64 = rand::random();
    let workflow = build_workflow(model, prompt, width, height, seed);
    run_workflow(&client, base, workflow, Instant::now()).await
}

/// Submit one workflow, wait for its picture, save it under
/// `~/.kinai/pics/` and return where clients can load it. Shared by
/// `generate` (text to picture) and `edit` (change a sent picture).
async fn run_workflow(
    client: &reqwest::Client,
    base: &str,
    workflow: Value,
    t_start: Instant,
) -> Result<GeneratedImage> {
    let client_id = Uuid::new_v4().to_string();
    let submit_resp = client
        .post(format!("{base}/prompt"))
        .json(&json!({ "prompt": workflow, "client_id": client_id }))
        .send()
        .await
        .context("submit to ComfyUI")?;

    if !submit_resp.status().is_success() {
        let status = submit_resp.status();
        let body = submit_resp.text().await.unwrap_or_default();
        bail!("{}", summarize_comfy_error(status, &body));
    }

    let submit: Value = submit_resp.json().await.context("parse submit response")?;
    if let Some(node_errors) = submit.get("node_errors") {
        if node_errors.is_object() && !node_errors.as_object().unwrap().is_empty() {
            bail!("ComfyUI workflow had node errors: {node_errors}");
        }
    }
    let prompt_id = submit
        .get("prompt_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ComfyUI didn't return a prompt_id"))?
        .to_string();

    // Poll history endpoint for up to 6 minutes (same as CCC).
    let mut outputs: Option<Value> = None;
    for _ in 0..360 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let r = match client
            .get(format!("{base}/history/{prompt_id}"))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            _ => continue,
        };
        let j: Value = match r.json().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let entry = match j.get(&prompt_id) {
            Some(e) => e.clone(),
            None => continue,
        };
        // ComfyUI reports execution errors in status.status_str / messages.
        if let Some(status_str) = entry
            .get("status")
            .and_then(|s| s.get("status_str"))
            .and_then(|s| s.as_str())
        {
            if status_str == "error" {
                let msgs = entry
                    .get("status")
                    .and_then(|s| s.get("messages"))
                    .cloned()
                    .unwrap_or(Value::Null);
                bail!("ComfyUI execution error: {msgs}");
            }
        }
        let entry_outputs = entry.get("outputs").cloned().unwrap_or(Value::Null);
        if entry_outputs.is_object() && !entry_outputs.as_object().unwrap().is_empty() {
            outputs = Some(entry_outputs);
            break;
        }
    }
    let outputs = outputs.ok_or_else(|| anyhow!("ComfyUI didn't return outputs after 6 minutes"))?;

    // Find the first node that produced images.
    let mut img_info: Option<Value> = None;
    if let Some(obj) = outputs.as_object() {
        for (_, v) in obj {
            if let Some(imgs) = v.get("images").and_then(|i| i.as_array()) {
                if let Some(first) = imgs.first() {
                    img_info = Some(first.clone());
                    break;
                }
            }
        }
    }
    let img_info = img_info.ok_or_else(|| anyhow!("ComfyUI outputs contained no images"))?;
    let filename = img_info
        .get("filename")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ComfyUI image entry missing filename"))?;
    let img_type = img_info
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("output");
    let subfolder = img_info
        .get("subfolder")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let view_url = format!(
        "{base}/view?filename={filename}&type={img_type}&subfolder={subfolder}"
    );
    let img_resp = client
        .get(&view_url)
        .send()
        .await
        .context("fetch image from ComfyUI")?;
    if !img_resp.status().is_success() {
        bail!("Fetching ComfyUI image failed: {}", img_resp.status());
    }
    let bytes = img_resp
        .bytes()
        .await
        .context("read image bytes from ComfyUI")?;

    // Persist to ~/.kinai/pics/<uuid>.<ext>. ComfyUI returns PNGs from
    // SaveImage; keep that extension verbatim in case it ever varies.
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png");
    let pic_uuid = Uuid::new_v4();
    let saved_name = format!("{pic_uuid}.{ext}");
    let dir = pics_dir();
    std::fs::create_dir_all(&dir).context("create ~/.kinai/pics directory")?;
    let local_path = dir.join(&saved_name);
    std::fs::write(&local_path, &bytes).context("save image to disk")?;

    Ok(GeneratedImage {
        url_path: format!("/v1/pic/{saved_name}"),
        local_path,
        bytes: bytes.len(),
        elapsed_secs: t_start.elapsed().as_secs_f64(),
    })
}

/// Label shown under an edited picture.
pub const EDIT_LABEL: &str = "Qwen Image 2.1 Turbo";

/// Largest picture `/pic` accepts for editing. A phone photo is 2–6 MB;
/// this only stops something absurd from being pushed to the server.
const MAX_EDIT_BYTES: usize = 25 * 1024 * 1024;

/// How large the edited picture comes out, as the model's "resolution"
/// (about that many pixels squared, in the sent picture's own shape).
/// Never 0: that keeps the sent size, and a 12-megapixel phone photo then
/// takes about five minutes instead of ten seconds.
pub fn edit_resolution(model: Model) -> u32 {
    match model {
        Model::Turbo => 1024,
        Model::Hq => 1536,
    }
}

/// The first picture among a message's attachments, as raw bytes plus the
/// file extension the server should store it under. `None` when there is
/// no picture, or it is in a format the picture server cannot open.
pub fn first_picture(attachments: &[crate::db::Attachment]) -> Option<(Vec<u8>, &'static str)> {
    use base64::Engine;
    for a in attachments {
        if a.kind != "image" {
            continue;
        }
        let Some(url) = a.data_url.as_deref() else { continue };
        let Some(rest) = url.strip_prefix("data:") else { continue };
        let Some((meta, b64)) = rest.split_once(";base64,") else { continue };
        let ext = match meta.to_ascii_lowercase().as_str() {
            "image/png" => "png",
            "image/jpeg" | "image/jpg" => "jpg",
            "image/webp" => "webp",
            _ => continue,
        };
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
            if !bytes.is_empty() {
                return Some((bytes, ext));
            }
        }
    }
    None
}

/// Workflow that changes one uploaded picture as `prompt` says. The node
/// graph and the nine sampling steps are those of ComfyUI's own "Qwen
/// Image 2.1 Turbo: Image edit" template, without its optional prompt
/// rewriter.
fn build_edit_workflow(prompt: &str, image_name: &str, resolution: u32, seed: u64) -> Value {
    let safe_prompt: String = prompt.chars().take(4000).collect();
    json!({
        "1":  { "class_type": "LoadImage",             "inputs": { "image": image_name } },
        "2":  { "class_type": "UNETLoader",            "inputs": { "unet_name": "qwen_image_2.1_turbo_int8_convrot.safetensors", "weight_dtype": "default" } },
        "3":  { "class_type": "QwenImage21Cache",      "inputs": { "model": ["2", 0], "device": "auto", "dtype": "default" } },
        "4":  { "class_type": "CLIPLoader",            "inputs": { "clip_name": "qwen3vl_8b_int8_convrot.safetensors", "type": "qwen_image", "device": "default" } },
        "5":  { "class_type": "VAELoader",             "inputs": { "vae_name": "qwen_image_2.1_vae_bf16.safetensors" } },
        "6":  { "class_type": "TextEncodeQwenImage21", "inputs": { "clip": ["4", 0], "vae": ["5", 0], "prompt": safe_prompt, "negative_prompt": "", "resolution": resolution, "images.image_1": ["1", 0] } },
        "7":  { "class_type": "CFGGuider",             "inputs": { "model": ["3", 0], "positive": ["6", 0], "negative": ["6", 1], "cfg": 1 } },
        "8":  { "class_type": "RandomNoise",           "inputs": { "noise_seed": seed } },
        "9":  { "class_type": "KSamplerSelect",        "inputs": { "sampler_name": "euler" } },
        "10": { "class_type": "ManualSigmas",          "inputs": { "sigmas": "1.0, 0.978453, 0.95418, 0.926626, 0.89508, 0.845148, 0.704534, 0.414568, 0" } },
        "11": { "class_type": "SamplerCustomAdvanced", "inputs": { "noise": ["8", 0], "guider": ["7", 0], "sampler": ["9", 0], "sigmas": ["10", 0], "latent_image": ["6", 2] } },
        "12": { "class_type": "VAEDecode",             "inputs": { "samples": ["11", 0], "vae": ["5", 0] } },
        "13": { "class_type": "SaveImage",             "inputs": { "filename_prefix": "kinai_picedit", "images": ["12", 0] } },
    })
}

/// A 1×1 transparent PNG: what an uploaded picture is overwritten with
/// once its edit is done, so family photos do not pile up on the server.
const BLANK_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0x60, 0x60, 0x60, 0x60,
    0x00, 0x00, 0x00, 0x05, 0x00, 0x01, 0x7A, 0xA8, 0x57, 0x50, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// Put `bytes` into the server's input folder under `name`, replacing a
/// file of that name. Returns the name the server stored it under.
async fn upload_input(client: &reqwest::Client, base: &str, name: &str, bytes: Vec<u8>) -> Result<String> {
    let part = reqwest::multipart::Part::bytes(bytes).file_name(name.to_string());
    let form = reqwest::multipart::Form::new()
        .part("image", part)
        .text("overwrite", "true");
    let resp = client
        .post(format!("{base}/upload/image"))
        .multipart(form)
        .send()
        .await
        .context("send the picture to ComfyUI")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("{}", summarize_comfy_error(status, &body));
    }
    let j: Value = resp.json().await.context("parse upload response")?;
    let stored = j.get("name").and_then(|v| v.as_str()).unwrap_or(name);
    Ok(match j.get("subfolder").and_then(|v| v.as_str()) {
        Some(sub) if !sub.is_empty() => format!("{sub}/{stored}"),
        _ => stored.to_string(),
    })
}

/// Change a sent picture as `prompt` says and return the result the same
/// way `generate` does. `ext` is the picture's file extension as given by
/// `first_picture`.
pub async fn edit(
    base_url: &str,
    model: Model,
    prompt: &str,
    picture: Vec<u8>,
    ext: &str,
) -> Result<GeneratedImage> {
    if !is_configured(base_url) {
        bail!("ComfyUI base_url is not configured (set it in Settings → Image generation)");
    }
    if picture.len() > MAX_EDIT_BYTES {
        bail!("that picture is too large to change ({} MB; the limit is 25 MB)", picture.len() / (1024 * 1024));
    }
    let base = base_url.trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .context("build http client")?;

    let t_start = Instant::now();
    let upload_name = format!("kinai_edit_{}.{ext}", Uuid::new_v4().simple());
    let stored = upload_input(&client, base, &upload_name, picture).await?;
    let workflow = build_edit_workflow(prompt, &stored, edit_resolution(model), rand::random());
    let result = run_workflow(&client, base, workflow, t_start).await;
    // Whatever happened, do not leave the sent picture on the server. The
    // server has no way to delete an input, so it is overwritten in place.
    if let Err(e) = upload_input(&client, base, &upload_name, BLANK_PNG.to_vec()).await {
        tracing::warn!("comfyui: could not blank the uploaded picture after the edit: {e}");
    }
    result
}

/// Render a human-readable error from a non-success ComfyUI HTTP
/// response WITHOUT dumping a multi-kilobyte HTML page into the chat
/// bubble. Olares' "ComfyUI Unavailable" 503 page is ~3 KB of inline
/// `<style>` + `<script>`; before this helper, that whole blob ended
/// up rendered verbatim under "/picHQ failed after 0.0s: …" — which
/// was both unreadable and a privacy leak (the page embeds the
/// host's admin subdomains).
///
/// Strategy:
///   1. If the body looks like HTML, try to extract an `<h1>` or
///      `<title>` so the user sees the actual error label
///      ("ComfyUI Unavailable" etc.).
///   2. Otherwise fall back to a canned message keyed off the HTTP
///      status (503 / 502 / 504 / generic).
///   3. Plain-text and JSON bodies pass through but are capped at
///      300 chars so a future verbose backend can't blow up the
///      bubble.
fn summarize_comfy_error(status: reqwest::StatusCode, body: &str) -> String {
    let trimmed = body.trim();
    let lower = trimmed.to_lowercase();
    let looks_html = trimmed.starts_with('<')
        || lower.contains("<!doctype")
        || lower.contains("<html")
        || lower.contains("<body");

    if looks_html {
        if let Some(headline) = extract_tag_text(trimmed, &lower, "<h1>", "</h1>")
            .or_else(|| extract_tag_text(trimmed, &lower, "<title>", "</title>"))
        {
            return format!(
                "ComfyUI is unavailable: {headline} (HTTP {})",
                status.as_u16()
            );
        }
        return match status.as_u16() {
            503 => "ComfyUI is unavailable (HTTP 503). The image-generation service \
                    isn't running or isn't reachable from KinAI. Check the ComfyUI \
                    host or try again in a moment."
                .into(),
            502 => "ComfyUI returned HTTP 502 Bad Gateway. The upstream image \
                    service is misconfigured or down."
                .into(),
            504 => "ComfyUI returned HTTP 504 Gateway Timeout. The upstream image \
                    service didn't respond in time."
                .into(),
            _ => format!(
                "ComfyUI returned HTTP {} with an HTML error page (no usable detail).",
                status.as_u16()
            ),
        };
    }

    if trimmed.is_empty() {
        return format!("ComfyUI returned HTTP {} with no body.", status.as_u16());
    }
    let snippet = if trimmed.chars().count() > 300 {
        let cut: String = trimmed.chars().take(300).collect();
        format!("{cut}…")
    } else {
        trimmed.to_string()
    };
    format!("ComfyUI rejected submission (HTTP {}): {snippet}", status)
}

/// Pull the visible text out of the first `<open>...</close>` pair,
/// stripping any nested tags. Case-insensitive match on the tag
/// names (lookups use the caller-supplied lowercase copy of `orig`),
/// content is sliced from the original-case string so the user sees
/// "ComfyUI Unavailable", not "comfyui unavailable".
fn extract_tag_text(orig: &str, lower: &str, open: &str, close: &str) -> Option<String> {
    let start = lower.find(open)?;
    let body_start = start + open.len();
    let body_lower = lower.get(body_start..)?;
    let end_rel = body_lower.find(close)?;
    let text = orig.get(body_start..body_start + end_rel)?;
    let mut out = String::new();
    let mut depth: u32 = 0;
    for c in text.chars() {
        if c == '<' {
            depth += 1;
        } else if c == '>' {
            depth = depth.saturating_sub(1);
        } else if depth == 0 {
            out.push(c);
        }
    }
    let cleaned = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() || cleaned.chars().count() > 200 {
        None
    } else {
        Some(cleaned)
    }
}

#[cfg(test)]
mod summarize_tests {
    use super::*;

    #[test]
    fn html_503_extracts_h1() {
        let body = "<html><body><h1>ComfyUI Unavailable</h1><p>down</p></body></html>";
        let s = summarize_comfy_error(reqwest::StatusCode::SERVICE_UNAVAILABLE, body);
        assert!(s.contains("ComfyUI Unavailable"), "got: {s}");
        assert!(s.contains("503"), "got: {s}");
        assert!(!s.contains("<"), "shouldn't leak HTML: {s}");
    }

    #[test]
    fn html_503_no_h1_falls_back_to_canned() {
        let body = "<html><body><div>oops</div></body></html>";
        let s = summarize_comfy_error(reqwest::StatusCode::SERVICE_UNAVAILABLE, body);
        assert!(s.contains("503"), "got: {s}");
        assert!(!s.contains("<"), "shouldn't leak HTML: {s}");
    }

    #[test]
    fn plain_text_passes_through_capped() {
        let s = summarize_comfy_error(
            reqwest::StatusCode::BAD_REQUEST,
            "node 12 missing input",
        );
        assert!(s.contains("node 12 missing input"), "got: {s}");
        assert!(s.contains("400"), "got: {s}");
    }

    #[test]
    fn long_text_is_truncated() {
        let body = "x".repeat(2000);
        let s = summarize_comfy_error(reqwest::StatusCode::BAD_REQUEST, &body);
        assert!(s.chars().count() < 500, "should be capped: len={}", s.chars().count());
        assert!(s.ends_with('…'), "should end with ellipsis: {s}");
    }

    #[test]
    fn empty_body_named() {
        let s = summarize_comfy_error(reqwest::StatusCode::BAD_GATEWAY, "");
        assert!(s.contains("502"), "got: {s}");
        assert!(s.contains("no body"), "got: {s}");
    }
}

/// Parse a `/pic …` or `/picHQ …` chat message.
///
/// Accepted shapes:
///   `/pic <prompt>`                 → 1280x720 default
///   `/pic 1024x1024 <prompt>`       → custom size
///   `/picHQ <prompt>`               → 1024x1024 default (HQ)
///   `/picHQ 1280x720 <prompt>`      → custom size
///
/// Returns (model, width, height, prompt) on success.
pub fn parse_slash(text: &str) -> Option<(Model, u32, u32, String)> {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();
    // The command prefixes are ASCII, so their byte length is identical in
    // the lowercased copy and the original. Slice the ORIGINAL by that
    // fixed prefix length to get the original-case body. (Deriving the cut
    // from the lowercased *remainder* length panicked/underflowed when a
    // body char's lowercase form had a different byte length, e.g. "İ" —
    // a crafted `/pic İ…` could crash the whole Telegram poll loop.)
    let (model, prefix_len) = if lower.starts_with("/pichq") {
        (Model::Hq, "/pichq".len())
    } else if lower.starts_with("/pic") {
        (Model::Turbo, "/pic".len())
    } else {
        return None;
    };
    let body = trimmed[prefix_len..].trim();
    if body.is_empty() {
        // empty body → usage error, surface as Some with empty prompt so the
        // caller can render a usage hint
        return Some((model, 1280, 720, String::new()));
    }
    // Optional leading dimensions: WxH or W×H
    let mut width = match model {
        Model::Turbo => 1280,
        Model::Hq => 1024,
    };
    let mut height = match model {
        Model::Turbo => 720,
        Model::Hq => 1024,
    };
    static DIM_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"^(\d{2,5})\s*[xX×]\s*(\d{2,5})\s+(.+)$").unwrap()
    });
    let prompt = if let Some(caps) = DIM_RE.captures(body) {
        let w: u32 = caps[1].parse().unwrap_or(width);
        let h: u32 = caps[2].parse().unwrap_or(height);
        if (64..=2048).contains(&w) && (64..=2048).contains(&h) {
            width = w;
            height = h;
            caps[3].trim().to_string()
        } else {
            body.to_string()
        }
    } else {
        body.to_string()
    };
    Some((model, width, height, prompt))
}

#[cfg(test)]
mod parse_slash_tests {
    use super::{parse_slash, Model};

    #[test]
    fn unicode_body_does_not_panic() {
        // "İ" (U+0130) lowercases to 2 chars / different byte length —
        // this used to underflow `trimmed.len() - rest.len()` and panic,
        // killing the Telegram poll loop. Must now parse cleanly.
        for t in ["/pic İstanbul skyline", "/picHQ İİİ", "/pic 北京", "/pic café"] {
            let r = parse_slash(t);
            assert!(r.is_some(), "{t:?} should parse");
        }
    }

    #[test]
    fn preserves_original_case_body_and_dims() {
        let (m, w, h, p) = parse_slash("/picHQ 1024x768 A Red Barn").unwrap();
        assert!(matches!(m, Model::Hq));
        assert_eq!((w, h), (1024, 768));
        assert_eq!(p, "A Red Barn");
        let (_, _, _, p2) = parse_slash("/pic Hello World").unwrap();
        assert_eq!(p2, "Hello World");
    }

    #[test]
    fn empty_body_returns_usage_sentinel() {
        let (_, _, _, p) = parse_slash("/pic").unwrap();
        assert!(p.is_empty());
        assert!(parse_slash("/notacommand").is_none());
    }
}

#[cfg(test)]
mod edit_tests {
    use super::*;
    use crate::db::Attachment;

    fn picture(mime: &str, b64: &str) -> Attachment {
        Attachment {
            kind: "image".into(),
            mime: Some(mime.into()),
            name: Some("picture".into()),
            data_url: Some(format!("data:{mime};base64,{b64}")),
        }
    }

    #[test]
    fn first_picture_decodes_what_the_server_can_open() {
        let (bytes, ext) = first_picture(&[picture("image/jpeg", "QUJD")]).expect("jpeg");
        assert_eq!((bytes.as_slice(), ext), (b"ABC".as_slice(), "jpg"));
        assert_eq!(first_picture(&[picture("image/png", "QUJD")]).unwrap().1, "png");
        assert_eq!(first_picture(&[picture("IMAGE/WEBP", "QUJD")]).unwrap().1, "webp");
    }

    #[test]
    fn first_picture_skips_what_it_cannot_use() {
        // A format the server cannot open, a non-picture, broken base64, a
        // picture whose bytes were dropped from history: none is usable.
        let mut doc = picture("application/pdf", "QUJD");
        doc.kind = "file".into();
        let mut dropped = picture("image/png", "QUJD");
        dropped.data_url = None;
        let unusable = [picture("image/heic", "QUJD"), doc, picture("image/png", "***"), dropped];
        assert!(first_picture(&unusable).is_none());
        // …and the first usable one after them is found.
        let mut all = unusable.to_vec();
        all.push(picture("image/png", "QUJD"));
        assert_eq!(first_picture(&all).unwrap().1, "png");
    }

    #[test]
    fn edit_workflow_names_the_picture_and_never_keeps_its_full_size() {
        for model in [Model::Turbo, Model::Hq] {
            let res = edit_resolution(model);
            assert!((512..=2048).contains(&res), "resolution 0 would keep a phone photo's full size");
            let wf = build_edit_workflow("add a paper hat", "kinai_edit_x.jpg", res, 7);
            assert_eq!(wf["1"]["inputs"]["image"], "kinai_edit_x.jpg");
            assert_eq!(wf["6"]["inputs"]["prompt"], "add a paper hat");
            assert_eq!(wf["6"]["inputs"]["resolution"], res);
            assert_eq!(wf["6"]["inputs"]["images.image_1"], json!(["1", 0]));
            assert_eq!(wf["8"]["inputs"]["noise_seed"], 7);
        }
    }

    #[test]
    fn a_very_long_instruction_is_cut() {
        let wf = build_edit_workflow(&"é".repeat(9000), "p.png", 1024, 1);
        assert_eq!(wf["6"]["inputs"]["prompt"].as_str().unwrap().chars().count(), 4000);
    }

    #[test]
    fn the_blank_is_a_real_png() {
        assert!(BLANK_PNG.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
        assert!(BLANK_PNG.ends_with(&[b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]));
    }
}
