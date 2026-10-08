# Video transcripts

Lets KinAI answer "watch this" for a YouTube link by reading the video's
captions. There are two ways to turn it on; with neither, KinAI simply
doesn't offer it.

1. **Built in (any host Mac) — the usual way.** Settings → Tools →
   **Set up video transcripts**. Nothing else to install.
2. **Your own transcript service** (a Linux box on your network) — the
   rest of this document. Fill in its address under Settings → Tools and
   it is used instead of the built-in one.

## Built in

The button downloads the official standalone yt-dlp for macOS
(`yt-dlp_macos`, a universal binary that needs no Python), checks it
against the `SHA2-256SUMS` file of the same release, and proves it starts
before calling it installed. It lives in
`~/Library/Application Support/KinAI/yt-dlp/`, deliberately outside
`~/.kinai`. Installed means on: there is no separate switch. **Remove**
deletes it again.

**The separation.** KinAI's host holds every family member's
conversations, and yt-dlp is third-party code handed URLs that ultimately
come from chat. So yt-dlp never runs inside KinAI, and never runs plainly
either: every call goes through macOS's `sandbox-exec` with a profile that
cannot read or write `~/.kinai` (conversations, keys, config), `~/.ssh` or
the keychains, and cannot write login items or `KinAI.app`. It keeps the
network (it has to reach YouTube), the temp folder it unpacks into, and its
own folder (it updates itself there). If the sandbox is missing, nothing
runs. A unit test runs the real sandbox against a fake `.kinai` on every
macOS build.

**Keeping up with YouTube.** yt-dlp loses to YouTube within weeks on a
stale build, so the host runs `yt-dlp -U` once a day while it is
installed (its own updater verifies the release). Transcripts are cached
for good in `~/.kinai/transcripts`; a repeat question never touches
YouTube. The caption logic, including the `en-orig` trap below, is the
same as the service's (`src/transcripts.rs`).

## Your own service: why it is a separate machine

The original design ran `yt-dlp` only on a household server, for two
reasons that the built-in path now answers differently: bundling it into
a notarized app would mean re-releasing whenever YouTube changes (the
built-in path downloads and self-updates the binary instead), and the
host should not run third-party code that takes untrusted URLs (the
built-in path runs it in the sandbox described above). A separate machine
is still the strongest isolation, so the service stays supported.

## What the service is

`transcriptd.py` — one file, Python standard library only, no
dependencies beyond `yt-dlp` itself.

```
GET /health              -> {"ok": true, "ytdlp": "2026.08.19"}
GET /transcript?url=...  -> {"title","duration","language","text","cached"}
                            or 4xx/5xx {"error","kind"}
```

`kind` is one of `unsupported`, `no_captions`, `rate_limited`, `failed`,
so KinAI can tell the family *which* thing went wrong instead of "it
didn't work".

Three behaviours matter:

- **Host allowlist.** Only YouTube hosts are accepted; everything else is
  rejected with 400. See the security note below — this is not a
  convenience check.
- **Cache.** Transcripts never change, so they are stored by video id
  under `~/.kinai-transcripts` forever. A repeat question is instant and
  never touches YouTube.
- **One at a time.** A process-wide lock serialises `yt-dlp`. YouTube
  rate-limits per IP and a household shares one; a 429 was reproduced
  during development.

## Security: why the allowlist is load-bearing

`fetch_page` refuses LAN, loopback and link-local addresses so that a web
page can never talk the model into probing the family's own servers. This
service sits *at* a LAN address, so it is a door around that guard.

There are therefore two locks, and both must hold:

1. **The service** rejects any URL whose host is not a known YouTube host.
2. **KinAI** (`src/tools/video_transcript.rs`) refuses the same set before
   making the call, because the *model* chooses this tool's argument and a
   hostile page can influence what the model asks for.

The built-in path has the same two locks: the tool's check above, then
`src/transcripts.rs` extracts the video id against the same host list and
hands yt-dlp a URL rebuilt from that id (after `--`), never the one the
model passed.

Without them, `video_transcript("http://192.168.1.210:8081/v1/models")`
would read a model server through a component built to be helpful. Do not
widen either list casually; if you add a host, add it in both places and
extend the tests that assert LAN addresses are refused.

The service URL itself is host-configured only — like `searxng_url`, it is
a deliberate exception the household owner opens, never something a
fetched page can influence.

## Install (Linux, systemd)

Currently on Olares (`192.168.1.210`), which is always on and already hosts
the model servers.

```bash
uv tool install yt-dlp                      # or pipx/pip
scp transcriptd.py olares@192.168.1.210:~/
sudo install -m 644 kinai-transcript.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now kinai-transcript
curl -s http://192.168.1.210:8099/health
```

The unit runs as an unprivileged user with `ProtectSystem=strict`,
`ProtectHome=read-only` and `NoNewPrivileges`, writing only to its cache
directory — it shells out to `yt-dlp` on URLs that ultimately come from
chat, so its blast radius is kept small.

Then paste `http://192.168.1.210:8099` into Settings → Tools.

## Known limits

- **Auto-generated captions.** Wording is imperfect and there are no
  speaker labels. The tool says so in its output so the model doesn't
  quote captions as verbatim speech.
- **Not every video has captions.** The tool reports that distinctly
  rather than guessing from the title.
- **YouTube changes break `yt-dlp`.** Cached videos keep working; new
  ones fail until `yt-dlp` is updated. The built-in one updates itself
  daily; on your own service run `uv tool upgrade yt-dlp`. This is the
  standing cost of the feature and was accepted knowingly.
- **YouTube only.** Other sites need a host added to both allowlists.
- **Never ask for the translated `en` track alone.** A video with only
  auto-captions lists `en-orig` (what the speech recogniser heard) and
  `en` (that track run through YouTube's translator, English to
  English). YouTube refuses the translated one with 429 every time, so a
  service asking for `en` reports "rate limited" for a video that will
  never work. The service asks for `en-orig` first, lets one refused
  track fail (`--ignore-errors`, which is why `--no-warnings` is off),
  and only says `rate_limited` when no caption file arrived at all.
  Captions the uploader wrote still win over `en-orig`. Found
  2026-10-07, when an auto-captioned video failed this way.

## When something goes wrong

Every user-visible failure string this tool can produce is also
registered in `force_search::OUTAGE_CLAIMS`. That is not optional: KinAI's
own honest "I couldn't get the transcript" otherwise survives in the
thread and gets repeated as a standing incapacity long after the service
recovers — exactly how "the search backend is down" outlived the Exa
outage by two days. **If you add a failure message, add it there too.**

```bash
sudo systemctl status kinai-transcript
sudo journalctl -u kinai-transcript -n 50
python3 services/test_transcriptd.py        # fake yt-dlp, no network
```

A "rate limited" that never lifts on one video: run the same `yt-dlp`
by hand with `--list-subs` and fetch each English track on its own. If
only one track is refused, it is the track choice, not the IP.
