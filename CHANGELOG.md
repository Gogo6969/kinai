# Changelog

All notable changes to KinAI are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.29] — 2026-05-21

### Added

- **Invite the family → Expires NEVER option.** The invite-
  generation form now has an *Expires* dropdown with `7 days`,
  `30 days`, `90 days`, `1 year`, and `Never`. Picking *Never*
  encodes a ~100-year TTL on the JWT (kept under any practical
  upper bound) and the invite list renders these rows as
  "Never expires" instead of a date. Existing time-bounded
  invites still expire as before; the option is just additive.
  You can always revoke a never-expiring invite from the same
  page.

### Changed

- **Repo description / README pitch corrected.** Previous copy
  implied "one Mac runs the LLM" — that's wrong. KinAI hosts on
  a Mac, but the actual LLM can live anywhere on your LAN:
  Ollama on a Linux box, vLLM on a workstation, LM Studio on
  Windows, llama.cpp on a Mac mini, an Olares — the host just
  bridges to whatever OpenAI-compatible endpoint you point at.
  README intro + Cargo.toml description + repo description all
  rewritten to reflect the bridge model accurately, and now
  explicitly note that phones / tablets reach KinAI through
  the family's Telegram bot (no native iOS / Android app yet).

## [0.2.28] — 2026-05-21

### Changed

- **`/fast` and `/deep` are now sticky per thread.** Previously
  the slash prefix only routed a single turn — the next plain
  message snapped back to the global default (fast), which felt
  surprising. Now typing `/deep <prompt>` switches the active
  slot for the current thread, and subsequent plain-text
  messages keep going to deep until you type `/fast` to switch
  back. The per-message model badge from v0.2.27 (⚡ / 🧠 next
  to the metrics) shows which slot answered each turn, so you
  always see the current state. The slot choice is persisted
  on the thread row, so it survives KinAI restarts.

## [0.2.27] — 2026-05-20

### Added

