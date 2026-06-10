<script lang="ts">
  import type { Attachment, Message, TurnMetrics } from '$lib/api';
  import { renderMarkdown } from '$lib/markdown';
  import { app } from '$lib/stores/app.svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { Check, Copy, FileText, Search, RefreshCw, Pencil, Volume2, Square } from '@lucide/svelte';

  let {
    message,
    streaming = false,
    metrics = null,
    canRegenerate = false,
  }: {
    message: { content: string; role: string; sender: string; created_at?: string } & Partial<Message>;
    streaming?: boolean;
    metrics?: TurnMetrics | null;
    /** True only for the last assistant message — gates the regenerate
     *  affordance so older replies don't show it. */
    canRegenerate?: boolean;
  } = $props();

  // Regenerate / edit are host-only: the commands return an error in
  // client mode (no local authoritative DB), so we hide the buttons there
  // rather than surface a failure toast.
  const isHost = $derived(app.config?.mode === 'host');

  // Desktop speak button (host-only: audio plays on the host's
  // speakers). Click = speak, click again = stop. The speaking state
  // lives in the app store so a reply started by AUTO-speak still shows
  // "stop" on its bubble, and only one bubble ever shows it.
  const ttsEnabled = $derived(isHost && (app.config?.tts?.enabled ?? false));
  const speaking = $derived(message.id != null && app.speakingMsgId === message.id);
  function toggleSpeak() {
    if (!message.id) return;
    void app.toggleSpeak(message.id, message.content);
  }

  // Inline edit state for user messages. `editing` swaps the bubble for a
  // textarea; Save calls editAndResend, which drops everything after this
  // message and re-runs the turn with the new text.
  let editing = $state(false);
  let draft = $state('');
  let editEl = $state<HTMLTextAreaElement | null>(null);
  function startEdit() {
    if (!message.id || app.busy) return;
    draft = message.content;
    editing = true;
  }
  function cancelEdit() {
    editing = false;
    draft = '';
  }
  async function saveEdit() {
    const text = draft.trim();
    if (!text || app.busy || !message.id) return;
    editing = false;
    await app.editAndResend(message.id, text);
  }
  // Focus + select the textarea once it mounts so the user can type/replace
  // immediately. Re-runs when `editing` flips and `editEl` binds.
  $effect(() => {
    if (editing && editEl) {
      editEl.focus();
      editEl.select();
    }
  });

  // Per-session prompt snapshot for assistant messages — populated by
  // the host's kinai://prompt-debug event right after generation. Empty
  // for: user messages, assistant messages from before this session,
  // and any turn the host couldn't serialize. The 🔍 button only
  // shows when this is non-empty.
  const promptSnapshot = $derived(message.id ? app.promptDebug[message.id] : undefined);

  /**
   * Abbreviate a long model id for the per-message badge so it fits
   * the existing metrics row. Most providers prefix with an org
   * ("olares/gpt-oss-20b", "openai/gpt-4o-mini"); we drop that. Ollama
   * tags append a `:q4_K_M`-style suffix that's mostly noise for the
   * "which model answered me" question, so we trim the tag too unless
   * the resulting name would be too short (e.g. "llama3.1" alone
   * could mean any size — keep ":8b" or ":70b").
   *
   * Then cap at 24 chars with an ellipsis. Result is something like
   * `gpt-oss-20b`, `qwen2.5:72b`, `claude-haiku-4-5`, `gemini-2.5-flash`.
   */
  function abbreviateModel(full: string): string {
    if (!full) return '';
    let s = full.trim();
    const slash = s.lastIndexOf('/');
    if (slash >= 0 && slash < s.length - 1) {
      s = s.slice(slash + 1);
    }
    // Drop quantisation/format suffix after the size tag — keep the
    // first :size segment if it conveys parameter count, drop the rest.
    const colon = s.indexOf(':');
    if (colon >= 0) {
      const tag = s.slice(colon + 1);
      const sizeMatch = tag.match(/^([0-9]+\.?[0-9]*[bBmM])/);
      if (sizeMatch) {
        s = `${s.slice(0, colon)}:${sizeMatch[1].toLowerCase()}`;
      }
    }
    if (s.length > 24) s = s.slice(0, 23) + '…';
    return s;
  }
  const modelAbbrev = $derived(metrics?.model ? abbreviateModel(metrics.model) : '');
  /** Glyph + tooltip text for the slot — keeps the badge compact:
   *  fast = ⚡, deep = 🧠. Empty slot label = no glyph (single-model
   *  setups don't need the visual cue). */
  function slotGlyph(slot: string | undefined): string {
    if (slot === 'fast') return '⚡';
    if (slot === 'deep') return '🧠';
    return '';
  }
  const slotIcon = $derived(slotGlyph(metrics?.slot));
  let opening = $state(false);

  // Copy-to-clipboard state. We push BOTH plain text (raw markdown) and
  // rich HTML so paste-targets pick the best representation:
  //   * Notes / Word / Pages → HTML (headings, code blocks, bold survive)
  //   * Terminal / code editors → plain (no garbage tags)
  // Falls back to writeText() on browsers without ClipboardItem (none of
  // our WKWebView/WebView2 targets, but defensive).
  let copyState = $state<'idle' | 'copied' | 'failed'>('idle');
  let copyTimer: ReturnType<typeof setTimeout> | undefined;
  async function copyMessage() {
    try {
      const plain = message.content;
      const rich = html;
      if (
        typeof navigator !== 'undefined' &&
        navigator.clipboard?.write &&
        typeof ClipboardItem !== 'undefined'
      ) {
        await navigator.clipboard.write([
          new ClipboardItem({
            'text/plain': new Blob([plain], { type: 'text/plain' }),
            'text/html': new Blob([rich], { type: 'text/html' }),
          }),
        ]);
      } else {
        await navigator.clipboard.writeText(plain);
      }
      copyState = 'copied';
    } catch {
      copyState = 'failed';
    }
    if (copyTimer) clearTimeout(copyTimer);
    copyTimer = setTimeout(() => {
      copyState = 'idle';
    }, 1500);
  }
  // Open the prompt JSON in the user's default editor — way safer than
  // trying to render 50-100KB inside an in-app <details><pre>, which
  // can freeze the WebView on macOS. Writes to
  // ~/.kinai/prompts/<msg_id>.json, then asks the shell plugin to open
  // that file path. macOS routes to TextEdit (or whatever's default for
  // .json); Windows to Notepad / VSCode / whichever.
  async function openPromptSnapshot() {
    if (!message.id || !promptSnapshot || opening) return;
    opening = true;
    try {
      // The Rust command writes the file AND launches the OS's
      // default-handler for .json in one step, so we don't need a
      // separate shell-plugin call (and don't trip its URL-only
      // scope guard, which rejects file paths).
      await invoke<string>('write_prompt_snapshot', {
        msgId: message.id,
        body: promptSnapshot,
      });
    } catch (err) {
      const msg = String(err).replace(/^Error:\s*/, '');
      window.dispatchEvent(
        new CustomEvent('kin-toast', { detail: { msg: `✗ Couldn't open prompt: ${msg}`, ms: 5000 } })
      );
    } finally {
      opening = false;
    }
  }

  const html = $derived(renderMarkdown(message.content));
  const attachments = $derived<Attachment[]>(message.attachments ?? []);
  const time = $derived(
    message.created_at
      ? new Date(message.created_at).toLocaleTimeString([], {
          hour: '2-digit',
          minute: '2-digit',
        })
      : ''
  );

  const isUser = $derived(message.role === 'user');

  function formatDuration(ms: number): string {
    if (ms < 1000) return `${ms}ms`;
    if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
    const m = Math.floor(ms / 60_000);
    const s = Math.floor((ms % 60_000) / 1000);
    return `${m}m ${s}s`;
  }
