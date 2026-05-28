//! Backend auto-detection.
//!
//! Two phases for the LAN scan (no live-host gate — too easy to miss boxes
//! that only run an LLM and nothing else, e.g. SSH/HTTP disabled):
//!   1. **LLM port scan** — TCP-probe every (host, llm_port) pair in the
//!      /24 with bounded concurrency. ~60 ports × 254 hosts ≈ 15k probes.
//!      Dead hosts return RST/timeout fast; live ones come back quickly.
//!   2. **HTTP(S) probe** — for each live host:port, try both `http://`
//!      and `https://` against the provider's API endpoint. Accept
//!      self-signed certs (LAN trust; JWT on the WS layer is what
//!      authenticates).
//!
//! Worst-case ~25–35s on a /24 — the price of not assuming any "discovery"
//! port is open on the target. The port set is generated from dense
//! CONTIGUOUS RANGES around each known LLM base port (see `llm_ports`),
//! not a sparse hand-picked list — because the single most common real
//! miss is "the user started a second/third model instance on the next
//! port over" (vLLM 8000→8001→8002…, llama.cpp 8080→8081→8082…, Ollama
//! 11434→11435…). The larger list is offset by higher phase-1 concurrency
//! so the wall-clock cost is unchanged.

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

/// LLM-likely TCP port + the best-guess provider label. We probe both
/// http and https on each one. The provider/label is only a UI hint —
/// the HTTP probe confirms the real shape via `/v1/models` or Ollama's
/// `/api/tags` regardless of which guess the port implied.
#[derive(Clone, Copy)]
struct LlmPort {
    port: u16,
    provider: &'static str,
    label: &'static str,
}

/// Inclusive port range for a backend family. Generates one `LlmPort`
/// per port so "the next instance over" (8000→8001→8002…) is covered
/// without enumerating every port by hand.
#[derive(Clone, Copy)]
struct PortRange {
    start: u16,
    end: u16,
    provider: &'static str,
    label: &'static str,
}

/// Contiguous ranges around each known LLM base port. The key design
/// choice: people run multiple llama.cpp / vLLM / Ollama instances on
/// incrementing ports, so we sweep a band around each base rather than
/// a few sparse points. Keep ranges modest — every extra port multiplies
/// the probe count by the host count.
const LLM_PORT_RANGES: &[PortRange] = &[
    PortRange { start: 11434, end: 11439, provider: "ollama",        label: "Ollama" },
    PortRange { start: 1234,  end: 1237,  provider: "lmstudio",      label: "LM Studio" },
    PortRange { start: 8000,  end: 8010,  provider: "vllm",          label: "vLLM" },
    PortRange { start: 8080,  end: 8095,  provider: "llamacpp",      label: "llama.cpp" },
    PortRange { start: 5000,  end: 5005,  provider: "openai-compat", label: "OpenAI-compat" },
    PortRange { start: 7860,  end: 7861,  provider: "openai-compat", label: "Gradio" },
    PortRange { start: 8888,  end: 8889,  provider: "openwebui",     label: "Open WebUI" },
    PortRange { start: 9090,  end: 9091,  provider: "openai-compat", label: "OpenAI-compat" },
    PortRange { start: 30000, end: 30001, provider: "openai-compat", label: "SGLang" },
];

/// One-off ports that don't belong to a contiguous band.
const LLM_PORT_SINGLES: &[LlmPort] = &[
    LlmPort { port: 80,   provider: "openai-compat", label: "OpenAI-compat (proxy)" },
    LlmPort { port: 443,  provider: "openai-compat", label: "OpenAI-compat (TLS)" },
    LlmPort { port: 8443, provider: "openai-compat", label: "OpenAI-compat (TLS)" },
    LlmPort { port: 1337, provider: "openai-compat", label: "Jan" },
    LlmPort { port: 3000, provider: "openai-compat", label: "OpenAI-compat" },
    LlmPort { port: 4000, provider: "openai-compat", label: "LiteLLM" },
    LlmPort { port: 8123, provider: "openai-compat", label: "OpenAI-compat" },
    LlmPort { port: 9000, provider: "openai-compat", label: "OpenAI-compat" },
    LlmPort { port: 9292, provider: "llamacpp",      label: "llama.cpp" },
    LlmPort { port: 9999, provider: "openai-compat", label: "OpenAI-compat" },
    LlmPort { port: 2242, provider: "openai-compat", label: "Aphrodite" },
];

/// Materialize the full port set from ranges + singles, deduped and
/// sorted. Built once per scan (cheap — ~60 entries).
fn llm_ports() -> Vec<LlmPort> {
    let mut out: Vec<LlmPort> = Vec::new();
    for r in LLM_PORT_RANGES {
        for port in r.start..=r.end {
            out.push(LlmPort {
                port,
                provider: r.provider,
                label: r.label,
            });
        }
    }
    out.extend_from_slice(LLM_PORT_SINGLES);
    out.sort_by_key(|p| p.port);
    out.dedup_by_key(|p| p.port);
    out
}

