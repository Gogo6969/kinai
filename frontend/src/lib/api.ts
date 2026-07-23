import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

export type Mode = 'unconfigured' | 'host' | 'client';
export type Theme = 'dark' | 'light' | 'system';

export interface LlmSettings {
  provider: string;
  base_url: string;
  model: string;
  context_window: number;
  api_key: string | null;
  temperature: number;
  max_tokens: number;
  system_addendum: string;
  /** Pause toggle. When `false` this slot is hidden from slash menus
   *  and skipped during message routing. Defaults to `true` for the
   *  fast slot, `false` for the deep slot (until the user fills it). */
  enabled: boolean;
}

export interface HostSettings {
  bind_addr: string;
  port: number;
  family_name: string;
  mdns_enabled: boolean;
  rate_limit_rpm: number;
}

export interface PinnedHost {
  label: string;
  host_url: string;
  host_token: string;
}

export interface ClientSettings {
  host_url: string | null;
  host_token: string | null;
  host_label: string | null;
  display_name: string;
  pinned_hosts: PinnedHost[];
}

export interface OverlaySettings {
  hotkey: string;
  always_on_top: boolean;
  font_size: number;
  auto_close_on_blur: boolean;
}

export type SearchEngine = 'duckduckgo' | 'exa';

export interface ToolSettings {
  web_search: boolean;
  x_search: boolean;
  calculator: boolean;
  datetime: boolean;
  image_search: boolean;
  search_engine: SearchEngine;
  search_api_key: string | null;
}

export interface VisionEndpoint {
  label: string;
  base_url: string;
  model: string;
  api_key: string | null;
}

export interface VisionSettings {
  enabled: boolean;
  primary: VisionEndpoint;
  failover: VisionEndpoint;
}

/** Image generation via ComfyUI — powers /pic + /picHQ slash commands.
 *  Empty base_url means disabled (no slash commands shown). */
export interface ComfyConfig {
  base_url: string;
  default_model: string;
}

/** Voice replies (TTS) — host-side, macOS `say` engine. `enabled`
 *  is the master switch; family members opt in per Telegram chat
 *  with /voice, and the host desktop gets a speak button. */
export interface TtsConfig {
  enabled: boolean;
  voice_en: string;
  voice_de: string;
  /** Host desktop chat: speak finished replies automatically instead of
   *  waiting for the speak button. Telegram voice notes are unaffected. */
  auto_speak: boolean;
}

/** One installed system voice from `say -v '?'`. */
export interface TtsVoice {
  name: string;
  lang: string;
}

/** Voice input (speech-to-text) for Telegram voice messages — host-side
 *  Whisper. Disabled until a model is downloaded. */
export interface SttConfig {
  enabled: boolean;
  model: string;
}

export interface SttModelStatus {
  id: string;
  label: string;
  size_mb: number;
  downloaded: boolean;
}

export interface SttStatus {
  enabled: boolean;
  model: string;
  ready: boolean;
  models: SttModelStatus[];
}

/** Telegram bot integration — host-side. Empty bot_token means
 *  the feature is off (no long-poll loop, no UI for clients). */
export interface TelegramConfig {
  bot_token: string;
  bot_username: string;
}

export interface TelegramLinkStatus {
  bot_configured: boolean;
  bot_username: string;
  paired: boolean;
  username: string | null;
  first_name: string | null;
  paired_at: string | null;
}

/** Per-machine startup behavior. Only relevant when "Launch at login"
 *  is enabled — the OS launches KinAI with `--autostart` in argv, and
 *  the binary consults this struct to decide whether to surface the
 *  window or stay hidden in the tray. Manual launches ignore it. */
export interface StartupSettings {
  /** True (default): autostart launches keep the window hidden,
   *  tray icon only. False: window comes up immediately, just like a
   *  manual launch would. */
  autostart_minimized: boolean;
}

