//! Backend auto-detection.
//!
//! Two phases for the LAN scan (no live-host gate — too easy to miss boxes
//! that only run an LLM and nothing else, e.g. SSH/HTTP disabled):
//!   1. **LLM port scan** — TCP-probe every (host, llm_port) pair in the
//!      /24 with bounded concurrency. 26 ports × 254 hosts = ~6.6k probes.
//!      Dead hosts return RST/timeout fast; live ones come back quickly.
//!   2. **HTTP(S) probe** — for each live host:port, try both `http://`
//!      and `https://` against the provider's API endpoint. Accept
//!      self-signed certs (LAN trust; JWT on the WS layer is what
//!      authenticates).
//!
//! Worst-case ~25–35s on a /24 — the price of not assuming any "discovery"
//! port is open on the target.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::Result;
use futures_util::stream::StreamExt;
use serde::{Deserialize, Serialize};

use crate::config::LlmSettings;

#[derive(Debug, Clone, Serialize)]
pub struct DetectedBackend {
    pub provider: String,
    pub base_url: String,
    pub label: String,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelCaps {
    pub context_length: Option<u32>,
}

/// LLM-likely TCP ports + the best-guess provider label. We probe both
/// http and https on each one. Sorted roughly by popularity.
#[derive(Clone, Copy)]
struct LlmPort {
    port: u16,
    provider: &'static str,
    label: &'static str,
}

const LLM_PORTS: &[LlmPort] = &[
    // Well-known defaults
    LlmPort { port: 11434, provider: "ollama",    label: "Ollama" },
    LlmPort { port: 11435, provider: "ollama",    label: "Ollama" },
    LlmPort { port: 11436, provider: "ollama",    label: "Ollama" },
    LlmPort { port: 1234,  provider: "lmstudio",  label: "LM Studio" },
    LlmPort { port: 1235,  provider: "lmstudio",  label: "LM Studio" },
    LlmPort { port: 8000,  provider: "vllm",      label: "vLLM" },
    LlmPort { port: 8001,  provider: "vllm",      label: "vLLM" },
    LlmPort { port: 8080,  provider: "llamacpp",  label: "llama.cpp" },
    LlmPort { port: 8081,  provider: "llamacpp",  label: "llama.cpp" },
    LlmPort { port: 8082,  provider: "llamacpp",  label: "llama.cpp" },
    LlmPort { port: 8088,  provider: "llamacpp",  label: "llama.cpp" },
    LlmPort { port: 8090,  provider: "llamacpp",  label: "llama.cpp" },
    LlmPort { port: 8888,  provider: "openwebui", label: "Open WebUI" },
    LlmPort { port: 8889,  provider: "openai-compat", label: "OpenAI-compat" },
    // Reverse-proxy / TLS variants
    LlmPort { port: 443,   provider: "openai-compat", label: "OpenAI-compat (TLS)" },
    LlmPort { port: 8443,  provider: "openai-compat", label: "OpenAI-compat (TLS)" },
    // Dev-server / Gradio / custom
    LlmPort { port: 3000,  provider: "openai-compat", label: "OpenAI-compat" },
    LlmPort { port: 4000,  provider: "openai-compat", label: "OpenAI-compat" },
    LlmPort { port: 5000,  provider: "openai-compat", label: "OpenAI-compat" },
    LlmPort { port: 5001,  provider: "openai-compat", label: "OpenAI-compat" },
    LlmPort { port: 7860,  provider: "openai-compat", label: "Gradio" },
    LlmPort { port: 7861,  provider: "openai-compat", label: "Gradio" },
    LlmPort { port: 9000,  provider: "openai-compat", label: "OpenAI-compat" },
    LlmPort { port: 9090,  provider: "openai-compat", label: "OpenAI-compat" },
    LlmPort { port: 9091,  provider: "openai-compat", label: "OpenAI-compat" },
    LlmPort { port: 9292,  provider: "llamacpp",  label: "llama.cpp" },
    LlmPort { port: 9999,  provider: "openai-compat", label: "OpenAI-compat" },
];

/// Detect backends on localhost only (fast).
pub async fn detect_all() -> Vec<DetectedBackend> {
    // No host discovery — just scan the LLM port list against 127.0.0.1.
    scan_targets(vec!["127.0.0.1".to_string()]).await
}

/// Scan every private-IPv4 /24 the host is connected to.
pub async fn scan_local_network() -> Vec<DetectedBackend> {
    use std::net::IpAddr;

    let mut subnets: BTreeSet<[u8; 3]> = BTreeSet::new();
    if let Ok(ifaces) = local_ip_address::list_afinet_netifas() {
        for (_name, ip) in ifaces {
            if let IpAddr::V4(v4) = ip {
                let o = v4.octets();
                if v4.is_loopback() || v4.is_link_local() {
                    continue;
                }
                // Skip CGNAT / Tailscale (100.64.0.0/10).
                if o[0] == 100 && (o[1] & 0xC0) == 0x40 {
                    continue;
                }
                let private = o[0] == 10
                    || (o[0] == 172 && (16..=31).contains(&o[1]))
                    || (o[0] == 192 && o[1] == 168);
                if !private {
                    continue;
                }
                subnets.insert([o[0], o[1], o[2]]);
            }
        }
    }
    if subnets.is_empty() {
        if let Ok(IpAddr::V4(v4)) = local_ip_address::local_ip() {
            let o = v4.octets();
            subnets.insert([o[0], o[1], o[2]]);
        }
    }

    let mut hosts: Vec<String> = Vec::new();
    for s in &subnets {
        tracing::info!("scanning subnet {}.{}.{}.0/24", s[0], s[1], s[2]);
        for h in 1u8..=254 {
            hosts.push(format!("{}.{}.{}.{}", s[0], s[1], s[2], h));
        }
    }

    scan_targets(hosts).await
}

/// Phase 1 + 2: TCP-probe every LLM port on every target host, then HTTP(S)
/// probe the ones that answer. No liveness gate — boxes that only run an
/// LLM (e.g. MINISFORUM dedicated to llama.cpp on :8081) have nothing else
/// to phone home on.
async fn scan_targets(hosts: Vec<String>) -> Vec<DetectedBackend> {
    if hosts.is_empty() {
        return Vec::new();
    }

    // Phase 1: TCP precheck across every (host, LLM port) pair.
    let mut probes: Vec<(String, LlmPort)> = Vec::with_capacity(hosts.len() * LLM_PORTS.len());
    for ip in &hosts {
        for kind in LLM_PORTS {
            probes.push((ip.clone(), *kind));
        }
    }
    tracing::info!(
        "phase 1: {} TCP probes ({} hosts × {} ports)",
        probes.len(),
        hosts.len(),
        LLM_PORTS.len()
    );

    let candidates: Vec<(String, LlmPort)> = futures_util::stream::iter(probes)
        .map(|(ip, kind)| async move {
            let addr = format!("{}:{}", ip, kind.port);
            let ok = tokio::time::timeout(
                Duration::from_millis(400),
                tokio::net::TcpStream::connect(&addr),
            )
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false);
            if ok { Some((ip, kind)) } else { None }
        })
        .buffer_unordered(64)
        .filter_map(|x| async move { x })
        .collect()
        .await;

