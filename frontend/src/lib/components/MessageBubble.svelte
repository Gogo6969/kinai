<script lang="ts">
  import type { Attachment, Message, TurnMetrics } from '$lib/api';
  import { renderMarkdown } from '$lib/markdown';
  import { app } from '$lib/stores/app.svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { FileText, Search } from '@lucide/svelte';

  let {
    message,
    streaming = false,
    metrics = null,
  }: {
    message: { content: string; role: string; sender: string; created_at?: string } & Partial<Message>;
    streaming?: boolean;
    metrics?: TurnMetrics | null;
  } = $props();

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

<div class="flex flex-col gap-1 {isUser ? 'items-end' : 'items-start'}">
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
  {#if isUser}
    <!-- User bubbles: the right-aligned teal bubble already says "you" by
         position + color, so the name/timestamp row is suppressed to reduce
         visual noise. -->
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
      {#if promptSnapshot}
        <button
          type="button"
          class="ml-1 inline-flex items-center gap-1 text-white/40 hover:text-teal-300 transition-colors cursor-pointer disabled:opacity-50"
          onclick={openPromptSnapshot}
          disabled={opening}
          title="Open the exact prompt KinAI sent to the LLM in your default editor"
        >
          <Search size={11} />
          <span>{opening ? 'opening…' : 'prompt'}</span>
        </button>
      {/if}
    </div>
  {/if}
</div>
