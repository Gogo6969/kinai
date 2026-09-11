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
  /** UI state for the Create button: blocks double-clicks while the
   *  Tauri IPC is in flight, drives the spinner. */
  let busy = $state(false);
  /** Surfaces failure modes that previously silently no-op'd:
   *  empty label, backend IPC errors (DB locked, signing key
   *  unreadable, etc.). */
  let createError = $state<string>('');

  /** Heuristic: an invite issued with the "never" sentinel comes back
   *  with expires_at well past any human lifetime. If the year is
   *  more than 50 years out from now, treat it as never-expiring for
   *  display purposes. */
  function isNeverExpiring(iso: string): boolean {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return false;
    return d.getFullYear() - new Date().getFullYear() > 50;
  }

  // ---- Reminder API key ----
  // A different kind of credential in the same table: it cannot open a
  // chat socket, and its short code is never redeemable, so the token
  // itself is the only way to use it — copied from here by hand.
  let keyLabel = $state('Automation');
  let keyBusy = $state(false);
  let keyError = $state('');
  let newKey = $state<Invite | null>(null);
  let copiedKey = $state(false);

  async function createKey() {
    if (keyBusy) return;
    const label = keyLabel.trim();
    if (!label) {
      keyError = 'Give the key a name so you can tell them apart later.';
      return;
    }
    keyBusy = true;
    keyError = '';
    try {
      // 0 = never expires, matching the invite form's sentinel.
      newKey = await api.createApiKey({ label, ttl_days: 0 });
      await refresh();
    } catch (e) {
      keyError = `Could not create the key: ${e}`;
    } finally {
      keyBusy = false;
    }
  }

  function copyKey() {
    if (!newKey) return;
    navigator.clipboard.writeText(newKey.jwt);
    copiedKey = true;
    setTimeout(() => (copiedKey = false), 1500);
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
    createError = '';
    const trimmed = label.trim();
    if (!trimmed) {
      // Previously this was a silent `return`, which made the button
      // look broken — user clicks, nothing happens, no clue why. Now
      // we surface a visible message so the user can actually fix it.
      createError = 'Please enter a label so you can recognise this invite later.';
      return;
    }
    if (busy) return;
    busy = true;
    try {
      await api.generateInvite({ label: trimmed, ttl_days: ttl });
      label = 'Family device';
      await refresh();
    } catch (e) {
      // Previously these IPC failures (DB locked, signing key
      // unreadable, etc.) were unhandled promise rejections — they
      // silently bubbled into the void and the user saw "nothing
      // happened". Surface the message so the failure is debuggable.
      createError = `Couldn't create invite: ${String(e).replace(/^Error:\s*/, '')}`;
    } finally {
      busy = false;
    }
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
        <button
          class="kin-btn-primary self-start mt-6 disabled:opacity-60"
          onclick={create}
          disabled={busy}
        >
          {busy ? 'Creating…' : 'Create invite'}
        </button>
      </div>
      {#if createError}
        <div class="rounded-lg border border-red-400/30 bg-red-400/10 text-red-200 px-3 py-2 text-sm">
          {createError}
        </div>
      {/if}
    </div>

    <div class="kin-card space-y-4">
      <div>
        <h2 class="font-semibold">Reminder API key</h2>
        <p class="text-sm text-white/50 mt-1">
          For a script, a cron job or a Shortcut that should add reminders to
          <em>your</em> calendar without going through a conversation. It cannot
          open a chat, read anyone's messages, or write to another family
          member's calendar. Revoke it from the list below like any invite.
        </p>
      </div>
      <div class="flex flex-wrap items-end gap-3">
        <label class="text-sm">
          <div class="text-white/60 mb-1">What is it for?</div>
          <input class="kin-field mt-1" bind:value={keyLabel} placeholder="Automation" />
        </label>
        <button
          class="kin-btn-primary disabled:opacity-60"
          onclick={createKey}
          disabled={keyBusy}
        >
          {keyBusy ? 'Creating…' : 'Create API key'}
        </button>
      </div>

      {#if keyError}
        <div class="rounded-lg border border-red-400/30 bg-red-400/10 text-red-200 px-3 py-2 text-sm">
          {keyError}
        </div>
      {/if}

      {#if newKey}
        <div class="space-y-2">
          <div class="text-sm text-white/60">
            Copy this now and keep it somewhere private — it is the key itself,
            not a code someone types.
          </div>
          <div class="flex gap-2">
            <input class="kin-field font-mono text-xs flex-1" readonly value={newKey.jwt} />
            <button class="kin-btn" onclick={copyKey}>
              <Copy size={14} /> {copiedKey ? 'Copied' : 'Copy'}
            </button>
          </div>
          <details class="text-xs text-white/50">
            <summary class="cursor-pointer hover:text-white/70">How to use it</summary>
            <pre class="mt-2 whitespace-pre-wrap break-all bg-black/30 rounded p-2">curl -X POST {newKey.host_url.replace('ws://', 'http://').replace('/kin', '')}/v1/reminders \
  -H "Authorization: Bearer YOUR_KEY" \
  -H "Content-Type: application/json" \
  -d '{'{'}"text":"Bins out","in_minutes":540{'}'}'</pre>
          </details>
        </div>
      {/if}
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
      {:else}
        <p class="text-xs text-white/30 text-center pt-1">
          Revoked or expired codes are cleared automatically about a week later.
        </p>
      {/if}
    </div>
  </div>
</main>
