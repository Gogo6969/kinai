<script lang="ts">
  import { api } from '$lib/api';
  import { byPlatform } from '$lib/platform';

  /**
   * Why "no hosts found", per platform.
   *
   * Discovery is mDNS (`_kinai._tcp.local.`, UDP 5353) with the host
   * itself on TCP 4847 — so the honest answer is "same network, and let
   * KinAI through the firewall", which is a different sentence on each
   * OS. This screen used to show the macOS one to everybody, which is
   * how a Windows user ended up being told to open System Settings →
   * Privacy & Security, a pane Windows does not have.
   */
  const discoveryHelp = byPlatform({
    macos:
      'Both computers need to be on the same Wi-Fi — not the guest network. Then open System Settings → Privacy & Security → Local Network and switch KinAI on, on this Mac and on the one hosting.',
    windows:
      'Both computers need to be on the same Wi-Fi — not the guest network. In Settings → Network & internet, set that network to Private, then in Windows Security → Firewall use "Allow an app through firewall" and tick KinAI.',
    linux:
      'Both computers need to be on the same Wi-Fi — not the guest network. If a firewall is running, let KinAI through: sudo ufw allow 5353/udp and sudo ufw allow 4847/tcp.',
  });
  import { app } from '$lib/stores/app.svelte';
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import Logo from '$lib/components/Logo.svelte';
  import { Wifi, Loader2, Check, RefreshCw, ArrowLeft, LogOut } from '@lucide/svelte';

  type Discovered = { family_name: string; instance: string; host_url: string };

  let urlOrCode = $state('');
  let displayName = $state('');
  let busy = $state(false);
  let scanning = $state(false);
  let error = $state('');
  let selected = $state<Discovered | null>(null);

  // Merge mDNS-discovered hosts with the saved-config fallback. If the
  // user has connected before, we keep the last-known host URL so they
  // can redeem a fresh 6-char code against it even when mDNS comes up
  // empty (Local Network permission off, host on a different subnet, etc.).
  const discovered = $derived.by(() => {
    const live = app.discoveredHosts;
    const savedUrl = app.config?.client.host_url ?? null;
    const savedLabel = app.config?.client.host_label ?? null;
    if (savedUrl && !live.some((x) => x.host_url === savedUrl)) {
      const fallback: Discovered = {
        family_name: savedLabel || 'Last known host',
        instance: 'saved://' + savedUrl,
        host_url: savedUrl,
      };
      return [...live, fallback];
    }
    return live;
  });

  const hasSavedHost = $derived(!!app.config?.client.host_url);
  // Where Back goes: home if there's an active client config (so the chat
  // sidebar renders), otherwise the welcome screen route.
  const backTarget = $derived(hasSavedHost ? '/' : '/');

  onMount(() => {
    if (app.config && !displayName) displayName = app.config.client.display_name;
    if (!selected && discovered.length > 0) selected = discovered[0];
    void rescan({ silent: true });
  });

  $effect(() => {
    if (!selected && discovered.length > 0) selected = discovered[0];
  });

  async function rescan({ silent = false }: { silent?: boolean } = {}) {
    if (scanning) return;
    scanning = true;
    if (!silent) error = '';
    try {
      await api.rescanKinaiHosts();
    } catch (e) {
      if (!silent) error = `Couldn't rescan: ${String(e).replace(/^Error:\s*/, '')}`;
    } finally {
      setTimeout(() => (scanning = false), 800);
    }
  }

  async function forgetHost() {
    if (
      !confirm(
        'Disconnect from this host and forget the saved invite? You\'ll need a fresh code or link to connect again.'
      )
    )
      return;
    try {
      await api.disconnectAndForget();
      await app.load();
      // Clear local selection state since the saved-host fallback is gone.
      selected = null;
      urlOrCode = '';
    } catch (e) {
      error = `Couldn't forget host: ${String(e).replace(/^Error:\s*/, '')}`;
    }
  }

  const looksLikeUrl = $derived(urlOrCode.trim().toLowerCase().startsWith('kinai://'));
  const cleanedCode = $derived(urlOrCode.trim().toLowerCase().replace(/\s+/g, ''));
  const looksLikeCode = $derived(/^[a-z0-9]{6}$/i.test(cleanedCode));

  async function connect() {
    error = '';
    busy = true;
    try {
      const input = urlOrCode.trim();
      let payload;
      if (looksLikeUrl) {
        payload = await api.consumeInvite(input);
      } else if (looksLikeCode) {
        if (!selected) {
          throw new Error(
            "Pick a KinAI host above so we know where to redeem the code. " +
            "If no hosts are listed, tap 'Scan again' below — or paste the " +
            "full kinai:// link instead, which skips the search entirely."
          );
        }
        payload = await api.redeemInviteCode(selected.host_url, cleanedCode);
      } else {
        throw new Error(
          'Paste a kinai:// link, or a 6-character invite code (letters + digits).'
        );
      }
      await api.connectClient({
        host_url: payload.host_url,
        token: payload.token,
        display_name: displayName,
        label: payload.label,
      });
      await app.load();
      goto('/');
    } catch (e) {
      error = String(e).replace(/^Error:\s*/, '');
    } finally {
      busy = false;
    }
  }