    tracing::info!("phase 1: {} TCP candidates", candidates.len());
    for (ip, kind) in &candidates {
        tracing::info!("  live {}:{} ({})", ip, kind.port, kind.provider);
    }
    if candidates.is_empty() {
        return Vec::new();
    }

    // Phase 3: HTTP+HTTPS probe. Accept self-signed certs (local-network
    // trust; JWT on the WebSocket layer is what actually authenticates).
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(2500))
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let backends: Vec<DetectedBackend> = futures_util::stream::iter(candidates)
        .map(|(ip, kind)| {
            let client = client.clone();
            async move {
                for scheme in ["http", "https"] {
                    let base_url = format!("{}://{}:{}", scheme, ip, kind.port);

                    // Ollama-style first when the port suggests it.
                    if kind.provider == "ollama" {
                        if let Ok(models) = list_via_ollama(&client, &base_url).await {
                            tracing::info!("matched ollama at {}", base_url);
                            return Some(DetectedBackend {
                                provider: kind.provider.into(),
                                base_url,
                                label: kind.label.into(),
                                models,
                            });
                        }
                    }
                    // OpenAI-compatible — works for vLLM, llama.cpp,
                    // llama-swap, LM Studio, and any drop-in clone.
                    if let Ok(models) = list_via_openai(&client, &base_url, None).await {
                        tracing::info!("matched openai-compat at {}", base_url);
                        return Some(DetectedBackend {
                            provider: kind.provider.into(),
                            base_url,
                            label: kind.label.into(),
                            models,
                        });
                    }
                    // Last resort: try Ollama on non-Ollama-port hosts.
                    if kind.provider != "ollama" {
                        if let Ok(models) = list_via_ollama(&client, &base_url).await {
                            tracing::info!("matched ollama-on-{} at {}", kind.port, base_url);
                            return Some(DetectedBackend {
                                provider: "ollama".into(),
                                base_url,
                                label: "Ollama".into(),
                                models,
                            });
                        }
                    }
                }
                None
            }
        })
        .buffer_unordered(16)
        .filter_map(|x| async move { x })
        .collect()
        .await;

    tracing::info!("phase 2: {} backends confirmed", backends.len());

    // Dedup by base_url in case both schemes happened to match.
    let mut seen = BTreeSet::new();
    backends
        .into_iter()
        .filter(|b| seen.insert(b.base_url.clone()))
        .collect()
}

