<script lang="ts">
  import { api, events, type PeerSummary } from '$lib/api';
  import { onMount, onDestroy } from 'svelte';
  import { goto } from '$app/navigation';
  import { UserMinus, RefreshCw } from '@lucide/svelte';

  let peers = $state<PeerSummary[]>([]);
  const cleanups: Array<() => void> = [];

  onMount(() => {
    refresh();
    (async () => {
      cleanups.push(await events.onPeerJoined(() => refresh()));
      cleanups.push(await events.onPeerLeft(() => refresh()));
    })();
  });
  onDestroy(() => cleanups.forEach((u) => u()));

  async function refresh() {
    peers = await api.listPeers();
  }
  async function kick(id: string) {
    if (!confirm('Disconnect this family member?')) return;
    await api.revokePeer(id);
    await refresh();
  }
</script>

<main class="h-screen w-screen bg-ink-950 overflow-y-auto">
  <div class="max-w-3xl mx-auto px-8 py-10 space-y-6">
    <header class="flex items-center justify-between gap-3">
      <div class="flex items-center gap-2">
        <button class="kin-btn" onclick={() => goto('/')} title="Back to chat">←</button>
        <h1 class="text-2xl font-bold tracking-tight">Manage family</h1>
      </div>
      <div class="flex gap-2">
        <button class="kin-btn" onclick={refresh}><RefreshCw size={14} /> Refresh</button>
        <button class="kin-btn-primary" onclick={() => goto('/host/invite')}>+ Invite</button>
      </div>
    </header>

    {#if peers.length === 0}
      <div class="kin-card text-center text-white/50">
        No one is connected right now. Generate an invite and share it.
      </div>
    {:else}
      <div class="space-y-2">
        {#each peers as p}
          <div class="kin-card flex items-center justify-between">
            <div>
              <div class="font-semibold">{p.display_name}</div>
              <div class="text-xs text-white/50">
                Joined {new Date(p.first_seen).toLocaleTimeString()} ·
                Invite {p.invite_id}
              </div>
            </div>
            <button class="kin-btn-ghost text-red-300/80 hover:text-red-300" onclick={() => kick(p.id)}>
              <UserMinus size={14} /> Disconnect
            </button>
          </div>
        {/each}
      </div>
    {/if}
  </div>
</main>
