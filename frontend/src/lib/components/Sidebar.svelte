<script lang="ts">
  import { app } from '$lib/stores/app.svelte';
  import { api } from '$lib/api';
  import { goto } from '$app/navigation';
  import Logo from './Logo.svelte';
  import { Plus, Settings as Cog, Users, Trash2, Cpu, RefreshCw } from '@lucide/svelte';

  async function newChat() {
    await app.newThread();
  }

  function select(id: string) {
    app.activeThreadId = id;
    void app.loadActive();
  }

  async function remove(id: string, e: Event) {
    e.stopPropagation();
    if (confirm('Delete this conversation?')) await app.deleteThread(id);
  }

  let reconnecting = $state(false);
  async function reconnectNow() {
    if (reconnecting) return;
    reconnecting = true;
    try {
      await api.reconnectClient();
    } catch (e) {
      console.warn('reconnect', e);
    } finally {
      // Hold the spinner ~1.2s — the dial + Hello round-trip on a healthy
      // LAN finishes well before that, so the button visibly returns to
      // its normal label only after the user has had time to see what
      // happened (the dot turning green or an error chip updating).
      setTimeout(() => (reconnecting = false), 1200);
    }
  }
</script>

<aside class="w-64 h-full bg-ink-950/70 border-r border-white/5 flex flex-col">
  <div class="px-4 pt-4 pb-3 flex items-center justify-between">
    <div class="flex items-center gap-2">
      <Logo size={26} />
      <span class="font-semibold tracking-tight">KinAI</span>
    </div>
    <button class="kin-btn-ghost !px-2 !py-1.5" onclick={newChat} aria-label="New chat">
      <Plus size={16} />
    </button>
  </div>

  {#if app.config?.mode === 'host'}
    <div class="px-3 pb-2">
      <div class="text-[10px] uppercase text-white/40 tracking-wider px-2 mb-1">Host</div>
      <div class="text-xs text-white/70 px-2 truncate">
        {app.config.host.family_name}
        {#if app.stats?.peers_connected ?? 0 > 0}
          <span class="kin-badge ml-1">{app.stats?.peers_connected} online</span>
        {/if}
      </div>
    </div>
  {:else if app.config?.mode === 'client' && app.config.client.host_label}
    {@const cs = app.clientStatus}
    {@const dotClass = cs === null
      ? 'bg-amber-400'
      : cs.connected
        ? 'bg-emerald-400'
        : 'bg-red-400'}
    <div class="px-3 pb-2">
      <div class="text-[10px] uppercase text-white/40 tracking-wider px-2 mb-1 flex items-center gap-1.5">
        <span class="inline-block w-2 h-2 rounded-full {dotClass} {cs?.connected ? 'animate-pulse' : ''}"></span>
        {cs === null ? 'Connecting' : cs.connected ? 'Connected to' : 'Disconnected from'}
      </div>
      <div class="text-xs text-white/70 px-2 truncate">
        {app.config.client.host_label}
      </div>
      {#if cs && !cs.connected && cs.error}
        <div class="text-[11px] text-red-300/80 px-2 mt-1 break-words">{cs.error}</div>
      {/if}
      {#if cs && !cs.connected}
        <div class="flex items-center gap-2 px-2 mt-1.5">
          <button
            class="text-[11px] inline-flex items-center gap-1 text-teal-300 hover:text-teal-200 disabled:opacity-50"
            onclick={reconnectNow}
            disabled={reconnecting}
          >
            <RefreshCw size={11} class={reconnecting ? 'animate-spin' : ''} />
            {reconnecting ? 'Reconnecting…' : 'Reconnect now'}
          </button>
          <span class="text-white/30">·</span>
          <button
            class="text-[11px] text-teal-300/80 hover:text-teal-200"
            onclick={() => goto('/client')}
          >
            Change host →
          </button>
        </div>
      {/if}
    </div>
  {/if}

  <nav class="flex-1 overflow-y-auto px-2 py-2 space-y-0.5">
    {#each app.threads as t (t.id)}
      <button
        type="button"
        class="group w-full text-left rounded-md px-3 py-2 text-sm truncate flex items-center justify-between gap-2 transition-colors
               {app.activeThreadId === t.id
                 ? 'bg-white/10 text-white'
                 : 'text-white/70 hover:bg-white/5'}"
        onclick={() => select(t.id)}
      >
        <span class="truncate">{t.title || 'Untitled'}</span>
        <span
          role="button"
          tabindex="0"
          class="opacity-0 group-hover:opacity-100 text-white/60 hover:text-red-300 transition-opacity"
          onclick={(e) => remove(t.id, e)}
          onkeydown={(e) => { if (e.key === 'Enter') remove(t.id, e); }}
          aria-label="Delete conversation"
        >
          <Trash2 size={13} />
        </span>
      </button>
    {/each}
    {#if app.threads.length === 0}
      <div class="text-xs text-white/40 px-3 py-2">No conversations yet.</div>
    {/if}
  </nav>

  <div class="border-t border-white/5 px-2 py-2 space-y-0.5">
    {#if app.config?.mode === 'host'}
      <button class="w-full kin-btn justify-start" onclick={() => goto('/host')}>
        <Cpu size={14} /> Backend & model
      </button>
      <button class="w-full kin-btn justify-start" onclick={() => goto('/host/family')}>
        <Users size={14} /> Manage family
      </button>
    {/if}
    <button class="w-full kin-btn justify-start" onclick={() => goto('/settings')}>
      <Cog size={14} /> Settings
    </button>
  </div>
</aside>
