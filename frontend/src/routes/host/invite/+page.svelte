<script lang="ts">
  import { api, type Invite } from '$lib/api';
  import QRCode from 'qrcode';
  import { onMount, tick } from 'svelte';
  import { goto } from '$app/navigation';
  import { Copy, RefreshCw, Trash2 } from '@lucide/svelte';

  let label = $state('Family device');
  // TTL options. `0` is the "never expires" sentinel — the backend
  // encodes it as a ~100-year JWT so any check in the host's
  // validate_token still passes for the realistic lifetime of the
  // recipient device. Render-time we detect the far-future date
  // and show "Never" in the invite-list summary.
  let ttl = $state<number>(30);
  let invites = $state<Invite[]>([]);
  let qrSvgs = $state<Record<string, string>>({});

  /** Heuristic: an invite issued with the "never" sentinel comes back
   *  with expires_at well past any human lifetime. If the year is
   *  more than 50 years out from now, treat it as never-expiring for
   *  display purposes. */
  function isNeverExpiring(iso: string): boolean {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return false;
    return d.getFullYear() - new Date().getFullYear() > 50;
  }

  onMount(refresh);

  async function refresh() {
    invites = await api.listInvites();
    await renderQrs();
  }

  async function renderQrs() {
    await tick();
    const next: Record<string, string> = {};
    for (const inv of invites) {
      next[inv.id] = await QRCode.toString(inv.qr_payload, {
        type: 'svg',
        color: { dark: '#ffffff', light: '#00000000' },
        margin: 1,
        width: 180,
      });
    }
    qrSvgs = next;
  }

  async function create() {
    if (!label.trim()) return;
    await api.generateInvite({ label, ttl_days: ttl });
    label = 'Family device';
    await refresh();
  }

  async function revoke(id: string) {
    if (!confirm('Revoke this invite? Anyone with the code will lose access.')) return;
    await api.revokeInvite(id);
    await refresh();
  }

  function copy(text: string) {
    navigator.clipboard.writeText(text);
  }
</script>

<main class="h-screen w-screen bg-ink-950 overflow-y-auto">
  <div class="max-w-3xl mx-auto px-8 py-10 space-y-6">
    <header class="flex items-center justify-between">
      <h1 class="text-2xl font-bold tracking-tight">Invite the family</h1>
      <button class="kin-btn" onclick={() => goto('/')}>← Back to chat</button>
    </header>

    <div class="kin-card space-y-4">
      <h2 class="font-semibold">New invite</h2>
      <div class="grid grid-cols-[1fr_180px_auto] gap-3">
        <label class="block">
          <span class="text-sm text-white/70">Label</span>
          <input class="kin-field mt-1" bind:value={label} placeholder="e.g. Mom's iPad" />
        </label>
        <label class="block">
          <span class="text-sm text-white/70">Expires</span>
          <select class="kin-field mt-1" bind:value={ttl}>
            <option value={7}>7 days</option>
            <option value={30}>30 days</option>
            <option value={90}>90 days</option>
            <option value={365}>1 year</option>
            <option value={0}>Never</option>
          </select>
          <p class="text-xs text-white/40 mt-1">
            {#if ttl === 0}
              The invite never expires. You can always revoke it from this page.
            {:else}
              You can revoke it any time before then.
            {/if}
          </p>
        </label>
        <button class="kin-btn-primary self-start mt-6" onclick={create}>Create invite</button>
      </div>
    </div>

    <div class="space-y-4">
      {#each invites as inv}
        <div
          class="kin-card grid grid-cols-[180px_1fr] gap-5
                 {inv.revoked ? 'opacity-40' : ''}"
        >
          <div class="bg-black/40 rounded-lg p-3 grid place-items-center">
            {@html qrSvgs[inv.id] ?? ''}
          </div>
          <div class="space-y-3 text-sm min-w-0">
            <div class="flex items-center justify-between">
              <div>
                <div class="font-semibold">{inv.label || 'Untitled'}</div>
                <div class="text-xs text-white/50">
                  {#if isNeverExpiring(inv.expires_at)}
                    Never expires
                  {:else}
                    Expires {new Date(inv.expires_at).toLocaleDateString()}
                  {/if}
                  {#if inv.revoked}<span class="text-red-400">· revoked</span>{/if}
                </div>
              </div>
              {#if !inv.revoked}
                <button class="kin-btn-ghost text-red-300/80 hover:text-red-300" onclick={() => revoke(inv.id)}>
                  <Trash2 size={14} /> Revoke
                </button>
              {/if}
            </div>
            <div>
              <div class="text-[10px] uppercase text-white/40 tracking-wider mb-1">6-char code</div>
              <div class="flex items-center gap-2">
                <span class="font-mono text-base tracking-widest bg-black/40 px-2 py-1 rounded">{inv.short_code}</span>
                <button class="kin-btn-ghost !px-2" onclick={() => copy(inv.short_code)} aria-label="Copy code">
                  <Copy size={14} />
                </button>
              </div>
            </div>
            <div>
              <div class="text-[10px] uppercase text-white/40 tracking-wider mb-1">Join link</div>
              <div class="flex items-center gap-2">
                <span class="font-mono text-xs text-white/70 truncate min-w-0">{inv.join_url}</span>
                <button class="kin-btn-ghost !px-2" onclick={() => copy(inv.join_url)} aria-label="Copy link">
                  <Copy size={14} />
                </button>
              </div>
            </div>
          </div>
        </div>
      {/each}
      {#if invites.length === 0}
        <div class="kin-card text-sm text-white/50 text-center">No invites yet.</div>
      {/if}
    </div>
  </div>
</main>
