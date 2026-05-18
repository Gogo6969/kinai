<script lang="ts">
  import type { Attachment, Message, TurnMetrics } from '$lib/api';
  import { renderMarkdown } from '$lib/markdown';
  import { app } from '$lib/stores/app.svelte';
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
  // and any turn the host couldn't serialize. The 🔍 toggle only
  // shows when this is non-empty.
  const promptSnapshot = $derived(message.id ? app.promptDebug[message.id] : undefined);
  let showPrompt = $state(false);

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
      {#if promptSnapshot}
        <button
          type="button"
          class="ml-1 inline-flex items-center gap-1 text-white/40 hover:text-teal-300 transition-colors"
          onclick={() => (showPrompt = !showPrompt)}
          title="Show the exact prompt KinAI sent to the LLM for this reply"
        >
          <Search size={11} />
          <span>prompt</span>
        </button>
      {/if}
    </div>
    {#if showPrompt && promptSnapshot}
      <details
        open
        class="mt-2 mx-1.5 rounded-lg border border-white/10 bg-black/40 text-xs"
      >
        <summary class="cursor-pointer select-none px-3 py-2 text-white/60 hover:text-white">
          Prompt that produced this reply ({promptSnapshot.length.toLocaleString()} chars)
        </summary>
        <pre
          class="overflow-x-auto p-3 max-h-[60vh] whitespace-pre-wrap break-words text-[11px] leading-snug text-white/75 font-mono">{promptSnapshot}</pre>
      </details>
    {/if}
  {/if}
</div>