export interface AppConfig {
  mode: Mode;
  theme: Theme;
  /** Primary chat model. Reachable as `/fast <prompt>` when both
   *  slots are active; the default routing target for plain
   *  (non-slash) messages. */
  llm: LlmSettings;
  /** Optional secondary "deep" model. Reachable as `/deep <prompt>`.
   *  Empty + disabled by default; becomes the default routing target
   *  when `llm` is paused or unconfigured. */
  llm_deep: LlmSettings;
  /** Optional "balanced" middle slot. Reachable as `/balanced <prompt>`. */
  llm_balanced: LlmSettings;
  /** ONLINE fact-check model (per-message button; not a chat slot). */
  llm_factcheck: LlmSettings;
  host: HostSettings;
  client: ClientSettings;
  overlay: OverlaySettings;
  tools: ToolSettings;
  vision: VisionSettings;
  comfyui: ComfyConfig;
  telegram: TelegramConfig;
  tts: TtsConfig;
  stt: SttConfig;
  startup: StartupSettings;
  /** True once first-run setup finished (completed or skipped) — gates
   *  the splash + /setup wizard. Existing configured installs are
   *  auto-marked true by the backend on load. */
  setup_completed: boolean;
}

export interface ThreadMeta {
  id: string;
  title: string;
  created_at: string;
  updated_at: string;
  /** Owner peer id — 'host' for the host's own threads, the invite's
   *  6-char short_code for client peers. Stored only on the host. */
  peer_id?: string;
}

/** One cross-thread search result (see `search_messages`). */
export interface SearchHit {
  thread_id: string;
  thread_title: string;
  message_id: string;
  snippet: string;
  created_at: string;
}

/** One row in the user_facts table — the persistent per-peer memory
 *  layer the LLM can read on every turn and add to via the `remember`
 *  tool. `source` is "tool" (LLM remembered it during a chat),
 *  "extractor" (passive background extraction), or "manual" (user
 *  typed it into Settings → Memory). */
export interface UserFact {
  id: string;
  peer_id: string;
  key: string;
  value: string;
  source: 'tool' | 'extractor' | 'manual';
  source_msg_id: string | null;
  created_at: string;
  updated_at: string;
}

export interface Attachment {
  kind: 'image' | 'file';
  mime: string | null;
  name: string | null;
  data_url: string | null;
}

export interface Message {
  id: string;
  thread_id: string;
  role: 'user' | 'assistant' | 'system' | 'summary';
  sender: string;
  content: string;
  attachments: Attachment[];
  created_at: string;
  summarized_into: string | null;
  metrics?: TurnMetrics | null;
}

export interface DetectedBackend {
  provider: string;
  base_url: string;
  label: string;
  models: string[];
}

export interface TestConnectionResult {
  ok: boolean;
  models: string[];
  error: string | null;
  latency_ms: number;
}

export interface Invite {
  id: string;
  short_code: string;
  jwt: string;
  host_url: string;
  label: string;
  join_url: string;
  qr_payload: string;
  created_at: string;
  expires_at: string;
  revoked: boolean;
}

export interface ResolvedInvite {
  host_url: string;
  token: string;
  label: string;
}

export interface PeerSummary {
  id: string;
  display_name: string;
  invite_id: string;
  first_seen: string;
  last_seen: string;
}

export interface HostInfo {
  family_name: string;
  host_version: string;
  host_model: string;
  host_search_engine: string;
  host_vision: string;
  /** `@username` of the host's Telegram bot, or empty string if the
   *  host owner hasn't configured one. Drives the "Family bot: @foo"
   *  label + Connect-Telegram button on client peers. */
  host_telegram_bot: string;
  /** Active model slots the host serves, in fast→balanced→deep order.
   *  Builds the client's `/fast` / `/balanced` / `/deep` slash-menu
   *  entries (a client's local LLM config is empty). Optional because
   *  hosts ≤0.2.79 don't send it. `alive` is the connect-time liveness
   *  probe result (absent on hosts ≤0.2.80). */
  host_slots?: Array<{ slug: string; model: string; alive?: boolean | null }>;
  /** Host has the online fact-check model configured (gates the button
   *  on client peers). Absent on hosts ≤0.2.81. */
  host_fact_check?: boolean;
}