- **Per-reply model badge.** Each assistant message's metrics row
  now ends with the model that produced it, abbreviated to fit:
  e.g. `· ⚡ gpt-oss-20b` for the fast slot, `· 🧠 qwen2.5:72b`
  for the deep slot, or just the model name when only one slot is
  configured. The full model id is in the hover tooltip. The
  badge is hidden for slash-command turns (no LLM involved). Same
  rendering on macOS and Windows — it's in the shared Svelte
  `MessageBubble` component, and the model name flows through the
  per-turn metrics that both chat paths (host's
  `commands::send_message` and client peers' WS `run_chat_turn`)
  already populate, so the badge shows up regardless of which
  device sent the message.

## [0.2.26] — 2026-05-20

### Changed

- **Change-backend screen now configures both LLM slots in one
  step.** When you open *Change backend* (host owner only),
  the configure step now shows two model cards side-by-side:
  the existing **Fast model** plus the new **Deep model
  (optional)**. Each card has the same fields (provider, base
  URL, model, context window, test connection) plus its own
  Active / Paused toggle. The detected-backend list at the top
  has explicit **Fast** and **Deep** assign buttons per
  backend, so a single discovered server can be assigned to
  either slot — or to both for testing.
- **No more auto-rescan when entering the screen.** Backend
  discovery (Localhost + Network scan) now runs only on
  explicit user action — the *Localhost* or *Network* buttons.
  The screen used to auto-rescan whenever the cache was older
  than 1 hour, which fired on most visits and felt noisy.
  Cached results stay visible across visits; you can refresh
  on demand. The one exception is a fresh KinAI install with
  no scan history yet — that still runs one initial scan so
  the new-user flow isn't an empty list.

## [0.2.25] — 2026-05-19

### Added

- **Two-model chat: `Fast model` + optional `Deep model`.**
  KinAI's Settings now exposes a second LLM slot — a typically
  larger, slower model — alongside the existing one. Two new
  slash commands route a turn to a specific slot:
  - `/fast <prompt>` → the existing default model
  - `/deep <prompt>` → the new secondary model
  Both slashes appear in the autocomplete only when BOTH slots
  are active (configured + not paused) — a single-model setup
  hides them so the menu stays uncluttered. The routing token
  is stripped before the LLM sees the prompt; the original
  message text (including the `/fast` or `/deep`) is preserved
  in chat history.
  Falls back gracefully: if only `deep` is configured, plain
  messages route there; if only `fast` is configured, behaviour
  is identical to v0.2.24 and earlier.
- **Pause toggle per model.** Each slot has an Active / Paused
  switch in Settings. A paused slot is excluded from slash
  autocomplete AND skipped during default routing — useful for
  temporarily disabling one without losing its configuration.

## [0.2.24] — 2026-05-19

### Fixed

- **🎤 Microphone now actually works** (the v0.2.x mic regression
  that survived v0.2.19, .21, .22, and .23). Users granted KinAI
  **both** *Microphone* AND *Speech Recognition* in System
  Settings and still saw "Speech recognition is blocked" because
  KinAI's signed binary had NO hardened-runtime entitlements at
  all. macOS's hardened runtime gates every privileged capability
  — the `NSMicrophoneUsageDescription` in Info.plist makes the
  OS *prompt* for permission, but the OS won't *grant* the
  underlying syscall unless the binary also carries the matching
  `com.apple.security.device.audio-input` entitlement. The
  Privacy toggle becomes a no-op against an unentitled binary.

  Compare with the Claude desktop app on the same machine:
  ```
  Claude:  com.apple.security.device.audio-input  = true ✓
  KinAI:   <no entitlements>                              ✗
  ```

  Added `entitlements.plist` with `audio-input`, `camera` (for
  future vision-via-webcam), and `allow-jit` (WKWebView JS), and
  updated `scripts/deploy.sh` to pass `--entitlements` to
  `codesign`. Verified post-build via
  `codesign -d --entitlements -` that the bits are present, and
  webkitSpeechRecognition.start() actually succeeds now.

## [0.2.23] — 2026-05-19

### Fixed

- **shellOpen actually works now (Privacy buttons + every other
  external link).** v0.2.22 added `plugins.shell.open` as a regex
  string to whitelist macOS Privacy-pane URLs, but I missed that
  Tauri 2's shell plugin wraps the regex with `^…$` anchors,
  and my pattern had no trailing match for "rest of the URL". So
  `^^(https?://|x-apple.systempreferences:|ms-settings:)$`
  required URLs to END right after the scheme — meaning EVERY
  URL was silently rejected, including http(s) ones used by the
  X handle / GitHub / website links in Settings. Appended `.+`
  to the regex so it matches `<scheme>` followed by the rest of
  the URL. Tested by clicking the buttons in v0.2.23 → System
  Settings actually opens to the right Privacy pane now.

## [0.2.22] — 2026-05-19

### Fixed

- **Mic-error Privacy-pane buttons now actually open System
  Settings.** v0.2.21 added two helpful "Open Mic settings" /
  "Open Speech Recognition settings" deep-links next to the
  permission-blocked error, but clicking either was a no-op —
  Tauri 2's shell plugin scope rejects any URL whose scheme
  isn't in the `plugins.shell.open` regex, and we only had the
  default (http/https). The custom `x-apple.systempreferences:`
  scheme was silently denied. Extended the scope regex to
  whitelist macOS's Privacy-pane deep-link scheme (and
  `ms-settings:` for the eventual Windows equivalent), so both
  buttons now jump straight to the right pane. The error handler
  also surfaces a fallback message with manual instructions if
  the shell open ever fails for any other reason.

## [0.2.21] — 2026-05-19

### Fixed

- **Overlay slash commands (`/help`, `/pic`, `/picHQ`) now
  actually render their reply.** The Spotlight-style overlay
  showed an empty result area for any non-streaming reply: the
  overlay's `onAssistantDone` listener only flipped `busy` off
  and never read `message.content`, so slash-command outputs
  (which the host sends in one shot, without streaming tokens)
  ended up invisible. Streaming LLM replies worked because they
  fed `streamingContent` via `onToken`. Now `onAssistantDone`
  backfills `streamingContent` from `message.content` when it's
  empty, so both streaming and non-streaming paths render in the
  overlay.

- **Mic error UX now explains the macOS two-permission model.**
  Users granted KinAI Microphone access in System Settings and
  still saw "Speech recognition is blocked" — because macOS
  requires a **second** permission, *Speech Recognition*, in a
  separate Privacy pane. The Web Speech API can't tell them
  apart (both return `not-allowed`), so the v0.2.19 error
  message only mentioned Microphone, leaving users stuck.
  Now the error explicitly says both are required, includes a
  "quit and relaunch" hint (WebKit caches the denied state for
  the lifetime of the process), and exposes **two** deep-link
  buttons: *Open Mic settings* and *Open Speech Recognition
  settings*. Both link to the right Privacy panes in System
  Settings on macOS 13+. Windows / Linux hide the buttons since
  there's no equivalent URL.