/// Best-effort raise of this process's open-file soft limit, called once
/// at startup. macOS launches GUI apps with `RLIMIT_NOFILE` soft = 256,
/// which is tight: the app already holds ~100+ fds (webview, DB pool,
/// WebSocket clients) before a LAN scan opens its concurrent probe
/// sockets. Raising the soft limit toward the hard cap (clamped to a
/// sane 8192) prevents `EMFILE` both for scanning AND for a busy host
/// serving many family clients. Failure is non-fatal — `scan_concurrency`
/// still sizes itself to whatever limit is actually in effect.
#[cfg(unix)]
pub fn raise_fd_limit_best_effort() {
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        // Target 8192, but never exceed the hard cap (which may be
        // RLIM_INFINITY — then 8192 is our self-imposed ceiling).
        let target = if lim.rlim_max == libc::RLIM_INFINITY {
            8192
        } else {
            lim.rlim_max.min(8192)
        };
        if lim.rlim_cur >= target {
            return; // already ample
        }
        let new = libc::rlimit {
            rlim_cur: target,
            rlim_max: lim.rlim_max,
        };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &new) == 0 {
            tracing::info!("raised RLIMIT_NOFILE soft limit {} -> {}", lim.rlim_cur, target);
        } else {
            tracing::warn!("could not raise RLIMIT_NOFILE (soft={})", lim.rlim_cur);
        }
    }
}

#[cfg(not(unix))]
pub fn raise_fd_limit_best_effort() {
    // Windows doesn't impose a comparable per-process socket fd cap on the
    // tokio/IOCP path, so there's nothing to raise.
}

/// Phase-1 fan-out, sized to the fd budget actually in effect so the scan
/// never exhausts file descriptors. Each in-flight probe holds one socket;
/// we reserve headroom for the fds the app already has open (webview, DB,
/// WS clients ≈ 100–150) before committing the rest to probes. On a raised
/// limit (see `raise_fd_limit_best_effort`) this yields a fast 256-wide
/// scan; on the bare macOS 256-fd default it backs off to ~96 so the scan
/// still completes instead of failing with EMFILE.
fn scan_concurrency() -> usize {
    #[cfg(unix)]
    {
        unsafe {
            let mut lim = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 {
                let soft = lim.rlim_cur as usize;
                // Reserve 160 fds for the rest of the app, cap the fan-out
                // at 256 (more sockets churn buys little), floor at 16.
                return soft.saturating_sub(160).clamp(16, 256);
            }
        }
        96
    }
    #[cfg(not(unix))]
    {
        256
    }
}

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
    let ports = llm_ports();
    let mut probes: Vec<(String, LlmPort)> = Vec::with_capacity(hosts.len() * ports.len());
    for ip in &hosts {
        for kind in &ports {
            probes.push((ip.clone(), *kind));
        }
    }
    tracing::info!(
        "phase 1: {} TCP probes ({} hosts × {} ports)",
        probes.len(),
        hosts.len(),
        ports.len()
    );

    // Fan-out sized to the live fd budget (see `scan_concurrency`). TCP
    // connects are cheap, but each in-flight probe holds a socket — too
    // many at once exhausts the process's file descriptors and the whole
    // scan fails with EMFILE (which is exactly how a 256-wide fan-out
    // silently found *nothing* under macOS's 256-fd GUI default). Dead
    // hosts RST or hit the 400ms timeout fast.
    let concurrency = scan_concurrency();
    tracing::info!("phase 1: scan concurrency = {concurrency}");
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
        .buffer_unordered(concurrency)
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

#[cfg(test)]
mod port_coverage_tests {
    use super::*;
    use std::collections::BTreeSet;

    fn port_set() -> BTreeSet<u16> {
        llm_ports().iter().map(|p| p.port).collect()
    }

    #[test]
    fn covers_well_known_default_ports() {
        let ports = port_set();
        for (name, port) in [
            ("Ollama", 11434u16),
            ("LM Studio", 1234),
            ("vLLM", 8000),
            ("llama.cpp", 8080),
            ("Open WebUI", 8888),
            ("SGLang", 30000),
            ("Jan", 1337),
            ("LiteLLM", 4000),
            ("Text-gen-webui/Kobold", 5000),
        ] {
            assert!(
                ports.contains(&port),
                "{name} default port {port} must be in the scan set"
            );
        }
    }

    #[test]
    fn covers_adjacent_instance_ports() {
        // The whole point of the range-based fix: a second/third model
        // started on the next port over must be discoverable. These all
        // fell in GAPS of the old sparse list.
        let ports = port_set();
        for p in [8002u16, 8083, 8085, 8089, 8091, 1236, 11437, 5002] {
            assert!(
                ports.contains(&p),
                "adjacent-instance port {p} must be covered by a range"
            );
        }
    }

    #[test]
    fn scan_concurrency_stays_within_safe_bounds() {
        // Whatever the host's fd limit, the fan-out must never exceed the
        // 256 cap (the value that caused EMFILE) and never drop to zero.
        let c = scan_concurrency();
        assert!(
            (16..=256).contains(&c),
            "scan concurrency {c} must be in [16, 256]"
        );
    }

    #[test]
    fn ports_are_unique_and_reasonable() {
        let list = llm_ports();
        let set = port_set();
        assert_eq!(list.len(), set.len(), "no duplicate ports");
        // Sanity: stay well under a count that would blow up scan time.
        assert!(
            list.len() < 90,
            "port set grew to {} — keep it lean (probes = ports × 254 hosts)",
            list.len()
        );
        assert!(list.iter().all(|p| p.port > 0));
    }
}
