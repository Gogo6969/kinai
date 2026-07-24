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
  type Report,
  type TurnMetrics,
} from '$lib/api';
import { getCurrentWindow } from '@tauri-apps/api/window';

class AppStore {
  config = $state<AppConfig | null>(null);
  /** This machine can synthesize speech (macOS). Mac CLIENTS speak
   *  locally with their own voices; hosts are always Macs today. */
  ttsSupported = $state(false);
  threads = $state<ThreadMeta[]>([]);
  activeThreadId = $state<string | null>(null);
  messages = $state<Record<string, Message[]>>({});
  streaming = $state<Record<string, string>>({}); // client_msg_id -> partial content
  reasoning = $state<Record<string, string>>({}); // client_msg_id -> reasoning trace
  toolActivity = $state<Record<string, { name: string; ok?: boolean }[]>>({});
  /** Failed turns that produced NO assistant message, keyed by
   *  client_msg_id. Rendered as an inline (ephemeral, not persisted)
   *  error bubble in the thread the turn belonged to — previously a
   *  failed turn only flashed a 5s toast and left the thread looking
   *  like the AI silently ignored the question. Entries for a thread
   *  are dropped when the user sends the next message there. */
  turnErrors = $state<Record<string, { threadId: string | null; message: string }>>({});
  /** Thread of the in-flight turn. In CLIENT mode a failure arrives as a
   *  kinai://error event long after send() returned — attribute its error
   *  bubble to the thread the turn ran in, not whatever thread the user
   *  happens to be looking at by then. */
  activeTurnThreadId: string | null = null;
  /** Per-message report state, keyed by assistant message id: the
   *  button confirms in place instead of opening a dialog. */
  reportState = $state<Record<string, { status: 'sending' | 'sent' | 'error'; message: string }>>({});
  /** Reported answers the HOST has received (host mode only). */
  reports = $state<Report[]>([]);
  /** Open (unreviewed) report count — drives the sidebar badge. */
  openReports = $state(0);
  /** Per-message fact-check panels, keyed by assistant message id.
   *  Ephemeral — never persisted, never part of model context. */
  factChecks = $state<
    Record<string, { status: 'loading' | 'done' | 'error'; report: string }>
  >({});
  /** Per-message metrics keyed by the persisted assistant message id. */
  metricsByMsgId = $state<Record<string, TurnMetrics>>({});
  stats = $state<RuntimeStats | null>(null);
  busy = $state(false);
  /** client_msg_id of the turn currently in flight. Set the moment we
   *  call api.sendMessage, cleared on AssistantDone / disconnect /
   *  cancel. The Stop button uses this to address its cancel call at
   *  the correct turn rather than a global "cancel everything." */
  activeTurnId = $state<string | null>(null);
  /** Messages the user typed while a turn was already in flight. They send
   *  automatically — one at a time — as each prior turn finishes (or is
   *  stopped), so a follow-up can be fired off without waiting. The × on a
   *  chip drops it; hitting Stop lets the next one through. */
  queued = $state<
    { id: string; threadId: string | null; content: string; attachments: Attachment[] }[]
  >([]);
  /** Recently-sent prompts, oldest → newest, for ↑/↓ recall in the composer
   *  (shell / Claude-Code style). Capped; not persisted across restarts. */
  inputHistory = $state<string[]>([]);
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
    /** Active model slots the host serves — builds the client's slash
     *  menu (absent on hosts ≤0.2.79; `alive` absent on ≤0.2.80). */
    host_slots?: Array<{ slug: string; model: string; alive?: boolean | null }>;
    /** Host has the online fact-check model configured. */
    host_fact_check?: boolean;
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
    this.ttsSupported = await api.ttsSupported().catch(() => false);
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
    await this.loadReports();
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