export interface RuntimeStats {
  model_loaded: string | null;
  peers_connected: number;
  host_url: string | null;
  last_first_token_ms: number | null;
  client_connected: boolean;
  client_error: string | null;
  host_info: HostInfo | null;
}

export interface ToolListEntry {
  name: string;
  description: string;
  schema: unknown;
}

export interface TokenDelta {
  client_msg_id: string;
  delta: string;
}

export interface ToolDeltaEvent {
  client_msg_id: string;
  event:
    | { kind: 'Started'; name: string; args: string }
    | { kind: 'Finished'; name: string; ok: boolean; result: string };
}

export interface TurnMetrics {
  first_token_ms: number;
  total_ms: number;
  output_tokens: number;
  tps: number;
  /** Full LLM model id (e.g. "olares/gpt-oss-20b"). Empty for slash
   *  commands. The UI abbreviates this for display. */
  model?: string;
  /** "fast", "balanced", "deep", or "" — used to render a small slot badge
   *  alongside the model name. */
  slot?: string;
}

export interface AssistantDone {
  client_msg_id: string;
  message: Message;
  metrics: TurnMetrics;
  /** Desktop auto-speak decision for this reply, computed by the host
   *  backend (auto-speak setting folded in; /voice confirmations force
   *  it). Absent on client-mode events → no playback. */
  speak?: boolean | null;
  /** True when this turn changed the config (e.g. /voice toggled
   *  auto-speak) — the store refetches config so Settings stays fresh. */
  config_changed?: boolean;
}

