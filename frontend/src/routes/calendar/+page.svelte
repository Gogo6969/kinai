<script lang="ts">
  import { app } from '$lib/stores/app.svelte';
  import type { Reminder } from '$lib/api';
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { Check, Clock, Link as LinkIcon, RefreshCw, Trash2 } from '@lucide/svelte';
  import {
    dayLabel,
    dayOf,
    minutesUntilTomorrowNine,
    shortLink,
    splitLinks,
    timeOf,
  } from '$lib/reminders';

  onMount(() => {
    void app.loadReminders();
  });

  /** Reminders bucketed by the day of `due_local`, chronological.
   *  Ordered by `due_at` — the real instant — so a member who set
   *  reminders in two zones still sees them in the order they fire, and
   *  grouped by day AND zone so a heading is never a claim about the
   *  wrong day. */
  const days = $derived.by(() => {
    const byDay = new Map<string, { key: string; tz: string; items: Reminder[] }>();
    const sorted = [...app.reminders].sort((a, b) => a.due_at.localeCompare(b.due_at));
    for (const r of sorted) {
      const key = dayOf(r.due_local);
      const mapKey = `${key}|${r.tz}`;
      const group = byDay.get(mapKey);
      if (group) group.items.push(r);
      else byDay.set(mapKey, { key, tz: r.tz, items: [r] });
    }
    return [...byDay.values()].map((g) => ({
      key: `${g.key}|${g.tz}`,
      label: dayLabel(g.key, new Date(), g.tz),
      items: g.items,
    }));
  });

  /** The device's own zone, so a row set elsewhere can say where its
   *  time belongs instead of quietly showing a foreign clock. */
  const deviceTz = Intl.DateTimeFormat().resolvedOptions().timeZone ?? '';

  /** Ids with an action in flight — their buttons are disabled, so a
   *  second click cannot cross-cancel the first request. */
  let pending = $state<string[]>([]);

  /** Ids whose full text is showing. A reminder can now run to about a
   *  hundred words, and a list where every row is a paragraph is not a
   *  calendar — so rows clamp to two lines until asked. */
  let opened = $state<string[]>([]);
  const toggleOpen = (id: string) =>
    (opened = opened.includes(id) ? opened.filter((x) => x !== id) : [...opened, id]);

  /** Id of the fired row whose Snooze menu is open (one at a time). */
  let snoozeOpen = $state<string | null>(null);

  async function act(id: string, action: 'ack' | 'snooze' | 'delete', minutes?: number) {
    snoozeOpen = null;
    if (pending.includes(id)) return;
    pending = [...pending, id];
    try {
      await app.reminderAction(id, action, minutes);
    } catch (e) {
      const msg = String(e).replace(/^Error:\s*/, '');
      console.warn('reminder action failed:', e);
      window.dispatchEvent(
        new CustomEvent('kin-toast', {
          detail: { msg: `✗ Couldn't update the reminder: ${msg}`, ms: 5000 },
        })
      );
    } finally {
      pending = pending.filter((x) => x !== id);
    }
  }

  function remove(r: Reminder) {
    if (!confirm('Delete this reminder?')) return;
    void act(r.id, 'delete');
  }
</script>

<!-- Close an open Snooze menu on a click anywhere else, or on Escape. -->
<svelte:window
  onclick={(e) => {
    if (snoozeOpen && !(e.target as HTMLElement | null)?.closest('[data-snooze]')) {
      snoozeOpen = null;
    }
  }}
  onkeydown={(e) => {
    if (e.key === 'Escape') snoozeOpen = null;
  }}
/>

