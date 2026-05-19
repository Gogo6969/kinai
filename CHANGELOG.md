# Changelog

All notable changes to KinAI are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.15] — 2026-05-19

### Fixed

- **Client peers couldn't see Telegram threads in their KinAI
  sidebar.** The `list_threads` and `load_thread` Tauri commands
  read only the client's *local* DB (filtered by `HOST_PEER`),
  but Telegram-originated threads live in the *host's* DB tagged
  with the client's invite_id and never made it to the client's
  local DB. Result: even though the host correctly fanned out
  `Envelope::Message` over the WebSocket and the client received
  it, the sidebar refresh couldn't find the matching thread row
  and the Telegram conversation stayed invisible.
  Both commands are now mode-aware — in Client mode they send
  `Envelope::ListThreads` / `LoadThread` to the host and await
  the response (10s timeout, falls back to local DB on failure).
  The host returns the threads + messages filtered by the
  client's invite identity, so Telegram conversations now show
  up in client sidebars and load fully on click.

## [0.2.14] — 2026-05-19

### Fixed

- **Telegram → KinAI fan-out.** Messages sent from a paired
  Telegram chat were persisted on the host but never surfaced to
  the KinAI UI in real time — the chat only appeared after the
  user manually reloaded their thread list. The router now emits
  `kinai://message` for the host's own Telegram thread and pushes
  `Envelope::Message` over the WebSocket for paired client peers,
  matching the in-app chat fan-out. The sidebar also auto-refreshes
  its thread list when it sees a message for an unknown thread, so
  a fresh Telegram conversation appears in the sidebar without a
  reload.

## [0.2.13] — 2026-05-19

### Added

- **Telegram is now a first-class family chat surface.** Every
  family member can chat with KinAI from their phone over Telegram
  — same model, same memory, same threads. The host owner sets up
  **one shared bot via @BotFather** (once), and each member pairs
  their own Telegram from *Settings → Telegram* via QR code. Each
  user has their own private 1:1 chat with the bot; other family
  members never see your messages.
- **Bidirectional sync.** Messages sent from Telegram show up in
  the matching KinAI thread instantly. Assistant replies generated
  inside KinAI on that thread are also mirrored back to your
  Telegram chat, so the conversation stays in step on the phone.
- **Slash commands work in Telegram.** `/help`, `/pic`, `/picHQ`
  behave identically to in-app — and `/pic`-generated images are
  uploaded as actual Telegram photos (not just links).
- **Client-mode pairing via WebSocket.** The Connect-Telegram QR
  flow works for the host AND for every connected client peer.
  Behind the scenes, the client's Tauri command sends a
  `RequestTelegramPair` envelope to the host, which mints a token
  bound to that client's invite identity and replies with the
  scannable URL. No bot tokens ever leave the host machine.
- **Changelog window.** Pops up once after each update so you can
  see what's new. Built from this `CHANGELOG.md` embedded in the
  binary; "Got it" stamps the seen version into your local config.

### Fixed

- **Bot-username advertised over Welcome.** The host's `Welcome`
  envelope now carries `host_telegram_bot` so client peers can
  show *"Family bot: @vidfame_kinai_bot"* and enable their pair
  button without a separate round-trip.

## [0.2.11] — 2026-05-19

### Added

- **Telegram foundation (host-only).** First slice of the Telegram
  integration: bot token Setting, long-poll loop against the
  Telegram Bot API, router that handles `/start` pairing,
  inbound messages → KinAI LLM → reply back, slash commands,
  `/pic` outbound as actual photos. Pairing UI initially gated to
  the host peer; client-mode pairing followed in v0.2.13.

## [0.2.10] — 2026-05-18

### Fixed

- **Window close button** now hides the main window on macOS
  instead of destroying it, matching the Windows behavior added in
  0.2.9 — relaunching KinAI from the Dock now brings the window
  back instead of silently no-op'ing.
- **Overlay transparency on macOS** also bumped to 90% (was
  Windows-only in 0.2.9). Improves readability over light desktop
  backgrounds.
- **Settings save feedback** flashes a green ✓ for 2.5s next to the
  Save button after every successful settings change.
- **macOS image download** routed through a Rust IPC command so
  WKWebView's App Transport Security doesn't block plain-HTTP
  attachment downloads from the host.
- **Prompt debug panel** ("🔍 prompt" toggle under each assistant
  message) strips inline image data URLs before serializing — a
  single attached PNG used to bloat the JSON to 7-8 MB and lock
  the WebView when the user clicked the button.

## [0.2.9] — 2026-05-18

### Added
- **Windows client.** Windows 10/11 (x86_64) is now a first-class
  client platform. Pair to a Mac host via 6-character invite code
  or full `kinai://join?…` link, send messages, get streamed replies
  back from the Mac mini's local LLM. The installer is an MSI (or
  NSIS-style `.exe`) under *Releases → Assets*. Windows code-signing
  isn't in this release yet, so SmartScreen will warn the first time
  — click *More info → Run anyway* (one-time). The full source is
  public, so the binary is auditable end-to-end. **Windows is
  client-only for now** — hosting still requires macOS.
- **`/pic` and `/picHQ` slash commands.** Optional. When the host
  owner points KinAI's Settings → Image generation at a ComfyUI
  server (e.g. an Olares One on the LAN), every paired family device
  gets two new chat commands: `/pic <prompt>` (Z-Image Turbo,
  ~5s, 1280×720) and `/picHQ <prompt>` (Z-Image Base, ~30s,
  1024×1024). Optional `WxH` prefix overrides the size
  (`/pic 1024x1024 a sunset over Miami`). Empty `base_url` =
  feature disabled, no flags visible.
