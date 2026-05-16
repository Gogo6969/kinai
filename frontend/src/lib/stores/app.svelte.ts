import {
  api,
  events,
  type AppConfig,
  type Attachment,
  type DetectedBackend,
  type Message,
  type RuntimeStats,
  type ThreadMeta,
  type TurnMetrics,
} from '$lib/api';

class AppStore {
  config = $state<AppConfig | null>(null);
  threads = $state<ThreadMeta[]>([]);
  activeThreadId = $state<string | null>(null);
  messages = $state<Record<string, Message[]>>({});
  streaming = $state<Record<string, string>>({}); // client_msg_id -> partial content
  reasoning = $state<Record<string, string>>({}); // client_msg_id -> reasoning trace
  toolActivity = $state<Record<string, { name: string; ok?: boolean }[]>>({});
  /** Per-message metrics keyed by the persisted assistant message id. */
  metricsByMsgId = $state<Record<string, TurnMetrics>>({});
  stats = $state<RuntimeStats | null>(null);
  busy = $state(false);
  /** Live status of the client → host WebSocket. `null` until the first
   *  status event arrives so the UI can avoid flashing "Disconnected"
   *  during the dial. */
  clientStatus = $state<{ connected: boolean; error?: string } | null>(null);
  /** Info advertised by the host on connect — model name + search engine,
   *  shown read-only on the client (the host controls these). */
  hostInfo = $state<{
    family_name: string;
    host_version: string;
    host_model: string;
    host_search_engine: string;
  } | null>(null);
  /** mDNS-discovered KinAI hosts on the local network. Kept at the store
   *  level (not inside /client/+page.svelte) because the discovery event
   *  stream starts firing as soon as `startListening` runs — if we waited
   *  until the user navigates to /client to subscribe, we'd miss every
   *  resolution that already happened during app startup. */
  discoveredHosts = $state<{ family_name: string; instance: string; host_url: string }[]>([]);
  /** Latest update advertised by the host (or GitHub fallback) — drives
   *  the banner at the top of the chat. Cleared once the user starts the
   *  install. */
  updateAvailable = $state<{
    version: string;
    current: string;
    source: 'host' | 'github';
    body?: string;
  } | null>(null);
  /** Install progress 0–100 once the user clicks Install. `null` between
   *  installs. */
  updateProgress = $state<number | null>(null);
  /** Backend-scan caches keyed by source so they survive route changes.
   *  `at` is the epoch-ms when the scan completed. */
  detectCache = $state<{ results: DetectedBackend[]; at: number } | null>(null);
  scanCache = $state<{ results: DetectedBackend[]; at: number } | null>(null);
  cleanups: Array<() => void> = [];

  async load() {
    this.config = await api.getConfig();
    if (this.config.mode !== 'unconfigured') {
      this.threads = await api.listThreads();
      if (this.threads.length === 0) {
        const t = await api.createThread('Welcome');
        this.threads = [t];
      }
      this.activeThreadId = this.threads[0].id;
      await this.loadActive();
    }
    await this.refreshStats();
  }

  async refreshStats() {
    try {
      this.stats = await api.runtimeStats();
      // Hydrate clientStatus from the snapshot so the sidebar's dot
      // doesn't get stuck on "Connecting…" when the first
      // `kinai://client-status` event was emitted before this UI got a
      // chance to subscribe (typical race during app launch).
      if (this.config?.mode === 'client' && this.stats) {
        this.clientStatus = {
          connected: this.stats.client_connected,
          error: this.stats.client_error ?? undefined,
        };
        if (this.stats.host_info) {
          this.hostInfo = this.stats.host_info;
        }
      }
    } catch (e) {
      console.warn('stats', e);
    }
  }

  async loadActive() {
    if (!this.activeThreadId) return;
    this.messages[this.activeThreadId] = await api.loadThread(this.activeThreadId);
  }

  async newThread(title?: string) {
    const t = await api.createThread(title);
    this.threads = [t, ...this.threads];
    this.activeThreadId = t.id;
    this.messages[t.id] = [];
  }

  async deleteThread(id: string) {
    await api.deleteThread(id);
    this.threads = this.threads.filter((t) => t.id !== id);
    if (this.activeThreadId === id) {
      this.activeThreadId = this.threads[0]?.id ?? null;
      if (this.activeThreadId) await this.loadActive();
    }
  }

