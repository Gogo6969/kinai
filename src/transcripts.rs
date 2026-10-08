//! Built-in video transcripts: KinAI's own yt-dlp, run inside a macOS
//! sandbox on the host Mac.
//!
//! The household transcript service (`services/transcriptd.py`) needs a
//! Linux box and Python, so a family that downloads KinAI from GitHub never
//! had video reading at all. This does the same job on the host: one
//! Settings button downloads the official standalone yt-dlp (checked against
//! the checksum file of the same release), and every run happens under
//! `sandbox-exec` with a profile that cannot read `~/.kinai` (the family's
//! conversations, keys and config), `~/.ssh` or the keychains, and cannot
//! write login items or the app bundle. Third-party code that is handed URLs
//! which ultimately come from chat still never gets near the family's data.
//! If the sandbox is missing, nothing runs: this fails closed.
//!
//! A configured `transcript_url` (a household's own service) wins; this is
//! what runs when that field is empty. Installed means on: there is no
//! separate switch, so a Settings save can never put the two out of step.
//!
//! The caption logic mirrors `services/transcriptd.py`, including the trap
//! written down there: on an auto-captioned video plain `en` is YouTube's
//! English-to-English translation of `en-orig`, and YouTube answers it with
//! 429 every time. So `en-orig` is asked for first, `--ignore-errors` keeps
//! one refused track from losing the others, and a caption file on disk
//! wins over whatever stderr says.

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

const REPO: &str = "https://github.com/yt-dlp/yt-dlp";
/// The universal (arm64 + x86_64) standalone build: no Python needed.
const ASSET: &str = "yt-dlp_macos";
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
/// Same wall clock as the service.
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
/// The standalone build unpacks itself on every start; leave room for
/// `--version` and `-U` on a slow disk.
const QUICK_TIMEOUT: Duration = Duration::from_secs(90);
/// Same cap as the service: a three-hour podcast is ~150k characters.
const MAX_TEXT: usize = 200_000;

/// One yt-dlp at a time. YouTube rate-limits per IP and a household
/// shares one; install, update and remove take it too, so none of them
/// can swap the binary out from under a running fetch.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

static VTT_TAG: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new(r"<[^>]+>").expect("static regex"));

/// Outside `~/.kinai` on purpose: the sandbox denies that whole tree, so a
/// binary kept there could not even be started.
pub fn tool_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("KinAI")
        .join("yt-dlp")
}

fn binary() -> PathBuf {
    tool_dir().join(ASSET)
}

fn version_file() -> PathBuf {
    tool_dir().join("version.txt")
}

/// Finished transcripts. Written by the host after parsing, never by the
/// sandboxed process, so it can live with the rest of KinAI's data.
fn cache_dir() -> PathBuf {
    crate::config::AppConfig::config_dir().join("transcripts")
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// This machine can run them: macOS with its sandbox.
    pub supported: bool,
    pub installed: bool,
    /// The yt-dlp version recorded at install or the last update.
    pub version: Option<String>,
}

pub fn supported() -> bool {
    cfg!(target_os = "macos") && Path::new(SANDBOX_EXEC).is_file()
}

pub fn installed() -> bool {
    binary().is_file()
}

/// Whether `video_transcript` can be served without a household service.
pub fn available() -> bool {
    supported() && installed()
}