## [0.2.20] — 2026-05-19

### Fixed

- **Windows in-app auto-update is now actually wired up.** All
  the host-side plumbing has been in place since v0.2.0 — the
  `/v1/update/manifest` endpoint serves a per-platform Tauri
  updater manifest, `scripts/deploy.sh::stage_windows_update`
  pulls the latest Windows updater bundle from the
  `test-windows.yml` workflow's artifact, and the client's
  updater plugin polls the host on every reconnect. But the
  workflow's `actions/upload-artifact@v4` step was only uploading
  2 of the 6 produced files (`.msi` and `.exe` — silently dropping
  the four `.msi.zip` / `.nsis.zip` / `.sig` patterns that the
  multi-line glob couldn't match). The host's stage step saw no
  updater bundle, the manifest never advertised `windows-x86_64`,
  and Windows clients sat on whatever version they installed
  manually with no in-app update prompt. Fixed by replacing the
  fragile multi-line glob with a single `bundle/**` capture that
  catches every file under the tauri-bundler output directory
  (~7 files including all the auto-update sidecars). Windows
  clients on v0.2.7+ should now receive an in-app update banner
  on every release the same way macOS clients do.

## [0.2.19] — 2026-05-19

### Added

- **Project website surfaced everywhere.** Added
  [kin-ai.replit.app](https://kin-ai.replit.app) as the GitHub
  repo homepage, the `homepage` field in `Cargo.toml`, a *Website*
  button on the About page, and a third link in the Settings
  footer next to *Follow on X* and *GitHub*. Every install now
  has a one-click path to the public site.

### Changed

- **Friendlier mic-button errors.** Pressing the overlay's mic
  button without microphone permission used to surface a raw
  `Speech recognition error: not-allowed` string that lingered in
  the UI. The message is now human-readable
  ("Microphone access is blocked. Open System Settings → Privacy
  & Security → Microphone and turn KinAI on."), shows a **Fix**
  button that deep-links to macOS's Privacy → Microphone pane,
  and auto-dismisses after 8 seconds. A small ✕ dismisses it
  manually too. Same treatment for `audio-capture` (no mic
  detected) and `network` (offline) errors.

## [0.2.18] — 2026-05-19

### Fixed

- **Quick-chat overlay hotkey no longer drags the main KinAI
  window forward.** Pressing the global hotkey (default
  `CmdOrCtrl+Space`) was popping the small "Ask KinAI…" overlay
  AND pulling the full app window into view at the same time. The
  cause was a cross-platform side effect of `set_focus()` on the
  overlay: on macOS focusing any KinAI window activates the app,
  which brings every visible KinAI window to the foreground;
  Windows does the equivalent. The toggle now hides the main
  window first so the only KinAI surface left visible is the
  Spotlight-style input. Bring the main window back via tray →
  *Open KinAI* (or by relaunching from the Dock / Start menu).

## [0.2.17] — 2026-05-19

### Changed

- **KinAI→Telegram echo: combine Q&A into one bot message with a
  blockquote question.** v0.2.16 sent the user's question and the
  assistant's reply as two separate bot messages, which made the
  question look like the bot was talking to itself (Telegram bots
  can't impersonate the human, so both ended up styled as bot
  messages). The two posts now collapse into a single bot message
  where the question is rendered inside a Telegram
  `<blockquote>` (HTML parse_mode — gives it the indented
  left-bar look Telegram users recognise from forwards / reply
  previews) and the assistant reply flows below it in plain text.
  /pic photo replies use the same combined format as a caption.
- **"Typing…" indicator while the LLM thinks.** Before kicking off
  the LLM turn, the bot fires a one-shot `sendChatAction(typing)`
  so the phone user sees "Bot is typing…" in the Telegram chat
  while the response is being generated. Best-effort, ~5s default
  on Telegram's side.

## [0.2.16] — 2026-05-19

### Fixed

- **User questions typed in KinAI weren't mirroring to Telegram.**
  v0.2.13's bidirectional sync only echoed assistant replies — when
  the user typed a question from KinAI's UI on a Telegram thread,
  the answer arrived on Telegram but the question itself didn't,
  leaving the phone chat with replies that had no visible prompt.
  KinAI now also pushes the user's question to Telegram (prefixed
  `💬 You:` since Telegram bots can't impersonate the human user).
  Same gating: only fires on Telegram threads for a paired peer,
  and skips messages that originated on Telegram in the first
  place (to avoid bouncing them back).

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