  async send(
    content: string,
    attachments: Attachment[] = [],
    opts: { threadId?: string; fromQueue?: boolean } = {}
  ) {
    if (!content.trim() && attachments.length === 0) return;
    this.rememberPrompt(content);
    // Latch busy + seed placeholders synchronously, BEFORE the first await
    // below. A queued message can drain straight into here, and the user
    // might fire another (a second Enter, or voice input) during the thread
    // self-heal's createThread round-trip — latching busy now makes that
    // second message queue behind this turn instead of racing a parallel
    // send. The thinking-dots bubble also shows instantly as a bonus.
    const clientMsgId = crypto.randomUUID();
    this.busy = true;
    this.activeTurnId = clientMsgId;
    // We deliberately DO NOT delete these in a `finally` — in Client mode
    // `api.sendMessage` returns instantly (the real work is on the host) and
    // clearing the maps here would race the inbound event stream. The
    // AssistantDone listener finalizes and cleans them up.
    this.streaming[clientMsgId] = '';
    this.reasoning[clientMsgId] = '';
    this.toolActivity[clientMsgId] = [];

    // Resolve the destination thread. A queued message carries the thread it
    // was typed in (opts.threadId) so it still lands there even if the user
    // has since switched threads. Fall back to the active thread, and
    // self-heal a fresh one only if there's genuinely none.
    let targetThreadId = opts.threadId ?? this.activeThreadId;
    // A fresh USER-TYPED attempt in a thread clears its stale error
    // bubbles — the user is retrying; the old notice served its purpose.
    // Queue-drained sends skip this: they fire immediately AFTER a
    // failure records its bubble, which the user hasn't seen yet.
    if (!opts.fromQueue) {
      for (const [k, v] of Object.entries(this.turnErrors)) {
        if (v.threadId === targetThreadId) delete this.turnErrors[k];
      }
      this.turnErrors = { ...this.turnErrors };
    }
    if (targetThreadId && !this.threads.some((t) => t.id === targetThreadId)) {
      // The captured thread was deleted while the message waited in queue.
      targetThreadId = this.activeThreadId;
    }
    if (!targetThreadId) {
      try {
        const t = await api.createThread('New conversation');
        this.threads = [t, ...this.threads];
        this.messages[t.id] = [];
        this.activeThreadId = t.id;
        targetThreadId = t.id;
      } catch (e) {
        console.error(e);
        delete this.streaming[clientMsgId];
        delete this.reasoning[clientMsgId];
        delete this.toolActivity[clientMsgId];
        this.busy = false;
        this.activeTurnId = null;
        return;
      }
    }

    // Auto-name the destination thread from this first user message if the
    // title is still a system-assigned placeholder. We deliberately do NOT
    // rename "Telegram" threads or any title the user has customised. Fires
    // before sendMessage so the sidebar updates in the same tick.
    const PLACEHOLDER_TITLES = new Set([
      'New conversation',
      'Welcome',
      'Quick chat',
    ]);
    const targetThread = this.threads.find((t) => t.id === targetThreadId);
    const hasNoPriorMessages = !this.messages[targetThreadId]?.length;
    if (
      targetThread &&
      hasNoPriorMessages &&
      PLACEHOLDER_TITLES.has(targetThread.title) &&
      content.trim()
    ) {
      const newTitle = this.deriveThreadTitle(content);
      if (newTitle !== targetThread.title) {
        // Fire-and-forget — a slow renameThread IPC shouldn't block sending.
        void this.renameThread(targetThreadId, newTitle).catch((e) =>
          console.warn('auto-rename failed', e)
        );
      }
    }

    this.activeTurnThreadId = targetThreadId;
    try {
      await api.sendMessage({
        thread_id: targetThreadId,
        content,
        client_msg_id: clientMsgId,
        attachments,
      });
    } catch (e) {
      console.error(e);
      // Drop this turn's placeholders so the user doesn't see a
      // permanently-spinning bubble.
      delete this.streaming[clientMsgId];
      delete this.reasoning[clientMsgId];
      delete this.toolActivity[clientMsgId];
      this.busy = false;
      this.activeTurnId = null;
      // In HOST mode a pipeline/tool error rejects right here (there's no
      // kinai://error event for local turns). Surface it as an INLINE
      // error bubble in the thread — a 5s toast was too easy to miss and
      // left the thread looking like the question was silently ignored.
      const emsg = String(e).replace(/^Error:\s*/, '');
      // A deliberate Stop is not a failure — no error bubble for it.
      if (!/cancelled before LLM responded/i.test(emsg)) {
        this.turnErrors = {
          ...this.turnErrors,
          [clientMsgId]: { threadId: targetThreadId, message: emsg },
        };
      }
      // Don't CLEAR the queue (a transient error mustn't discard the user's
      // other messages — a real disconnect does that). But DO advance it, so
      // an errored turn doesn't leave the next message stranded as "Queued".
      this.drainQueue();
    }
  }

