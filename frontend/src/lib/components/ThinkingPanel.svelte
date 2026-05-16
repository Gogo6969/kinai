<script lang="ts">
  import { Brain, ChevronDown, ChevronRight } from '@lucide/svelte';

  let { text, live = false }: { text: string; live?: boolean } = $props();
  let open = $state(false);

  // Auto-collapse when finished streaming (live → false) once content arrives.
  // For now always start collapsed; user can expand.
</script>

{#if text && text.trim().length > 0}
  <div class="self-start max-w-[85%] rounded-2xl rounded-bl-md bg-white/[0.03] border border-white/10 text-white/70 text-sm">
    <button
      type="button"
      class="w-full flex items-center gap-2 px-3 py-2 text-left hover:bg-white/[0.04] rounded-2xl rounded-bl-md transition-colors"
      onclick={() => (open = !open)}
    >
      {#if open}
        <ChevronDown size={14} class="opacity-60" />
      {:else}
        <ChevronRight size={14} class="opacity-60" />
      {/if}
      <Brain size={14} class={live ? 'text-teal-300 animate-pulse-soft' : 'text-white/40'} />
      <span class="text-xs font-medium uppercase tracking-wider">
        {live ? 'Thinking…' : 'Reasoning'}
      </span>
      <span class="text-xs text-white/40 ml-auto">
        {text.length.toLocaleString()} chars
      </span>
    </button>
    {#if open}
      <div class="px-3 pb-3 pt-1 text-xs leading-relaxed text-white/55 whitespace-pre-wrap border-t border-white/5 max-h-72 overflow-y-auto font-mono">
        {text}
      </div>
    {/if}
  </div>
{/if}
