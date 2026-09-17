<script lang="ts">
  import { api, type Invite } from '$lib/api';
  import QRCode from 'qrcode';
  import { onMount, tick } from 'svelte';
  import { goto } from '$app/navigation';
  import { Check, Copy, Pencil, RefreshCw, Trash2, X } from '@lucide/svelte';

  /** Mirrors `invite::MAX_LABEL_CHARS` on the host, so the three fields
   *  that write this column agree with each other and with the gate
   *  that would otherwise refuse them after a round trip. */
  const MAX_LABEL = 60;

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

  /** The expiry as a number, or null when the stored value is not a
   *  date at all. Parsed in one place so the readers below cannot
   *  disagree about a row they cannot read. */
  function expiryOf(inv: Invite): number | null {
    const t = Date.parse(inv.expires_at);
    return Number.isNaN(t) ? null : t;
  }

  /** Heuristic: an invite issued with the "never" sentinel comes back
   *  with expires_at well past any human lifetime. If the year is
   *  more than 50 years out from now, treat it as never-expiring for
   *  display purposes. */
  function isNeverExpiring(iso: string): boolean {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return false;
    return d.getFullYear() - new Date().getFullYear() > 50;
  }

  function isExpired(inv: Invite): boolean {
    if (isNeverExpiring(inv.expires_at)) return false;
    const t = expiryOf(inv);
    // A date the page cannot read is NOT quietly filed as expired — it
    // is shown, with the expiry line saying it is unknown, because a
    // working credential hidden by a parse failure is the worse half of
    // the trade.
    return t !== null && t < Date.now();
  }

  /** A reminder API key, not somebody's device. Same table so one Revoke
   *  button covers both — which is exactly why it stays in the list;
   *  the badge is what tells them apart. */
  function isKey(inv: Invite): boolean {
    return inv.scope === 'automation';
  }

  /** Can this still let something in? Revoked and expired cannot. A key
   *  can, which is the reason to keep it in front of the host. */
  function isActive(inv: Invite): boolean {
    return !inv.revoked && !isExpired(inv);
  }

  // ---- The list, and what it is showing ----
  // Defaults to the family's live devices: the page keeps revoked rows
  // for about a week as a record, and after a few months of ordinary use
  // they are most of the list. The count of what is hidden sits under
  // the list with one click back to everything, so nothing is lost —
  // only out of the way.
  let activeOnly = $state(true);
  const shown = $derived(activeOnly ? invites.filter(isActive) : invites);
  const hidden = $derived(invites.length - shown.length);
  const hiddenWhat = $derived.by(() => {
    if (!activeOnly) return '';
    const gone = invites.filter((i) => !isActive(i));
    const revoked = gone.filter((i) => i.revoked).length;
    const expired = gone.length - revoked;
    const bits: string[] = [];
    if (revoked) bits.push(`${revoked} revoked`);
    if (expired) bits.push(`${expired} expired`);
    return bits.join(', ');
  });

  // ---- Renaming a device ----
  // The host's own note about whose device this is. Only the name moves:
  // the code, the link and the QR are the credential and stay exactly as
  // they were, so one already stuck on the fridge keeps working.
  let editingId = $state<string | null>(null);
  let editLabel = $state('');
  let renaming = $state(false);
  let renameError = $state('');

  /** Focus and select the name the moment the field appears. The
   *  `autofocus` attribute does not fire for an element Svelte inserts
   *  after load, so the first keystroke went to the page instead. */
  function takeFocus(node: HTMLInputElement) {
    node.focus();
    node.select();
  }

  function startRename(inv: Invite) {
    editingId = inv.id;
    // Counted in code points, like the host's own check — a name from
    // before there was a limit would otherwise load too long to save
    // and refuse every edit short of retyping it.
    editLabel = [...inv.label].slice(0, MAX_LABEL).join('');
    renameError = '';
  }

  function cancelRename() {
    editingId = null;
    editLabel = '';
    renameError = '';
  }

  async function commitRename(inv: Invite) {
    if (renaming) return;
    const next = editLabel.trim();
    // Unchanged is a quiet no-op; emptied is a mistake worth saying out
    // loud, or the field just closes and the old name snaps back with
    // no explanation.
    if (next === inv.label) {
      cancelRename();
      return;
    }
    if (!next) {
      renameError = 'Give the device a name so you can tell it apart later.';
      return;
    }
    renaming = true;
    renameError = '';
    try {
      await api.renameInvite(inv.id, next);
    } catch (e) {
      renameError = `Couldn't rename: ${String(e).replace(/^Error:\s*/, '')}`;
      return;
    } finally {
      renaming = false;
    }
    // Only once the rename itself has landed. Re-reading the list can
    // fail on its own, and reporting that as a failed rename would tell
    // the host their change was lost while the host has it.
    cancelRename();
    await refresh();
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
    // An open editor on the row being taken away would keep a Save
    // button the host can only be refused by.
    if (editingId === id) cancelRename();
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
          <input class="kin-field mt-1" bind:value={label} maxlength={MAX_LABEL} placeholder="e.g. Mom's iPad" />
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
          <input class="kin-field mt-1" bind:value={keyLabel} maxlength={MAX_LABEL} placeholder="Automation" />
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
      {#if invites.length > 0}
        <div class="flex items-center justify-between gap-3 px-1">
          <h2 class="font-semibold">
            {activeOnly ? 'Active devices' : 'All invites'}
            <span class="text-white/40 font-normal">· {shown.length}</span>
          </h2>
          <button class="kin-btn" onclick={() => { activeOnly = !activeOnly; cancelRename(); }}>
            {activeOnly ? 'Show all' : 'Active only'}
          </button>
        </div>
      {/if}

      {#each shown as inv (inv.id)}
        <div
          class="kin-card grid grid-cols-[180px_1fr] gap-5
                 {inv.revoked || isExpired(inv) ? 'opacity-40' : ''}"
        >
          <div class="bg-black/40 rounded-lg p-3 grid place-items-center">
            {@html qrSvgs[inv.id] ?? ''}
          </div>
          <div class="space-y-3 text-sm min-w-0">
            <div class="flex items-center justify-between gap-3">
              <div class="min-w-0">
                {#if editingId === inv.id && !inv.revoked}
                  <div class="flex items-center gap-2">
                    <input
                      class="kin-field !py-1 min-w-0"
                      bind:value={editLabel}
                      maxlength={MAX_LABEL}
                      use:takeFocus
                      disabled={renaming}
                      aria-label="Device name"
                      onkeydown={(e) => {
                        if (e.key === 'Enter') void commitRename(inv);
                        if (e.key === 'Escape') cancelRename();
                      }}
                    />
                    <button
                      type="button"
                      class="kin-btn-primary !px-2"
                      disabled={renaming}
                      onclick={() => void commitRename(inv)}
                      aria-label="Save name"
                      title="Save"
                    >
                      <Check size={14} />
                    </button>
                    <button
                      type="button"
                      class="kin-btn !px-2"
                      disabled={renaming}
                      onclick={cancelRename}
                      aria-label="Cancel rename"
                      title="Cancel"
                    >
                      <X size={14} />
                    </button>
                  </div>
                  {#if renameError}
                    <p class="text-xs text-red-300 mt-1" role="alert">{renameError}</p>
                  {/if}
                {:else}
                  <div class="flex items-center gap-2 group min-w-0">
                    <span class="font-semibold truncate">{inv.label || 'Untitled'}</span>
                    {#if !inv.revoked}
                      <button
                        type="button"
                        class="kin-btn-ghost !px-1.5 opacity-0 group-hover:opacity-100 focus:opacity-100 transition-opacity"
                        onclick={() => startRename(inv)}
                        aria-label="Rename this device"
                        title="Rename"
                      >
                        <Pencil size={13} />
                      </button>
                    {/if}
                  </div>
                {/if}
                <div class="text-xs text-white/50">
                  {#if isNeverExpiring(inv.expires_at)}
                    Never expires
                  {:else if expiryOf(inv) === null}
                    Expiry unknown
                  {:else}
                    Expires {new Date(inv.expires_at).toLocaleDateString()}
                  {/if}
                  {#if inv.revoked}<span class="text-red-400">· revoked</span>
                  {:else if isExpired(inv)}<span class="text-amber-300/80">· expired</span>{/if}
                  {#if isKey(inv)}<span class="text-white/40">· API key</span>{/if}
                </div>
              </div>
              {#if !inv.revoked}
                <button class="kin-btn-ghost text-red-300/80 hover:text-red-300 shrink-0" onclick={() => revoke(inv.id)}>
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
      {:else if shown.length === 0}
        <div class="kin-card text-sm text-white/50 text-center">
          No active devices. <button class="underline hover:text-white/80" onclick={() => { activeOnly = false; cancelRename(); }}>Show all {invites.length}</button>
        </div>
      {/if}

      {#if hidden > 0 && shown.length > 0}
        <p class="text-xs text-white/40 text-center pt-1">
          {hidden} hidden{hiddenWhat ? ` (${hiddenWhat})` : ''} ·
          <button class="underline hover:text-white/70" onclick={() => { activeOnly = false; cancelRename(); }}>Show all</button>
        </p>
      {/if}

      {#if invites.length > 0}
        <p class="text-xs text-white/30 text-center pt-1">
          Revoked or expired codes are cleared automatically about a week later.
        </p>
      {/if}
    </div>
  </div>
</main>