  // ---- Send queue + prompt history --------------------------------------

  /** Send now if idle, otherwise line the message up to go out when the
   *  current turn finishes. Used by the composer so the user never has to
   *  wait on a long answer before asking the next thing. */
  enqueue(content: string, attachments: Attachment[] = []) {
    if (!content.trim() && attachments.length === 0) return;
    if (!this.busy) {
      void this.send(content, attachments);
      return;
    }
    // Capture the thread this was typed in so it still lands here even if
    // the user switches threads before the queue drains.
    this.queued = [
      ...this.queued,
      {
        id: crypto.randomUUID(),
        threadId: this.activeThreadId,
        content,
        attachments,
      },
    ];
  }

  /** Drop a still-pending queued message (the × on its chip). */
  removeQueued(id: string) {
    this.queued = this.queued.filter((q) => q.id !== id);
  }

  /** Fire the next queued message if one's waiting and nothing's in flight.
   *  Called on every turn completion (success or Stop) so the queue drains
   *  one turn at a time, in order. */
  private drainQueue() {
    if (this.busy || this.queued.length === 0) return;
    const [next, ...rest] = this.queued;
    this.queued = rest;
    void this.send(next.content, next.attachments, {
      threadId: next.threadId ?? undefined,
      fromQueue: true,
    });
  }

