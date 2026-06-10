import {
  api,
  events,
  type AppConfig,
  type Attachment,
  type DetectedBackend,
  type Message,
  type RuntimeStats,
  type ThreadMeta,
  type SearchHit,
  type TurnMetrics,
} from '$lib/api';
import { getCurrentWindow } from '@tauri-apps/api/window';

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
  /** client_msg_id of the turn currently in flight. Set the moment we
   *  call api.sendMessage, cleared on AssistantDone / disconnect /
   *  cancel. The Stop button uses this to address its cancel call at
   *  the correct turn rather than a global "cancel everything." */
  activeTurnId = $state<string | null>(null);
  /** Live status of the client → host WebSocket. `null` until the first
   *  status event arrives so the UI can avoid flashing "Disconnected"
   *  during the dial. */
  clientStatus = $state<{ connected: boolean; error?: string } | null>(null);
  /** Per-session cache of the full prompt JSON that produced each
   *  assistant message. Keyed by assistant message id, not persisted.
   *  Populated when the host emits `kinai://prompt-debug` right after
   *  generating a reply. The chat UI shows a 🔍 toggle only when an
   *  entry exists for the rendered message. */
  promptDebug = $state<Record<string, string>>({});
  /** Info advertised by the host on connect — model name + search engine,
   *  shown read-only on the client (the host controls these). */
  hostInfo = $state<{
    family_name: string;
    host_version: string;
    host_model: string;
    host_search_engine: string;
    host_vision?: string;
    /** `@username` of the host's Telegram bot (empty = not configured).
     *  Drives the "Family bot: @foo" label in Settings → Telegram on
     *  client peers. */
    host_telegram_bot?: string;
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

  // ---- Cross-thread message search --------------------------------------
  searchQuery = $state('');
  searchResults = $state<SearchHit[]>([]);
  searching = $state(false);
  private searchTimer: ReturnType<typeof setTimeout> | null = null;
  private searchSeq = 0; // guards against out-of-order async results

  /** Debounced search (200ms) so we don't fire an IPC per keystroke. */
  runSearch(query: string) {
    this.searchQuery = query;
    if (this.searchTimer) clearTimeout(this.searchTimer);
    if (!query.trim()) {
      this.searchResults = [];
      this.searching = false;
      return;
    }
    this.searching = true;
    const seq = ++this.searchSeq;
    this.searchTimer = setTimeout(async () => {
      try {
        const hits = await api.searchMessages(query);
        if (seq !== this.searchSeq) return; // a newer query superseded this one
        this.searchResults = hits;
      } catch (e) {
        if (seq !== this.searchSeq) return;
        console.warn('search failed', e);
        this.searchResults = [];
      } finally {
        if (seq === this.searchSeq) this.searching = false;
      }
    }, 200);
  }

  clearSearch() {
    this.searchSeq++; // invalidate any in-flight result
    if (this.searchTimer) clearTimeout(this.searchTimer);
    this.searchQuery = '';
    this.searchResults = [];
    this.searching = false;
  }

  /** Jump to the thread of a search hit and scroll/flash the message. */
  async jumpToHit(hit: SearchHit) {
    this.activeThreadId = hit.thread_id;
    await this.loadActive();
    this.clearSearch();
    requestAnimationFrame(() => {
      const el = document.getElementById(`msg-${hit.message_id}`);
      el?.scrollIntoView({ behavior: 'smooth', block: 'center' });
      el?.classList.add('kin-flash');
      setTimeout(() => el?.classList.remove('kin-flash'), 1600);
    });
  }

  /**
   * Rename a thread. Persists via Rust command, then patches the local
   * store so the sidebar updates without a full reload. Idempotent; safe
   * to call with the same title twice.
   */
  async renameThread(id: string, title: string) {
    const trimmed = title.trim();
    if (!trimmed) return;
    await api.renameThread(id, trimmed);
    const idx = this.threads.findIndex((t) => t.id === id);
    if (idx >= 0) {
      this.threads[idx] = { ...this.threads[idx], title: trimmed };
    }
  }

  /**
   * Derive a short, human-readable title from the first message a user
   * sends in a thread. Used to replace the placeholder "New conversation"
   * / "Welcome" / "Quick chat" titles the moment a real topic appears, so
   * the sidebar stops looking like a list of identical bookmarks.
   *
   * Rules:
   *   - Collapse whitespace.
   *   - Prefer to break at the first sentence-ending punctuation that
   *     falls between 10 and 60 chars in (keeps "Why is the sky blue?"
   *     intact instead of cutting it).
   *   - Otherwise cap at 50 chars with an ellipsis.
   *   - Strip trailing punctuation (".", "!", "?", ",", ";", ":") — they
   *     look noisy in a one-line list.
   *   - Empty input falls back to the original "New conversation" so we
   *     never end up with a blank title row.
   */
  private deriveThreadTitle(text: string): string {
    const flat = text.replace(/\s+/g, ' ').trim();
    if (!flat) return 'New conversation';
    let title = flat;
    const sentenceMatch = flat.match(/^(.{10,60}?[.!?])(\s|$)/);
    if (sentenceMatch) {
      title = sentenceMatch[1];
    } else if (flat.length > 50) {
      title = flat.slice(0, 47).trimEnd() + '…';
    }
    title = title.replace(/[.!?,;:]+$/, '').trim();
    return title || 'New conversation';
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
    // Auto-name the thread from this first user message if the title is
    // still one of the system-assigned placeholders. We deliberately do
    // NOT rename "Telegram" threads (those carry a meaningful label that
    // tells the user which thread is bound to the bot) or any title the
    // user has already customised. Fires before sendMessage so the
    // sidebar updates in the same tick the message goes out — no flash
    // of "New conversation" → real title.
    const PLACEHOLDER_TITLES = new Set([
      'New conversation',
      'Welcome',
      'Quick chat',
    ]);
    const activeThread = this.threads.find((t) => t.id === this.activeThreadId);
    const hasNoPriorMessages = !this.messages[this.activeThreadId]?.length;
    if (
      activeThread &&
      hasNoPriorMessages &&
      PLACEHOLDER_TITLES.has(activeThread.title) &&
      content.trim()
    ) {
      const newTitle = this.deriveThreadTitle(content);
      if (newTitle !== activeThread.title) {
        // Fire-and-forget — we don't want a slow renameThread IPC to
        // block sending the actual message. If it fails the user can
        // still rename manually via the sidebar edit affordance.
        void this.renameThread(this.activeThreadId, newTitle).catch((e) =>
          console.warn('auto-rename failed', e)
        );
      }
    }
    const clientMsgId = crypto.randomUUID();
    this.busy = true;
    this.activeTurnId = clientMsgId;
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
      this.activeTurnId = null;
    }
  }

  // ---- Regenerate / edit-and-resend (host-only) -------------------------

  /** Spin up the same streaming bookkeeping `send()` uses so the
   *  thinking-dots bubble renders and `onAssistantDone` cleans up. */
  private beginTurn(): string {
    const clientMsgId = crypto.randomUUID();
    this.busy = true;
    this.activeTurnId = clientMsgId;
    this.streaming[clientMsgId] = '';
    this.reasoning[clientMsgId] = '';
    this.toolActivity[clientMsgId] = [];
    return clientMsgId;
  }

  /** Tear down a turn's placeholders after an IPC error and re-sync the
   *  thread from the authoritative DB so the UI can't get stuck. */
  private async failTurn(clientMsgId: string, e: unknown) {
    console.error(e);
    delete this.streaming[clientMsgId];
    delete this.reasoning[clientMsgId];
    delete this.toolActivity[clientMsgId];
    this.streaming = { ...this.streaming };
    this.reasoning = { ...this.reasoning };
    this.toolActivity = { ...this.toolActivity };
    this.busy = false;
    this.activeTurnId = null;
    await this.loadActive();
  }

  /**
   * Re-run the last assistant reply. Optimistically drops the old reply
   * (and anything after it) from the local list, then invokes the
   * host-only command; the new reply streams in over the kinai:// events
   * and `onAssistantDone` finalizes it. On error we reload from the DB.
   */
  async regenerateLast(assistantId: string) {
    const threadId = this.activeThreadId;
    if (!threadId || this.busy) return;
    const list = this.messages[threadId] ?? [];
    const idx = list.findIndex((m) => m.id === assistantId);
    if (idx < 0) return;
    // Optimistic truncation: drop the old assistant reply (+ anything after).
    this.messages[threadId] = list.slice(0, idx);
    this.messages = { ...this.messages };
    const clientMsgId = this.beginTurn();
    try {
      await api.regenerateMessage({
        thread_id: threadId,
        assistant_msg_id: assistantId,
        client_msg_id: clientMsgId,
      });
    } catch (e) {
      await this.failTurn(clientMsgId, e);
    }
  }

  /**
   * Edit a previous user message and re-run from there. Keeps the edited
   * user bubble (with the new text), drops everything after it, then
   * invokes the host-only command. On error we reload from the DB.
   */
  async editAndResend(userMsgId: string, newText: string) {
    const threadId = this.activeThreadId;
    if (!threadId || this.busy) return;
    const trimmed = newText.trim();
    if (!trimmed) return;
    const list = this.messages[threadId] ?? [];
    const idx = list.findIndex((m) => m.id === userMsgId);
    if (idx < 0) return;
    // Keep the edited user bubble, drop everything after it.
    const edited = { ...list[idx], content: trimmed };
    this.messages[threadId] = [...list.slice(0, idx), edited];
    this.messages = { ...this.messages };
    const clientMsgId = this.beginTurn();
    try {
      await api.editAndResend({
        thread_id: threadId,
        user_msg_id: userMsgId,
        new_content: trimmed,
        client_msg_id: clientMsgId,
      });
    } catch (e) {
      await this.failTurn(clientMsgId, e);
    }
  }

  // ---- Voice playback (host desktop) -------------------------------------
  // Centralized so every speak button — and auto-speak, which starts with
  // no button press at all — shares ONE source of truth. The bubble whose
  // message id matches `speakingMsgId` renders as "stop"; everyone else
  // shows "speak". Backend allows one playback at a time anyway.
  speakingMsgId = $state<string | null>(null);
  private speakPoll: ReturnType<typeof setInterval> | null = null;

  /** Start speaking a message (kills any current playback first). */
  async speakMessage(msgId: string, content: string) {
    try {
      await api.speakText(content);
    } catch (e) {
      this.speakingMsgId = null;
      window.dispatchEvent(
        new CustomEvent('kin-toast', {
          detail: { msg: `✗ ${String(e).replace(/^Error:\s*/, '')}`, ms: 4000 },
        })
      );
      return;
    }
    this.speakingMsgId = msgId;
    // `say` is a separate process — poll so the button flips back to
    // "speak" when playback finishes on its own.
    if (this.speakPoll) clearInterval(this.speakPoll);
    this.speakPoll = setInterval(async () => {
      try {
        const still = await api.isSpeaking();
        if (!still) this.clearSpeaking();
      } catch {
        this.clearSpeaking();
      }
    }, 1000);
  }

  async stopSpeaking() {
    this.clearSpeaking();
    try {
      await api.stopSpeaking();
    } catch (e) {
      console.warn('stopSpeaking', e);
    }
  }

  /** Speak/stop toggle for a message's speak button. */
  async toggleSpeak(msgId: string, content: string) {
    if (this.speakingMsgId === msgId) {
      await this.stopSpeaking();
    } else {
      await this.speakMessage(msgId, content);
    }
  }

  private clearSpeaking() {
    this.speakingMsgId = null;
    if (this.speakPoll) {
      clearInterval(this.speakPoll);
      this.speakPoll = null;
    }
  }

  /**
   * Abort the current chat turn. Sends a stop_generation IPC at the
   * Rust side (cancels the LLM stream + any pending tool iteration via
   * the CancellationToken plumbing), then unconditionally clears the
   * local placeholders so the UI is responsive even if the backend
   * takes a moment to wind down a stuck HTTP read. Safe to call when
   * nothing is in flight — no-ops cleanly.
   */
  async cancelCurrentTurn() {
    const id = this.activeTurnId;
    // Tell the backend to abort first (cheapest path: cancel the
    // token, the in-flight request unwinds). Don't await indefinitely
    // — if the host is wedged the IPC could hang, and the user is
    // already frustrated. Promise.race with a 2s ceiling.
    try {
      await Promise.race([
        api.stopGeneration(id ?? undefined),
        new Promise<void>((resolve) => setTimeout(resolve, 2000)),
      ]);
    } catch (e) {
      console.warn('stopGeneration IPC failed', e);
    }
    // Locally clear regardless: if the host honored cancel we'll get
    // an AssistantDone (which is a no-op against an already-cleared
    // turn); if it didn't, the user at least gets their UI back.
    if (id) {
      delete this.streaming[id];
      delete this.reasoning[id];
      delete this.toolActivity[id];
      this.streaming = { ...this.streaming };
      this.reasoning = { ...this.reasoning };
      this.toolActivity = { ...this.toolActivity };
    }
    this.busy = false;
    this.activeTurnId = null;
  }

  pushMessage(m: Message) {
    const list = this.messages[m.thread_id] ?? [];
    if (list.some((existing) => existing.id === m.id)) return;
    this.messages[m.thread_id] = [...list, m];
    const meta = this.threads.find((t) => t.id === m.thread_id);
    if (meta) {
      meta.updated_at = m.created_at;
      this.threads = [meta, ...this.threads.filter((t) => t.id !== meta.id)];
    } else {
      // The message landed on a thread we don't know about — most likely
      // a Telegram-originated turn that just created a fresh thread on
      // the host. Refresh the thread list so the sidebar surfaces it.
      // Fire-and-forget; we don't await here because pushMessage is
      // called from synchronous event handlers.
      void this.refreshThreads();
    }
  }

  /** Refetch the thread list from the host/DB. Idempotent — safe to
   *  call from any event handler. */
  async refreshThreads() {
    try {
      this.threads = await api.listThreads();
    } catch (e) {
      console.warn('refreshThreads failed', e);
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
        // Build a NEW array + NEW objects (no in-place mutation of items
        // already published into $state) so the tool pill's ok/✓/✗ flip
        // reliably re-renders regardless of how a consumer reads it.
        const prev = this.toolActivity[client_msg_id] ?? [];
        let arr;
        if (event.kind === 'Started') {
          arr = [...prev, { name: event.name }];
        } else if (event.kind === 'Finished') {
          // Flip the LAST not-yet-finished tool of this name (same as the
          // prior reverse().find), but immutably.
          let idx = -1;
          for (let j = prev.length - 1; j >= 0; j--) {
            if (prev[j].name === event.name && prev[j].ok === undefined) { idx = j; break; }
          }
          arr = idx === -1 ? prev : prev.map((t, j) => (j === idx ? { ...t, ok: event.ok } : t));
        } else {
          arr = prev;
        }
        this.toolActivity = { ...this.toolActivity, [client_msg_id]: arr };
      })
    );
    this.cleanups.push(
      await events.onPromptDebug(({ assistant_msg_id, prompt }) => {
        this.promptDebug[assistant_msg_id] = prompt;
        this.promptDebug = { ...this.promptDebug };
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
        this.activeTurnId = null;
        // Auto-speak (host desktop): read the finished reply aloud
        // without a button press. Only the MAIN window reacts — the
        // overlay receives the same broadcast event and would otherwise
        // start a duplicate playback.
        if (
          message?.id &&
          message.content &&
          this.config?.mode === 'host' &&
          this.config?.tts?.enabled &&
          this.config?.tts?.auto_speak &&
          getCurrentWindow().label === 'main'
        ) {
          void this.speakMessage(message.id, message.content);
        }
      })
    );
    this.cleanups.push(await events.onStats((s) => (this.stats = s)));
    this.cleanups.push(
      await events.onClientStatus((s) => {
        this.clientStatus = { connected: s.connected, error: s.error };
        // If the WebSocket drops mid-turn we'll never see an
        // AssistantDone — release the Send button so the user can retry
        // instead of being stuck on the Stop icon forever.
        if (!s.connected) {
          this.busy = false;
          this.activeTurnId = null;
        }
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
    // Re-check for updates when the window regains focus. The periodic
    // backend check only runs every 4h, so a long-running window could
    // sit on a stale version until restart (the "I had to restart the
    // app before the banner showed" case). Throttled to once a minute.
    let lastFocusCheck = 0;
    this.cleanups.push(
      await getCurrentWindow().onFocusChanged(({ payload: focused }) => {
        if (!focused) return;
        const now = Date.now();
        if (now - lastFocusCheck < 60_000) return;
        lastFocusCheck = now;
        void api.checkUpdates().catch(() => {});
      })
    );
  }

  stopListening() {
    this.cleanups.forEach((u) => u());
    this.cleanups = [];
  }
}

export const app = new AppStore();