<main class="h-screen w-screen bg-ink-950 overflow-y-auto">
  <div class="max-w-3xl mx-auto px-8 py-10 space-y-6">
    <header class="flex items-center justify-between gap-3">
      <div class="flex items-center gap-2">
        <button class="kin-btn" onclick={() => goto('/')} title="Back to chat">←</button>
        <h1 class="text-2xl font-bold tracking-tight">Calendar</h1>
      </div>
      <div class="flex gap-2">
        <button class="kin-btn" onclick={() => app.loadReminders()}>
          <RefreshCw size={14} /> Refresh
        </button>
      </div>
    </header>

    <p class="text-sm text-white/50 -mt-2">
      Reminders you asked KinAI to keep. Times are shown the way you set them.
    </p>

    {#if app.reminders.length === 0}
      <div class="kin-card text-center text-white/50">
        Nothing scheduled. Ask KinAI:
        <span class="text-white/70">“Remind me tomorrow at 9 to …”</span>
      </div>
    {/if}

    {#snippet row(r: Reminder)}
      {@const parts = splitLinks(r.text)}
      {@const isOpen = opened.includes(r.id)}
      <!-- The actions wrap BELOW the text when the window is narrow: with
           everything locked on one line the text column collapsed to a
           three-word ribbon. `min-w` on the text is what forces the wrap. -->
      <div
        class="kin-card !py-3 flex flex-wrap items-start gap-x-3 gap-y-2
               {r.status === 'done' ? 'opacity-60' : ''}"
      >
        <div class="shrink-0 w-12 pt-0.5">
          <div class="font-mono text-sm text-teal-300">{timeOf(r.due_local)}</div>
          {#if r.tz && deviceTz && r.tz !== deviceTz}
            <!-- Set in another zone: say so, or the clock reads as local. -->
            <div class="text-[10px] text-white/40 truncate" title={r.tz}>
              {r.tz.split('/').pop()?.replace(/_/g, ' ')}
            </div>
          {/if}
        </div>
        <div class="flex-1 min-w-[15rem]">
          <p
            class="text-sm whitespace-pre-wrap break-words {isOpen ? '' : 'kin-row-clamp'}"
          >
            {parts.prose || r.text}
          </p>
          {#if (parts.prose || r.text).length > 90}
            <button
              class="mt-1 text-xs text-teal-300 hover:text-teal-200 transition-colors"
              aria-expanded={isOpen}
              onclick={() => toggleOpen(r.id)}
            >
              {isOpen ? 'Show less' : 'Show more'}
            </button>
          {/if}
          {#each parts.links.slice(0, 2) as href (href)}
            <a
              {href}
              class="mt-1.5 flex items-center gap-1.5 text-xs text-teal-300 hover:text-teal-200
                     transition-colors no-underline max-w-full"
              title={href}
            >
              <LinkIcon size={12} class="shrink-0 opacity-70" aria-hidden="true" />
              <span class="truncate min-w-0">{shortLink(href, 46)}</span>
            </a>
          {/each}
          {#if parts.links.length > 2}
            <p class="mt-1 text-[11px] text-white/35">
              +{parts.links.length - 2} more link{parts.links.length > 3 ? 's' : ''}
            </p>
          {/if}
        </div>
        <div class="flex items-center gap-2 shrink-0 ml-auto">
          {#if r.status === 'fired'}
            <span class="kin-badge !bg-amber-400/20 !text-amber-200 shrink-0">due now</span>
          {:else if r.status === 'done'}
            <span class="kin-badge shrink-0">done</span>
          {/if}
          <div class="flex gap-1 shrink-0">
          {#if r.status === 'fired'}
            <button
              class="kin-btn-ghost text-teal-300/80 hover:text-teal-300"
              onclick={() => act(r.id, 'ack')}
              disabled={pending.includes(r.id)}
              title="Mark as done"
            >
              <Check size={14} /> Done
            </button>
            <div class="relative" data-snooze>
              <button
                class="kin-btn-ghost text-white/60"
                onclick={() => (snoozeOpen = snoozeOpen === r.id ? null : r.id)}
                disabled={pending.includes(r.id)}
                title="Remind me again later"
                aria-haspopup="menu"
                aria-expanded={snoozeOpen === r.id}
              >
                <Clock size={14} /> Snooze
              </button>
              {#if snoozeOpen === r.id}
                <div
                  class="absolute right-0 mt-1 z-10 kin-glass rounded-lg p-1 min-w-[10rem] flex flex-col"
                  role="menu"
                >
                  <button
                    class="kin-btn-ghost justify-start"
                    role="menuitem"
                    disabled={pending.includes(r.id)}
                    onclick={() => act(r.id, 'snooze', 10)}
                  >
                    10 min
                  </button>
                  <button
                    class="kin-btn-ghost justify-start"
                    role="menuitem"
                    disabled={pending.includes(r.id)}
                    onclick={() => act(r.id, 'snooze', 60)}
                  >
                    1 h
                  </button>
                  <button
                    class="kin-btn-ghost justify-start"
                    role="menuitem"
                    disabled={pending.includes(r.id)}
                    onclick={() => act(r.id, 'snooze', minutesUntilTomorrowNine())}
                  >
                    Tomorrow 9:00
                  </button>
                </div>
              {/if}
            </div>
          {:else}
            <button
              class="kin-btn-ghost text-red-300/70 hover:text-red-300"
              onclick={() => remove(r)}
              title="Delete this reminder"
            >
              <Trash2 size={14} />
            </button>
          {/if}
          </div>
        </div>
      </div>
    {/snippet}

    {#each days as day (day.key)}
      <section class="space-y-2">
        <h2
          class="text-sm font-semibold {day.label === 'Today' || day.label === 'Tomorrow'
            ? 'text-teal-300'
            : 'text-white/50'}"
        >
          {day.label}
        </h2>
        {#each day.items as r (r.id)}
          {@render row(r)}
        {/each}
      </section>
    {/each}
  </div>
</main>

<style>
  /* Two lines keeps a list of reminders scannable; the rest is one click
     away, and the popup shows it in full when the reminder fires. */
  .kin-row-clamp {
    display: -webkit-box;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    overflow: hidden;
  }
</style>
