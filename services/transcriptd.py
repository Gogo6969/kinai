#!/usr/bin/env python3
"""KinAI transcript service — captions for a video URL, on the family's own hardware.

Wraps yt-dlp behind one endpoint so KinAI never executes third-party code
itself. Deliberately narrow:

  * ONLY recognised video hosts are accepted. This service sits at a LAN
    address, so it is a door around fetch_page's SSRF guard — without a
    host allowlist a hostile page could get the model to call it with
    http://192.168.1.210:8081/... and read the model servers. The
    allowlist is the security property; do not widen it casually.
  * One yt-dlp at a time. YouTube rate-limits per IP and a household
    shares one; a 429 was reproduced during development.
  * Transcripts are immutable, so they are cached by video id forever.
    A repeat question costs nothing and never touches YouTube.

GET /health              -> {"ok": true, "ytdlp": "<version>"}
GET /transcript?url=...  -> {"title","duration","language","text","cached"}
                         -> 4xx/5xx {"error","kind"} where kind is one of
                            unsupported | no_captions | rate_limited | failed
"""
from __future__ import annotations   # `str | None` on Python 3.9 too

import json, os, re, subprocess, sys, threading, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

YTDLP = os.path.expanduser("~/.local/bin/yt-dlp")
CACHE = os.path.expanduser("~/.kinai-transcripts")
PORT = int(os.environ.get("TRANSCRIPT_PORT", "8099"))
FETCH_TIMEOUT = 120           # yt-dlp wall clock
MAX_TEXT = 200_000            # a 3h podcast is ~150k chars

# The security boundary. Suffix match on the host, so "youtube.com.evil.tld"
# does not slip through and neither does a bare IP address.
ALLOWED_HOSTS = (
    "youtube.com", "www.youtube.com", "m.youtube.com", "music.youtube.com",
    "youtu.be", "www.youtu.be",
)
_lock = threading.Lock()      # one yt-dlp at a time, process-wide


def video_id(url: str) -> str | None:
    """Stable cache key. Returns None when the URL is not a supported video."""
    try:
        u = urllib.parse.urlparse(url)
    except ValueError:
        return None
    if u.scheme not in ("http", "https"):
        return None
    host = (u.hostname or "").lower()
    if host not in ALLOWED_HOSTS:
        return None
    if host.endswith("youtu.be"):
        vid = u.path.lstrip("/").split("/")[0]
    elif u.path.startswith(("/shorts/", "/embed/", "/live/")):
        vid = u.path.split("/")[2] if len(u.path.split("/")) > 2 else ""
    else:
        vid = urllib.parse.parse_qs(u.query).get("v", [""])[0]
    return vid if re.fullmatch(r"[A-Za-z0-9_-]{6,20}", vid or "") else None


def vtt_to_text(vtt: str) -> str:
    """VTT -> prose. Drops cue timings, positioning and duplicate lines
    (auto-captions repeat each line as the karaoke window scrolls)."""
    out, seen_last = [], None
    for raw in vtt.splitlines():
        line = raw.strip()
        if (not line or line == "WEBVTT" or "-->" in line
                or line.isdigit() or line.startswith(("Kind:", "Language:", "NOTE"))):
            continue
        line = re.sub(r"<[^>]+>", "", line).strip()      # inline <c> timing tags
        if not line or line == seen_last:
            continue
        out.append(line)
        seen_last = line
    return " ".join(out)[:MAX_TEXT]


def fetch(url: str, vid: str) -> dict:
    """Run yt-dlp for metadata + auto-captions. Raises RuntimeError(kind, msg)."""
    tmp = os.path.join(CACHE, f".tmp-{vid}")
    cmd = [
        YTDLP, "--skip-download", "--no-playlist", "--no-warnings",
        "--write-auto-subs", "--write-subs", "--sub-langs", "en.*,en",
        "--sub-format", "vtt", "--print-json", "-o", tmp + ".%(ext)s", url,
    ]
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=FETCH_TIMEOUT)
    except subprocess.TimeoutExpired:
        raise RuntimeError("failed", f"the transcript service timed out after {FETCH_TIMEOUT}s")
    err = (p.stderr or "").lower()
    if "429" in err or "too many requests" in err:
        raise RuntimeError("rate_limited",
                           "YouTube is rate-limiting transcript downloads right now; try again in a few minutes")
    meta = {}
    for line in (p.stdout or "").splitlines():
        if line.startswith("{"):
            try:
                meta = json.loads(line)
                break
            except json.JSONDecodeError:
                pass
    # yt-dlp names the file <tmp>.<lang>.vtt; take the first match.
    vtt_path = next((os.path.join(CACHE, f) for f in sorted(os.listdir(CACHE))
                     if f.startswith(f".tmp-{vid}") and f.endswith(".vtt")), None)
    if not vtt_path:
        if p.returncode != 0:
            raise RuntimeError("failed", (p.stderr or "yt-dlp failed").strip().splitlines()[-1][:300])
        raise RuntimeError("no_captions", "this video has no captions available")
    try:
        with open(vtt_path, encoding="utf-8", errors="replace") as fh:
            text = vtt_to_text(fh.read())
    finally:
        for f in os.listdir(CACHE):
            if f.startswith(f".tmp-{vid}"):
                try:
                    os.remove(os.path.join(CACHE, f))
                except OSError:
                    pass
    if not text.strip():
        raise RuntimeError("no_captions", "this video's captions were empty")
    return {
        "title": meta.get("title") or "",
        "duration": meta.get("duration") or 0,
        "language": meta.get("language") or "en",
        "text": text,
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _send(self, code: int, payload: dict):
        body = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        if u.path == "/health":
            try:
                v = subprocess.run([YTDLP, "--version"], capture_output=True,
                                   text=True, timeout=15).stdout.strip()
            except Exception:
                v = "unavailable"
            return self._send(200, {"ok": v != "unavailable", "ytdlp": v})
        if u.path != "/transcript":
            return self._send(404, {"error": "not found", "kind": "failed"})

        url = urllib.parse.parse_qs(u.query).get("url", [""])[0]
        vid = video_id(url)
        if not vid:
            # The allowlist refusal. Deliberately says nothing about what
            # else this host can reach.
            return self._send(400, {
                "error": "only YouTube video links are supported",
                "kind": "unsupported"})

        cached = os.path.join(CACHE, f"{vid}.json")
        if os.path.exists(cached):
            with open(cached, encoding="utf-8") as fh:
                got = json.load(fh)
            got["cached"] = True
            return self._send(200, got)

        with _lock:                                  # one at a time
            if os.path.exists(cached):               # won by another thread
                with open(cached, encoding="utf-8") as fh:
                    got = json.load(fh)
                got["cached"] = True
                return self._send(200, got)
            try:
                got = fetch(url, vid)
            except RuntimeError as e:
                kind, msg = e.args
                code = 429 if kind == "rate_limited" else 404 if kind == "no_captions" else 502
                return self._send(code, {"error": msg, "kind": kind})
            except Exception as e:  # noqa: BLE001 - never leak a stack trace
                return self._send(502, {"error": f"transcript service error: {e}"[:300],
                                        "kind": "failed"})
        tmp = cached + ".part"
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump(got, fh)
        os.replace(tmp, cached)                      # atomic; no half-written cache
        got["cached"] = False
        return self._send(200, got)

    def log_message(self, fmt, *args):
        sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))


if __name__ == "__main__":
    os.makedirs(CACHE, exist_ok=True)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
