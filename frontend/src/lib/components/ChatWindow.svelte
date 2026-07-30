<script lang="ts">
  import MessageBubble from './MessageBubble.svelte';
  import MicButton from './MicButton.svelte';
  import ThinkingDots from './ThinkingDots.svelte';
  import ThinkingPanel from './ThinkingPanel.svelte';
  import ToolPill from './ToolPill.svelte';
  import { app } from '$lib/stores/app.svelte';
  import { renderMarkdown as renderStreamMarkdown } from '$lib/markdown';
  import { api, type Attachment } from '$lib/api';
  import { fileToDataUrl } from '$lib/image';
  import { resolveActiveModel, slotFromCommand } from '$lib/activeModel';
  import { Send, Square, Paperclip, X, FileText, Image as ImageIcon } from '@lucide/svelte';

  let input = $state('');
  let scrollEl: HTMLDivElement | undefined = $state();
  let inputEl: HTMLTextAreaElement | undefined = $state();
  let fileInputEl: HTMLInputElement | undefined = $state();
  let pendingAttachments = $state<Attachment[]>([]);
  let dragging = $state(false);
  let attachmentError = $state('');
  // ↑/↓ prompt recall. `historyIndex === null` means we're editing the live
  // draft; once recall starts it indexes into app.inputHistory and
  // `draftStash` holds whatever the user was typing so ↓ can restore it.
  let historyIndex = $state<number | null>(null);
  let draftStash = '';
  // Guards the programmatic `input = …` assignment below so that if writing
  // it ever does trip the textarea's oninput handler, that handler won't
  // mistake a recall for a manual edit and reset historyIndex mid-recall.
  let recallingHistory = false;

  function recallHistory(dir: -1 | 1, el: HTMLTextAreaElement) {
    const hist = app.inputHistory;
    if (hist.length === 0) return;
    recallingHistory = true;
    if (dir === -1) {
      // Older. Enter recall from the live draft on the first ↑.
      if (historyIndex === null) {
        draftStash = input;
        historyIndex = hist.length;
      }
      if (historyIndex > 0) historyIndex -= 1;
      input = hist[historyIndex];
    } else {
      // Newer. Walk back toward — and finally restore — the live draft.
      if (historyIndex === null) return;
      if (historyIndex < hist.length - 1) {
        historyIndex += 1;
        input = hist[historyIndex];
      } else {
        historyIndex = null;
        input = draftStash;
      }
    }
    recallingHistory = false;
    queueMicrotask(() => {
      autoGrowTextarea(el);
      el.selectionStart = el.selectionEnd = input.length;
    });
  }

  // Slash-command autocomplete. Always shows the full list when the
  // input is exactly "/" or matches "/<partial>"; the menu disappears as
  // soon as the user types a space (because they're now writing the
  // prompt body), submits, or presses Escape.
  // Static base list — `/pic`, `/picHQ`, `/help` are always
  // technically callable (image-gen handler decides at run time).
  const BASE_SLASH_COMMANDS: Array<{ cmd: string; desc: string; hint?: string }> = [
    {
      cmd: '/pic',
      desc: 'Generate image · default 1280×720, ~5s',
      hint: 'Optional WxH prefix: /pic 1024x1024 sunset over Miami',
    },
    {
      cmd: '/picHQ',
      desc: 'HQ image · default 1024×1024, ~30s',
      hint: 'Optional WxH prefix: /picHQ 1280x720 sunset over Miami',
    },
    {
      cmd: '/newchat',
      desc: 'Start a fresh chat — no prior context (memory kept)',
      hint: 'Add a question to ask right away: /newchat what is rust?',
    },
    {
      cmd: '/voice',
      desc: 'Toggle spoken replies',
      hint: 'KinAI app: replies read aloud · Telegram: voice notes. /voice on | off',
    },
    { cmd: '/help', desc: 'List slash commands' },
  ];

  // `/fast` / `/balanced` / `/deep` only make sense when ≥2 model slots
  // are active — with one slot there's no choice to route, so the
  // slashes are pointless clutter. Hosts derive the active set from the
  // local config (mirroring the backend's `LlmSettings::is_active()`);
  // client peers have NO local LLM config, so they use the slot list
  // the host advertises in the Welcome envelope instead (hosts ≤0.2.79
  // don't send one — those clients keep the old empty-menu behavior).
  function isSlotActive(slot: { enabled?: boolean; base_url?: string; model?: string } | undefined): boolean {
    if (!slot) return false;
    if (slot.enabled === false) return false;
    return Boolean(slot.base_url?.trim() && slot.model?.trim());
  }
  const SLOT_MENU_TEXT: Record<string, { label: string; hint: string }> = {
    fast: { label: 'fast', hint: 'Plain messages already default to this model.' },
    balanced: { label: 'balanced', hint: 'The middle ground — smarter than fast, quicker than deep.' },
    deep: { label: 'deep', hint: 'Slower but typically higher quality.' },
  };
  // Host-mode slot liveness, refreshed when the slash menu opens (the
  // backend caches probes for 15s, so this stays cheap). Client mode
  // reads liveness from the host's Welcome advertisement instead.
  let slotHealth = $state<Record<string, boolean> | null>(null);
  let lastHealthFetch = 0;
  const SLASH_COMMANDS = $derived.by(() => {
    const cfg = app.config;
    // Wire list only while actually IN client mode — app.hostInfo is
    // never cleared, so after a live client→host reconfiguration a
    // stale advertisement would otherwise beat the fresh local config.
    const wire = cfg?.mode === 'client' ? app.hostInfo?.host_slots : undefined;
    const slots: Array<{ slug: string; model: string; alive?: boolean | null }> = wire?.length
      ? wire
          .filter((s) => Object.hasOwn(SLOT_MENU_TEXT, s.slug))
          // Fresh liveness (fetched when the menu opened) beats the
          // connect-time snapshot from the Welcome advertisement.
          .map((s) => ({ ...s, alive: slotHealth?.[s.slug] ?? s.alive }))
      : [
          { slug: 'fast', llm: cfg?.llm },
          { slug: 'balanced', llm: cfg?.llm_balanced },
          { slug: 'deep', llm: cfg?.llm_deep },
        ]
          .filter(({ llm }) => isSlotActive(llm))
          .map(({ slug, llm }) => ({ slug, model: llm!.model, alive: slotHealth?.[slug] }));
    // Model switches only appear when there's an actual choice (≥2 slots).
    // A configured-but-offline slot stays LISTED (hiding it while its
    // server restarts would make the menu flicker confusingly) but is
    // marked, and the turn-level failover covers actually picking it.
    const extra =
      slots.length >= 2
        ? slots.map(({ slug, model, alive }) => ({
            cmd: `/${slug}`,
            desc:
              alive === false
                ? `⚠️ ${SLOT_MENU_TEXT[slug].label} model (${model}) — offline right now, auto-switches`
                : `Use the ${SLOT_MENU_TEXT[slug].label} model (${model})`,
            hint: SLOT_MENU_TEXT[slug].hint,
          }))
        : [];
    return [...extra, ...BASE_SLASH_COMMANDS];
  });
  let slashIndex = $state(0);
  const slashFiltered = $derived.by(() => {
    // trimStart only — a TRAILING space means the command is complete
    // (applySlash appends one), and the menu must close so the next
    // Return submits. With trim() the menu re-captured Return forever,
    // making bare slash commands unsendable by keyboard.
    const t = input.trimStart();
    if (!t.startsWith('/') || /\s/.test(t)) return [];
    const q = t.toLowerCase();
    return SLASH_COMMANDS.filter((c) => c.cmd.toLowerCase().startsWith(q));
  });
  $effect(() => {
    void slashFiltered.length;
    slashIndex = 0;
  });
  // While the slash menu is showing, refresh slot liveness (throttled —
  // the backend's own cache makes repeat calls ~free, this throttle just
  // avoids an IPC call per keystroke). Hosts probe locally; clients ask
  // the host over the WS (slot_health handles both).
  $effect(() => {
    if (slashFiltered.length === 0) return;
    const now = Date.now();
    if (now - lastHealthFetch < 5000) return;
    lastHealthFetch = now;
    api
      .slotHealth()
      .then((h) => (slotHealth = h))
      .catch(() => {});
  });
  function applySlash(cmd: string) {
    input = cmd + ' ';
    queueMicrotask(() => {
      inputEl?.focus();
      autoGrowTextarea(inputEl);
    });
  }

  // Hard cap matches the server's MAX_BYTES (25 MB) so the user gets a
  // friendly local error instead of a wire-time rejection.
  const MAX_BYTES = 25 * 1024 * 1024;
  const SUPPORTED_MIMES = /^(application\/pdf|image\/(png|jpe?g|gif|webp))$/i;

  async function ingestFiles(files: FileList | File[]) {
    attachmentError = '';
    const accepted: Attachment[] = [];
    for (const file of Array.from(files)) {
      if (file.size > MAX_BYTES) {
        attachmentError = `${file.name} is too large (${Math.round(file.size / (1024 * 1024))} MB; max 25 MB).`;
        continue;
      }
      if (file.type && !SUPPORTED_MIMES.test(file.type)) {
        attachmentError = `${file.name}: only PDFs and images are supported right now.`;
        continue;
      }
      try {
        const dataUrl = await fileToDataUrl(file);
        accepted.push({
          kind: file.type.startsWith('image/') ? 'image' : 'file',
          mime: file.type || null,
          name: file.name || null,
          data_url: dataUrl,
        });
      } catch (e) {
        attachmentError = `Couldn't read ${file.name}: ${String(e)}`;
      }
    }
    if (accepted.length) {
      pendingAttachments = [...pendingAttachments, ...accepted];
    }
  }


  function removeAttachment(idx: number) {
    pendingAttachments = pendingAttachments.filter((_, i) => i !== idx);
  }

  function onPaste(e: ClipboardEvent) {
    const items = e.clipboardData?.items;
    if (!items) return;
    const files: File[] = [];
    for (const item of Array.from(items)) {
      if (item.kind === 'file') {
        const f = item.getAsFile();
        if (f) files.push(f);
      }
    }
    if (files.length > 0) {
      e.preventDefault();
      void ingestFiles(files);
    }
  }

  function onDragOver(e: DragEvent) {
    if (!e.dataTransfer?.types?.includes('Files')) return;
    e.preventDefault();
    dragging = true;
  }
  function onDragLeave() {
    dragging = false;
  }
  function onDrop(e: DragEvent) {
    e.preventDefault();
    dragging = false;
    const files = e.dataTransfer?.files;
    if (files && files.length > 0) void ingestFiles(files);
  }

  function autoGrowTextarea(el?: HTMLTextAreaElement) {
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = Math.min(el.scrollHeight, 200) + 'px';
  }
  // Re-fit whenever the bound value changes (covers paste, programmatic
  // changes, and submit-clears).
  $effect(() => {
    void input;
    queueMicrotask(() => autoGrowTextarea(inputEl));
  });

  const messages = $derived(
    app.activeThreadId ? app.messages[app.activeThreadId] ?? [] : []
  );

  // Which model is answering right now, shown on the composer so the slot
  // in force is never a guess. A `/fast|/balanced|/deep` still sitting in
  // the box counts immediately — the badge should track what pressing
  // Enter would actually do, not what the last turn did.
  const pendingSlot = $derived(slotFromCommand(input));
  const activeModel = $derived.by(() => {
    const resolved = resolveActiveModel(
      messages,
      app.metricsByMsgId,
      app.config,
      app.hostInfo?.host_slots,
    );
    if (!pendingSlot || pendingSlot === resolved.slot) return resolved;
    // Re-resolve for the slot the user is about to switch to.
    return resolveActiveModel(
      [{ id: '__pending', role: 'user', content: `/${pendingSlot}` } as any],
      {},
      app.config,
      app.hostInfo?.host_slots,
    );
  });
  // A turn is "in progress" as soon as ANY of its streams have produced
  // anything — token deltas, chain-of-thought, or a tool call. Restricting
  // this to `streaming` keys alone would hide the thinking-dots placeholder
  // during the long pre-token phase of a tool-using response (the model
  // emits reasoning + tool events first, real tokens only after the tool
  // returns), which is exactly when the live indicator matters most.
  const streamingIds = $derived(
    Array.from(
      new Set([
        ...Object.keys(app.streaming),
        ...Object.keys(app.reasoning),
        ...Object.keys(app.toolActivity),
      ])
    )
  );
  const toolIds = $derived(Object.keys(app.toolActivity));
  /** Failed-turn error bubbles belonging to the ACTIVE thread. Also
   *  gates the empty-state welcome screen — a brand-new thread whose
   *  first message failed must show the error, not the welcome. */
  const threadErrors = $derived(
    Object.entries(app.turnErrors).filter(([, v]) => v.threadId === app.activeThreadId)
  );

  // Two-part autoscroll:
  //   * When the message count changes (user sent, assistant arrived) →
  //     always scroll to the bottom. New activity is what the user wants
  //     to see; their explicit action implies intent to follow.
  //   * While tokens are streaming → only stay pinned if they were already
  //     near the bottom. Re-reading older content doesn't get yanked.
  $effect(() => {
    void messages.length;
    queueMicrotask(() => {
      if (scrollEl) scrollEl.scrollTop = scrollEl.scrollHeight;
    });
  });
  $effect(() => {
    void Object.values(app.streaming).join('|').length;
    void Object.values(app.reasoning).join('|').length;
    if (!scrollEl) return;
    const nearBottom =
      scrollEl.scrollHeight - scrollEl.scrollTop - scrollEl.clientHeight < 120;
    if (!nearBottom) return;
    queueMicrotask(() => {
      if (scrollEl) scrollEl.scrollTop = scrollEl.scrollHeight;
    });
  });

  // `/newchat` → trailing question ('' if bare), or null if not the
  // command. Case-insensitive; `/newchatx` is treated as normal text.
  function parseNewchat(text: string): string | null {
    const m = /^\/newchat(?:\s+([\s\S]*))?$/i.exec(text.trim());
    return m ? (m[1] ?? '').trim() : null;
  }

  async function submit(e: Event) {
    e.preventDefault();
    const text = input.trim();
    if (!text && pendingAttachments.length === 0) return;
    historyIndex = null;
    // /newchat — open a fresh thread so a new question doesn't reuse the
    // current conversation's context (persistent memory is kept). Any
    // trailing question is asked in the new thread. Only honored when idle;
    // a /newchat typed mid-turn is queued as plain text instead of forking
    // the thread out from under the running answer.
    const fresh = parseNewchat(text);
    if (fresh !== null && !app.busy) {
      input = '';
      pendingAttachments = [];
      attachmentError = '';
      queueMicrotask(() => autoGrowTextarea(inputEl));
      await app.newThread();
      if (fresh) await app.send(fresh, []);
      return;
    }
    const atts = pendingAttachments;
    input = '';
    pendingAttachments = [];
    attachmentError = '';
    queueMicrotask(() => autoGrowTextarea(inputEl));
    // enqueue() sends right away when idle, or lines the message up to go
    // out as soon as the in-flight turn finishes.
    app.enqueue(text, atts);
  }
