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

export interface AppConfig {
  mode: Mode;
  theme: Theme;
  llm: LlmSettings;
  host: HostSettings;
  client: ClientSettings;
  overlay: OverlaySettings;
  tools: ToolSettings;
  vision: VisionSettings;
  comfyui: ComfyConfig;
  telegram: TelegramConfig;
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
}

export interface AssistantDone {
  client_msg_id: string;
  message: Message;
  metrics: TurnMetrics;
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
  setOverlaySettings: (overlay: OverlaySettings) =>
    invoke<AppConfig>('set_overlay_settings', { overlay }),
  setTheme: (theme: Theme) => invoke<AppConfig>('set_theme', { theme }),
  setVisionSettings: (vision: VisionSettings) =>
    invoke<AppConfig>('set_vision_settings', { vision }),
  testVisionEndpoint: (args: { base_url: string; model: string; api_key?: string | null }) =>
    invoke<{ ok: boolean; latency_ms: number; error: string | null; reply: string }>(
      'test_vision_endpoint',
      { args }
    ),
  setComfyConfig: (comfyui: ComfyConfig) =>
    invoke<AppConfig>('set_comfy_config', { comfyui }),
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
  loadThread: (threadId: string) => invoke<Message[]>('load_thread', { threadId }),
  createThread: (title?: string) => invoke<ThreadMeta>('create_thread', { title: title ?? null }),
  deleteThread: (threadId: string) => invoke<void>('delete_thread', { threadId }),
  renameThread: (threadId: string, title: string) =>
    invoke<void>('rename_thread', { threadId, title }),

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
  stopGeneration: () => invoke<void>('stop_generation'),

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
  onUpdateProgress: (cb: (p: { progress: number }) => void): Promise<UnlistenFn> =>
    listen('kinai://update-progress', (e) => cb(e.payload as any)),
};
