<script lang="ts">
  import { app } from '$lib/stores/app.svelte';
  import type { Reminder } from '$lib/api';
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { Check, Clock, RefreshCw, Trash2 } from '@lucide/svelte';
  import { dayLabel, dayOf, minutesUntilTomorrowNine, timeOf } from '$lib/reminders';

  onMount(() => {
    void app.loadReminders();
  });

  /** Reminders bucketed by the day of `due_local`, chronological. */
  const days = $derived.by(() => {
    const byDay = new Map<string, Reminder[]>();
    const sorted = [...app.reminders].sort((a, b) => a.due_local.localeCompare(b.due_local));
    for (const r of sorted) {
      const key = dayOf(r.due_local);
      const list = byDay.get(key);
      if (list) list.push(r);
      else byDay.set(key, [r]);
    }
    return [...byDay].map(([key, items]) => ({ key, label: dayLabel(key), items }));
  });

  /** Id of the fired row whose Snooze menu is open (one at a time). */
  let snoozeOpen = $state<string | null>(null);

  async function act(id: string, action: 'ack' | 'snooze' | 'delete', minutes?: number) {
    snoozeOpen = null;
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
      <div class="kin-card !py-3 flex items-center gap-3 {r.status === 'done' ? 'opacity-60' : ''}">
        <div class="font-mono text-sm text-teal-300 shrink-0 w-12">{timeOf(r.due_local)}</div>
        <div class="flex-1 min-w-0 text-sm whitespace-pre-wrap break-words">{r.text}</div>
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
              title="Mark as done"
            >
              <Check size={14} /> Done
            </button>
            <div class="relative" data-snooze>
              <button
                class="kin-btn-ghost text-white/60"
                onclick={() => (snoozeOpen = snoozeOpen === r.id ? null : r.id)}
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
                    onclick={() => act(r.id, 'snooze', 10)}
                  >
                    10 min
                  </button>
                  <button
                    class="kin-btn-ghost justify-start"
                    role="menuitem"
                    onclick={() => act(r.id, 'snooze', 60)}
                  >
                    1 h
                  </button>
                  <button
                    class="kin-btn-ghost justify-start"
                    role="menuitem"
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