pub async fn list_models(settings: &LlmSettings) -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(2500))
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    match settings.provider.as_str() {
        "ollama" => list_via_ollama(&client, &settings.base_url).await,
        _ => list_via_openai(&client, &settings.base_url, settings.api_key.as_deref()).await,
    }
}

pub async fn query_model_caps(
    provider: &str,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
) -> Result<ModelCaps> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    match provider {
        "ollama" => caps_via_ollama(&client, base_url, model).await,
        "llamacpp" => caps_via_llamacpp(&client, base_url, api_key, model).await,
        _ => caps_via_openai(&client, base_url, api_key, model).await,
    }
}

async fn caps_via_openai(
    client: &reqwest::Client,
    base: &str,
    api_key: Option<&str>,
    model: &str,
) -> Result<ModelCaps> {
    let url = format!("{}/v1/models", base.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(k) = api_key {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("{} responded {}", url, resp.status());
    }
    let value: serde_json::Value = resp.json().await?;
    let entry = value
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|arr| arr.iter().find(|m| m.get("id").and_then(|i| i.as_str()) == Some(model)));
    let ctx = entry
        .and_then(|e| e.get("max_model_len"))
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    Ok(ModelCaps { context_length: ctx })
}

async fn caps_via_ollama(
    client: &reqwest::Client,
    base: &str,
    model: &str,
) -> Result<ModelCaps> {
    let url = format!("{}/api/show", base.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "name": model }))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("ollama /api/show responded {}", resp.status());
    }
    let value: serde_json::Value = resp.json().await?;
    let ctx = value
        .get("model_info")
        .and_then(|mi| mi.as_object())
        .and_then(|obj| {
            obj.iter()
                .find(|(k, _)| k.ends_with(".context_length") || *k == "general.context_length")
                .and_then(|(_, v)| v.as_u64())
        })
        .map(|n| n as u32);
    Ok(ModelCaps { context_length: ctx })
}

/// Probe a llama.cpp `server` instance.
///
/// llama.cpp's `/v1/models` does NOT include `max_model_len` (it only
/// returns `id`, `object`, `created`, `owned_by`). The actual runtime
/// context window — what was passed via `-c` on startup — lives in
/// `/props.default_generation_settings.n_ctx`. We hit that first.
///
/// Note: the model file's GGUF metadata can advertise a much larger
/// architecturally-supported window (e.g. Qwen3 is trained to 128k),
/// but the SERVER will only accept up to `n_ctx`. So `/props` is the
/// authoritative number — it's the ceiling that won't cause truncation.
///
/// `/v1/models` is a fallback in case the user pointed a non-standard
/// build (or a non-llama.cpp server they mislabeled) at this path —
/// some forks expose `max_model_len` there.
async fn caps_via_llamacpp(
    client: &reqwest::Client,
    base: &str,
    api_key: Option<&str>,
    model: &str,
) -> Result<ModelCaps> {
    let url = format!("{}/props", base.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(k) = api_key {
        req = req.bearer_auth(k);
    }
    if let Ok(resp) = req.send().await {
        if resp.status().is_success() {
            if let Ok(value) = resp.json::<serde_json::Value>().await {
                let ctx = value
                    .get("default_generation_settings")
                    .and_then(|s| s.get("n_ctx"))
                    .and_then(|n| n.as_u64())
                    .map(|n| n as u32);
                if ctx.is_some() {
                    return Ok(ModelCaps { context_length: ctx });
                }
            }
        }
    }
    // /props didn't help — try the OpenAI-compat path as a last resort.
    caps_via_openai(client, base, api_key, model).await
}

async fn list_via_ollama(client: &reqwest::Client, base: &str) -> Result<Vec<String>> {
    let url = format!("{}/api/tags", base.trim_end_matches('/'));
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("ollama responded {}", resp.status());
    }
    #[derive(Deserialize)]
    struct R { models: Vec<M> }
    #[derive(Deserialize)]
    struct M { name: String }
    let parsed: R = resp.json().await?;
    Ok(parsed.models.into_iter().map(|m| m.name).collect())
}

async fn list_via_openai(
    client: &reqwest::Client,
    base: &str,
    api_key: Option<&str>,
) -> Result<Vec<String>> {
    let url = format!("{}/v1/models", base.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(k) = api_key {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("{} responded {}", url, resp.status());
    }
    #[derive(Deserialize)]
    struct R { data: Vec<M> }
    #[derive(Deserialize)]
    struct M { id: String }
    let parsed: R = resp.json().await?;
    Ok(parsed.data.into_iter().map(|m| m.id).collect())
}