</script>

<div
  id={message.id ? `msg-${message.id}` : undefined}
  class="flex flex-col gap-1 {isUser ? 'items-end' : 'items-start'}"
>
  {#if isUser && editing}
    <div class="max-w-[85%] w-80 rounded-2xl rounded-br-md px-3 py-2.5 bg-teal-500">
      <textarea
        bind:this={editEl}
        bind:value={draft}
        rows="3"
        class="w-full bg-white/25 text-ink-900 rounded-md px-2 py-1.5 text-sm outline-none resize-y placeholder:text-ink-900/50"
        onkeydown={(e) => {
          if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
            e.preventDefault();
            void saveEdit();
          } else if (e.key === 'Escape') {
            e.preventDefault();
            cancelEdit();
          }
        }}
      ></textarea>
      <div class="flex justify-end gap-3 mt-1.5 text-xs">
        <button type="button" class="text-ink-900/70 hover:text-ink-900" onclick={cancelEdit}>
          Cancel
        </button>
        <button
          type="button"
          class="font-medium text-ink-900 hover:underline disabled:opacity-50 disabled:no-underline"
          onclick={saveEdit}
          disabled={!draft.trim() || app.busy}
        >
          Save &amp; resend
        </button>
      </div>
    </div>
  {:else}
  <div
    class="max-w-[85%] rounded-2xl px-4 py-2.5 kin-prose
           {isUser
             ? 'bg-teal-500 text-ink-900 rounded-br-md'
             : 'bg-white/5 border border-white/10 text-ink-50 rounded-bl-md'}"
  >
    {#if attachments.length > 0}
      <div class="flex flex-wrap gap-2 mb-2 -mx-1">
        {#each attachments as att}
          {#if att.kind === 'image' && att.data_url}
            <img
              src={att.data_url}
              alt={att.name ?? 'image'}
              class="max-h-48 rounded-lg border border-black/10 object-contain"
            />
          {:else}
            <div
              class="flex items-center gap-2 bg-black/10 {isUser
                ? 'border border-ink-900/15'
                : 'border border-white/10'} rounded-lg px-2 py-1.5 text-xs"
            >
              <FileText size={14} class={isUser ? 'opacity-70' : 'text-teal-300'} />
              <span class="truncate max-w-[200px]">{att.name ?? 'attachment'}</span>
            </div>
          {/if}
        {/each}
      </div>
    {/if}
    {@html html}
    {#if streaming}
      <span class="inline-block w-1.5 h-4 bg-current align-middle ml-0.5 animate-pulse-soft"></span>
    {/if}
  </div>
  {/if}
  {#if isUser && !editing}
    <!-- User bubbles: the right-aligned teal bubble already says "you" by
         position + color, so the name/timestamp row is suppressed to reduce
         visual noise. The copy button is the only affordance we keep here
         so users can re-grab their own prompt for editing/sharing. -->
    <div class="flex gap-2 text-xs text-white/30 px-1.5 items-center">
      {#if isHost && message.id}
        <button
          type="button"
          class="inline-flex items-center gap-1 hover:text-teal-300 transition-colors cursor-pointer disabled:opacity-50"
          onclick={startEdit}
          disabled={app.busy}
          title="Edit this message and re-run from here"
          aria-label="Edit message"
        >
          <Pencil size={11} />
          <span>edit</span>
        </button>
      {/if}
      <button
        type="button"
        class="inline-flex items-center gap-1 hover:text-teal-300 transition-colors cursor-pointer"
        onclick={copyMessage}
        title={copyState === 'failed'
          ? 'Copy failed — try again'
          : 'Copy message (with formatting) to clipboard'}
        aria-label="Copy message"
      >
        {#if copyState === 'copied'}
          <Check size={11} />
          <span>copied</span>
        {:else if copyState === 'failed'}
          <Copy size={11} />
          <span>failed</span>
        {:else}
          <Copy size={11} />
          <span>copy</span>
        {/if}
      </button>
    </div>
  {:else}
    <div class="flex gap-2 text-xs text-white/40 px-1.5 items-center flex-wrap">
      <span>{message.sender}</span>
      {#if time}
        <span>·</span>
        <span>{time}</span>
      {/if}
      {#if metrics}
        <span>·</span>
        <span
          class="font-mono"
          title="time-to-first-token · tokens/second · total turn duration · output tokens"
        >
          {formatDuration(metrics.first_token_ms)} ttft
          {#if metrics.tps > 0}
            · {metrics.tps.toFixed(1)} tok/s
          {/if}
          · {formatDuration(metrics.total_ms)}
          · {metrics.output_tokens.toLocaleString()} tok
        </span>
      {/if}
      {#if modelAbbrev}
        <span>·</span>
        <span
          class="font-mono text-teal-300/70"
          title="LLM model that produced this reply{metrics?.slot ? ` (${metrics.slot} slot)` : ''} — full id: {metrics?.model}"
        >{slotIcon ? slotIcon + ' ' : ''}{modelAbbrev}</span>
      {/if}
      <!-- Action cluster: kept in ONE non-wrapping group so the buttons
           never scatter across two lines when the metrics row is long
           enough to force a flex wrap — the whole cluster moves to the
           next line together instead. -->
      <span class="ml-auto inline-flex items-center gap-2 whitespace-nowrap">
        {#if ttsEnabled && !streaming}
          <button
            type="button"
            class="inline-flex items-center gap-1 text-white/40 hover:text-teal-300 transition-colors cursor-pointer"
            onclick={toggleSpeak}
            title={speaking ? 'Stop speaking' : 'Read this reply aloud'}
            aria-label={speaking ? 'Stop speaking' : 'Speak reply'}
          >
            {#if speaking}
              <Square size={11} />
              <span>stop</span>
            {:else}
              <Volume2 size={11} />
              <span>speak</span>
            {/if}
          </button>
        {/if}
        {#if promptSnapshot}
          <button
            type="button"
            class="inline-flex items-center gap-1 text-white/40 hover:text-teal-300 transition-colors cursor-pointer disabled:opacity-50"
            onclick={openPromptSnapshot}
            disabled={opening}
            title="Open the exact prompt KinAI sent to the LLM in your default editor"
          >
            <Search size={11} />
            <span>{opening ? 'opening…' : 'prompt'}</span>
          </button>
        {/if}
        <button
          type="button"
          class="inline-flex items-center gap-1 text-white/40 hover:text-teal-300 transition-colors cursor-pointer"
          onclick={copyMessage}
          title={copyState === 'failed'
            ? 'Copy failed — try again'
            : 'Copy reply (with formatting) to clipboard'}
          aria-label="Copy reply"
        >
          {#if copyState === 'copied'}
            <Check size={11} />
            <span>copied</span>
          {:else if copyState === 'failed'}
            <Copy size={11} />
            <span>failed</span>
          {:else}
            <Copy size={11} />
            <span>copy</span>
          {/if}
        </button>
        {#if canRegenerate && isHost}
          <button
            type="button"
            class="inline-flex items-center gap-1 text-white/40 hover:text-teal-300 transition-colors cursor-pointer disabled:opacity-50"
            onclick={() => message.id && app.regenerateLast(message.id)}
            disabled={app.busy}
            title="Re-run this reply"
            aria-label="Regenerate reply"
          >
            <RefreshCw size={11} />
            <span>regenerate</span>
          </button>
        {/if}
      </span>
    </div>
  {/if}
</div>