export const api = {
  getConfig: () => invoke<AppConfig>('get_config'),
  setMode: (args: {
    mode: Mode;
    host?: HostSettings;
    client?: ClientSettings;
    overlay?: OverlaySettings;
    tools?: ToolSettings;
  }) => invoke<AppConfig>('set_mode', { args }),
  setLlmSettings: (llm: LlmSettings) => invoke<AppConfig>('set_llm_settings', { llm }),
  /** Save the secondary "deep" LLM slot. Same shape as
   *  setLlmSettings, separate command server-side. */
  setLlmBalancedSettings: (llm: LlmSettings) =>
    invoke<AppConfig>('set_llm_balanced_settings', { llm }),
  setLlmFactcheckSettings: (llm: LlmSettings) =>
    invoke<AppConfig>('set_llm_factcheck_settings', { llm }),
  /** Markdown fact-check report for one assistant message (throws with a
   *  plain-language message on failure). Host runs locally; clients
   *  round-trip to the host. */
  factCheckMessage: (messageId: string) =>
    invoke<string>('fact_check_message', { messageId }),
  setLlmDeepSettings: (llm: LlmSettings) =>
    invoke<AppConfig>('set_llm_deep_settings', { llm }),
  setOverlaySettings: (overlay: OverlaySettings) =>
    invoke<AppConfig>('set_overlay_settings', { overlay }),
  setTheme: (theme: Theme) => invoke<AppConfig>('set_theme', { theme }),
  setStartupSettings: (startup: StartupSettings) =>
    invoke<AppConfig>('set_startup_settings', { startup }),
  setVisionSettings: (vision: VisionSettings) =>
    invoke<AppConfig>('set_vision_settings', { vision }),
  testVisionEndpoint: (args: { base_url: string; model: string; api_key?: string | null }) =>
    invoke<{ ok: boolean; latency_ms: number; error: string | null; reply: string }>(
      'test_vision_endpoint',
      { args }
    ),
  setComfyConfig: (comfyui: ComfyConfig) =>
    invoke<AppConfig>('set_comfy_config', { comfyui }),
  setTtsConfig: (tts: TtsConfig) => invoke<AppConfig>('set_tts_config', { tts }),
  ttsSupported: () => invoke<boolean>('tts_supported'),
  setSttConfig: (stt: SttConfig) => invoke<AppConfig>('set_stt_config', { stt }),
  sttStatus: () => invoke<SttStatus>('stt_status'),
  downloadSttModel: (modelId: string) =>
    invoke<AppConfig>('download_stt_model', { args: { model_id: modelId } }),
  listTtsVoices: () => invoke<TtsVoice[]>('list_tts_voices'),
  previewTtsVoice: (args: { voice: string; lang: string }) =>
    invoke<void>('preview_tts_voice', { args }),
  speakText: (text: string) => invoke<void>('speak_text', { args: { text } }),
  stopSpeaking: () => invoke<void>('stop_speaking'),
  isSpeaking: () => invoke<boolean>('is_speaking'),
  testComfyEndpoint: (args: { base_url: string }) =>
    invoke<{ ok: boolean; latency_ms: number; error: string | null }>(
      'test_comfy_endpoint',
      { args }
    ),
  testTelegramToken: (args: { bot_token: string }) =>
    invoke<{ ok: boolean; bot_username: string | null; error: string | null }>(
      'test_telegram_token',
      { args }
    ),
  setTelegramToken: (args: { bot_token: string }) =>
    invoke<AppConfig>('set_telegram_token', { args }),
  requestTelegramPair: () =>
    invoke<{ url: string; expires_in_secs: number }>('request_telegram_pair'),
  telegramLinkStatus: () => invoke<TelegramLinkStatus>('telegram_link_status'),
  unpairTelegram: () => invoke<void>('unpair_telegram'),

  detectBackends: () => invoke<DetectedBackend[]>('detect_backends'),
  scanLocalNetwork: () => invoke<DetectedBackend[]>('scan_local_network'),
  rescanKinaiHosts: () => invoke<void>('rescan_kinai_hosts'),
  localIp: () => invoke<{ ip: string | null; hostname: string | null }>('local_ip'),
  kinaiVersion: () =>
    invoke<{
      version: string;
      build_time: number;
      git_commit: string;
      target: string;
      repository: string;
    }>('kinai_version'),
  /** Reveal ~/.kinai/logs in the OS file manager (for bug reports). */
  openLogsDir: () => invoke<void>('open_logs_dir'),
  /** Stamp first-run setup as done — splash + wizard never show again. */
  markSetupCompleted: () => invoke<AppConfig>('mark_setup_completed'),
  /** One-shot test message against an arbitrary chat endpoint (wizard). */
  testLlmEndpoint: (args: { base_url: string; model: string; api_key: string | null }) =>
    invoke<{ ok: boolean; latency_ms: number; reply: string; error: string | null }>(
      'test_llm_endpoint',
      { args },
    ),
  queryModelCaps: (args: {
    provider: string;
    base_url: string;
    api_key?: string | null;
    model: string;
  }) => invoke<{ context_length: number | null }>('query_model_caps', { args }),
  listLocalModels: () => invoke<string[]>('list_local_models'),
  testLlmConnection: (args: { provider: string; base_url: string; api_key?: string | null }) =>
    invoke<TestConnectionResult>('test_llm_connection', { args }),

  listThreads: () => invoke<ThreadMeta[]>('list_threads'),
  searchMessages: (query: string) => invoke<SearchHit[]>('search_messages', { query }),
  loadThread: (threadId: string) => invoke<Message[]>('load_thread', { threadId }),
  createThread: (title?: string) => invoke<ThreadMeta>('create_thread', { title: title ?? null }),
  deleteThread: (threadId: string) => invoke<void>('delete_thread', { threadId }),
  renameThread: (threadId: string, title: string) =>
    invoke<void>('rename_thread', { threadId, title }),

  // Persistent per-user memory — Settings → Memory page reads via
  // listUserFacts, mutates via save/delete/clear. Keys are short
  // lowercase identifiers, values are short free-form strings. Saving
  // an existing key overwrites; saving a fresh key adds a row.
  listUserFacts: () => invoke<UserFact[]>('list_user_facts'),
  saveUserFact: (args: { key: string; value: string }) =>
    invoke<UserFact>('save_user_fact', { args }),
  deleteUserFact: (id: string) => invoke<void>('delete_user_fact', { id }),
  clearUserFacts: () => invoke<number>('clear_user_facts'),

  sendMessage: (args: {
    thread_id: string;
    content: string;
    client_msg_id: string;
    sender?: string;
    attachments?: Attachment[];
  }) => invoke<{ user_message: Message; assistant_message: Message; first_token_ms: number }>(
    'send_message',
    { args }
  ),
  /**
   * Re-run the assistant reply for the turn that produced
   * `assistant_msg_id`. Host-only — returns an error in client mode. The
   * new reply streams in over the same kinai:// event channel as a normal
   * send (keyed by `client_msg_id`).
   */
  regenerateMessage: (args: {
    thread_id: string;
    assistant_msg_id: string;
    client_msg_id: string;
  }) => invoke<{ user_message: Message; assistant_message: Message }>('regenerate_message', { args }),
  /**
   * Edit a previous user message and re-run from there. Everything after
   * the edited message is dropped. Host-only.
   */
  editAndResend: (args: {
    thread_id: string;
    user_msg_id: string;
    new_content: string;
    client_msg_id: string;
  }) => invoke<{ user_message: Message; assistant_message: Message }>('edit_and_resend', { args }),
  /**
   * Cancel an in-flight chat turn. Pass the client_msg_id used by the
   * original sendMessage call to cancel that specific turn; omit the
   * argument to cancel every in-flight turn (the "panic button" path).
   * Safe to call when nothing is running — silently no-ops on the host.
   */
  stopGeneration: (clientMsgId?: string) =>
    invoke<void>('stop_generation', {
      args: clientMsgId ? { client_msg_id: clientMsgId } : null,
    }),

  startHost: () => invoke<void>('start_host'),
  stopHost: () => invoke<void>('stop_host'),
  connectClient: (args: {
    host_url: string;
    token: string;
    display_name?: string;
    label?: string;
  }) => invoke<void>('connect_client', { args }),
  disconnectClient: () => invoke<void>('disconnect_client'),
  disconnectAndForget: () => invoke<void>('disconnect_and_forget'),
  reconnectClient: () => invoke<void>('reconnect_client'),

  generateInvite: (args: { label: string; ttl_days?: number }) =>
    invoke<Invite>('generate_invite', { args }),
  listInvites: () => invoke<Invite[]>('list_invites'),
  revokeInvite: (inviteId: string) => invoke<void>('revoke_invite', { inviteId }),
  consumeInvite: (code: string) => invoke<ResolvedInvite>('consume_invite', { code }),
  redeemInviteCode: (hostUrl: string, code: string) =>
    invoke<ResolvedInvite>('redeem_invite_code', { hostUrl, code }),

  listPeers: () => invoke<PeerSummary[]>('list_peers'),
  revokePeer: (peerId: string) => invoke<void>('revoke_peer', { peerId }),

  toggleOverlay: () => invoke<void>('toggle_overlay'),

  listTools: () => invoke<ToolListEntry[]>('list_tools'),
  testTool: (name: string, argsJson: string) =>
    invoke<string>('test_tool', { args: { name, args_json: argsJson } }),

  runtimeStats: () => invoke<RuntimeStats>('runtime_stats'),
  /** Liveness of the host's ACTIVE model slots (label -> alive), served
   *  from a short-TTL cache. Empty map on client instances — their menu
   *  reads liveness from the host's Welcome advertisement instead. */
  slotHealth: () => invoke<Record<string, boolean>>('slot_health'),
  checkUpdates: () => invoke<void>('check_updates'),
  installUpdate: () => invoke<void>('install_update'),
};

