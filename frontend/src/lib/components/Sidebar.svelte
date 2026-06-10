<script lang="ts">
  import { app } from '$lib/stores/app.svelte';
  import { api } from '$lib/api';
  import { goto } from '$app/navigation';
  import Logo from './Logo.svelte';
  import { Plus, Settings as Cog, Users, Trash2, Cpu, RefreshCw, Pencil, Search, X } from '@lucide/svelte';
  import { tick } from 'svelte';

  async function newChat() {
    const previousId = app.activeThreadId;
    try {
      await app.newThread();
      // Belt-and-suspenders: if activeThreadId didn't actually change,
      // the IPC succeeded but the store didn't pick up the new thread.
      // Surface this rather than silently leaving the user in the old
      // thread (the exact failure mode that caused topic-bleed bug).
      if (!app.activeThreadId || app.activeThreadId === previousId) {
        throw new Error(
          'New chat created but the UI did not switch to it. Try clicking + again.'
        );
      }
    } catch (e) {
      const msg = String(e).replace(/^Error:\s*/, '');
      console.warn('newChat failed:', e);
      // Visible toast; reuses the global one from +layout.svelte.
      window.dispatchEvent(
        new CustomEvent('kin-toast', { detail: { msg: `✗ Couldn't start a new chat: ${msg}`, ms: 5000 } })
      );
    }
  }

  function select(id: string) {
    app.activeThreadId = id;
    void app.loadActive();
  }

  async function remove(id: string, e: Event) {
    e.stopPropagation();
    if (confirm('Delete this conversation?')) await app.deleteThread(id);
  }

  // ---- Inline rename ----
  // One thread at a time is editable; clicking the pencil swaps the
  // title span for an <input>. Enter / blur commits, Escape cancels.
  // Empty input is treated as "no change" so a fumbled edit can't
  // result in an unlabeled row.
  let editingId = $state<string | null>(null);
  let editValue = $state('');

  async function startRename(id: string, currentTitle: string, e: Event) {
    e.stopPropagation();
    editingId = id;
    editValue = currentTitle;
    await tick();
    const input = document.getElementById(`thread-rename-${id}`) as HTMLInputElement | null;
    input?.focus();
    input?.select();
  }

  async function commitRename(id: string) {
    const next = editValue.trim();
    const current = app.threads.find((t) => t.id === id)?.title ?? '';
    if (next && next !== current) {
      try {
        await app.renameThread(id, next);
      } catch (e) {
        window.dispatchEvent(
          new CustomEvent('kin-toast', {
            detail: { msg: `✗ Couldn't rename: ${String(e).replace(/^Error:\s*/, '')}`, ms: 4000 },
          })
        );
      }
    }
    editingId = null;
    editValue = '';
  }

  function cancelRename() {
    editingId = null;
    editValue = '';
  }

  function onRenameKey(id: string, e: KeyboardEvent) {
    if (e.key === 'Enter') {
      e.preventDefault();
      void commitRename(id);
    } else if (e.key === 'Escape') {
      e.preventDefault();
      cancelRename();
    }
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
        {app.hostInfo?.family_name ?? app.config.client.host_label}
      </div>
      {#if app.hostInfo?.family_name && app.config.client.host_label && app.hostInfo.family_name !== app.config.client.host_label}
        <div class="text-[10px] text-white/40 px-2 truncate">
          joined as {app.config.client.host_label}
        </div>
      {/if}
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

  {#if app.config?.mode === 'host'}
    <!-- Cross-thread message search. Host-only: the host holds the
         authoritative DB, so search runs locally; clients would need a
         WS round-trip we deliberately deferred to keep their connection
         path unchanged. -->
    <div class="px-3 pb-2">
      <div class="relative">
        <Search size={14} class="absolute left-2.5 top-1/2 -translate-y-1/2 text-white/40 pointer-events-none" />
        <input
          type="text"
          placeholder="Search messages…"
          class="w-full bg-white/5 rounded-md pl-8 pr-7 py-1.5 text-sm text-white placeholder:text-white/40 outline-none focus:ring-1 focus:ring-teal-400/60"
          value={app.searchQuery}
          oninput={(e) => app.runSearch(e.currentTarget.value)}
          onkeydown={(e) => { if (e.key === 'Escape') app.clearSearch(); }}
        />
        {#if app.searchQuery}
          <button
            class="absolute right-2 top-1/2 -translate-y-1/2 text-white/40 hover:text-white"
            onclick={() => app.clearSearch()}
            aria-label="Clear search"
          >
            <X size={13} />
          </button>
        {/if}
      </div>
    </div>
  {/if}

  {#if app.config?.mode === 'host' && app.searchQuery.trim()}
    <nav class="flex-1 overflow-y-auto px-2 py-2 space-y-0.5">
      {#if app.searching}
        <div class="text-xs text-white/40 px-3 py-2">Searching…</div>
      {:else if app.searchResults.length === 0}
        <div class="text-xs text-white/40 px-3 py-2">No matches.</div>
      {:else}
        {#each app.searchResults as hit (hit.message_id)}
          <button
            class="w-full text-left rounded-md px-3 py-2 text-sm text-white/70 hover:bg-white/5"
            onclick={() => app.jumpToHit(hit)}
          >
            <div class="truncate font-medium text-white/90">{hit.thread_title || 'Untitled'}</div>
            <div class="truncate text-xs text-white/50">{hit.snippet}</div>
          </button>
        {/each}
      {/if}
    </nav>
  {:else}
  <nav class="flex-1 overflow-y-auto px-2 py-2 space-y-0.5">
    {#each app.threads as t (t.id)}
      <div
        role="button"
        tabindex="0"
        class="group w-full text-left rounded-md px-3 py-2 text-sm flex items-center justify-between gap-2 transition-colors cursor-pointer
               {app.activeThreadId === t.id
                 ? 'bg-white/10 text-white'
                 : 'text-white/70 hover:bg-white/5'}"
        onclick={() => { if (editingId !== t.id) select(t.id); }}
        onkeydown={(e) => { if (editingId !== t.id && e.key === 'Enter') select(t.id); }}
      >
        {#if editingId === t.id}
          <input
            id="thread-rename-{t.id}"
            type="text"
            class="flex-1 min-w-0 bg-white/10 text-white rounded px-2 py-0.5 text-sm outline-none ring-1 ring-teal-400/60 focus:ring-teal-300"
            bind:value={editValue}
            onclick={(e) => e.stopPropagation()}
            onkeydown={(e) => onRenameKey(t.id, e)}
            onblur={() => commitRename(t.id)}
            maxlength="80"
          />
        {:else}
          <span class="truncate flex-1 min-w-0">{t.title || 'Untitled'}</span>
          <span class="shrink-0 flex items-center gap-1.5">
            <span
              role="button"
              tabindex="0"
              class="opacity-0 group-hover:opacity-100 text-white/60 hover:text-teal-300 transition-opacity"
              onclick={(e) => startRename(t.id, t.title || '', e)}
              onkeydown={(e) => { if (e.key === 'Enter') startRename(t.id, t.title || '', e); }}
              aria-label="Rename conversation"
              title="Rename"
            >
              <Pencil size={13} />
            </span>
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
          </span>
        {/if}
      </div>
    {/each}
    {#if app.threads.length === 0}
      <div class="text-xs text-white/40 px-3 py-2">No conversations yet.</div>
    {/if}
  </nav>
  {/if}

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