</script>

<section class="flex flex-col h-full">
  <div bind:this={scrollEl} class="flex-1 overflow-y-auto px-6 py-6 space-y-4">
    {#if messages.length === 0 && streamingIds.length === 0 && threadErrors.length === 0}
      {@const isHost = app.config?.mode === 'host'}
      {@const peerCount = app.stats?.peers_connected ?? 0}
      <div class="h-full flex flex-col items-center justify-center text-center max-w-md mx-auto px-6 gap-3">
        <div class="text-5xl mb-1">🏠</div>
        {#if isHost}
          <p class="text-base font-semibold text-white/85">
            Your family's private AI is running on this Mac.
          </p>
          <p class="text-sm text-white/55 leading-relaxed">
            KinAI streams answers from your local model, remembers each
            family member's conversations independently, and never sends a
            byte to the cloud unless you turn on a vision endpoint.
          </p>
          <div class="text-xs text-white/40 space-y-1 mt-2">
            {#if peerCount === 0}
              <p>No family devices joined yet. Open
                <span class="text-white/60">Manage family</span> to create
                an invite.</p>
            {:else}
              <p>{peerCount} family {peerCount === 1 ? 'device' : 'devices'} joined.
                Their chats are private — you only see your own here.</p>
            {/if}
            <p class="pt-2">
              Press <code class="bg-white/5 px-1.5 py-0.5 rounded">⌘ Space</code>
              anywhere to summon the overlay, or type below.
            </p>
          </div>
        {:else}
          <p class="text-base font-semibold text-white/85">
            Welcome to KinAI.
          </p>
          <p class="text-sm text-white/55 leading-relaxed">
            Ask anything — your conversations stay private to you. Other
            family members on the same host can't see them.
          </p>
          <p class="text-xs text-white/40 mt-2">
            Try: <code class="bg-white/5 px-1.5 py-0.5 rounded">show me a photo of the Eiffel Tower</code>
          </p>
        {/if}
      </div>
    {:else}
      {#each messages as m, i (m.id)}
        <MessageBubble
          message={m}
          metrics={app.metricsByMsgId[m.id] ?? m.metrics ?? null}
          canRegenerate={i === messages.length - 1 &&
            m.role === 'assistant' &&
            streamingIds.length === 0}
        />
      {/each}
      {#each streamingIds as id (id)}
        {#if app.toolActivity[id]?.length}
          <div class="flex flex-wrap gap-2 px-1">
            {#each app.toolActivity[id] as t}
              <ToolPill name={t.name} ok={t.ok} />
            {/each}
          </div>
        {/if}
        {#if app.reasoning[id]}
          <ThinkingPanel text={app.reasoning[id]} live={!app.streaming[id]} />
        {/if}
        <!-- Always render the assistant bubble during streaming so the DOM
             node is stable. Show dots inside until the first visible token,
             then the bubble fills in place without any swap-induced jitter. -->
        <div class="flex flex-col gap-1 items-start">
          <div
            class="max-w-[85%] rounded-2xl rounded-bl-md px-4 py-2.5 bg-white/5 border border-white/10 text-ink-50 kin-prose"
          >
            {#if app.streaming[id]}
              {@html renderStreamMarkdown(app.streaming[id])}
              <span class="inline-block w-1.5 h-4 bg-current align-middle ml-0.5 animate-pulse-soft"></span>
            {:else}
              <ThinkingDots size="md" />
            {/if}
          </div>
          <div class="flex gap-2 text-xs text-white/40 px-1.5">
            <span>KinAI</span>
          </div>
        </div>
      {/each}
      {#each threadErrors as [id, err] (id)}
        <!-- A turn that failed without producing an answer. Ephemeral (not
             persisted, not part of the model's context) — cleared when the
             user sends the next message in this thread. -->
        <div class="flex flex-col gap-1 items-start">
          <div
            class="max-w-[85%] rounded-2xl rounded-bl-md px-4 py-2.5 bg-red-500/10
                   border border-red-400/25 text-red-100/90 text-sm whitespace-pre-wrap break-words"
          >
            {err.message}
          </div>
          <div class="flex gap-2 text-xs text-white/40 px-1.5">
            <span>KinAI · error — this answer wasn't saved</span>
          </div>
        </div>
      {/each}
      {#each app.queued as q (q.id)}
        <!-- Messages typed while the turn above is still running. They send
             automatically, in order, as each prior answer finishes. -->
        <div class="flex flex-col gap-1 items-end">
          <div
            class="max-w-[85%] rounded-2xl rounded-br-md px-4 py-2.5 bg-teal-500/10
                   border border-dashed border-teal-300/25 text-ink-50/80
                   flex items-start gap-2"
          >
            <div class="min-w-0">
              {#if q.content}
                <span class="whitespace-pre-wrap break-words">{q.content}</span>
              {/if}
              {#if q.attachments.length > 0}
                <div
                  class="flex items-center gap-1 text-xs text-teal-200/70 {q.content
                    ? 'mt-1'
                    : ''}"
                >
                  <Paperclip size={12} class="flex-shrink-0" />
                  <span
                    >{q.attachments.length}
                    {q.attachments.length === 1 ? 'attachment' : 'attachments'}</span
                  >
                </div>
              {/if}
            </div>
            <button
              type="button"
              class="shrink-0 mt-0.5 text-white/40 hover:text-white/80 transition-colors"
              onclick={() => app.removeQueued(q.id)}
              aria-label="Remove queued message"
              title="Remove from queue"
            >
              <X size={13} />
            </button>
          </div>
          <div class="text-xs text-teal-200/60 px-1.5">Queued · sends next</div>
        </div>
      {/each}
    {/if}
  </div>

  <form
    onsubmit={submit}
    ondragover={onDragOver}
    ondragleave={onDragLeave}
    ondrop={onDrop}
    class="relative border-t border-white/5 bg-ink-900/40 px-4 py-3 {dragging
      ? 'ring-2 ring-teal-400/60 ring-inset'
      : ''}"
  >
    {#if dragging}
      <div
        class="absolute inset-0 flex items-center justify-center pointer-events-none
               bg-teal-500/5 backdrop-blur-sm text-teal-200 text-sm font-medium z-10"
      >
        Drop a PDF or image to attach
      </div>
    {/if}
    <input
      type="file"
      bind:this={fileInputEl}
      class="hidden"
      accept="application/pdf,image/*"
      multiple
      onchange={(e) => {
        const files = (e.currentTarget as HTMLInputElement).files;
        if (files) void ingestFiles(files);
        (e.currentTarget as HTMLInputElement).value = '';
      }}
    />
    {#if pendingAttachments.length > 0}
      <div class="flex flex-wrap gap-2 mb-2 px-1">
        {#each pendingAttachments as att, i}
          <div
            class="flex items-center gap-2 bg-white/5 border border-white/10
                   rounded-lg pl-2 pr-1 py-1 text-xs text-white/80 max-w-[200px]"
          >
            {#if att.kind === 'image' && att.data_url}
              <img
                src={att.data_url}
                alt={att.name ?? 'image'}
                class="w-7 h-7 rounded object-cover flex-shrink-0"
              />
            {:else}
              <FileText size={14} class="text-teal-300 flex-shrink-0" />
            {/if}
            <span class="truncate">{att.name ?? 'attachment'}</span>
            <button
              type="button"
              onclick={() => removeAttachment(i)}
              class="text-white/40 hover:text-red-300 flex-shrink-0 p-0.5"
              aria-label="Remove attachment"
            >
              <X size={12} />
            </button>
          </div>
        {/each}
      </div>
    {/if}
    {#if attachmentError}
      <div class="text-xs text-red-300 mb-2 px-1">{attachmentError}</div>
    {/if}
    {#if slashFiltered.length > 0}
      <div class="kin-glass rounded-xl mb-2 overflow-hidden text-sm">
        {#each slashFiltered as c, i}
          <button
            type="button"
            class="w-full text-left px-3 py-2 flex items-center justify-between gap-3 transition-colors {i === slashIndex ? 'bg-teal-500/15 text-white' : 'text-white/80 hover:bg-white/5'}"
            onclick={() => applySlash(c.cmd)}
            onmouseenter={() => (slashIndex = i)}
          >
            <span class="font-mono text-teal-300 flex-shrink-0">{c.cmd}</span>
            <span class="text-xs text-white/50 truncate">{c.desc}</span>
          </button>
        {/each}
        {#if slashFiltered[slashIndex]?.hint}
          <div class="px-3 py-1.5 text-[11px] text-teal-200/70 border-t border-white/5 font-mono">
            {slashFiltered[slashIndex].hint}
          </div>
        {/if}
        <div class="px-3 py-1.5 text-[10px] text-white/40 border-t border-white/5">
          <kbd class="bg-white/10 px-1 rounded">↑↓</kbd> navigate
          <kbd class="bg-white/10 px-1 rounded ml-1">Tab</kbd> accept
          <kbd class="bg-white/10 px-1 rounded ml-1">Esc</kbd> dismiss
        </div>
      </div>
    {/if}
    <!-- items-center, not items-end: with a single-row textarea the caret
         and placeholder sat against the bottom of the glass box while the
         padding above stayed empty. Centred, the input reads as the middle
         of the bar and only drifts upward once the textarea grows. -->
    <div class="kin-glass rounded-xl px-4 py-2.5 flex items-center gap-3">
      <button
        type="button"
        class="kin-btn !px-2"
        onclick={() => fileInputEl?.click()}
        aria-label="Attach file"
        title="Attach a PDF or image (you can also drag-drop or paste)"
      >
        <Paperclip size={14} />
      </button>
      <textarea
        bind:this={inputEl}
        bind:value={input}
        rows="1"
        placeholder="Message KinAI…"
        class="kin-input resize-none overflow-y-auto leading-relaxed"
        style="max-height: 200px;"
        oninput={(e) => {
          // Any real edit drops us out of history recall so the next ↑
          // starts again from the newest prompt — but a programmatic recall
          // write (guarded by recallingHistory) must not count as an edit.
          if (!recallingHistory) historyIndex = null;
          autoGrowTextarea(e.currentTarget as HTMLTextAreaElement);
        }}
        onpaste={onPaste}
        onkeydown={(e) => {
          // Slash-command menu navigation takes precedence when it's open.
          if (slashFiltered.length > 0) {
            if (e.key === 'ArrowDown') {
              e.preventDefault();
              slashIndex = (slashIndex + 1) % slashFiltered.length;
              return;
            }
            if (e.key === 'ArrowUp') {
              e.preventDefault();
              slashIndex =
                (slashIndex - 1 + slashFiltered.length) % slashFiltered.length;
              return;
            }
            if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
              e.preventDefault();
              applySlash(slashFiltered[slashIndex].cmd);
              return;
            }
            if (e.key === 'Escape') {
              e.preventDefault();
              input = '';
              return;
            }
          }
          // ↑/↓ recall earlier prompts (shell / Claude-Code style). ↑ only
          // kicks in when the caret is at the very start (so it still moves
          // between lines mid-message); once recalling, ↓ walks back to the
          // draft. Skipped while a recall isn't active and there's nothing
          // to recall.
          const el = e.currentTarget as HTMLTextAreaElement;
          if (e.key === 'ArrowUp') {
            const atStart = el.selectionStart === 0 && el.selectionEnd === 0;
            if (historyIndex !== null || atStart) {
              e.preventDefault();
              recallHistory(-1, el);
              return;
            }
          }
          if (e.key === 'ArrowDown' && historyIndex !== null) {
            e.preventDefault();
            recallHistory(1, el);
            return;
          }
          if (e.key === 'Enter' && !e.shiftKey) submit(e);
        }}
      ></textarea>
      <!-- Active-model badge. Until now the slot in force was only visible
           in the footer of an answer that had already arrived, so on a
           fresh thread — or after a `/deep` three questions ago — the user
           had no way to know which model was about to answer. `alive:false`
           (host has probed the slot as unreachable) shows amber, because
           the turn will fail over to another slot. -->
      {#if activeModel.model || activeModel.slot}
        <div
          class="hidden sm:flex items-center gap-1 shrink-0 select-none text-[11px] font-mono
                 px-2 py-1 rounded-lg border
                 {activeModel.alive
            ? 'border-white/10 bg-white/5 text-white/45'
            : 'border-amber-400/30 bg-amber-400/10 text-amber-200/80'}"
          title={activeModel.alive
            ? `Answering with the ${activeModel.slot} model${activeModel.model ? ` (${activeModel.model})` : ''}. Switch with /fast, /balanced or /deep.`
            : `The ${activeModel.slot} model looks unreachable — KinAI will switch to an available one for this turn.`}
        >
          <span aria-hidden="true">{activeModel.glyph}</span>
          <span class="max-w-[9rem] truncate">{activeModel.model || activeModel.slot}</span>
          {#if !activeModel.alive}<span aria-hidden="true">⚠</span>{/if}
        </div>
      {/if}
      <MicButton
        compact
        ontranscript={(t) => {
          input = t;
          queueMicrotask(() => autoGrowTextarea(inputEl));
        }}
        onsend={(t) => {
          if (!t.trim() && pendingAttachments.length === 0) return;
          const atts = pendingAttachments;
          input = '';
          pendingAttachments = [];
          historyIndex = null;
          queueMicrotask(() => autoGrowTextarea(inputEl));
          // Queue behind an in-flight turn just like the typed path.
          app.enqueue(t, atts);
        }}
      />
      {#if app.busy}
        <button
          type="button"
          class="kin-btn"
          onclick={() => app.cancelCurrentTurn().catch(() => {})}
          aria-label="Stop generating"
          title="Stop this turn"
        >
          <Square size={14} />
        </button>
      {:else}
        <button
          type="submit"
          class="kin-btn-primary"
          disabled={!input.trim() && pendingAttachments.length === 0}
        >
          <Send size={14} />
          Send
        </button>
      {/if}
    </div>
  </form>
</section>