- **`/help` and `?`.** Lists available slash commands in chat.
  Adapts based on whether image-gen is configured.
- **Slash-command autocomplete popup** in the chat input — type
  `/`, get a menu, ↑↓ to navigate, Tab to accept, Esc to dismiss.
  Resolution hint surfaces under the highlighted command.
- **`↗ Open` + `↓ Download` buttons** under every chat image.
  Tauri's WebView blocks the browser-native right-click options
  (Open in New Window, Save Image As…) on both platforms, so the
  buttons route through the shell + fs plugins explicitly. Click
  the image itself = Open. Download pops a Save dialog.
- **Host auto-pulls new GitHub releases hourly** and stages them
  for connected family clients. No more manual deploy steps —
  publish a release on GitHub, every family Mac auto-updates from
  the host within hours. (Windows in-app auto-update is still
  manual until we ship the `.nsis.zip` updater bundle alongside.)

### Fixed
- **Cross-machine URL-paste pairing.** Pasting a `kinai://join?…`
  invite link on any device that wasn't the one that minted it
  used to fail with *"Invalid signature"* — `peek_token` was
  validating the JWT against the LOCAL public key, but the host
  signed with a DIFFERENT keypair. Signature is now only verified
  on the WebSocket handshake (where the host actually has the
  matching key). Unblocked Windows pairing entirely.
- **Sidebar showed the invite label as the host name** ("WindowsPC"
  instead of "Our Family"). Now uses `hostInfo.family_name` from
  the Welcome envelope, with the invite label as a small subtitle
  *"joined as WindowsPC"*.
- **Window can't reopen after close.** On Windows, closing the X
  destroyed the main window; relaunching the .exe trickled into
  the single-instance handler which called `show()` on a destroyed
  window — silent no-op. Now the X hides the window; show()
  brings it back.
- **Overlay too transparent on Windows.** WebView2 doesn't render
  backdrop-blur as effectively as macOS NSVisualEffectView. Bumped
  the glass-card opacity from 70% → 90%.
- **Visible feedback when saving Settings** — green ✓ pill next to
  the Save button for 2.5s after success, red inline error on
  failure.
- **macOS image download "TypeError: Load failed".** WKWebView's
  App Transport Security blocked JS `fetch()` against the host's
  plain-HTTP `/v1/pic/...` URL (even though `<img src>` loaded the
  same URL fine via a different code path). Download now goes
  through a Rust IPC command using `reqwest`, which has no ATS
  restriction.
- **Auto-updater on macOS** stopped producing the signed
  `.app.tar.gz` updater bundle because the deploy script's
  internal tar invocation included AppleDouble metadata files
  (`._KinAI.app`) that Rust's `tar` crate then tried to write
  alongside the real `.app` directory and crashed with *"failed
  to unpack"*. Added `COPYFILE_DISABLE=1` to the tar step.

### Known limitations
- macOS only for hosting. Windows + Linux host support is a later
  roadmap item.
- **Windows auto-update inside the app is not wired up yet.**
  Tauri's `--bundles updater` flag isn't producing the `.nsis.zip`
  / `.msi.zip` auto-update bundles we'd need to push updates to
  Windows clients silently. Until that's resolved, Windows users
  re-download the `.msi` from GitHub Releases when notified of a
  new version. macOS clients auto-update through the host on every
  release.
- **SmartScreen warning on first install of the Windows MSI.**
  Cleared by a one-time *More info → Run anyway*. Goes away once
  we buy a Windows code-signing certificate ($200–500/yr).

## [0.2.6] — 2026-05-16

### Added
- **Signed + notarized macOS builds.** KinAI is now signed with Apple's
  Developer ID Application certificate (Wolfgang Gabler, team
  L5VWNX44MY) and notarized through Apple's notary service via
  `notarytool` + an App Store Connect API key. End users no longer see
  *"KinAI can't be opened because Apple cannot check it for malicious
  software"* on first launch — just the standard *"KinAI was
  downloaded from the internet — open?"* prompt that any signed
  third-party app gets. `spctl --assess` now reports
  `source=Notarized Developer ID`.
- `scripts/deploy.sh` sources `~/.kinai/keys/apple.env` for local
  signing credentials; CI consumes the same credentials via GitHub
  secrets.

### Fixed
- **Signing the .app failed on developer Macs with iCloud Drive
  enabled** ("resource fork, Finder information, or similar detritus
  not allowed"). macOS's `bird` daemon re-attaches
  `com.apple.FinderInfo` + `com.apple.fileprovider.fpfs#P` xattrs to
  any new bundle under `~/Documents` faster than the build can strip
  them, and `codesign --force` refuses to sign a bundle with those
  present. Workaround in `scripts/deploy.sh`: tauri-bundler now
  produces an UNSIGNED `.app`, which we copy to `/tmp/kinai-sign/`
  (bird ignores `/tmp`), then xattr-strip + codesign + notarytool +
  staple there, then copy the signed/stapled bundle back into the
  project tree for DMG packaging and updater tarballing.

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

- macOS only. Windows and Linux are coming.
- The vision pipeline disables tools for image turns — most providers
  reject function calling on multipart messages and we'd rather get
  a clean image analysis than silent failures.

## [0.1.x] — early iteration

Internal pre-release versions (0.1.0 through 0.1.38). Folded into 0.2.0.
