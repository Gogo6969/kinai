# Video transcript service

Lets KinAI answer "watch this" for a YouTube link by reading the video's
captions. Optional: leave **Settings → Tools → Video transcript service**
empty and KinAI never offers the tool at all.

## Why it is a separate service

KinAI does not run `yt-dlp` itself, for two reasons.

`yt-dlp` is in a permanent arms race with YouTube and updates constantly;
bundling it into a notarized desktop app means shipping a Python runtime
and re-releasing whenever YouTube changes. And KinAI's host process holds
every family member's conversations — it should not be executing
third-party code that takes untrusted URLs.

So the capability lives where the family already runs services, and KinAI
calls it the same way it calls SearXNG.

## What it is

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

Without them, `video_transcript("http://192.168.1.25:8081/v1/models")`
would read a model server through a component built to be helpful. Do not
widen either list casually; if you add a host, add it in both places and
extend the tests that assert LAN addresses are refused.

The service URL itself is host-configured only — like `searxng_url`, it is
a deliberate exception the household owner opens, never something a
fetched page can influence.

## Install (Linux, systemd)

Currently on Olares (`192.168.1.25`), which is always on and already hosts
the model servers.

```bash
uv tool install yt-dlp                      # or pipx/pip
scp transcriptd.py olares@192.168.1.25:~/
sudo install -m 644 kinai-transcript.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now kinai-transcript
curl -s http://192.168.1.25:8099/health
```

The unit runs as an unprivileged user with `ProtectSystem=strict`,
`ProtectHome=read-only` and `NoNewPrivileges`, writing only to its cache
directory — it shells out to `yt-dlp` on URLs that ultimately come from
chat, so its blast radius is kept small.

Then paste `http://192.168.1.25:8099` into Settings → Tools.

## Known limits

- **Auto-generated captions.** Wording is imperfect and there are no
  speaker labels. The tool says so in its output so the model doesn't
  quote captions as verbatim speech.
- **Not every video has captions.** The tool reports that distinctly
  rather than guessing from the title.
- **YouTube changes break `yt-dlp`.** Cached videos keep working; new
  ones fail until `yt-dlp` is updated (`uv tool upgrade yt-dlp`). This is
  the standing cost of the feature and was accepted knowingly.
- **YouTube only.** Other sites need a host added to both allowlists.

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
```
