<script lang="ts">
  /**
   * Due-reminder popup — shows the first of `app.dueReminders`.
   *
   * Mounted by `+layout.svelte` next to ChangelogModal so it overlays
   * every route. Each button calls `app.reminderAction`, which re-syncs
   * the list and drops the id from the queue; the next queued reminder
   * (if any) takes its place, otherwise the popup closes.
   *
   * Escape means "snooze 10 min" — never acknowledge: a reflex keypress
   * must not silently discard something the member asked to be told
   * about. Clicking the backdrop does nothing for the same reason.
   */
  import { onMount } from 'svelte';
  import { BellRing, Check, ExternalLink, Link as LinkIcon } from '@lucide/svelte';
  import { app } from '$lib/stores/app.svelte';
  import {
    dayLabel,
    dayOf,
    lateness,
    minutesUntilTomorrowNine,
    shortLink,
    splitLinks,
    timeOf,
  } from '$lib/reminders';

  const current = $derived(app.dueReminders[0] ?? null);
  let busy = $state(false);

  /** Re-render the "late" badge as time passes, so a popup left on screen
   *  does not keep insisting it is one minute late an hour later. */
  let tick = $state(0);
  onMount(() => {
    const id = setInterval(() => (tick += 1), 30_000);
    return () => clearInterval(id);
  });

  /** Words to read, and addresses to tap, kept apart so a long URL can
   *  never push the sentence off the card. At most two rows are shown. */
  const split = $derived(current ? splitLinks(current.text) : { prose: '', links: [] });
  const shownLinks = $derived(split.links.slice(0, 2));
  const moreLinks = $derived(Math.max(0, split.links.length - shownLinks.length));
  const day = $derived(
    current ? dayLabel(dayOf(current.due_local), new Date(), current.tz).toLowerCase() : ''
  );
  const late = $derived.by(() => {
    void tick;
    return current ? lateness(current.due_at) : null;
  });
  /** A reminder that is only links still needs something to read. */
  const heroText = $derived(split.prose || (current ? current.text : ''));

  /** Long reminders collapse to a few lines with a "Show more". Whether
   *  the text actually overflows is measured rather than guessed from a
   *  character count — wrapping depends on the words, the font size and
   *  the member's UI scale. */
  let proseEl = $state<HTMLParagraphElement | null>(null);
  let expanded = $state(false);
  let overflows = $state(false);
  $effect(() => {
    // Re-measure whenever the reminder or the expansion changes.
    void heroText;
    void expanded;
    const el = proseEl;
    if (!el) return;
    // Measured while collapsed; expanding never un-overflows it.
    if (!expanded) overflows = el.scrollHeight > el.clientHeight + 1;
  });
  // A new reminder starts collapsed, however the last one was left.
  $effect(() => {
    void current?.id;
    expanded = false;
  });

  async function act(action: 'ack' | 'snooze', minutes?: number) {
    const r = current;
    if (!r || busy) return;
    busy = true;
    try {
      await app.reminderAction(r.id, action, minutes);
    } catch (e) {
      const msg = String(e).replace(/^Error:\s*/, '');
      console.warn('reminder action failed:', e);
      window.dispatchEvent(
        new CustomEvent('kin-toast', {
          detail: { msg: `✗ Couldn't update the reminder: ${msg}`, ms: 5000 },
        })
      );
      // Don't hold the whole UI hostage behind a failing call. The row is
      // still `fired` on the host, so the next list refresh brings it back.
      app.dueReminders = app.dueReminders.filter((x) => x.id !== r.id);
    } finally {
      busy = false;
    }
  }

  function onKeyDown(e: KeyboardEvent) {
    // ChangelogModal (mounted first, stacked above) claims Escape with
    // preventDefault when it is open — don't snooze underneath it.
    if (e.key === 'Escape' && current && !e.defaultPrevented) {
      e.preventDefault();
      void act('snooze', 10);
    }
  }

  onMount(() => {
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  });
</script>