pub fn status() -> Status {
    let version = std::fs::read_to_string(version_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Status {
        supported: supported(),
        installed: installed(),
        version,
    }
}

/// Default-allow, then fence off what a compromised yt-dlp must never
/// reach. The network stays open (it has to talk to YouTube), and so do the
/// temp dir it unpacks into and its own folder (it updates itself there).
fn sandbox_profile(home: &Path, app_bundle: Option<&Path>) -> String {
    fn quoted(p: &Path) -> String {
        p.display().to_string().replace('\\', "\\\\").replace('"', "\\\"")
    }
    // The sandbox matches resolved paths, so a rule on a path that runs
    // through a symlink (macOS's /var -> /private/var, or a moved home)
    // would silently match nothing. Fence both spellings when they differ.
    fn spellings(p: &Path) -> Vec<PathBuf> {
        let resolved = std::fs::canonicalize(p).ok().or_else(|| {
            let parent = std::fs::canonicalize(p.parent()?).ok()?;
            Some(parent.join(p.file_name()?))
        });
        match resolved {
            Some(r) if r != p => vec![p.to_path_buf(), r],
            _ => vec![p.to_path_buf()],
        }
    }
    let mut rule = String::from("(version 1)(allow default)");
    let mut deny = |ops: &str, target: &Path| {
        for s in spellings(target) {
            rule.push_str(&format!("(deny {ops} (subpath \"{}\"))", quoted(&s)));
        }
    };
    for d in [".kinai", ".ssh", "Library/Keychains"] {
        deny("file-read* file-write*", &home.join(d));
    }
    deny("file-write*", &home.join("Library/LaunchAgents"));
    if let Some(bundle) = app_bundle {
        deny("file-write*", bundle);
    }
    rule
}

/// The running app's `…/KinAI.app`, so the sandbox can refuse writes to it.
fn app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|a| a.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf)
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run the installed yt-dlp inside the sandbox. Never call this unless
/// `supported()`: there is no unsandboxed fallback.
async fn run(args: &[String], timeout: Duration) -> Result<Run> {
    if !supported() {
        bail!("macOS's sandbox is not available, so yt-dlp is not run");
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let mut cmd = tokio::process::Command::new(SANDBOX_EXEC);
    cmd.arg("-p")
        .arg(sandbox_profile(&home, app_bundle().as_deref()))
        .arg(binary())
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // The standalone build is a bootloader plus a child; a group of its
    // own lets a timeout take both, not just the one that was spawned.
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd.spawn()?;
    let pid = child.id();
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(out) => {
            let out = out?;
            Ok(Run {
                code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = pid {
                // SAFETY: plain signal to the process group created above.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
            #[cfg(not(unix))]
            let _ = pid;
            bail!("yt-dlp did not finish within {}s", timeout.as_secs())
        }
    }
}

/// The checksum for `name` in a `SHA2-256SUMS` file, or None.
fn expected_sha256(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let hash = it.next()?;
        let file = it.next()?.trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

/// The tag `releases/latest` redirects to, read from the redirect itself.
async fn latest_tag(client: &reqwest::Client) -> Result<String> {
    let resp = client.get(format!("{REPO}/releases/latest")).send().await?;
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow!("GitHub did not say which yt-dlp release is the latest"))?;
    let tag = location.rsplit('/').next().unwrap_or_default();
    if tag.is_empty()
        || !tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        bail!("unexpected yt-dlp release tag {tag:?}");
    }
    Ok(tag.to_string())
}

/// Download, verify and install the latest yt-dlp, then prove it starts
/// inside the sandbox. Also how a broken install is repaired.
pub async fn install() -> Result<Status> {
    if !supported() {
        bail!("built-in video transcripts need a Mac host with macOS's sandbox");
    }
    let _one = ONE_AT_A_TIME.lock().await;
    let dir = tool_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let ua = concat!("KinAI/", env!("CARGO_PKG_VERSION"));
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .user_agent(ua)
        .build()?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .user_agent(ua)
        .build()?;

    // Resolve "latest" once and take the checksums and the binary from
    // that same release: two separate "latest" requests could straddle a
    // new release and fail the check for no reason.
    let tag = latest_tag(&no_redirect).await?;
    let base = format!("{REPO}/releases/download/{tag}");
    let sums = client
        .get(format!("{base}/SHA2-256SUMS"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let want = expected_sha256(&sums, ASSET)
        .ok_or_else(|| anyhow!("yt-dlp {tag} publishes no checksum for {ASSET}"))?;
    let bytes = client
        .get(format!("{base}/{ASSET}"))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let got = hex::encode(sha2::Sha256::digest(&bytes));
    if got != want {
        bail!("the downloaded yt-dlp did not match its published checksum, so nothing was installed");
    }

    let part = dir.join(format!("{ASSET}.part"));
    tokio::fs::write(&part, &bytes).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755)).await?;
    }
    tokio::fs::rename(&part, binary()).await?;

    // A download is not an install: prove it runs where it will run.
    match record_version().await {
        Ok(v) => tracing::info!("built-in transcripts: installed yt-dlp {v}"),
        Err(e) => {
            let _ = tokio::fs::remove_file(binary()).await;
            bail!("yt-dlp downloaded but would not start in the sandbox: {e}");
        }
    }
    Ok(status())
}

/// Ask the sandboxed binary its version and remember it for Settings.
async fn record_version() -> Result<String> {
    let r = run(&["--version".into()], QUICK_TIMEOUT).await?;
    let v = r.stdout.trim().to_string();
    if r.code != Some(0) || v.is_empty() || !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
        bail!("unexpected answer to --version (exit {:?})", r.code);
    }
    tokio::fs::write(version_file(), &v).await?;
    Ok(v)
}

