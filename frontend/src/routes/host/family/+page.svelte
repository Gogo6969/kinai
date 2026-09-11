<script lang="ts">
  import { api, events, type PeerSummary } from '$lib/api';
  import { onMount, onDestroy } from 'svelte';
  import { goto } from '$app/navigation';
  import { UserMinus, RefreshCw, CirclePause, CirclePlay } from '@lucide/svelte';

  let peers = $state<PeerSummary[]>([]);
  let busy = $state<string | null>(null);
  let error = $state('');
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
    try {
      peers = await api.listPeers();
      error = '';
    } catch (e) {
      // This page had no error handling at all: a failed call left the
      // list silently stale, which on a page about who can reach your
      // household is the worst possible way to be wrong.
      error = `Could not load the family list: ${e}`;
    }
  }

  /** Run one action, keeping the row disabled until the list is back. */
  async function act(inviteId: string, fn: () => Promise<void>) {
    busy = inviteId;
    try {
      await fn();
      error = '';
    } catch (e) {
      error = `That didn't work: ${e}`;
    } finally {
      busy = null;
      await refresh();
    }
  }

  function pause(p: PeerSummary) {
    act(p.invite_id, () => api.pausePeer(p.invite_id));
  }

  function resume(p: PeerSummary) {
    act(p.invite_id, () => api.resumePeer(p.invite_id));
  }

  function disconnect(p: PeerSummary) {
    // Irreversible: there is no un-revoke anywhere in KinAI, so the
    // confirm has to say what actually happens rather than "are you sure".
    const ok = confirm(
      `Disconnect ${p.display_name}?\n\n` +
        `This ends their session AND permanently revokes their invite code. ` +
        `They cannot come back until you create a new invite and share it with them.\n\n` +
        `To stop them temporarily instead, use Pause.`
    );
    if (!ok) return;
    act(p.invite_id, () => api.disconnectPeer(p.invite_id));
  }

  function when(iso: string): string {
    const d = new Date(iso);
    const today = new Date();
    const sameDay = d.toDateString() === today.toDateString();
    return sameDay
      ? `today at ${d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}`
      : d.toLocaleDateString(undefined, { day: 'numeric', month: 'short', year: 'numeric' });
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

    <p class="text-sm text-white/50">
      Everyone who can reach this household, whether or not their device is on
      right now. Revoked invites are on the
      <button class="underline hover:text-white/70" onclick={() => goto('/host/invite')}>
        Invites
      </button>
      page.
    </p>

    {#if error}
      <div class="kin-card text-sm text-red-300/90">{error}</div>
    {/if}

    {#if peers.length === 0}
      <div class="kin-card text-center text-white/50">
        No one can reach this household yet. Generate an invite and share it.
      </div>
    {:else}
      <div class="space-y-2">
        {#each peers as p (p.invite_id)}
          {@const off = p.state !== 'connected'}
          <div class="kin-card flex flex-wrap items-center justify-between gap-3">
            <div class="min-w-[12rem]">
              <div class="font-semibold flex items-center gap-2 {off ? 'text-white/45' : ''}">
                <span
                  class="inline-block w-2 h-2 rounded-full shrink-0 {p.state === 'connected'
                    ? 'bg-teal-400'
                    : p.state === 'paused'
                      ? 'bg-amber-400'
                      : 'bg-white/25'}"
                  title={p.state}
                ></span>
                {p.display_name}
                {#if p.label && p.label !== p.display_name}
                  <span class="text-xs font-normal text-white/40">({p.label})</span>
                {/if}
              </div>
              <div class="text-xs text-white/50">
                {#if p.state === 'connected'}
                  Connected now
                {:else if p.state === 'paused'}
                  Paused{p.last_seen ? ` · last seen ${when(p.last_seen)}` : ''}
                {:else if p.last_seen}
                  Not connected · last seen {when(p.last_seen)}
                {:else}
                  Not connected · has never used this invite
                {/if}
              </div>
            </div>
            <div class="flex gap-1 ml-auto">
              {#if p.state === 'paused'}
                <button
                  class="kin-btn-ghost text-teal-300/80 hover:text-teal-300"
                  disabled={busy === p.invite_id}
                  onclick={() => resume(p)}
                >
                  <CirclePlay size={14} /> Resume
                </button>
              {:else}
                <button
                  class="kin-btn-ghost text-amber-300/80 hover:text-amber-300"
                  disabled={busy === p.invite_id}
                  onclick={() => pause(p)}
                >
                  <CirclePause size={14} /> Pause
                </button>
              {/if}
              <button
                class="kin-btn-ghost text-red-300/80 hover:text-red-300"
                disabled={busy === p.invite_id}
                onclick={() => disconnect(p)}
              >
                <UserMinus size={14} /> Disconnect
              </button>
            </div>
          </div>
        {/each}
      </div>
    {/if}
  </div>
</main>