  /** Record a prompt for ↑/↓ recall, skipping consecutive duplicates and
   *  capping the buffer so a long session can't grow it unbounded. */
  private rememberPrompt(content: string) {
    const text = content.trim();
    if (!text) return;
    if (this.inputHistory[this.inputHistory.length - 1] === text) return;
    this.inputHistory = [...this.inputHistory, text].slice(-200);
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
  /** Flag an answer for the host. Sends only this question/answer pair
   *  — the rest of the conversation stays private, which is the promise
   *  the whole product rests on. */
  async report(messageId: string, question: string, answer: string,
               model: string, slot: string) {
    if (this.reportState[messageId]?.status === 'sending') return;
    this.reportState = {
      ...this.reportState,
      [messageId]: { status: 'sending', message: '' },
    };
    try {
      const msg = await api.reportAnswer({ messageId, question, answer, model, slot });
      this.reportState = {
        ...this.reportState,
        [messageId]: { status: 'sent', message: msg },
      };
      // Host reporting its own answer: refresh its list immediately.
      if (this.config?.mode === 'host') await this.loadReports();
    } catch (e) {
      this.reportState = {
        ...this.reportState,
        [messageId]: {
          status: 'error',
          message: String(e).replace(/^Error:\s*/, ''),
        },
      };
    }
  }

  /** Host: pull the reported answers + open count for the sidebar. */
  async loadReports() {
    if (this.config?.mode !== 'host') return;
    try {
      this.reports = await api.listReports();
      this.openReports = this.reports.filter((r) => !r.reviewed_at).length;
    } catch (e) {
      console.warn('reports', e);
    }
  }

  async setReportReviewed(id: string, reviewed: boolean) {
    await api.setReportReviewed(id, reviewed);
    await this.loadReports();
  }

  async deleteReport(id: string) {
    await api.deleteReport(id);
    await this.loadReports();
  }

  /** Run the online fact-check for one assistant message; the result
   *  renders as a panel under the bubble. Single-flight per message. */
  async factCheck(messageId: string) {
    if (this.factChecks[messageId]?.status === 'loading') return;
    this.factChecks = {
      ...this.factChecks,
      [messageId]: { status: 'loading', report: '' },
    };
    try {
      const report = await api.factCheckMessage(messageId);
      this.factChecks = {
        ...this.factChecks,
        [messageId]: { status: 'done', report },
      };
    } catch (e) {
      this.factChecks = {
        ...this.factChecks,
        [messageId]: {
          status: 'error',
          report: String(e).replace(/^Error:\s*/, ''),
        },
      };
    }
  }

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
    this.activeTurnThreadId = null;
    this.queued = [];
    // Same inline error bubble as send()'s catch — without it a failed
    // regenerate/edit-resend (e.g. every model server down) silently
    // reverted the thread as if nothing was ever asked.
    const emsg = String(e).replace(/^Error:\s*/, '');
    if (!/cancelled before LLM responded/i.test(emsg)) {
      this.turnErrors = {
        ...this.turnErrors,
        [clientMsgId]: { threadId: this.activeThreadId, message: emsg },
      };
    }
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
    // Stop abandons anything queued behind this turn too — a Stop that
    // silently kicked off the next question would surprise. (Each chip has
    // its own × for removing queued items individually without stopping.)
    this.queued = [];
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
      await events.onAssistantDone(({ client_msg_id, message, metrics, speak, config_changed }) => {
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
        // A queued follow-up (typed while this turn was running) goes out now.
        this.drainQueue();
        // The turn may have changed config (/voice toggles auto-speak) —
        // refetch quietly so Settings and future decisions stay fresh.
        if (config_changed) {
          void api.getConfig().then((c) => (this.config = c));
        }
        // Auto-speak: read the finished reply aloud without a button
        // press. On the HOST the backend decides per reply (`speak`:
        // setting + /voice overrides folded in). On a Mac CLIENT the
        // WS events carry no flag (except synthetic /voice
        // confirmations), so the client's own local setting decides.
        // Only the MAIN window reacts — the overlay receives the same
        // broadcast and would otherwise start a duplicate playback.
        const localAuto =
          this.ttsSupported &&
          !!this.config?.tts?.enabled &&
          !!this.config?.tts?.auto_speak;
        const wantSpeak =
          this.config?.mode === 'host'
            ? speak === true
            : this.ttsSupported && (speak ?? localAuto) === true;
        if (
          wantSpeak &&
          message?.id &&
          message.content &&
          getCurrentWindow().label === 'main'
        ) {
          void this.speakMessage(message.id, message.content);
        }
      })
    );
    this.cleanups.push(
      await events.onError((msg) => {
        // CLIENT mode surfaces a failed turn via this event (the host emits
        // kinai://error over the WS). Without a handler the spinner would
        // stick forever. (HOST-mode local turns reject api.sendMessage and
        // are handled in send()'s catch instead.) Release the turn, show the
        // error, and advance the queue so it isn't stranded.
        if (!this.busy && !this.activeTurnId) return;
        const id = this.activeTurnId;
        if (id) {
          delete this.streaming[id];
          delete this.reasoning[id];
          delete this.toolActivity[id];
          this.streaming = { ...this.streaming };
          this.reasoning = { ...this.reasoning };
          this.toolActivity = { ...this.toolActivity };
          // Inline error bubble in the TURN'S thread (latched at send
          // time — the user may have switched threads while the host ran
          // the turn). Same rationale as send()'s catch: a transient
          // toast reads as "no response". Deliberate Stops are silent.
          const emsg = String(msg).replace(/^Error:\s*/, '');
          if (!/cancelled before LLM responded/i.test(emsg)) {
            this.turnErrors = {
              ...this.turnErrors,
              [id]: {
                threadId: this.activeTurnThreadId ?? this.activeThreadId,
                message: emsg,
              },
            };
          }
        }
        this.busy = false;
        this.activeTurnId = null;
        this.drainQueue();
      })
    );
    this.cleanups.push(
      await events.onReport(() => {
        void this.loadReports();
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
          // Drop the in-flight turn's placeholders too — otherwise the
          // thinking-dots bubble for the interrupted turn lingers forever
          // (we won't get an AssistantDone to clean it up).
          const id = this.activeTurnId;
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
          this.queued = [];
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