pub async fn remove() -> Result<Status> {
    let _one = ONE_AT_A_TIME.lock().await;
    let dir = tool_dir();
    if dir.exists() {
        tokio::fs::remove_dir_all(&dir).await?;
    }
    Ok(status())
}

/// yt-dlp is in a permanent race with YouTube: new videos start failing
/// within weeks on a stale build. Its own updater verifies the release's
/// checksums and replaces the binary in place; its folder is the one
/// place outside the temp dir the sandbox lets it write.
pub async fn self_update() -> Result<String> {
    let _one = ONE_AT_A_TIME.lock().await;
    let r = run(&["-U".into()], QUICK_TIMEOUT).await?;
    let said = r
        .stdout
        .lines()
        .chain(r.stderr.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .last()
        .unwrap_or_default()
        .chars()
        .take(160)
        .collect::<String>();
    let v = record_version().await?;
    Ok(format!("yt-dlp {v} ({said})"))
}

/// Daily update check while built-in transcripts are installed; a no-op
/// loop on machines that never set them up.
pub fn spawn_daily_update() {
    tauri::async_runtime::spawn(async {
        // Not at launch: the host has better things to do in its first
        // minutes, and a relaunch loop must not hammer GitHub.
        tokio::time::sleep(Duration::from_secs(300)).await;
        loop {
            if available() {
                match self_update().await {
                    Ok(s) => tracing::info!("built-in transcripts: update check: {s}"),
                    Err(e) => tracing::warn!("built-in transcripts: update check failed: {e}"),
                }
            }
            tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
        }
    });
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub title: String,
    pub duration: u64,
    pub language: String,
    pub text: String,
}

/// A failed fetch, in the household service's four kinds
/// (`unsupported`, `no_captions`, `rate_limited`, `failed`), so the tool
/// turns both paths into the same sentences. `detail` is for the log and
/// never carries the video's title.
#[derive(Debug)]
pub struct FetchError {
    pub kind: &'static str,
    pub detail: String,
}

impl FetchError {
    fn new(kind: &'static str, detail: impl Into<String>) -> Self {
        Self { kind, detail: detail.into() }
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.kind, self.detail)
    }
}

/// The video id, or None when the URL is not a supported YouTube video.
/// The same host list as the tool's own allowlist, matched on the parsed
/// host, so `youtube.com.evil.tld` and bare IPs never get this far.
pub fn video_id(url: &str) -> Option<String> {
    let u = reqwest::Url::parse(url).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str()?.to_ascii_lowercase();
    if !crate::tools::video_transcript::ALLOWED_HOSTS.contains(&host.as_str()) {
        return None;
    }
    let id = if host.ends_with("youtu.be") {
        u.path().trim_start_matches('/').split('/').next()?.to_string()
    } else if let Some(rest) = ["/shorts/", "/embed/", "/live/"]
        .iter()
        .find_map(|p| u.path().strip_prefix(p))
    {
        rest.split('/').next()?.to_string()
    } else {
        u.query_pairs().find(|(k, _)| k == "v")?.1.into_owned()
    };
    let ok = (6..=20).contains(&id.len())
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then_some(id)
}

/// The arguments for one caption fetch. yt-dlp gets a URL rebuilt from
/// the id, never the one the model passed, and `--` ends option parsing
/// before it.
fn fetch_args(id: &str, out_template: &Path) -> Vec<String> {
    let mut a: Vec<String> = [
        "--ignore-config",
        "--no-update",
        "--socket-timeout",
        "30",
        "--skip-download",
        "--no-playlist",
        "--ignore-errors",
        "--write-auto-subs",
        "--write-subs",
        "--sub-langs",
        "en-orig,en.*,en",
        "--sub-format",
        "vtt",
        "--print-json",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push("--cache-dir".into());
    a.push(tool_dir().join("cache").display().to_string());
    a.push("-o".into());
    a.push(out_template.display().to_string());
    a.push("--".into());
    a.push(format!("https://www.youtube.com/watch?v={id}"));
    a
}

/// Caption languages yt-dlp wrote into `dir` as `t.<lang>.vtt`, sorted.
fn written_langs(dir: &Path) -> Vec<String> {
    let mut langs: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix("t.")?.strip_suffix(".vtt").map(str::to_string)
        })
        .collect();
    langs.sort();
    langs
}

