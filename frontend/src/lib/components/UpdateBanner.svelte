<script lang="ts">
  import { api } from '$lib/api';
  import { app } from '$lib/stores/app.svelte';
  import { Download, Loader2, X } from '@lucide/svelte';

  let installing = $state(false);
  let dismissed = $state(false);
  let error = $state('');

  const update = $derived(app.updateAvailable);
  const progress = $derived(app.updateProgress);

  async function install() {
    if (!update || installing) return;
    installing = true;
    error = '';
    app.updateProgress = 0;
    try {
      // The Tauri command relaunches the app on success — this promise
      // typically never resolves cleanly. We still catch errors for the
      // failure path.
      await api.installUpdate();
    } catch (e) {
      installing = false;
      app.updateProgress = null;
      error = String(e).replace(/^Error:\s*/, '');
    }
  }

  function dismiss() {
    // Local dismissal only — the next periodic check (~4h later) will
    // re-show if the update is still pending. We deliberately don't
    // persist "user said no to this version forever" because letting
    // someone permanently snooze a security fix would be bad.
    dismissed = true;
  }
</script>

{#if update && !dismissed}
  <div
    class="border-b border-teal-500/30 bg-gradient-to-r from-teal-500/10 to-teal-400/5
           px-4 py-2.5 flex items-center gap-3 text-sm"
    role="status"
  >
    <Download size={16} class="text-teal-300 flex-shrink-0" />
    <div class="flex-1 min-w-0">
      <span class="font-medium text-teal-100">KinAI v{update.version} is available</span>
      <span class="text-white/50 ml-2">
        (you're on v{update.current})
        {#if update.source === 'host'}
          · from your host
        {:else}
          · from GitHub
        {/if}
      </span>
      {#if error}
        <div class="text-xs text-red-300 mt-1">{error}</div>
      {/if}
      {#if installing && progress !== null}
        <div class="text-xs text-white/60 mt-1 font-mono">
          Downloading… {progress.toFixed(0)}%
        </div>
      {/if}
    </div>
    {#if installing}
      <button
        type="button"
        class="kin-btn !text-xs opacity-70 cursor-not-allowed"
        disabled
        aria-busy="true"
      >
        <Loader2 size={12} class="animate-spin" /> Installing…
      </button>
    {:else}
      <button
        type="button"
        class="kin-btn-primary !text-xs !px-3 !py-1.5"
        onclick={install}
      >
        Install & restart
      </button>
      <button
        type="button"
        class="text-white/40 hover:text-white/70 p-1"
        onclick={dismiss}
        aria-label="Dismiss"
        title="Dismiss until next check"
      >
        <X size={14} />
      </button>
    {/if}
  </div>
{/if}
