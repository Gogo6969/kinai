# Changelog

All notable changes to KinAI are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.5] — 2026-05-16 *(first public release)*

Patches folded in on top of 0.2.0 before the GitHub launch:

### Added
- **Settings → Follow on X · @gogo6969 + GitHub** links under the
  version line, so every install has a reachable maintainer channel
  without us shipping telemetry or phone-home.
- **Auto-recheck for updates on every successful (re)connect** — when
  the client's WebSocket handshake completes, the updater polls the
  host's manifest within 1 second. Result: a host bump propagates to
  clients within a few seconds of their next reconnect, instead of
  waiting up to the 4 h periodic interval.
- Project pitch reframed across `README.md`, `Cargo.toml`, and
  `tauri.conf.json`: **KinAI is the family-sharing layer for the local
  LLM you already run.** It's not the model itself.

### Fixed
- **Manage Family showed the same device twice** during reconnects —
  the server now evicts any prior peer entry with the same invite
  (`claims.sub`) before inserting the new one. "One invite, one live
  device" is now enforced, matching the recommended best practice of
  one invite per device.
- **Updater silently ignored host LAN manifests** because Tauri's
  default rejects non-HTTPS URLs. Enabled
  `plugins.updater.dangerousInsecureTransportProtocol: true` so LAN
  HTTP endpoints are accepted (signature verification still runs).

## [0.2.0] — 2026-05-15

The first public release. v0.2 covers everything a family needs to actually
*use* KinAI day-to-day: chat, attachments, vision, image search, multi-device
sync over your LAN, and self-distributed updates.

### Added

#### Attachments
- **PDF input** — drag, drop, paste, or attach a PDF; the host extracts text
  server-side via `pdf-extract` and inlines it into the LLM prompt. Works
  with any chat model.
- **Image input** — same UX as PDFs. Routed to a vision-capable endpoint
  (see Vision below).
- **25 MB per-file cap**, enforced on both frontend and backend.

#### Vision routing
- **Per-turn routing** — image-bearing turns automatically route based on
  whether the active chat model is vision-capable:
  - **Chat model is vision-capable** (Claude Sonnet/Opus, Gemini Pro/Flash,
    GPT-4o, llava, qwen-vl, moondream, minicpm-v, pixtral, cogvlm, internvl,
    phi-3-vision, …) → used directly, no rerouting.
  - **Otherwise** → routed to the configured Vision endpoint.
- **One-shot failover** — on transient cloud errors (5xx, 429, "high demand",
  RESOURCE_EXHAUSTED, "overloaded", timeouts) the request retries against
  the failover endpoint exactly once.
- **Three quick-fill presets in Settings + onboarding**: Gemini 2.5 Flash,
  Claude 3.5 Haiku (Anthropic OpenAI-compat alias), Local llava via Ollama.
  Presets fill `base_url` + `model` only; the user supplies the API key.
- **OpenAI multipart wire format** — same payload shape for every supported
  provider (Gemini, Anthropic, vLLM, Ollama). One code path covers all.
- **Test vision** button sends a 1×1 PNG with "what color?" to verify the
  endpoint accepts multipart before the first real request.

#### Image search
- New built-in `image_search` tool — find pictures on the web from a query.
- **DuckDuckGo mode (default)** uses Wikimedia Commons under the hood —
  free, CC-licensed, no API key. Strong coverage for landmarks, history,
  people, biology.
- **Exa mode** reuses the existing Exa API key. Each result contributes
  its primary image via `contents.images`.
- Model is instructed to call it for "show me a photo of X" / "what does
  Y look like" intent.

#### Safe inline image rendering
- The markdown pipeline now renders `![](url)` as actual `<img>` tags
  inside chat bubbles — for image-search results, model-generated markdown,
  or attached image previews.
- **URL allow-list**: `https://`, `http://` (host LAN), `data:image/*;base64,*`.
  Anything else (`javascript:`, `file:`, …) renders as a visible red
  "blocked image" pseudo-link, never as an executable element.
- Lazy-loaded, max-height 320 px, theme-aware border.

#### Multi-device infrastructure
- **Host-distributed auto-updates** — host stages signed `.app.tar.gz` +
  `.sig` under `~/.kinai/updates/<version>/<target>/` and serves them at
  three new endpoints:
  - `GET /v1/update/manifest` → Tauri-format JSON
  - `GET /v1/update/bundle.tar.gz` → the binary
  - `GET /v1/update/bundle.tar.gz.sig` → Minisign signature
- **Client updater** — polls the host first (on launch + every 4 h), falls
  back to GitHub Releases if the host has been unreachable for >24 h.
- **In-app update banner** with version, source ("from your host" /
  "from GitHub"), download progress %, Install & restart, Dismiss.
- **Signing keypair** generated via `pnpm tauri signer generate`; private
  key kept outside the repo at `~/.kinai/keys/updater.key`, public key
  baked into every install via `tauri.conf.json`.
- **Per-peer context isolation** — every thread + memory note tagged with
  the connecting peer's invite short_code. `list_threads`, `load_messages`,
  `search_memory` all filter by `peer_id`. Family members never see each
  other's chats, prompts, or summaries.
- **Auto-reconnect supervisor** with exponential backoff (2s → 30s cap)
  + "Reconnect now" button in the sidebar.