/// Captions the uploader wrote beat en-orig, the speech recogniser's own
/// track, which beats anything else that matched (en-GB, en-US, ...).
fn pick_captions(langs: &[String], manual: &HashSet<String>) -> Option<String> {
    langs
        .iter()
        .min_by_key(|l| (!manual.contains(*l), l.as_str() != "en-orig"))
        .cloned()
}

/// No caption file arrived: say why, from what yt-dlp printed.
fn classify_no_file(code: Option<i32>, stderr: &str) -> FetchError {
    let low = stderr.to_ascii_lowercase();
    if low.contains("429") || low.contains("too many requests") {
        return FetchError::new("rate_limited", "YouTube answered 429");
    }
    if code != Some(0) {
        return FetchError::new("failed", format!("yt-dlp exited {code:?}"));
    }
    FetchError::new("no_captions", "no caption track was written")
}

/// VTT -> prose. Drops cue timings, positioning and duplicate lines
/// (auto-captions repeat each line as the karaoke window scrolls).
pub(crate) fn vtt_to_text(vtt: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for raw in vtt.lines() {
        let line = raw.trim();
        if line.is_empty()
            || line == "WEBVTT"
            || line.contains("-->")
            || line.chars().all(|c| c.is_ascii_digit())
            || line.starts_with("Kind:")
            || line.starts_with("Language:")
            || line.starts_with("NOTE")
        {
            continue;
        }
        let clean = VTT_TAG.replace_all(line, "");
        let clean = clean
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&nbsp;", " ")
            .replace("&amp;", "&");
        let clean = clean.trim();
        if clean.is_empty() || out.last().is_some_and(|l| l == clean) {
            continue;
        }
        out.push(clean.to_string());
    }
    out.join(" ").chars().take(MAX_TEXT).collect()
}

fn read_cache(path: &Path) -> Option<Transcript> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_cache(path: &Path, t: &Transcript) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let part = path.with_extension("json.part");
    if let Ok(body) = serde_json::to_string(t) {
        if std::fs::write(&part, body).is_ok() {
            let _ = std::fs::rename(&part, path); // atomic; no half-written cache
        }
    }
}