{#if current}
  <!-- Backdrop is inert on purpose (see above); the card's buttons are
       the only way out. -->
  <div
    class="fixed inset-0 z-[90] bg-black/80 backdrop-blur-sm flex items-center justify-center p-4 animate-fade-in"
    role="presentation"
  >
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="reminder-title"
      class="kin-card kin-reminder-card max-w-md w-full !p-0 cursor-default text-left"
      tabindex="-1"
    >
      <header class="px-5 pt-4 pb-0">
        <div class="flex items-baseline gap-2">
          <BellRing size={15} class="kin-rem-accent self-center shrink-0" aria-hidden="true" />
          <span class="text-[15px] font-medium tabular-nums">{timeOf(current.due_local)}</span>
          <span class="text-xs text-white/40">{day}</span>
          {#if late}
            <span
              class="kin-rem-late ml-auto text-[11px] rounded-full px-2.5 py-0.5 shrink-0"
            >
              {late}
            </span>
          {:else if app.dueReminders.length > 1}
            <span class="ml-auto text-[11px] text-white/40 shrink-0">
              +{app.dueReminders.length - 1} waiting
            </span>
          {/if}
        </div>
      </header>

      <div class="px-5 pt-3.5 {expanded ? 'max-h-[45vh] overflow-y-auto' : ''}">
        <p
          bind:this={proseEl}
          id="reminder-title"
          class="text-[18px] font-medium leading-snug whitespace-pre-wrap break-words
                 {expanded ? '' : 'kin-clamp'}"
        >
          {heroText}
        </p>
        {#if overflows}
          <button
            type="button"
            class="kin-rem-accent mt-1.5 text-xs hover:opacity-80 transition-opacity"
            aria-expanded={expanded}
            onclick={() => (expanded = !expanded)}
          >
            {expanded ? 'Show less' : 'Show more'}
          </button>
        {/if}

        {#each shownLinks as href (href)}
          <a
            {href}
            class="kin-rem-inset mt-3 flex items-center gap-2 rounded-lg px-3 py-2.5 no-underline"
            title={href}
          >
            <LinkIcon size={15} class="text-white/40 shrink-0" aria-hidden="true" />
            <span class="kin-rem-accent text-xs truncate min-w-0">{shortLink(href)}</span>
            <ExternalLink size={14} class="text-white/35 shrink-0 ml-auto" aria-hidden="true" />
          </a>
        {/each}
        {#if moreLinks > 0}
          <p class="mt-2 text-[11px] text-white/35">+{moreLinks} more link{moreLinks > 1 ? 's' : ''} in this reminder</p>
        {/if}

        {#if app.dueReminders.length > 1 && late}
          <p class="mt-2 text-[11px] text-white/35">
            +{app.dueReminders.length - 1} more reminder{app.dueReminders.length > 2 ? 's' : ''} waiting
          </p>
        {/if}
      </div>

      <footer class="px-5 pt-4 pb-4 flex items-center gap-2">
        <div
          class="kin-rem-inset flex gap-1 rounded-lg p-[3px]"
          role="group"
          aria-label="Remind me again later"
        >
          <button
            type="button"
            class="kin-snooze"
            disabled={busy}
            onclick={() => act('snooze', 10)}
          >
            10 min
          </button>
          <button
            type="button"
            class="kin-snooze"
            disabled={busy}
            onclick={() => act('snooze', 60)}
          >
            1 hour
          </button>
          <button
            type="button"
            class="kin-snooze"
            disabled={busy}
            onclick={() => act('snooze', minutesUntilTomorrowNine())}
          >
            Tomorrow
          </button>
        </div>
        <button
          type="button"
          class="kin-btn-primary ml-auto"
          disabled={busy}
          onclick={() => act('ack')}
        >
          <Check size={15} /> {busy ? 'Saving…' : 'Done'}
        </button>
      </footer>
    </div>
  </div>
{/if}

<style>
  /* Opaque surface, same reason as ChangelogModal: `kin-card` is only
     bg-white/5 and leans on backdrop-blur, which WebKitGTK (the Linux
     client) doesn't render — the chat would bleed through the popup. */
  :global(.kin-reminder-card) {
    background-color: #0f172a !important;
  }
  :global(html.light .kin-reminder-card) {
    background-color: #ffffff !important;
  }

  /* Segmented snooze control: the three options share one surface so the
     eye reads them as a single "later" choice, leaving Done as the only
     button competing for attention. */
  .kin-snooze {
    @apply rounded-md px-2.5 py-1.5 text-xs font-medium text-ink-50/75
           hover:bg-white/10 hover:text-ink-50 transition-colors
           disabled:opacity-40 disabled:hover:bg-transparent;
  }
  :global(html.light) .kin-snooze {
    @apply text-ink-900/70 hover:bg-black/5 hover:text-ink-900;
  }

  /* Dark is the default; the light theme is an allowlist elsewhere in
     app.css, so anything new here states both sides itself. The teal and
     amber that read well on #0f172a are far too pale on white. */
  .kin-rem-accent {
    @apply text-teal-300;
  }
  :global(html.light) .kin-rem-accent {
    @apply text-teal-700;
  }

  .kin-rem-late {
    @apply text-amber-300 bg-amber-400/15;
  }
  :global(html.light) .kin-rem-late {
    @apply text-amber-800 bg-amber-400/25;
  }

  /* Inset surfaces (the link row, the snooze group) need a tint that is
     visible against the card in both themes — white-on-white vanishes. */
  .kin-rem-inset {
    @apply border border-white/10 bg-white/[0.04];
  }
  a.kin-rem-inset:hover,
  .kin-rem-inset:hover {
    @apply bg-white/[0.08];
  }
  :global(html.light) .kin-rem-inset {
    @apply border-black/10 bg-black/[0.03];
  }
  :global(html.light) a.kin-rem-inset:hover,
  :global(html.light) .kin-rem-inset:hover {
    @apply bg-black/[0.06];
  }

  /* Four lines is enough to read the gist of a long reminder without the
     popup growing taller than the window it interrupts. */
  .kin-clamp {
    display: -webkit-box;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 4;
    line-clamp: 4;
    overflow: hidden;
  }
</style>