- **6-character invite codes** — clients can type a code instead of pasting
  a full `kinai://join?…` URL. Host's `GET /v1/invite/redeem?code=XXX`
  resolves it to the JWT.
- **Live host info on every client** — Welcome envelope advertises model,
  search engine, vision label, host version. Client Settings shows it
  read-only.

#### Onboarding
- New **Vision setup step** in the host wizard between Search engine and
  Start hosting. Skippable. Three preset shortcuts + Test vision inline.
- **Saved-host fallback** on the Client setup page — if mDNS comes up
  empty, the previously-connected host appears in the list with a "saved"
  badge so a fresh invite code can still be redeemed.
- **"Scan again"** button on the Client setup page that re-fires the
  mDNS query (covers the case where the user just granted Local Network
  permission).
- **"Forget host"** flow on the Client setup page that disconnects and
  drops the mode back to Unconfigured.
- **Back button** on the Client setup page so users can leave without
  having to restart the app.

#### Settings & UX polish
- **Mode-aware Settings** — clients see "You" (display name) + read-only
  "Host" info (family, model, search engine, vision, host version) +
  Overlay & Theme. They don't see Local LLM, Search engine, or Test-tool
  cards (those belong to the host).
- **Live sidebar connection indicator** — green/amber/red dot driven by
  the actual WebSocket state, with the host's last error message
  surfaced inline.
- **Thinking-dots placeholder** stays visible during the pre-token phase
  of tool-using turns (reasoning + tool call before any visible token).
- **Update banner** at the top of the chat when a new version is staged.

### Changed

- **JWT audience validation** — the host's `stats.host_url` is now derived
  from the LAN IP (same form `invite::create` uses), not from
  `bind_addr` which is a listen spec like `0.0.0.0`. Previously every
  Hello frame was rejected with `InvalidAudience`. *(That's why client
  pairing wasn't working before this release.)*
- **Privacy fix** — the chat-turn pipeline used to `broadcast()` user
  messages and AssistantDone events to *every* connected peer + the
  host's UI. Now it `tx.send()`s only to the originating peer. Other
  family members no longer see each other's conversations.
- **Empty-state copy** — "KinAI remembers what your family says" was
  misleading (it implied shared memory). Now reads "KinAI remembers
  *your* conversations. Private to you. Other family members can't see
  your chats."
- **Updater** — switched to host-first with GitHub fallback (was
  GitHub-only). Required enabling
  `plugins.updater.dangerousInsecureTransportProtocol` so LAN HTTP
  endpoints are accepted (Tauri's default rejects non-HTTPS).
- **`run_chat_turn`** now upserts the thread row on first SendMessage
  from a client (the client's `thread_id` only lives in its local DB
  until then; without the upsert the FK on `messages` would reject
  the insert).
- **`send_message` command** is mode-aware — in Client mode it forwards
  `Envelope::SendMessage` over the active WebSocket instead of running
  the LLM pipeline locally.
- **Reconnect race** — `connect_client` aborts any in-flight client
  task before spawning the replacement, so two parallel WS connections
  can't fight over `net.client_tx`.

### Fixed

- **Client Mac crash on launch (0.1.25)** — `tokio::spawn` inside Tauri's
  synchronous `setup()` callback panicked with "must be called from
  within a Tokio runtime"; reverted to `tauri::async_runtime::spawn`.
- **Sidebar dot stuck on orange** — the `kinai://client-status` event
  fired before the UI subscribed; status now persists in `RuntimeStats`
  so `refreshStats()` hydrates the indicator on every load.
- **Chat ordering bug** — in Client mode, assistant replies were stuck
  as streaming bubbles forever (never moved to `messages[thread_id]`).
  Result: all user messages clustered at the top, all assistant replies
  at the bottom. Fixed by handling `Envelope::AssistantDone` correctly.
- **Discovery events missed** — moved the `onDiscovery` listener from
  `/client/+page.svelte`'s `onMount` to the global store's
  `startListening` so events fired during app boot aren't lost.
- **TPS readout nonsense** — clamps to 0 when generation phase is under
  200 ms or output is empty (was reporting 41 000 tok/s for empty
  responses).
- **Tool loops searching forever** — pipeline now caps at 5 tool rounds
  and forces a final no-tools synthesis turn so the model doesn't get
  stuck in "search → search → search".

### Security

- The updater plugin's `pubkey` is now committed in `tauri.conf.json`;
  the matching private key is generated locally at
  `~/.kinai/keys/updater.key` and **must never** be committed. The repo
  `.gitignore` already excludes `.kinai/`.
- Three Rust warnings cleaned up via `cargo fix` (unused imports,
  unnecessary `mut`).

### Known limitations

- No Windows build yet. The GitHub Actions workflow includes a
  `windows-latest` runner but the first real Windows compile has not
  been validated. See `Compatibility` in the README.
- The host serves bundles only for its own architecture (a Mac mini
  host won't have Windows binaries staged for Windows clients to
  auto-update until CI fills `~/.kinai/updates/<v>/windows-x86_64/`).
- The vision pipeline disables tools for image turns — most providers
  reject function calling on multipart messages and we'd rather get
  a clean image analysis than silent failures.

## [0.1.x] — early iteration

Internal pre-release versions (0.1.0 through 0.1.38). Folded into 0.2.0.
