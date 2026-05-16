//! ComfyUI integration — powers the `/pic` and `/picHQ` chat commands.
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
    let client_id = Uuid::new_v4().to_string();

    let t_start = Instant::now();
    let submit_resp = client
        .post(format!("{base}/prompt"))
        .json(&json!({ "prompt": workflow, "client_id": client_id }))
        .send()
        .await
        .context("submit to ComfyUI")?;

    if !submit_resp.status().is_success() {
        let status = submit_resp.status();
        let body = submit_resp.text().await.unwrap_or_default();
        bail!("ComfyUI rejected submission ({status}): {body}");
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
    let (model, rest) = if let Some(r) = lower.strip_prefix("/pichq") {
        (Model::Hq, r.to_string())
    } else if let Some(r) = lower.strip_prefix("/pic") {
        (Model::Turbo, r.to_string())
    } else {
        return None;
    };
    // The rest captured above is the lowercased remainder — fine for
    // dimension parsing, but we want the ORIGINAL-case prompt body.
    // Recompute body length so we can slice the original string.
    let cut = trimmed.len() - rest.len();
    let body = trimmed[cut..].trim();
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
    let dim_re = regex::Regex::new(r"^(\d{2,5})\s*[xX×]\s*(\d{2,5})\s+(.+)$").unwrap();
    let prompt = if let Some(caps) = dim_re.captures(body) {
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