</script>

<main class="h-screen w-screen bg-ink-950 overflow-y-auto">
  <div class="max-w-2xl mx-auto px-8 py-10">
    <header class="flex items-center justify-between gap-3 mb-8">
      <div class="flex items-center gap-3 min-w-0">
        <Logo size={32} />
        <h1 class="text-2xl font-bold tracking-tight truncate">Join your family's KinAI</h1>
      </div>
      <button class="kin-btn flex-shrink-0" onclick={() => goto(backTarget)} aria-label="Back">
        <ArrowLeft size={14} /> Back
      </button>
    </header>

    {#if hasSavedHost}
      <div class="kin-card mb-5 flex items-center justify-between gap-3">
        <div class="text-sm text-white/70 min-w-0">
          <div class="text-[10px] uppercase tracking-wider text-white/40 mb-0.5">Currently saved</div>
          <div class="truncate">
            <span class="font-medium">{app.config?.client.host_label || 'Family device'}</span>
            <span class="text-white/40 font-mono ml-2 text-xs">{app.config?.client.host_url}</span>
          </div>
        </div>
        <button
          class="kin-btn !text-red-300/80 hover:!text-red-300 flex-shrink-0"
          onclick={forgetHost}
          aria-label="Disconnect and forget host"
        >
          <LogOut size={14} /> Forget host
        </button>
      </div>
    {/if}

    <div class="kin-card space-y-4 mb-5">
      <label class="block">
        <span class="text-sm text-white/70">Your name</span>
        <input class="kin-field mt-1" bind:value={displayName} placeholder="e.g. Alex" />
      </label>
    </div>

    <div class="kin-card mb-5 space-y-3">
      <div class="flex items-center justify-between gap-2">
        <div class="flex items-center gap-2 text-sm text-white/70">
          <Wifi size={14} /> KinAI hosts on this network
        </div>
        <button class="kin-btn !px-2" type="button" onclick={() => rescan()} disabled={scanning} aria-label="Scan again">
          {#if scanning}
            <Loader2 size={14} class="animate-spin" /> Scanning…
          {:else}
            <RefreshCw size={14} /> Scan again
          {/if}
        </button>
      </div>

      {#if discovered.length > 0}
        {#each discovered as d}
          {@const isSelected = selected?.instance === d.instance}
          {@const isSaved = d.instance.startsWith('saved://')}
          <button
            type="button"
            onclick={() => (selected = d)}
            class="w-full text-left flex items-center justify-between border rounded-lg p-3 transition-colors
                   {isSelected
                     ? 'border-teal-400/60 bg-teal-500/10'
                     : 'border-white/10 hover:bg-white/5'}"
          >
            <div class="min-w-0">
              <div class="font-semibold">
                {d.family_name}
                {#if isSaved}
                  <span class="text-[10px] uppercase tracking-wider text-amber-300/80 ml-2">saved</span>
                {/if}
              </div>
              <div class="text-xs font-mono text-white/40 truncate">{d.host_url}</div>
            </div>
            {#if isSelected}
              <span class="kin-badge flex-shrink-0"><Check size={12} /> Selected</span>
            {:else}
              <span class="text-xs text-white/40 flex-shrink-0">Tap to select</span>
            {/if}
          </button>
        {/each}
        <p class="text-xs text-white/40">
          Pick the host that gave you a code, then enter the 6-character invite below.
        </p>
      {:else}
        <div class="text-sm text-white/60 border border-white/10 rounded-lg p-3 space-y-2">
          <p>No KinAI hosts found yet on this network.</p>
          <p class="text-xs text-white/40">
            {discoveryHelp} Then tap
            <span class="font-medium text-white/60">Scan again</span>. You can also paste
            a full <code class="bg-black/30 px-1 rounded">kinai://join…</code> link
            below to skip the search entirely.
          </p>
          <p class="text-xs text-white/40">
            Check the other computer too: KinAI has to be open and running as
            the host there.
          </p>
        </div>
      {/if}
    </div>

    <div class="kin-card space-y-4">
      <h2 class="font-semibold">Enter invite code or link</h2>
      <textarea
        bind:value={urlOrCode}
        rows="3"
        class="kin-field font-mono text-sm"
        placeholder="6-character code  ·  or  kinai://join?host=…&code=…&token=…"
      ></textarea>
      {#if urlOrCode.trim()}
        <div class="text-xs text-white/50">
          {#if looksLikeUrl}
            Detected: full KinAI invite link.
          {:else if looksLikeCode}
            Detected: 6-character code.
            {#if !selected}
              <span class="text-amber-300">Pick a host above first.</span>
            {:else}
              Will redeem against <span class="font-mono">{selected.family_name}</span>.
            {/if}
          {:else}
            Need a kinai:// link, or a 6-character invite code.
          {/if}
        </div>
      {/if}
      {#if error}
        <div class="text-sm text-red-300">{error}</div>
      {/if}
      <button
        class="kin-btn-primary w-full"
        disabled={busy || !urlOrCode.trim() || (looksLikeCode && !selected)}
        onclick={connect}
      >
        {#if busy}<Loader2 size={14} class="animate-spin" /> Connecting…{:else}Connect{/if}
      </button>
    </div>
  </div>
</main>
