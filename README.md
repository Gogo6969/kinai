<div align="center">

<img src="public/icons/icon.svg" width="120" alt="KinAI" />

# KinAI

**Share your private local AI with your family**

*KinAI isn't an AI — it's the family-sharing layer for the local LLM you already run. One Mac hosts your Ollama / LM Studio / vLLM / llama.cpp backend; every other family device joins via a 6-character invite. No cloud, no accounts, each member's chat private to them.*

[![License: MIT](https://img.shields.io/badge/License-MIT-00D4C8.svg)](LICENSE)
[![Tauri 2](https://img.shields.io/badge/Tauri-2-1E3A8A.svg)](https://tauri.app/)
[![Rust](https://img.shields.io/badge/Rust-stable-CE422B.svg)](https://www.rust-lang.org/)
[![SvelteKit](https://img.shields.io/badge/SvelteKit-2-FF3E00.svg)](https://kit.svelte.dev/)
[![macOS](https://img.shields.io/badge/macOS-available-22c55e.svg)](https://github.com/Gogo6969/kinai/releases)
[![Windows](https://img.shields.io/badge/Windows-coming%20soon-9ca3af.svg)](https://github.com/Gogo6969/kinai/issues)
[![Linux](https://img.shields.io/badge/Linux-coming%20soon-9ca3af.svg)](https://github.com/Gogo6969/kinai/issues)

</div>

> [!IMPORTANT]
> **v0.2.x is macOS-only (Apple Silicon + Intel).** The codebase is
> cross-platform Rust + SvelteKit, the protocol is OS-agnostic, and the
> build pipeline already includes Windows + Linux runners — but the
> binaries for those platforms aren't validated yet. Windows ships in
> **v0.3**, Linux in **v0.4**. Until then, the host *and* clients must
> run on a Mac.

---

## ⚡ Five-second pitch

You already run a local LLM on one of your machines — Ollama, LM Studio, vLLM, llama.cpp, whatever. KinAI is the **family-sharing layer** on top of it. Install KinAI on that same Mac in **Host Mode** to expose your model to the family. Install it on every other device in **Client Mode** (lightweight, no models downloaded) and join with a 6-character invite. Press **`Cmd+Space`** (macOS) or **`Ctrl+Space`** (Windows/Linux) from any app, ask anything, get an answer.

100% local. 100% private. 100% free. MIT licensed forever.

## 📸 Screenshots

<div align="center">

| | |
|:-:|:-:|
| ![Welcome screen](docs/screenshots/welcome.png) | ![Host setup with auto-detected backends](docs/screenshots/host-setup.png) |
| *First launch — Host or Join* | *Auto-detects Ollama, LM Studio, vLLM, llama.cpp, Open WebUI* |
| ![Host chat empty state](docs/screenshots/chat.png) | ![Vision: photo attached + analyzed](docs/screenshots/vision.png) |
| *Host view — family's AI running on this Mac* | *Drop a photo, KinAI routes to your vision endpoint* |

<p>
  <img src="docs/screenshots/image-search.png" alt="Image search rendered inline" width="78%" />
  <br/>
  <em>Ask for a photo → KinAI calls <code>image_search</code> → result renders inline</em>
</p>

</div>

## 🚀 Install on your Mac

KinAI runs on macOS today (Apple Silicon + Intel). Windows + Linux are
coming in v0.3 / v0.4. You'll install KinAI **twice**: once on the Mac
that has the LLM (the *Host*), and once on every other Mac in your
family (the *Clients*).

### Step 1 — Get a local LLM running on the host Mac

KinAI doesn't bundle a model — it connects to one you already have. If
you don't have one yet, **[Ollama](https://ollama.com)** is the easiest
on-ramp:

1. Download Ollama from <https://ollama.com> (one-click installer).
2. Open **Terminal** (`Cmd+Space` → type "Terminal" → Enter — we'll
   fix the Spotlight conflict in a minute).
3. Run:

   ```bash
   ollama pull llama3.1:8b
   ```

   That's about a 5 GB download. When it's done, run `ollama list` to
   verify the model is installed.

> Other backends KinAI auto-detects on first launch: **LM Studio**,
> **vLLM**, **llama.cpp server**, **Open WebUI**. Any
> OpenAI-compatible endpoint works.

### Step 2 — Install KinAI on the host Mac

1. Go to the **[Releases page](https://github.com/Gogo6969/kinai/releases/latest)**.
2. Download the matching DMG under *Assets*:
   - **Apple Silicon** (M1/M2/M3/M4): `KinAI_0.2.5_aarch64.dmg`
   - **Intel Mac**: *not yet built in v0.2.5 — [open an issue](https://github.com/Gogo6969/kinai/issues/new) and we'll prioritize.*
3. Double-click the downloaded `.dmg` file. A new window opens with
   the KinAI icon and an `Applications` shortcut.
4. **Drag `KinAI` onto the `Applications` folder shortcut** inside that
   window.
5. Open your **Applications** folder, find `KinAI`, and **right-click
   → Open** (not double-click — the right-click is important the
   first time).
6. macOS will warn: *"KinAI can't be opened because Apple cannot check
   it for malicious software."* This is normal for any app that
   isn't paid into the Apple Developer Program. Click **Open** in
   that dialog. You only have to do this once per Mac.
7. KinAI launches → click **🏠 Host KinAI here**.
8. The wizard auto-detects your local LLM in a few seconds → pick it
   → **Continue**, confirm the search engine + (optional) Vision
   endpoint → **Start hosting**.
9. From the chat sidebar, click **Manage family → + Invite**. You'll
   get a **6-character code** and a QR code — share one with each
   family member.

### Step 3 — Install KinAI on every other Mac in the family

On each other Mac:

1. Download the same DMG from the Releases page.
2. Drag `KinAI.app` into Applications, right-click → **Open** (same
   Gatekeeper dance as Step 2.5–6 above).
3. Click **👋 Join an existing host**.
4. Type your name (this is how the host sees you in *Manage family*).
5. Either pick the host from the auto-discovered list, **or** type the
   **6-character code** from the host. Click **Connect**.
6. Done. Start chatting.

### Daily use

Press **`Cmd+Space`** anywhere to summon the KinAI overlay (like
Spotlight, but for your family's AI). Type, press Enter, get an
answer.

> **Hotkey conflict with Spotlight?** `Cmd+Space` is the default for
> both. Pick one to remap in **System Settings → Keyboard → Keyboard
> Shortcuts → Spotlight** (change Spotlight to e.g.
> `Cmd+Option+Space`), or change KinAI's hotkey in **KinAI → Settings
> → Overlay → Global hotkey**.

### Updates

After everyone is on **v0.2.5 or later**, future updates flow
**automatically from your host to the rest of the family** — no
download needed. Each client shows a *"KinAI vX.Y.Z is available ·
from your host"* banner the moment the host ships a new version; click
**Install & restart** and you're on the latest.

## 🧠 How context never gets lost

Every reply is built from four layers, in this order:

```
┌──────────────────────────────────────────┐
│  1. System prompt                         │  ← KinAI identity + house rules
├──────────────────────────────────────────┤
│  2. Long-term memory (FTS5 BM25)          │  ← top-N matches from past summaries
├──────────────────────────────────────────┤
│  3. Summarized history                    │  ← rolling summary of older turns
├──────────────────────────────────────────┤
│  4. Recent verbatim turns (last 50)       │  ← raw conversation
├──────────────────────────────────────────┤
│  5. Current user message                  │  ← always kept intact
└──────────────────────────────────────────┘
                  ↓
       Token guard (tiktoken-rs)
                  ↓
       Streamed to your local LLM
```

When a thread exceeds 30 unsummarized messages, the oldest are folded into a long-term memory note and indexed. The token guard reserves space for the response and never drops the current turn. Your conversations survive restarts, model switches, and months of family use.

## 🛠️ What's in v0.2

| Feature | Status |
|---|---|
| Global hotkey overlay (Spotlight-style, translucent) | ✅ |
| Host mode with **auto-detection** of Ollama, LM Studio, vLLM, llama.cpp, Open WebUI | ✅ |
| Client mode with one-click invite join (6-char code or QR) | ✅ |
| Push-to-talk **voice input** (macOS Speech Recognition, Edge WebView2 on Windows) | ✅ |
| **PDF attachments** — drag-drop a PDF, model reads the text | ✅ |
| **Image attachments** + vision routing (CCC-style: chat-model-if-capable → Gemini Flash → Claude Haiku failover) | ✅ |
| **Image search** tool — find pictures via Wikimedia Commons (free) or Exa | ✅ |
| Safe inline image rendering (`https:` / `data:image/*` only, sandboxed) | ✅ |
| Built-in tools: web search, X/Twitter search, calculator, date/time, image search | ✅ |
| OpenAI-style function calling with tool execution loop | ✅ |
| Streaming responses (SSE) with **markdown + LaTeX + code blocks + tables** | ✅ |
| System tray icon with **live status** (model, peers connected) | ✅ |
| Customizable hotkey, theme, font size | ✅ |
| **End-to-end JWT (RS256)** auth on every WebSocket connection | ✅ |
| **Per-peer context isolation** — family members' chats never bleed into each other | ✅ |
| Local SQLite database (SQLx + WAL + FTS5 memory) | ✅ |
| **mDNS** local-network host discovery | ✅ |
| **Host-distributed auto-updates** — host serves the binary, clients pull over LAN, with GitHub fallback for off-LAN | ✅ |
| Rate limiting per client to prevent abuse | ✅ |
| Invite expiry (default 30 days) + revoke | ✅ |
| Family management (see peers, kick) | ✅ |
| Exponential-backoff reconnect supervisor (works through host restarts) | ✅ |

## 🗂️ Supported backends

| Backend | Auto-detected port | Notes |
|---|---|---|
| [Ollama](https://ollama.com/) | `11434` | First-class, model list via `/api/tags` |
| [LM Studio](https://lmstudio.ai/) | `1234` | OpenAI-compatible `/v1` |
| [vLLM](https://docs.vllm.ai/) | `8000` | OpenAI-compatible |
| [llama.cpp server](https://github.com/ggerganov/llama.cpp) | `8080` | OpenAI-compatible |
| [Open WebUI](https://openwebui.com/) | `8888` | OpenAI-compatible |
| Anything else with OpenAI-compatible REST | any | Set base URL manually |

## 🔐 Privacy

**Your data never leaves your computer.** KinAI has no telemetry, no analytics, no user accounts, no remote logging. Conversations live in `~/.kinai/kinai.db` on the host machine. Clients hold an invite JWT and nothing else. The repository contains every line of code that runs.

## 🗺️ Roadmap

| Version | Focus | Highlights |
|---|---|---|
| **v0.1** | MVP | Hotkey overlay, host/client, invite + JWT, tools, mDNS *(macOS)* |
| **v0.2** ⬅ *current* | Vision, attachments, family-grade updates *(macOS)* | PDFs, image attach + vision routing, image search inline, host-distributed signed updates, per-peer context isolation, reconnect supervisor |
| **v0.3** | **Windows support** + image generation | Validated Windows builds via CI, ComfyUI / A1111 routing, web-page ingest |
| **v0.4** | **Linux support** + RAG basics | Validated Linux builds (.deb / AppImage), doc upload + vector search |
| **v0.5** | Mobile + voice | iOS / Android via Tauri Mobile, Whisper STT + Piper TTS, voice-thread memory |
| **v1.0** | Plugins & polish | Custom-tool marketplace, multi-host switcher, admin dashboard, Olares One deep integration |
| **v2.0+** | Family knowledge | Advanced RAG over photos / PDFs / recipes, real-time translation |

All releases stay 100% open-source, local-first, and free forever.

## 🏗️ Architecture

```
┌──────────────────────────────────────────────────────────┐
│  Family devices (Clients)                                 │
│  macOS · Windows · Linux · (iOS/Android v0.4)             │
│         ▼ hotkey / chat / attach PDF or photo ▼           │
│        KinAI Overlay  →  WS Envelope                      │
└──────────────────────────┬───────────────────────────────┘
                           │  JWT-authenticated WebSocket
                           │  + mDNS LAN discovery
                           ▼
┌──────────────────────────────────────────────────────────┐
│  Host machine                                             │
│  ┌────────────────────────────────────────────────────┐  │
│  │ KinAI (Tauri 2 + Rust + Axum)                      │  │
│  │  • Per-peer context builder (threads + FTS5 memory)│  │
│  │  • PDF text extraction (pdf-extract)               │  │
│  │  • Vision router (chat-model → primary → failover) │  │
│  │  • Tool execution loop (web/x/image search + calc) │  │
│  │  • Streaming chat completion (SSE)                 │  │
│  │  • Host-served signed updates (LAN, Minisign)      │  │
│  └────────┬───────────────────────────────┬───────────┘  │
│           ▼                                ▼              │
│  ┌────────────────────┐         ┌───────────────────┐    │
│  │ Local LLM          │         │ Vision endpoint   │    │
│  │ (Ollama / LMS /…)  │         │ (Gemini, llava,…) │    │
│  └────────────────────┘         └───────────────────┘    │
└──────────────────────────────────────────────────────────┘
```

Each family member's chats live in their own bucket on the host's
SQLite — keyed by their invite's short code. Mom's "what did I tell
you about my recipes?" never sees Dad's prompts; the model never
recalls another peer's history.

**Stack:**

- **Shell**: Tauri 2 — single ~25 MB binary per OS
- **Backend**: Rust + Axum + Tokio + SQLx + jsonwebtoken (RS256)
- **Frontend**: SvelteKit (runes) + TailwindCSS + Lucide icons
- **Rendering**: `marked` + `marked-highlight` + `highlight.js` + KaTeX, with a safe-image allow-list
- **Networking**: WebSocket + mDNS-SD local discovery
- **Tokenization**: `tiktoken-rs` (cl100k)
- **PDF extraction**: `pdf-extract` (pure Rust)
- **Vision routing**: pluggable OpenAI-multipart endpoints (Gemini OpenAI-compat, Anthropic OpenAI-compat, llava/qwen-vl via Ollama/vLLM)
- **Updates**: signed (Minisign) bundles served from the host over LAN, GitHub Releases as off-LAN fallback

## 🧱 Repository layout

```
kinai/
├── src/                    # Rust backend
│   ├── auth/               # JWT (RS256) issuance + validation
│   ├── attachments.rs      # PDF text extraction (pdf-extract, pure Rust)
│   ├── context/            # never-lose-context builder + summarizer + token guard
│   ├── db/                 # SQLx + migrations + per-peer threads + memory FTS5
│   ├── llm/                # stream + auto-detect (Ollama/LMS/vLLM/llama.cpp)
│   ├── network/            # Axum HTTP+WS server, client dialer, invite, rate-limit, host-served updates
│   ├── tools/              # web/x/image search + calculator/datetime + execution loop
│   ├── vision.rs           # image-turn router: chat-model-if-capable, else primary, else failover
│   ├── commands.rs         # Tauri IPC surface
│   ├── tray.rs             # system tray + live status ticker
│   ├── hotkey.rs           # global shortcut
│   ├── discovery.rs        # mDNS advertise + browse
│   └── updater.rs          # host-first periodic check, GitHub fallback
├── frontend/               # SvelteKit app
│   └── src/
│       ├── routes/         # /, /host, /host/invite, /host/family, /client, /settings, /overlay
│       ├── lib/components/ # Logo, Sidebar, ChatWindow, Overlay, MessageBubble, ToolPill, UpdateBanner
│       ├── lib/markdown.ts # marked + highlight.js + KaTeX + safe inline images
│       └── lib/api.ts      # typed wrappers over every command + event
├── public/icons/           # SVG + PNG + ICO + ICNS bundle assets
├── docs/                   # host-guide.md, client-guide.md, screenshots/
├── .github/workflows/      # CI build matrix + release.yml
├── Cargo.toml
├── tauri.conf.json
└── README.md
```

## 🛠️ Build from source

Requirements: Rust stable, Node 20+, pnpm 9.

```bash
git clone https://github.com/Gogo6969/kinai
cd kinai

pnpm install                    # also installs frontend workspace
pnpm tauri dev                  # dev build (auto-reload)
pnpm tauri build                # production binary for this OS

# Per-OS bundle output:
#   macOS:   target/release/bundle/dmg/*.dmg + macos/*.app.tar.gz (updater)
#   Windows: target/release/bundle/{msi,nsis}/*
#   Linux:   target/release/bundle/{deb,appimage,rpm}/*

# Signing updater bundles (optional, host-distributed updates need this):
#   pnpm tauri signer generate -w ~/.kinai/keys/updater.key --ci
#   # then put the contents of the .pub file in tauri.conf.json plugins.updater.pubkey
#   # and the private key path in TAURI_SIGNING_PRIVATE_KEY when running build
```

## 🔧 Forking & rebranding

KinAI's source has **one place to edit** when forking — the repository URL
lives in `Cargo.toml`'s `[package].repository` field, and every other spot
that needs it reads it at compile time via `env!("CARGO_PKG_REPOSITORY")`:

- The HTTP `User-Agent` strings sent by `web_search` / `x_search`.
- The "GitHub" link on the About page (`/about`).
- The full version-identifier string the Settings page lets you copy.

Two things you'll also want to update for a real release of your fork:

1. **`tauri.conf.json` → `plugins.updater.endpoints`** — point this at your
   fork's releases JSON, otherwise auto-updates check the wrong repo.
2. **`tauri.conf.json` → `identifier`** — change `ai.kin.desktop` to your
   own reverse-DNS bundle id so macOS treats your app as separate.

Nothing else in the source tree has hardcoded URLs, account names, or
machine paths — the user's home directory (`~/.kinai/`) is auto-resolved,
the LAN scanner discovers its own subnet, and the LLM backend / Exa key /
hotkey / family name are all entered in-app at setup time. Everything is
designed to run unchanged on every household's hardware.

## 🤝 Contributing

KinAI is community-built. The way you can help today:

1. **Ship the MVP on more backends.** Test with TGI, mlc-llm, Open WebUI variants, and report what auto-detect misses.
2. **Skin & polish.** SvelteKit + Tailwind — pull requests with cleaner empty states or dark/light theme variants land fast.
3. **Tool authors.** Write a tool (Rust function + JSON schema). Drop it in [`src/tools/`](src/tools/) and wire into [`registry.rs`](src/tools/registry.rs).
4. **Bug reports.** Open an issue with your OS, backend, and the `~/.kinai/kinai.log` excerpt.

PRs that touch the protocol, JWT format, or context pipeline get extra review attention — please open an issue first.

## 📜 License

MIT — see [LICENSE](LICENSE). Forever free, forever yours.

---

<div align="center">

**KinAI — bring your family their own AI, on your own terms.**

[Host guide](docs/host-guide.md) · [Client guide](docs/client-guide.md)

</div>