  async send(content: string, attachments: Attachment[] = []) {
    if (!content.trim() && attachments.length === 0) return;
    // Self-heal: if there's no active thread (e.g. user deleted all of
    // them, or the welcome thread never got created), spin one up now so
    // the message has somewhere to land.
    if (!this.activeThreadId) {
      const t = await api.createThread('New conversation');
      this.threads = [t, ...this.threads];
      this.activeThreadId = t.id;
      this.messages[t.id] = [];
    }
    const clientMsgId = crypto.randomUUID();
    this.busy = true;
    // Pre-seed the placeholders so the thinking-dots bubble renders
    // immediately, even before the first reasoning/tool/token event
    // arrives. We deliberately DO NOT delete them in `finally` — in
    // Client mode `api.sendMessage` returns instantly with placeholders
    // (the real work is happening on the host) and clearing the maps
    // here would race the inbound event stream, leaving the user with
    // no live feedback during tool calls or long reasoning phases.
    // The AssistantDone listener finalizes and cleans up the entries
    // when the turn actually completes.
    this.streaming[clientMsgId] = '';
    this.reasoning[clientMsgId] = '';
    this.toolActivity[clientMsgId] = [];
    try {
      await api.sendMessage({
        thread_id: this.activeThreadId,
        content,
        client_msg_id: clientMsgId,
        attachments,
      });
    } catch (e) {
      console.error(e);
      // On a hard failure to even reach the host, drop the placeholders
      // so the user doesn't see a permanently-spinning bubble.
      delete this.streaming[clientMsgId];
      delete this.reasoning[clientMsgId];
      delete this.toolActivity[clientMsgId];
      this.busy = false;
    }
  }

  pushMessage(m: Message) {
    const list = this.messages[m.thread_id] ?? [];
    if (list.some((existing) => existing.id === m.id)) return;
    this.messages[m.thread_id] = [...list, m];
    const meta = this.threads.find((t) => t.id === m.thread_id);
    if (meta) {
      meta.updated_at = m.created_at;
      this.threads = [meta, ...this.threads.filter((t) => t.id !== meta.id)];
    }
  }

  async startListening() {
    this.cleanups.push(await events.onMessage((m) => this.pushMessage(m)));
    this.cleanups.push(
      await events.onToken(({ client_msg_id, delta }) => {
        this.streaming[client_msg_id] = (this.streaming[client_msg_id] ?? '') + delta;
        this.streaming = { ...this.streaming };
      })
    );
    this.cleanups.push(
      await events.onReasoning(({ client_msg_id, delta }) => {
        this.reasoning[client_msg_id] = (this.reasoning[client_msg_id] ?? '') + delta;
        this.reasoning = { ...this.reasoning };
      })
    );
    this.cleanups.push(
      await events.onTool(({ client_msg_id, event }) => {
        const arr = this.toolActivity[client_msg_id] ?? [];
        if (event.kind === 'Started') arr.push({ name: event.name });
        if (event.kind === 'Finished') {
          const last = [...arr].reverse().find((t) => t.name === event.name && t.ok === undefined);
          if (last) last.ok = event.ok;
        }
        this.toolActivity[client_msg_id] = arr;
        this.toolActivity = { ...this.toolActivity };
      })
    );
    this.cleanups.push(
      await events.onAssistantDone(({ client_msg_id, message, metrics }) => {
        // Finalize the assistant turn. In Host mode the local pipeline
        // also emits `kinai://message` for the assistant, so this push
        // would be a no-op dedup. In Client mode the host's wire protocol
        // ships the assistant body INSIDE the AssistantDone envelope
        // (rather than a separate Message frame), so without this push
        // the assistant bubble would live forever as a "streaming"
        // placeholder — causing every user message to appear stacked
        // above every assistant reply.
        if (message?.id) {
          this.pushMessage(message);
        }
        if (message?.id && metrics) {
          this.metricsByMsgId[message.id] = metrics;
          this.metricsByMsgId = { ...this.metricsByMsgId };
        }
        // Drop the streaming/reasoning/tool placeholders for this turn
        // — the persisted bubble takes their place.
        if (client_msg_id) {
          delete this.streaming[client_msg_id];
          delete this.reasoning[client_msg_id];
          delete this.toolActivity[client_msg_id];
          this.streaming = { ...this.streaming };
          this.reasoning = { ...this.reasoning };
          this.toolActivity = { ...this.toolActivity };
        }
        this.busy = false;
      })
    );
    this.cleanups.push(await events.onStats((s) => (this.stats = s)));
    this.cleanups.push(
      await events.onClientStatus((s) => {
        this.clientStatus = { connected: s.connected, error: s.error };
        // If the WebSocket drops mid-turn we'll never see an
        // AssistantDone — release the Send button so the user can retry
        // instead of being stuck on the Stop icon forever.
        if (!s.connected) this.busy = false;
      })
    );
    this.cleanups.push(
      await events.onWelcome((w) => {
        this.hostInfo = w;
      })
    );
    this.cleanups.push(
      await events.onDiscovery((d) => {
        if (!this.discoveredHosts.some((x) => x.instance === d.instance)) {
          this.discoveredHosts = [...this.discoveredHosts, d];
        }
      })
    );
    this.cleanups.push(
      await events.onUpdateAvailable((u) => {
        this.updateAvailable = u;
      })
    );
    this.cleanups.push(
      await events.onUpdateProgress((p) => {
        this.updateProgress = p.progress;
      })
    );
  }

  stopListening() {
    this.cleanups.forEach((u) => u());
    this.cleanups = [];
  }
}

export const app = new AppStore();
