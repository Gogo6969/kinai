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
  import { Check } from '@lucide/svelte';
  import { app } from '$lib/stores/app.svelte';
  import { minutesUntilTomorrowNine, timeOf } from '$lib/reminders';

  const current = $derived(app.dueReminders[0] ?? null);
  let busy = $state(false);

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
      class="kin-card kin-reminder-card max-w-md w-full p-0 cursor-default text-left"
      tabindex="-1"
    >
      <header class="px-5 py-4 border-b border-white/10 flex items-start justify-between gap-3">
        <div class="min-w-0">
          <div class="text-xs uppercase tracking-wider text-white/40">KinAI</div>
          <h2 id="reminder-title" class="text-lg font-semibold">⏰ Reminder</h2>
        </div>
        {#if app.dueReminders.length > 1}
          <span
            class="kin-badge !bg-amber-400/20 !text-amber-200 shrink-0"
            title="More reminders are waiting behind this one"
          >
            +{app.dueReminders.length - 1} more
          </span>
        {/if}
      </header>

      <div class="px-5 py-5 space-y-2">
        <p class="text-xl font-medium leading-snug whitespace-pre-wrap break-words">
          {current.text}
        </p>
        <p class="text-sm text-white/50">due {timeOf(current.due_local)}</p>
      </div>

      <footer class="px-5 py-3 border-t border-white/10 flex flex-wrap items-center justify-end gap-2">
        <button type="button" class="kin-btn" disabled={busy} onclick={() => act('snooze', 10)}>
          Snooze 10 min
        </button>
        <button type="button" class="kin-btn" disabled={busy} onclick={() => act('snooze', 60)}>
          Snooze 1 h
        </button>
        <button
          type="button"
          class="kin-btn"
          disabled={busy}
          onclick={() => act('snooze', minutesUntilTomorrowNine())}
        >
          Tomorrow 9:00
        </button>
        <button type="button" class="kin-btn-primary" disabled={busy} onclick={() => act('ack')}>
          <Check size={14} /> {busy ? 'Saving…' : 'Acknowledge'}
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
</style>
