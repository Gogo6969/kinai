<script lang="ts">
  import { onDestroy } from 'svelte';
  import { Brain, ChevronDown, ChevronRight } from '@lucide/svelte';

  let { text, live = false }: { text: string; live?: boolean } = $props();
  let open = $state(false);

  // --- Streaming performance ---
  // A deep reasoning model can emit hundreds of KB of chain-of-thought. The
  // old panel re-rendered (and the browser re-laid-out) the FULL pre-wrap
  // text node on every incoming delta, which froze the UI once the panel was
  // expanded — collapsed felt fine because the text node isn't in the DOM at
  // all. Two bounds fix it:
  //   1. Throttle: the visible text updates at most every THROTTLE_MS while
  //      streaming (leading + trailing, so nothing is ever lost).
  //   2. Tail cap: while live, only the last TAIL_CHARS are rendered — the
  //      newest thinking is what the user is watching anyway. The full trace
  //      renders once thinking finishes.
  const THROTTLE_MS = 150;
  const TAIL_CHARS = 6_000;

  let shown = $state('');
  let timer: ReturnType<typeof setTimeout> | null = null;
  let trailing = false;

  function tail(s: string): string {
    return s.length > TAIL_CHARS ? s.slice(s.length - TAIL_CHARS) : s;
  }

  $effect(() => {
    const full = text; // tracked
    if (!live) {
      // Finished: cancel any pending tick and render the complete trace once.
      if (timer) {
        clearTimeout(timer);
        timer = null;
      }
      trailing = false;
      shown = full;
      return;
    }
    if (timer) {
      // A tick is already scheduled — remember to repaint when it fires.
      trailing = true;
      return;
    }
    shown = tail(full);
    timer = setTimeout(() => {
      timer = null;
      if (trailing) {
        trailing = false;
        // `text` is a reactive prop getter — this reads the LATEST value.
        shown = tail(text);
      }
    }, THROTTLE_MS);
  });
  onDestroy(() => {
    if (timer) clearTimeout(timer);
  });

  // --- Stick-to-bottom scrolling ---
  // While thinking streams, the content grows faster than a human can chase
  // the scrollbar ("can't scroll to the end"). Pin the view to the bottom so
  // the newest thought is always visible; release the pin the moment the user
  // scrolls up to read something, re-engage when they return to the bottom.
  let scroller = $state<HTMLDivElement | null>(null);
  let follow = $state(true);

  function onScroll() {
    if (!scroller) return;
    follow = scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 48;
  }

  function toggle() {
    open = !open;
    if (open) follow = true; // expanding always lands on the newest thinking
  }

  $effect(() => {
    void shown; // repaint hook: run after each visible-text update
    if (open && follow && scroller) {
      scroller.scrollTop = scroller.scrollHeight;
    }
  });

  const hiddenChars = $derived(live && text.length > TAIL_CHARS ? text.length - TAIL_CHARS : 0);
</script>

{#if text && text.trim().length > 0}
  <div class="self-start max-w-[85%] rounded-2xl rounded-bl-md bg-white/[0.03] border border-white/10 text-white/70 text-sm">
    <button
      type="button"
      class="w-full flex items-center gap-2 px-3 py-2 text-left hover:bg-white/[0.04] rounded-2xl rounded-bl-md transition-colors"
      onclick={toggle}
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
      <div
        bind:this={scroller}
        onscroll={onScroll}
        class="px-3 pb-3 pt-1 text-xs leading-relaxed text-white/55 whitespace-pre-wrap border-t border-white/5 max-h-72 overflow-y-auto font-mono"
      >
        {#if hiddenChars > 0}
          <div class="not-italic text-white/35 mb-1">
            … following the newest thinking ({hiddenChars.toLocaleString()} earlier chars appear when it finishes)
          </div>
        {/if}
        {shown}
      </div>
    {/if}
  </div>
{/if}
