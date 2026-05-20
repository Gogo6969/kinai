<script lang="ts">
  import MessageBubble from './MessageBubble.svelte';
  import MicButton from './MicButton.svelte';
  import ThinkingDots from './ThinkingDots.svelte';
  import ThinkingPanel from './ThinkingPanel.svelte';
  import ToolPill from './ToolPill.svelte';
  import { app } from '$lib/stores/app.svelte';
  import { renderMarkdown as renderStreamMarkdown } from '$lib/markdown';
  import type { Attachment } from '$lib/api';
  import { Send, Square, Paperclip, X, FileText, Image as ImageIcon } from '@lucide/svelte';

  let input = $state('');
  let scrollEl: HTMLDivElement | undefined = $state();
  let inputEl: HTMLTextAreaElement | undefined = $state();
  let fileInputEl: HTMLInputElement | undefined = $state();
  let pendingAttachments = $state<Attachment[]>([]);
  let dragging = $state(false);
  let attachmentError = $state('');

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
    { cmd: '/help', desc: 'List slash commands' },
  ];

  // `/fast` and `/deep` only make sense when BOTH model slots are
  // active — when only one slot is configured, there's no choice to
  // route, so the slashes are pointless clutter. The check mirrors
  // the backend's `LlmSettings::is_active()` so menu visibility and
  // actual routing stay aligned.
  function isSlotActive(slot: { enabled?: boolean; base_url?: string; model?: string } | undefined): boolean {
    if (!slot) return false;
    if (slot.enabled === false) return false;
    return Boolean(slot.base_url?.trim() && slot.model?.trim());
  }
  const SLASH_COMMANDS = $derived.by(() => {
    const cfg = app.config;
    const fastActive = isSlotActive(cfg?.llm);
    const deepActive = isSlotActive(cfg?.llm_deep);
    const extra: Array<{ cmd: string; desc: string; hint?: string }> = [];
    if (fastActive && deepActive) {
      extra.push({
        cmd: '/fast',
        desc: `Use the fast model (${cfg!.llm.model})`,
        hint: 'Plain messages already default to this model.',
      });
      extra.push({
        cmd: '/deep',
        desc: `Use the deep model (${cfg!.llm_deep.model})`,
        hint: 'Slower but typically higher quality.',
      });
    }
    return [...extra, ...BASE_SLASH_COMMANDS];
  });
  let slashIndex = $state(0);
  const slashFiltered = $derived.by(() => {
    const t = input.trim();
    if (!t.startsWith('/') || t.includes(' ') || t.includes('\n')) return [];
    const q = t.toLowerCase();
    return SLASH_COMMANDS.filter((c) => c.cmd.toLowerCase().startsWith(q));
  });
  $effect(() => {
    void slashFiltered.length;
    slashIndex = 0;
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

  function fileToDataUrl(file: File): Promise<string> {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(reader.result as string);
      reader.onerror = () => reject(reader.error ?? new Error('read failed'));
      reader.readAsDataURL(file);
    });
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

  async function submit(e: Event) {
    e.preventDefault();
    const text = input.trim();
    if ((!text && pendingAttachments.length === 0) || app.busy) return;
    const atts = pendingAttachments;
    input = '';
    pendingAttachments = [];
    attachmentError = '';
    await app.send(text, atts);
  }
</script>

<section class="flex flex-col h-full">
  <div bind:this={scrollEl} class="flex-1 overflow-y-auto px-6 py-6 space-y-4">
    {#if messages.length === 0 && streamingIds.length === 0}
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
      {#each messages as m (m.id)}
        <MessageBubble
          message={m}
          metrics={app.metricsByMsgId[m.id] ?? m.metrics ?? null}
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
    <div class="kin-glass rounded-xl px-4 py-2.5 flex items-end gap-3">
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
        oninput={(e) => autoGrowTextarea(e.currentTarget as HTMLTextAreaElement)}
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
          if (e.key === 'Enter' && !e.shiftKey) submit(e);
        }}
      ></textarea>
      <MicButton
        compact
        ontranscript={(t) => {
          input = t;
          queueMicrotask(() => autoGrowTextarea(inputEl));
        }}
        onsend={async (t) => {
          if ((!t.trim() && pendingAttachments.length === 0) || app.busy) return;
          const atts = pendingAttachments;
          input = '';
          pendingAttachments = [];
          queueMicrotask(() => autoGrowTextarea(inputEl));
          await app.send(t, atts);
        }}
      />
      {#if app.busy}
        <button
          type="button"
          class="kin-btn"
          onclick={() => app.send('').catch(() => {})}
          aria-label="Stop"
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