export const events = {
  onMessage: (cb: (m: Message) => void): Promise<UnlistenFn> =>
    listen<Message>('kinai://message', (e) => cb(e.payload)),
  onToken: (cb: (d: TokenDelta) => void): Promise<UnlistenFn> =>
    listen<TokenDelta>('kinai://token', (e) => cb(e.payload)),
  onReasoning: (cb: (d: TokenDelta) => void): Promise<UnlistenFn> =>
    listen<TokenDelta>('kinai://reasoning', (e) => cb(e.payload)),
  onTool: (cb: (d: ToolDeltaEvent) => void): Promise<UnlistenFn> =>
    listen<ToolDeltaEvent>('kinai://tool', (e) => cb(e.payload)),
  onAssistantDone: (cb: (d: AssistantDone) => void): Promise<UnlistenFn> =>
    listen<AssistantDone>('kinai://assistant-done', (e) => cb(e.payload)),
  /** Diagnostic snapshot of the full prompt sent to the LLM for a
   *  given assistant message. Per-session in-memory only. */
  onPromptDebug: (
    cb: (d: { assistant_msg_id: string; prompt: string }) => void
  ): Promise<UnlistenFn> =>
    listen<{ assistant_msg_id: string; prompt: string }>(
      'kinai://prompt-debug',
      (e) => cb(e.payload)
    ),
  onOverlayFocus: (cb: () => void): Promise<UnlistenFn> =>
    listen('kinai://overlay-focus', () => cb()),
  onOpenRoute: (cb: (route: string) => void): Promise<UnlistenFn> =>
    listen<string>('kinai://open-route', (e) => cb(e.payload)),
  onError: (cb: (msg: string) => void): Promise<UnlistenFn> =>
    listen<string>('kinai://error', (e) => cb(e.payload)),
  onStats: (cb: (s: RuntimeStats) => void): Promise<UnlistenFn> =>
    listen<RuntimeStats>('kinai://stats', (e) => cb(e.payload)),
  onDiscovery: (
    cb: (p: { family_name: string; instance: string; host_url: string }) => void
  ): Promise<UnlistenFn> =>
    listen('kinai://discovery', (e) => cb(e.payload as any)),
  onPeerJoined: (cb: (p: { id: string; name: string }) => void): Promise<UnlistenFn> =>
    listen('kinai://peer-joined', (e) => cb(e.payload as any)),
  onPeerLeft: (cb: (p: { id: string }) => void): Promise<UnlistenFn> =>
    listen('kinai://peer-left', (e) => cb(e.payload as any)),
  onClientStatus: (
    cb: (s: { connected: boolean; url?: string; error?: string }) => void
  ): Promise<UnlistenFn> =>
    listen('kinai://client-status', (e) => cb(e.payload as any)),
  onWelcome: (cb: (w: HostInfo) => void): Promise<UnlistenFn> =>
    listen('kinai://welcome', (e) => cb(e.payload as any)),
  onUpdateAvailable: (
    cb: (u: { version: string; current: string; body?: string; source: 'host' | 'github' }) => void
  ): Promise<UnlistenFn> =>
    listen('kinai://update-available', (e) => cb(e.payload as any)),
  onSttDownloadProgress: (
    cb: (p: { model_id: string; progress: number }) => void
  ): Promise<UnlistenFn> =>
    listen<{ model_id: string; progress: number }>('kinai://stt-download-progress', (e) =>
      cb(e.payload)
    ),
  onUpdateProgress: (cb: (p: { progress: number }) => void): Promise<UnlistenFn> =>
    listen('kinai://update-progress', (e) => cb(e.payload as any)),
  /** Fired after the passive fact-extractor stores one or more new
   *  rows. Used by Settings → Memory to refresh its list without a
   *  manual reload. Payload is intentionally empty — the page just
   *  re-fetches the full list. */
  onUserFactsUpdated: (cb: () => void): Promise<UnlistenFn> =>
    listen('kinai://user-facts-updated', () => cb()),
};