/// A YouTube video's captions. Transcripts never change, so each is cached
/// by video id for good and a repeat question never touches YouTube.
pub async fn fetch(url: &str) -> std::result::Result<Transcript, FetchError> {
    let id = video_id(url).ok_or_else(|| FetchError::new("unsupported", "not a YouTube video link"))?;
    if !available() {
        return Err(FetchError::new("failed", "built-in transcripts are not set up"));
    }
    let cached = cache_dir().join(format!("{id}.json"));
    if let Some(t) = read_cache(&cached) {
        return Ok(t);
    }
    let _one = ONE_AT_A_TIME.lock().await;
    if let Some(t) = read_cache(&cached) {
        return Ok(t); // fetched by whoever held the lock before us
    }
    // Outside ~/.kinai, which the sandbox cannot write.
    let tmp = std::env::temp_dir().join(format!("kinai-yt-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).map_err(|e| FetchError::new("failed", e.to_string()))?;
    let result = fetch_into(&id, &tmp).await;
    let _ = std::fs::remove_dir_all(&tmp);
    let t = result?;
    write_cache(&cached, &t);
    Ok(t)
}

async fn fetch_into(id: &str, tmp: &Path) -> std::result::Result<Transcript, FetchError> {
    let r = run(&fetch_args(id, &tmp.join("t.%(ext)s")), FETCH_TIMEOUT)
        .await
        .map_err(|e| FetchError::new("failed", e.to_string()))?;
    let meta: serde_json::Value = r
        .stdout
        .lines()
        .find(|l| l.starts_with('{'))
        .and_then(|l| serde_json::from_str(l).ok())
        .unwrap_or(serde_json::Value::Null);
    let manual: HashSet<String> = meta
        .get("subtitles")
        .and_then(|v| v.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    // A file on disk wins over anything in stderr: a 429 on one track says
    // nothing about the track that did arrive.
    let Some(lang) = pick_captions(&written_langs(tmp), &manual) else {
        return Err(classify_no_file(r.code, &r.stderr));
    };
    let vtt = std::fs::read_to_string(tmp.join(format!("t.{lang}.vtt")))
        .map_err(|e| FetchError::new("failed", e.to_string()))?;
    let text = vtt_to_text(&vtt);
    if text.trim().is_empty() {
        return Err(FetchError::new("no_captions", "the caption track was empty"));
    }
    Ok(Transcript {
        title: meta.get("title").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        duration: meta.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0) as u64,
        language: meta.get("language").and_then(|v| v.as_str()).unwrap_or("en").to_string(),
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_id_accepts_the_youtube_shapes() {
        for (url, id) in [
            ("https://www.youtube.com/watch?v=abcDEF12345", "abcDEF12345"),
            ("https://youtube.com/watch?v=abcDEF12345&t=42s", "abcDEF12345"),
            ("https://m.youtube.com/watch?v=abcDEF12345", "abcDEF12345"),
            ("https://youtu.be/abcDEF12345?si=xyz", "abcDEF12345"),
            ("https://www.youtube.com/shorts/abcDEF12345", "abcDEF12345"),
            ("https://www.youtube.com/embed/abcDEF12345", "abcDEF12345"),
            ("https://www.youtube.com/live/abcDEF12345?feature=share", "abcDEF12345"),
        ] {
            assert_eq!(video_id(url).as_deref(), Some(id), "{url}");
        }
    }

    #[test]
    fn video_id_refuses_everything_else() {
        for url in [
            "https://youtube.com.evil.tld/watch?v=abcDEF12345",
            "http://192.168.1.210:8081/v1/models",
            "http://[::ffff:192.168.1.210]:8081/",
            "file:///etc/passwd",
            "https://vimeo.com/123456789",
            "https://www.youtube.com/watch?v=--exec=x",
            "https://www.youtube.com/watch?v=a b",
            "https://www.youtube.com/watch",
            "not a url",
        ] {
            assert_eq!(video_id(url), None, "{url}");
        }
    }

    #[test]
    fn fetch_args_rebuild_the_url_after_double_dash() {
        let a = fetch_args("abcDEF12345", Path::new("/tmp/x/t.%(ext)s"));
        let n = a.len();
        assert_eq!(a[n - 2], "--");
        assert_eq!(a[n - 1], "https://www.youtube.com/watch?v=abcDEF12345");
        let langs = a.iter().position(|s| s == "--sub-langs").unwrap();
        assert!(a[langs + 1].starts_with("en-orig,"), "en-orig must be asked for first");
        assert!(a.iter().any(|s| s == "--ignore-errors"));
        assert!(a.iter().any(|s| s == "--ignore-config"), "a user's yt-dlp config must not apply");
        // --ignore-errors turns a refused track into a WARNING; hiding
        // warnings would make a real 429 read as "no captions".
        assert!(!a.iter().any(|s| s == "--no-warnings"));
    }

    #[test]
    fn uploader_captions_then_en_orig_then_the_rest() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let none = HashSet::new();
        assert_eq!(pick_captions(&s(&["en", "en-orig"]), &none).as_deref(), Some("en-orig"));
        assert_eq!(pick_captions(&s(&["en-GB", "en-orig"]), &none).as_deref(), Some("en-orig"));
        let manual: HashSet<String> = ["en".to_string()].into();
        assert_eq!(pick_captions(&s(&["en", "en-orig"]), &manual).as_deref(), Some("en"));
        assert_eq!(pick_captions(&s(&["en-GB"]), &none).as_deref(), Some("en-GB"));
        assert_eq!(pick_captions(&[], &none), None);
    }

    #[test]
    fn no_file_is_classified_from_stderr() {
        let w429 = "WARNING: Unable to download video subtitles for 'en': HTTP Error 429: Too Many Requests";
        // --ignore-errors: the refusal is a WARNING and the exit is 0.
        assert_eq!(classify_no_file(Some(0), w429).kind, "rate_limited");
        assert_eq!(classify_no_file(Some(1), "ERROR: [youtube] x: This video is unavailable").kind, "failed");
        assert_eq!(classify_no_file(Some(0), "").kind, "no_captions");
    }

    #[test]
    fn vtt_becomes_prose() {
        let vtt = "WEBVTT\nKind: captions\nLanguage: en\n\n00:00:00.000 --> 00:00:02.000 align:start\n\
                   Stock<00:00:00.500><c> market</c> highs\n\n00:00:02.000 --> 00:00:04.000\n\
                   Stock market highs\nthe S&amp;P &lt;500&gt; too\n\n1\n";
        assert_eq!(vtt_to_text(vtt), "Stock market highs the S&P <500> too");
    }

    #[test]
    fn checksum_line_is_found_exactly() {
        let h = "a".repeat(64);
        let sums = format!("{}  yt-dlp_macos.zip\n{h}  yt-dlp_macos\n", "b".repeat(64));
        assert_eq!(expected_sha256(&sums, "yt-dlp_macos"), Some(h));
        assert_eq!(expected_sha256("short  yt-dlp_macos\n", "yt-dlp_macos"), None);
        assert_eq!(expected_sha256(&sums, "yt-dlp"), None);
    }

    // Unix paths: Windows joins with backslashes, and the sandbox is a
    // macOS feature anyway.
    #[cfg(unix)]
    #[test]
    fn profile_fences_off_the_family_data_and_escapes_paths() {
        let p = sandbox_profile(Path::new("/Users/a\"b"), Some(Path::new("/Applications/KinAI.app")));
        assert!(p.starts_with("(version 1)(allow default)"));
        assert!(p.contains(r#"(deny file-read* file-write* (subpath "/Users/a\"b/.kinai"))"#));
        assert!(p.contains(r#"/.ssh"))"#) && p.contains(r#"/Library/Keychains"))"#));
        assert!(p.contains(r#"(deny file-write* (subpath "/Users/a\"b/Library/LaunchAgents"))"#));
        assert!(p.contains(r#"(deny file-write* (subpath "/Applications/KinAI.app"))"#));
    }

    /// The property this module promises, tested against the real
    /// sandbox: a process under the profile cannot read a file in the
    /// fenced-off `.kinai`, but can read a sibling and use the temp dir.
    #[cfg(target_os = "macos")]
    #[test]
    fn sandbox_denies_the_family_data_for_real() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".kinai")).unwrap();
        std::fs::write(home.path().join(".kinai/secret.txt"), "family").unwrap();
        std::fs::write(home.path().join("other.txt"), "fine").unwrap();
        let profile = sandbox_profile(home.path(), None);
        let cat = |p: &Path| {
            std::process::Command::new(SANDBOX_EXEC)
                .arg("-p")
                .arg(&profile)
                .arg("/bin/cat")
                .arg(p)
                .output()
                .unwrap()
        };
        let denied = cat(&home.path().join(".kinai/secret.txt"));
        assert!(!denied.status.success(), "the sandbox let a process read .kinai");
        assert!(!String::from_utf8_lossy(&denied.stdout).contains("family"));
        let allowed = cat(&home.path().join("other.txt"));
        assert!(allowed.status.success(), "the sandbox blocked an ordinary file");
    }

    /// Live, end to end: what the Settings button does, then a real fetch
    /// through the sandbox, then the cache. Needs the internet; installs
    /// into this Mac's real tool folder (remove it from Settings or with
    /// `remove()` afterwards). A famous public video, not anyone's own.
    #[tokio::test]
    #[ignore = "live: downloads yt-dlp and reads YouTube"]
    async fn live_install_then_fetch() {
        let s = install().await.expect("install");
        assert!(s.installed && s.version.is_some(), "{s:?}");
        println!("installed yt-dlp {:?}", s.version);
        let url = "https://www.youtube.com/watch?v=jNQXAC9IVRw";
        let id = video_id(url).unwrap();
        let _ = std::fs::remove_file(cache_dir().join(format!("{id}.json")));
        let t0 = std::time::Instant::now();
        let t = fetch(url).await.expect("fetch");
        assert!(!t.text.trim().is_empty());
        println!("fetched in {:?}: {} ({}s): {}", t0.elapsed(), t.title, t.duration,
                 t.text.chars().take(100).collect::<String>());
        assert!(cache_dir().join(format!("{id}.json")).is_file(), "not cached");
        let t1 = std::time::Instant::now();
        let again = fetch(url).await.expect("cached fetch");
        assert_eq!(again.text, t.text);
        assert!(t1.elapsed() < Duration::from_millis(200), "second fetch should come from the cache");
    }
}
