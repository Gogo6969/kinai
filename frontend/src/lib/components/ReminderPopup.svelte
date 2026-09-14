<script lang="ts">
  /**
   * Due-reminder popup — shows one of `app.dueReminders` at a time, with
   * a pager when more than one is waiting.
   *
   * Mounted by `+layout.svelte` next to ChangelogModal so it overlays
   * every route. Each button calls `app.reminderAction`, which re-syncs
   * the list and drops the id from the queue; the queue closes up and the
   * card at the same position takes over, otherwise the popup closes.
   *
   * It used to show `dueReminders[0]` and nothing else, so opening the
   * app after five had gone off meant five forced decisions with no way
   * to look ahead — and the only sign there were more was an 11px line at
   * 35% opacity, which the amber "late" badge hid in exactly the case
   * where it mattered (they share the header's right-hand slot, and
   * anything waiting since before launch is late by definition).
   *
   * Escape means "snooze 10 min" — never acknowledge: a reflex keypress
   * must not silently discard something the member asked to be told
   * about. Clicking the backdrop does nothing for the same reason. Left
   * and Right move between cards without deciding anything.
   */
  import { onMount } from 'svelte';
  import {
    BellRing,
    Check,
    ChevronLeft,
    ChevronRight,
    ExternalLink,
    Link as LinkIcon,
    MessageSquare,
    Repeat2,
  } from '@lucide/svelte';
  import { goto } from '$app/navigation';
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

  /** Which card is on screen. Kept as a POSITION rather than an id: the
   *  queue only ever appends or removes one row, so a clamped index lands
   *  on the sensible card every time — handle the middle one of five and
   *  the one that shifted into its place is next; handle the last and you
   *  step back; a reminder firing while the popup is open changes only
   *  the count. An id would have to answer "and if that id is gone?"
   *  every time. */
  let index = $state(0);
  const total = $derived(app.dueReminders.length);
  const pos = $derived(total === 0 ? 0 : Math.min(index, total - 1));
  const current = $derived(app.dueReminders[pos] ?? null);
  // Fold the clamp back, or a reminder arriving after the member handled
  // the last card would jump them forward to it.
  $effect(() => {
    if (index !== pos) index = pos;
  });
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
  /** How a repeat reads. "" for a one-off. */
  const repeatWords = $derived(
    ({
      daily: 'Repeats every day',
      weekdays: 'Repeats every weekday',
      weekly: 'Repeats every week',
      monthly: 'Repeats every month',
    } as Record<string, string>)[current?.repeat ?? ''] ?? ''
  );
  /** While a repeating reminder is outstanding, `due_local` already
   *  points at the NEXT occurrence — the member is being poked about the
   *  one in `occurrence_local`. Showing due_local here would tell them
   *  tomorrow's time for today's reminder. */
  const shownClock = $derived(
    current && repeatWords && current.occurrence_local
      ? current.occurrence_local
      : (current?.due_local ?? '')
  );

  const split = $derived(current ? splitLinks(current.text) : { prose: '', links: [] });
  const shownLinks = $derived(split.links.slice(0, 2));
  const moreLinks = $derived(Math.max(0, split.links.length - shownLinks.length));
  // Same string the clock is read from: a repeating reminder's `due_local`
  // is already pointing at tomorrow while today's occurrence is still on
  // screen, and taking the day from it labelled this morning's 07:00
  // "tomorrow".
  const day = $derived(
    current ? dayLabel(dayOf(shownClock), new Date(), current.tz).toLowerCase() : ''
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

  async function act(action: 'ack' | 'snooze' | 'stop', minutes?: number) {
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

  function go(to: number) {
    if (busy) return;
    index = Math.max(0, Math.min(to, total - 1));
  }

  function onKeyDown(e: KeyboardEvent) {
    // ChangelogModal (mounted first, stacked above) claims Escape with
    // preventDefault when it is open — don't snooze underneath it.
    if (!current || e.defaultPrevented) return;
    const t = e.target as HTMLElement | null;
    if (t?.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(t?.tagName ?? '')) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      void act('snooze', 10);
      return;
    }
    // Reading is not deciding: the arrows never touch the reminder.
    if (e.key === 'ArrowLeft' && pos > 0) {
      e.preventDefault();
      go(pos - 1);
    } else if (e.key === 'ArrowRight' && pos < total - 1) {
      e.preventDefault();
      go(pos + 1);
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
          <span class="text-[15px] font-medium tabular-nums">{timeOf(shownClock)}</span>
          <span class="text-xs text-white/40">{day}</span>
          <!-- The badge and the pager used to fight over this slot as an
               if/else. They both belong here: "late" is about this card,
               "3 of 5" is about the pile. -->
          <div class="ml-auto flex items-center gap-1.5 shrink-0 self-center">
            {#if late}
              <span class="kin-rem-late text-[11px] rounded-full px-2.5 py-0.5">
                {late}
              </span>
            {/if}
            {#if total > 1}
              <div
                class="kin-rem-inset flex items-center rounded-lg p-[3px]"
                role="group"
                aria-label="Move between the reminders waiting"
              >
                <button
                  type="button"
                  class="kin-rem-page"
                  disabled={busy || pos === 0}
                  aria-label="Previous reminder"
                  onclick={() => go(pos - 1)}
                >
                  <ChevronLeft size={14} aria-hidden="true" />
                </button>
                <span class="kin-rem-count px-1 text-[11px] tabular-nums">{pos + 1} of {total}</span>
                <button
                  type="button"
                  class="kin-rem-page"
                  disabled={busy || pos >= total - 1}
                  aria-label="Next reminder"
                  onclick={() => go(pos + 1)}
                >
                  <ChevronRight size={14} aria-hidden="true" />
                </button>
              </div>
            {/if}
          </div>
        </div>
      </header>

      <!-- Cycling changes the dialog's contents under a screen reader
           without any announcement, and a live region created at the
           moment its first message appears is not read out — so this one
           exists for as long as the popup does. -->
      <p class="sr-only" aria-live="polite">
        {total > 1 ? `Reminder ${pos + 1} of ${total}. ` : ''}{heroText}
      </p>

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
        {#if current.thread_id}
          <button
            type="button"
            class="kin-rem-inset mt-3 flex w-full items-center gap-2 rounded-lg px-3 py-2.5"
            onclick={async () => {
              const r = current;
              if (!r) return;
              await act('snooze', 10);
              await app.openReminderThread(r);
              await goto('/');
            }}
          >
            <MessageSquare size={15} class="text-white/40 shrink-0" aria-hidden="true" />
            <span class="kin-rem-accent text-xs">Open the conversation this came from</span>
          </button>
        {/if}

        {#if moreLinks > 0}
          <p class="mt-2 text-[11px] text-white/35">+{moreLinks} more link{moreLinks > 1 ? 's' : ''} in this reminder</p>
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
      {#if repeatWords}
        <!-- Deliberately below the footer and quiet: Done means "this one
             is handled" and must stay the obvious action. Stopping a
             series is a different, irreversible intent and should never
             sit next to it. -->
        <div class="px-5 pb-4 -mt-1 flex items-center gap-2 text-[11px] text-white/40">
          <Repeat2 size={12} aria-hidden="true" />
          <span>{repeatWords} · next {timeOf(current.due_local)}</span>
          <button
            type="button"
            class="ml-auto underline hover:text-white/70 disabled:opacity-50"
            disabled={busy}
            onclick={() => {
              if (confirm(`Stop this repeating reminder?\n\nIt will not come back. Done just handles today's.`))
                act('stop');
            }}
          >
            Stop repeating
          </button>
        </div>
      {/if}
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

  /* Pager chevrons. `disabled:hover:bg-transparent` is repeated in the
     light block on purpose: `html.light .kin-rem-page:hover` outranks
     `.kin-rem-page:hover:disabled` on specificity, so without it a dead
     chevron still lights up under the pointer on white. */
  .kin-rem-page {
    @apply rounded-md p-1 text-ink-50/70 hover:bg-white/10 hover:text-ink-50
           transition-colors disabled:opacity-30 disabled:hover:bg-transparent;
  }
  :global(html.light) .kin-rem-page {
    @apply text-ink-900/60 hover:bg-black/5 hover:text-ink-900
           disabled:hover:bg-transparent;
  }

  /* "3 of 5". A bare text-white/55 was invisible on the light card — the
     same trap the accent and late colours above are spelled out for. */
  .kin-rem-count {
    @apply text-ink-50/60;
  }
  :global(html.light) .kin-rem-count {
    @apply text-ink-900/60;
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
