<script lang="ts">
  import {
    api,
    type DetectedBackend,
    type HostSettings,
    type LlmSettings,
    type TestConnectionResult,
    type ToolSettings,
    type VisionEndpoint,
    type VisionSettings,
  } from '$lib/api';
  import { THIS_COMPUTER } from '$lib/platform';
  import { displayModelName } from '$lib/modelName';
  import { app } from '$lib/stores/app.svelte';
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import Logo from '$lib/components/Logo.svelte';
  import {
    Search,
    Wifi,
    Loader2,
    Check,
    Sparkles,
    Plug,
    AlertCircle,
    HelpCircle,
  } from '@lucide/svelte';

  const SCAN_CACHE_MAX_MS = 60 * 60 * 1000; // 1 hour

  let step = $state<'detect' | 'configure' | 'tools' | 'search' | 'vision' | 'done'>('detect');
  let detecting = $state(false);
  let scanning = $state(false);
  let detected = $state<DetectedBackend[]>([]);
  let scanRanThisSession = $state(false);
  let pickedBackend = $state<DetectedBackend | null>(null);
  let availableModels = $state<string[]>([]);
  let localNet = $state<{ ip: string | null; hostname: string | null }>({ ip: null, hostname: null });
  let showBindHelp = $state(false);
  let lastScanAt = $state<number | null>(null);
  let nowTick = $state(Date.now());

  let host = $state<HostSettings>({
    family_name: 'Our Family',
    bind_addr: '0.0.0.0',
    port: 4847,
    mdns_enabled: true,
    rate_limit_rpm: 60,
  });
  let llm = $state<LlmSettings>({
    provider: 'ollama',
    base_url: 'http://localhost:11434',
    model: '',
    context_window: 8192,
    api_key: null,
    temperature: 0.7,
    max_tokens: 1024,
    system_addendum: '', image_recognition: 'auto',
    enabled: true,
  });
  // Secondary "deep" slot — same shape, but starts disabled + empty so
  // hosts who only want one model keep their existing single-slot
  // behaviour. The configure step now exposes BOTH slots so the user
  // can pick a fast + deep backend in one screen.
  let llmDeep = $state<LlmSettings>({
    provider: 'ollama',
    base_url: '',
    model: '',
    context_window: 8192,
    api_key: null,
    temperature: 0.7,
    max_tokens: 0,
    system_addendum: '', image_recognition: 'auto',
    enabled: false,
  });
  let pickedBackendDeep = $state<DetectedBackend | null>(null);
  let availableModelsDeep = $state<string[]>([]);
  let testingDeep = $state(false);
  let testResultDeep = $state<TestConnectionResult | null>(null);
  // The tool checkbox loop only ever indexes the on/off fields. Indexing by
  // `keyof ToolSettings` would widen the value to `string | boolean | null`,
  // because the settings object also carries the engine name, the API key
  // and the SearXNG URL.
  type BoolToolKey = {
    [K in keyof ToolSettings]: ToolSettings[K] extends boolean ? K : never;
  }[keyof ToolSettings];

  let tools = $state<ToolSettings>({
    web_search: true,
    x_search: true,
    calculator: true,
    datetime: true,
    image_search: true,
    search_engine: 'duckduckgo',
    search_api_key: null,
    searxng_url: 'http://127.0.0.1:8888',
    search_fallback_searxng: true,
  });
  const visionEmpty = (): VisionEndpoint => ({
    label: '',
    base_url: '',
    model: '',
    api_key: null,
  });
  let vision = $state<VisionSettings>({
    enabled: false,
    primary: visionEmpty(),
    failover: visionEmpty(),
  });
  let visionTestState = $state<'idle' | 'testing' | 'ok' | 'fail'>('idle');
  // SearXNG connection check. Result is cleared the moment the URL is
  // edited, so a green tick can never refer to a different address than
  // the one in the field.
  let searxngTesting = $state(false);
  let searxngTest = $state<
    { ok: boolean; message: string; sample: string; engines: string[] } | null
  >(null);
  async function testSearxng() {
    searxngTesting = true;
    searxngTest = null;
    try {
      const r = await api.testSearxng({ url: tools.searxng_url ?? '' });
      searxngTest = { ok: r.ok, message: r.message, sample: r.sample, engines: r.engines };
    } catch (e) {
      searxngTest = { ok: false, message: String(e), sample: '', engines: [] };
    } finally {
      searxngTesting = false;
    }
  }
  let visionTestMsg = $state('');
  async function testVision(slot: 'primary' | 'failover') {
    const ep = vision[slot];
    if (!ep.base_url || !ep.model) {
      visionTestState = 'fail';
      visionTestMsg = 'Set base URL and model first.';
      return;
    }
    visionTestState = 'testing';
    visionTestMsg = '';
    try {
      const res = await api.testVisionEndpoint({
        base_url: ep.base_url,
        model: ep.model,
        api_key: ep.api_key,
      });
      if (res.ok) {
        visionTestState = 'ok';
        visionTestMsg = `${ep.label || ep.model} replied in ${res.latency_ms}ms`;
      } else {
        visionTestState = 'fail';
        visionTestMsg = res.error ?? 'Test failed.';
      }
    } catch (e) {
      visionTestState = 'fail';
      visionTestMsg = String(e).replace(/^Error:\s*/, '');
    }
  }

  let testing = $state(false);
  let testResult = $state<TestConnectionResult | null>(null);

  // True when entering /host/ as an already-configured host — used to
  // re-open the wizard for backend/model changes without restarting the
  // running server.
  let reconfiguring = $state(false);

  // Deliberately NOT async: Svelte discards the cleanup function returned
  // from an async onMount, which silently leaked the 30s tick interval on
  // every visit to this screen. Nothing in here awaits, so the keyword was
  // vestigial anyway.
  onMount(() => {
    if (app.config) {
      host = { ...app.config.host };
      llm = { ...app.config.llm };
      if (app.config.llm_deep) {
        llmDeep = { ...app.config.llm_deep };
      }
      tools = { ...app.config.tools };
      if (app.config.vision) {
        vision = {
          enabled: app.config.vision.enabled,
          primary: { ...app.config.vision.primary },
          failover: { ...app.config.vision.failover },
        };
      }
      reconfiguring = app.config.mode === 'host';
    }
    api.localIp().then((info) => (localNet = info)).catch(() => {});

    // Scan policy: backend discovery now runs ONLY on explicit user
    // action — the Localhost / Network buttons. Re-entering this
    // screen no longer triggers an automatic rescan, which had been
    // firing every visit because the 1-hour TTL was tighter than how
    // often users navigate here in practice. We still hydrate from
    // the in-memory cache so the previously-discovered backends stay
    // visible, but never refresh them implicitly.
    //
    // The single exception is the very first run on a fresh install:
    // if the cache is empty (no prior scan in this app session AND no
    // cached results), kick off ONE initial scan so the user sees
    // something on the screen instead of an empty list with a
    // possibly-confusing "scan" button. Reconfiguring hosts skip
    // even that — they already have a backend configured and don't
    // need to be auto-scanned at.
    if (app.detectCache) {
      mergeResults(app.detectCache.results);
      scanRanThisSession = true;
      lastScanAt = app.detectCache.at;
    }
    if (app.scanCache) {
      mergeResults(app.scanCache.results);
      scanRanThisSession = true;
      lastScanAt = Math.max(lastScanAt ?? 0, app.scanCache.at);
    }
    const neverScanned = !app.detectCache && !app.scanCache;
    if (neverScanned && !reconfiguring) {
      void detect();
      void scanNetwork();
    }

    // Tick the "last scanned X ago" label every 30s.
    const tickId = setInterval(() => (nowTick = Date.now()), 30_000);
    return () => clearInterval(tickId);
  });

  async function detect() {
    detecting = true;
    try {
      const results = await api.detectBackends();
      app.detectCache = { results, at: Date.now() };
      lastScanAt = app.detectCache.at;
      mergeResults(results);
    } finally {
      detecting = false;
    }
  }

  async function scanNetwork() {
    scanning = true;
    scanRanThisSession = true;
    try {
      const results = await api.scanLocalNetwork();
      app.scanCache = { results, at: Date.now() };
      lastScanAt = app.scanCache.at;
      mergeResults(results);
    } finally {
      scanning = false;
    }
  }

  function relativeTime(epochMs: number | null): string {
    if (!epochMs) return '';
    void nowTick; // re-render on tick
    const diff = Math.max(0, Date.now() - epochMs);
    if (diff < 30_000) return 'just now';
    if (diff < 60_000) return `${Math.floor(diff / 1000)}s ago`;
    if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} min ago`;
    return `${Math.floor(diff / 3_600_000)} h ago`;
  }

  function mergeResults(results: DetectedBackend[]) {
    const merged = [...detected];
    for (const b of results) {
      if (!merged.some((x) => x.base_url === b.base_url)) merged.push(b);
    }
    detected = merged;
  }

  function pick(b: DetectedBackend) {
    pickedBackend = b;
    llm.provider = b.provider;
    llm.base_url = b.base_url;
    availableModels = [...b.models];
    if (b.models.length) llm.model = b.models[0];
    step = 'configure';
    testResult = null;
    void autofillContextWindow('fast');
  }

  function pickForDeep(b: DetectedBackend) {
    pickedBackendDeep = b;
    llmDeep.provider = b.provider;
    llmDeep.base_url = b.base_url;
    availableModelsDeep = [...b.models];
    if (b.models.length) llmDeep.model = b.models[0];
    // Setting a backend for the deep slot enables it — paused slots
    // were stripped from routing in v0.2.25; if the user wants to keep
    // a configured slot dormant they can flip the toggle afterwards.
    llmDeep.enabled = true;
    step = 'configure';
    testResultDeep = null;
    void autofillContextWindow('deep');
  }

  async function autofillContextWindow(slot: 'fast' | 'deep' = 'fast') {
    const target = slot === 'deep' ? llmDeep : llm;
    if (!target.model.trim() || !target.base_url.trim()) return;
    try {
      const caps = await api.queryModelCaps({
        provider: target.provider,
        base_url: target.base_url,
        api_key: target.api_key,
        model: target.model,
      });
      if (caps.context_length && caps.context_length > 0) {
        if (slot === 'deep') {
          llmDeep.context_window = caps.context_length;
        } else {
          llm.context_window = caps.context_length;
        }
      }
    } catch (e) {
      // Best-effort — leave context_window at the user-set value.
    }
  }

  function configureManually() {
    pickedBackend = null;
    availableModels = [];
    step = 'configure';
  }

  async function commit() {
    await api.setLlmSettings(llm);
    // Push deep slot too. Empty base_url + enabled=false is a valid
    // "disabled" state — the backend's `is_active()` decides whether
    // the slot is a routing candidate.
    await api.setLlmDeepSettings(llmDeep);
    await api.setMode({ mode: 'host', host, tools });
    await api.setVisionSettings(vision);
    if (reconfiguring) {
      // Server is already running — settings update is enough, no restart.
      // The next chat turn picks up the new LlmClient automatically.
      await app.load();
      goto('/');
      return;
    }
    await api.startHost();
    await app.load();
    step = 'done';
    setTimeout(() => goto('/host/invite'), 600);
  }

  async function testConnection() {
    testing = true;
    testResult = null;
    try {
      testResult = await api.testLlmConnection({
        provider: llm.provider,
        base_url: llm.base_url,
        api_key: llm.api_key,
      });
      if (testResult.ok && testResult.models.length) {
        availableModels = [...testResult.models];
        if (!testResult.models.includes(llm.model)) {
          llm.model = testResult.models[0];
        }
        void autofillContextWindow('fast');
      }
    } catch (e) {
      testResult = {
        ok: false,
        models: [],
        error: String(e),
        latency_ms: 0,
      };
    } finally {
      testing = false;
    }
  }

  /** Same test-connection flow as `testConnection`, just targeting
   *  the deep slot. Kept separate so the two sections show
   *  independent results / loading states. */
  async function testConnectionDeep() {
    testingDeep = true;
    testResultDeep = null;
    try {
      testResultDeep = await api.testLlmConnection({
        provider: llmDeep.provider,
        base_url: llmDeep.base_url,
        api_key: llmDeep.api_key,
      });
      if (testResultDeep.ok && testResultDeep.models.length) {
        availableModelsDeep = [...testResultDeep.models];
        if (!testResultDeep.models.includes(llmDeep.model)) {
          llmDeep.model = testResultDeep.models[0];
        }
        void autofillContextWindow('deep');
      }
    } catch (e) {
      testResultDeep = {
        ok: false,
        models: [],
        error: String(e),
        latency_ms: 0,
      };
    } finally {
      testingDeep = false;
    }
  }

  const busy = $derived(detecting || scanning);
  const scanLabel = $derived(
    detecting && scanning
      ? 'Scanning machine + LAN…'
      : detecting
        ? 'Scanning localhost…'
        : scanning
          ? 'Scanning network…'
          : ''
  );
</script>

<main class="h-screen w-screen bg-ink-950 overflow-y-auto">
  <div class="max-w-2xl mx-auto px-8 py-10">
    <header class="flex items-center justify-between gap-3 mb-8">
      <div class="flex items-center gap-3">
        <Logo size={32} />
        <h1 class="text-2xl font-bold tracking-tight">
          {reconfiguring ? 'Change backend' : 'Host KinAI'}
        </h1>
      </div>
      {#if reconfiguring}
        <button class="kin-btn" onclick={() => goto('/')}>← Back to chat</button>
      {/if}
    </header>

    {#if step === 'detect'}
      <div class="kin-card space-y-4">
        <div class="flex items-start justify-between gap-3">
          <div>
            <h2 class="font-semibold text-lg">Find your LLM</h2>
            <p class="text-sm text-white/60 mt-1">
              Scanning this machine and your local network for Ollama, LM Studio,
              vLLM, llama.cpp, or Open WebUI.
              {#if lastScanAt && !detecting && !scanning}
                <span class="text-white/40">Last scan {relativeTime(lastScanAt)}.</span>
              {/if}
            </p>
          </div>
          <div class="flex gap-2 flex-shrink-0">
            <button class="kin-btn" disabled={busy} onclick={detect} title="Re-scan localhost only">
              {#if detecting}
                <Loader2 size={14} class="animate-spin" />
              {:else}
                <Search size={14} />
              {/if}
              Localhost
            </button>
            <button class="kin-btn" disabled={busy} onclick={scanNetwork} title="Re-scan the local /24 subnet">
              {#if scanning}
                <Loader2 size={14} class="animate-spin" />
              {:else}
                <Wifi size={14} />
              {/if}
              Network
            </button>
          </div>
        </div>

        {#if busy && detected.length === 0}
          <div class="flex items-center gap-2 text-sm text-white/60 italic py-2">
            <Loader2 size={14} class="animate-spin" />
            {scanLabel}
          </div>
        {/if}

        <div class="space-y-2">
          {#each detected as b}
            <div class="kin-card">
              <div class="flex items-center justify-between gap-3">
                <div class="min-w-0">
                  <div class="font-semibold">{b.label}</div>
                  <div class="text-xs text-white/50 font-mono truncate">{b.base_url}</div>
                </div>
                <span class="kin-badge flex-shrink-0">
                  {b.models.length} model{b.models.length === 1 ? '' : 's'}
                </span>
              </div>
              {#if b.models.length > 0}
                <div class="text-xs text-white/40 mt-2 truncate">
                  {b.models.slice(0, 4).join(' · ')}{b.models.length > 4 ? ' …' : ''}
                </div>
              {/if}
              <!--
                A backend can be assigned to EITHER slot. The two
                buttons let the user wire a discovered server to their
                Fast (default) or Deep (`/deep`) slot independently;
                clicking Fast jumps straight to the configure step.
              -->
              <div class="flex flex-wrap gap-2 mt-3">
                <button
                  class="kin-btn-primary !text-xs"
                  type="button"
                  onclick={() => pick(b)}
                >Set as Fast →</button>
                <button
                  class="kin-btn !text-xs"
                  type="button"
                  onclick={() => pickForDeep(b)}
                >Set as Deep</button>
              </div>
            </div>
          {/each}

          {#if !busy && detected.length === 0}
            <div class="text-sm text-white/60 italic">
              {#if scanRanThisSession}
                Nothing found on localhost or your network's /24 subnet.
                Make sure the LLM server is bound to <code class="bg-black/40 px-1 rounded">0.0.0.0</code>
                (not just <code class="bg-black/40 px-1 rounded">127.0.0.1</code>).
              {/if}
            </div>
          {/if}

          {#if busy && detected.length > 0}
            <div class="flex items-center gap-2 text-xs text-white/40 italic py-1 px-1">
              <Loader2 size={12} class="animate-spin" />
              {scanLabel} — more may appear.
            </div>
          {/if}
        </div>

        <div class="border-t border-white/5 pt-4 flex flex-col sm:flex-row sm:items-center sm:justify-between gap-3">
          <div class="text-xs text-white/50 leading-relaxed sm:max-w-[60%]">
            None of these are yours? Configure the URL, model, and provider by hand instead.
          </div>
          <button class="kin-btn flex-shrink-0" type="button" onclick={configureManually}>
            Configure manually →
          </button>
        </div>
      </div>
    {:else if step === 'configure'}
      <div class="space-y-5">
        <div class="kin-card space-y-4">
          <h2 class="font-semibold text-lg">Family</h2>
          <label class="block">
            <span class="text-sm text-white/70">Family name</span>
            <input class="kin-field mt-1" bind:value={host.family_name} />
          </label>
          <div class="grid grid-cols-2 gap-3">
            <label class="block">
              <span class="text-sm text-white/70 flex items-center gap-1.5">
                Bind address
                <button
                  type="button"
                  class="text-white/50 hover:text-white/80 transition-colors"
                  aria-label="What's a bind address?"
                  onclick={() => (showBindHelp = !showBindHelp)}
                >
                  <HelpCircle size={13} />
                </button>
              </span>
              <input class="kin-field mt-1 font-mono" bind:value={host.bind_addr} />
              {#if localNet.ip}
                <p class="text-xs text-white/40 mt-1">
                  Your LAN IP is <code class="bg-black/40 px-1 rounded">{localNet.ip}</code>
                  {#if localNet.hostname}
                    (host <code class="bg-black/40 px-1 rounded">{localNet.hostname}.local</code>)
                  {/if}
                </p>
              {/if}
              {#if showBindHelp}
                <div class="mt-2 text-xs text-white/70 bg-black/40 border border-white/10 rounded-md p-3 space-y-2 leading-relaxed">
                  <p>
                    <strong>Bind address</strong> tells KinAI which network interface to listen on.
                  </p>
                  <ul class="list-disc pl-4 space-y-1 text-white/60">
                    <li>
                      <code class="bg-black/60 px-1 rounded">0.0.0.0</code> — listen on
                      <em>every</em> interface (recommended for a family server, so phones, tablets
                      and other laptops can connect).
                    </li>
                    <li>
                      <code class="bg-black/60 px-1 rounded">127.0.0.1</code> — only this machine.
                      Family members on other devices won't be able to connect.
                    </li>
                    <li>
                      <code class="bg-black/60 px-1 rounded">{localNet.ip ?? 'your-LAN-IP'}</code> —
                      only your wired/Wi-Fi LAN interface, useful if you have multiple networks.
                    </li>
                  </ul>
                </div>
              {/if}
            </label>
            <label class="block">
              <span class="text-sm text-white/70">Port</span>
              <input type="number" class="kin-field mt-1" bind:value={host.port} />
            </label>
          </div>
          <label class="flex items-center gap-2 text-sm text-white/80">
            <input type="checkbox" bind:checked={host.mdns_enabled} />
            Announce on local network (mDNS / Bonjour)
          </label>
        </div>

        <div class="kin-card space-y-4">
          <div class="flex items-center justify-between gap-2">
            <h2 class="font-semibold text-lg">Fast model</h2>
            <div class="flex gap-2">
              <button class="kin-btn" type="button" disabled={busy} onclick={detect}>
                {#if detecting}
                  <Loader2 size={14} class="animate-spin" />
                {:else}
                  <Search size={14} />
                {/if}
                Localhost
              </button>
              <button class="kin-btn" type="button" disabled={busy} onclick={scanNetwork}>
                {#if scanning}
                  <Loader2 size={14} class="animate-spin" />
                {:else}
                  <Wifi size={14} />
                {/if}
                Network
              </button>
            </div>
          </div>

          {#if detected.length > 0}
            <div class="space-y-1.5 border border-white/5 rounded-lg p-2 bg-black/20">
              <div class="text-xs text-white/50 px-1">
                {detected.length} backend{detected.length === 1 ? '' : 's'} found — assign to a slot:
              </div>
              {#each detected as b}
                {@const isFast = pickedBackend?.base_url === b.base_url}
                {@const isDeep = pickedBackendDeep?.base_url === b.base_url}
                <div
                  class="rounded-md px-3 py-2 flex items-center justify-between gap-2
                         {isFast || isDeep ? 'bg-teal-500/10 ring-1 ring-teal-400/40' : 'hover:bg-white/5 transition-colors'}"
                >
                  <div class="min-w-0">
                    <div class="text-sm font-semibold">{b.label}</div>
                    <div class="text-xs text-white/50 font-mono truncate">{b.base_url}</div>
                    {#if isFast || isDeep}
                      <div class="text-[10px] uppercase tracking-wider text-teal-300 mt-0.5">
                        {#if isFast && isDeep}Active for Fast + Deep
                        {:else if isFast}Active for Fast
                        {:else}Active for Deep{/if}
                      </div>
                    {/if}
                  </div>
                  <div class="flex items-center gap-2 flex-shrink-0">
                    <button
                      class="kin-btn !text-xs {isFast ? '!bg-teal-500/20' : ''}"
                      type="button"
                      onclick={() => pick(b)}
                    >Fast</button>
                    <button
                      class="kin-btn !text-xs {isDeep ? '!bg-teal-500/20' : ''}"
                      type="button"
                      onclick={() => pickForDeep(b)}
                    >Deep</button>
                    <span class="kin-badge">{b.models.length}</span>
                  </div>
                </div>
              {/each}
            </div>
          {/if}

          <div class="grid grid-cols-2 gap-3">
            <label class="block">
              <span class="text-sm text-white/70">Provider</span>
              <select class="kin-field mt-1" bind:value={llm.provider}>
                <option value="ollama">Ollama</option>
                <option value="lmstudio">LM Studio</option>
                <option value="vllm">vLLM</option>
                <option value="llamacpp">llama.cpp server</option>
                <option value="openai-compat">OpenAI-compatible</option>
              </select>
            </label>
            <label class="block">
              <span class="text-sm text-white/70">Base URL</span>
              <input
                class="kin-field mt-1 font-mono"
                bind:value={llm.base_url}
                placeholder="http://192.168.1.50:8000"
              />
              <p class="text-xs text-white/40 mt-1">
                vLLM <code>:8000</code> · llama.cpp <code>:8080</code> ·
                Ollama <code>:11434</code> · LM Studio <code>:1234</code>.
              </p>
            </label>
          </div>

          <div class="grid grid-cols-2 gap-3">
            <label class="block">
              <span class="text-sm text-white/70">Model</span>
              {#if availableModels.length > 0}
                <select
                  class="kin-field mt-1 font-mono"
                  bind:value={llm.model}
                  onchange={() => autofillContextWindow()}
                >
                  {#each availableModels as m}
                    <option value={m}>{displayModelName(m)}</option>
                  {/each}
                  {#if llm.model && !availableModels.includes(llm.model)}
                    <option value={llm.model}>{displayModelName(llm.model)} (custom)</option>
                  {/if}
                </select>
                <p class="text-xs text-white/40 mt-1">
                  Discovered from the server. {availableModels.length} option{availableModels.length === 1 ? '' : 's'}.
                </p>
              {:else}
                <input
                  class="kin-field mt-1 font-mono"
                  list="model-hints"
                  bind:value={llm.model}
                  placeholder="llama3.1:8b"
                />
                <datalist id="model-hints">
                  <option value="llama3.1:8b"></option>
                  <option value="qwen3:latest"></option>
                  <option value="gpt-oss-20b"></option>
                  <option value="qwen3-32b"></option>
                </datalist>
                <p class="text-xs text-white/40 mt-1">
                  Type the id, or use Test below to fetch the list.
                </p>
              {/if}
            </label>
            <label class="block">
              <span class="text-sm text-white/70">Context window (tokens)</span>
              <input type="number" class="kin-field mt-1" bind:value={llm.context_window} />
              <p class="text-xs text-white/40 mt-1">
                Auto-filled from the server when supported (vLLM, Ollama).
                Reply budget is computed automatically as
                <code class="bg-black/40 px-1 rounded">context − prompt</code>.
              </p>
            </label>
          </div>

          <label class="block">
            <span class="text-sm text-white/70">System addendum (optional)</span>
            <textarea
              class="kin-field mt-1 font-mono text-sm leading-relaxed"
              rows="3"
              bind:value={llm.system_addendum}
              placeholder="House rules for KinAI — e.g. 'Always answer in French.'"
            ></textarea>
          </label>

          <div class="border-t border-white/5 pt-4 space-y-3">
            <div class="flex items-center justify-between gap-3">
              <div>
                <div class="text-sm font-semibold">Test connection</div>
                <div class="text-xs text-white/50">
                  Verify the URL is reachable and refresh the model list.
                </div>
              </div>
              <button
                class="kin-btn"
                type="button"
                onclick={testConnection}
                disabled={testing || !llm.base_url.trim()}
              >
                {#if testing}
                  <Loader2 size={14} class="animate-spin" /> Testing…
                {:else}
                  <Plug size={14} /> Test
                {/if}
              </button>
            </div>

            {#if testResult}
              {#if testResult.ok}
                <div class="rounded-lg border border-teal-500/30 bg-teal-500/5 p-3 text-sm space-y-2">
                  <div class="flex items-center gap-2 text-teal-300">
                    <Check size={14} />
                    <span class="font-medium">Connected</span>
                    <span class="text-white/40 text-xs">
                      · {testResult.latency_ms} ms · {testResult.models.length} model{testResult.models.length === 1 ? '' : 's'}
                    </span>
                  </div>
                  {#if testResult.models.length === 0}
                    <div class="text-xs text-white/60">
                      Endpoint responded but reported zero models. Type the model id manually above.
                    </div>
                  {/if}
                </div>
              {:else}
                <div class="rounded-lg border border-red-500/30 bg-red-500/5 p-3 text-sm space-y-1">
                  <div class="flex items-center gap-2 text-red-300">
                    <AlertCircle size={14} />
                    <span class="font-medium">Could not reach the server</span>
                  </div>
                  <div class="text-xs text-white/70 font-mono break-all">
                    {testResult.error}
                  </div>
                  <div class="text-xs text-white/50">
                    Check IP and port, confirm the server is bound to <code>0.0.0.0</code>,
                    and that {THIS_COMPUTER} can reach it on your LAN.
                  </div>
                </div>
              {/if}
            {/if}
          </div>
        </div>

        <!--
          Deep model — optional second slot routed via `/deep`. Same
          field layout as Fast above; collapsed by default with a
          summary line when disabled, expands when the user clicks
          "Add" or assigns a detected backend.
        -->
        <div class="kin-card space-y-4">
          <div class="flex items-center justify-between gap-3">
            <div>
              <h2 class="font-semibold text-lg">Deep model (optional)</h2>
              <p class="text-xs text-white/50 mt-0.5">
                A second, typically larger / slower model. Reachable as
                <code class="bg-black/40 px-1 rounded">/deep &lt;prompt&gt;</code>
                in chat. Leave Base URL empty to skip.
              </p>
            </div>
            <label class="flex items-center gap-2 text-sm text-white/70 cursor-pointer select-none flex-shrink-0">
              <input type="checkbox" bind:checked={llmDeep.enabled} class="accent-teal-400" />
              <span>{llmDeep.enabled ? 'Active' : 'Paused'}</span>
            </label>
          </div>

          <div class="grid grid-cols-2 gap-3">
            <label class="block">
              <span class="text-sm text-white/70">Provider</span>
              <select class="kin-field mt-1" bind:value={llmDeep.provider}>
                <option value="ollama">Ollama</option>
                <option value="lmstudio">LM Studio</option>
                <option value="vllm">vLLM</option>
                <option value="llamacpp">llama.cpp server</option>
                <option value="openai-compat">OpenAI-compatible</option>
              </select>
            </label>
            <label class="block">
              <span class="text-sm text-white/70">Base URL</span>
              <input
                class="kin-field mt-1 font-mono"
                bind:value={llmDeep.base_url}
                placeholder="(empty = no deep model)"
              />
            </label>
          </div>

          <div class="grid grid-cols-2 gap-3">
            <label class="block">
              <span class="text-sm text-white/70">Model</span>
              {#if availableModelsDeep.length > 0}
                <select
                  class="kin-field mt-1 font-mono"
                  bind:value={llmDeep.model}
                  onchange={() => autofillContextWindow('deep')}
                >
                  {#each availableModelsDeep as m}
                    <option value={m}>{displayModelName(m)}</option>
                  {/each}
                  {#if llmDeep.model && !availableModelsDeep.includes(llmDeep.model)}
                    <option value={llmDeep.model}>{displayModelName(llmDeep.model)} (custom)</option>
                  {/if}
                </select>
              {:else}
                <input
                  class="kin-field mt-1 font-mono"
                  bind:value={llmDeep.model}
                  placeholder="e.g. qwen2.5:72b-instruct-q4_K_M"
                />
              {/if}
            </label>
            <label class="block">
              <span class="text-sm text-white/70">Context window (tokens)</span>
              <input type="number" class="kin-field mt-1" bind:value={llmDeep.context_window} />
            </label>
          </div>

          <label class="block">
            <span class="text-sm text-white/70">System addendum (optional)</span>
            <textarea
              class="kin-field mt-1 font-mono text-sm leading-relaxed"
              rows="2"
              bind:value={llmDeep.system_addendum}
              placeholder="Per-deep-slot system addition, if any."
            ></textarea>
          </label>

          <div class="border-t border-white/5 pt-4 space-y-3">
            <div class="flex items-center justify-between gap-3">
              <div>
                <div class="text-sm font-semibold">Test connection</div>
                <div class="text-xs text-white/50">
                  Verify the deep slot's URL is reachable.
                </div>
              </div>
              <button
                class="kin-btn"
                type="button"
                onclick={testConnectionDeep}
                disabled={testingDeep || !llmDeep.base_url.trim()}
              >
                {#if testingDeep}
                  <Loader2 size={14} class="animate-spin" /> Testing…
                {:else}
                  <Plug size={14} /> Test
                {/if}
              </button>
            </div>

            {#if testResultDeep}
              {#if testResultDeep.ok}
                <div class="rounded-lg border border-teal-500/30 bg-teal-500/5 p-3 text-sm space-y-2">
                  <div class="flex items-center gap-2 text-teal-300">
                    <Check size={14} />
                    <span class="font-medium">Connected</span>
                    <span class="text-white/40 text-xs">
                      · {testResultDeep.latency_ms} ms · {testResultDeep.models.length} model{testResultDeep.models.length === 1 ? '' : 's'}
                    </span>
                  </div>
                </div>
              {:else}
                <div class="rounded-lg border border-red-500/30 bg-red-500/5 p-3 text-sm space-y-1">
                  <div class="flex items-center gap-2 text-red-300">
                    <AlertCircle size={14} />
                    <span class="font-medium">Could not reach the server</span>
                  </div>
                  <div class="text-xs text-white/70 font-mono break-all">
                    {testResultDeep.error}
                  </div>
                </div>
              {/if}
            {/if}
          </div>
        </div>

        <div class="flex justify-between">
          <button class="kin-btn" onclick={() => (step = 'detect')}>← Back to detection</button>
          <button class="kin-btn-primary" onclick={() => (step = 'tools')}>Continue</button>
        </div>
      </div>
    {:else if step === 'tools'}
      <div class="kin-card space-y-4">
        <h2 class="font-semibold text-lg">Tools the LLM can call</h2>
        <p class="text-sm text-white/60">
          Each tool is callable via OpenAI-style function calling.
        </p>

        {#each [['web_search', 'Web search', 'Uses the engine you pick in Settings (DuckDuckGo free, Exa recommended).'], ['x_search', 'Social / discussion search', 'X tweets via Exa, or Hacker News when Exa isn\'t configured.'], ['calculator', 'Calculator', 'Safe arithmetic only.'], ['datetime', 'Date / time', 'Current local date and time.']] as [key, label, desc]}
          <label class="kin-card !p-3 flex items-center justify-between gap-3 cursor-pointer hover:bg-white/10 transition-colors">
            <div class="min-w-0">
              <div class="font-semibold">{label}</div>
              <div class="text-xs text-white/55">{desc}</div>
            </div>
            <input
              type="checkbox"
              checked={tools[key as BoolToolKey]}
              onchange={(e) => (tools = { ...tools, [key]: (e.target as HTMLInputElement).checked })}
            />
          </label>
        {/each}

        {#if tools.search_engine === 'exa'}
          <label class="flex items-start gap-2.5 mt-3 cursor-pointer">
            <input type="checkbox" class="mt-0.5" bind:checked={tools.search_fallback_searxng} />
            <span class="text-sm text-white/70">
              Use SearXNG if Exa runs out of credit
              <span class="block text-xs text-white/40 mt-0.5">
                Exa is metered. If its credits run out or the key is rejected,
                searches fall back to your own instance
                ({tools.searxng_url || 'not set'}) instead of failing, and the
                reply says which engine answered. Brief outages still retry Exa.
              </span>
            </span>
          </label>
        {/if}
        {#if tools.search_engine === 'searxng' || (tools.search_engine === 'exa' && tools.search_fallback_searxng)}
          <p class="text-xs text-white/40 mt-3 leading-relaxed">
            <span class="text-white/60">KinAI does not install SearXNG.</span>
            It is a separate service you run yourself — commonly via Docker on
            this machine or another box on your network. If you don't run one,
            leave the engine on DuckDuckGo or Exa; nothing here is required.
          </p>
        {/if}

        <div class="flex justify-between mt-4">
          <button class="kin-btn" onclick={() => (step = 'configure')}>Back</button>
          <button class="kin-btn-primary" onclick={() => (step = 'search')}>Continue</button>
        </div>
      </div>
    {:else if step === 'search'}
      <div class="kin-card space-y-4">
        <h2 class="font-semibold text-lg">Search engine</h2>
        <p class="text-sm text-white/60">
          KinAI's <code class="text-white/80">web_search</code> and
          <code class="text-white/80">x_search</code> tools both route through
          the engine you pick here, regardless of which LLM is running.
        </p>

        {#if tools.search_api_key && tools.search_api_key.trim().length > 0}
          <div class="rounded-lg border border-teal-500/30 bg-teal-500/5 p-3 text-sm">
            <div class="flex items-center gap-2 text-teal-300">
              <Check size={14} />
              <span class="font-medium">An API key is already saved on this host.</span>
            </div>
            <div class="text-xs text-white/60 mt-1">
              Engine: <strong>{tools.search_engine}</strong>. You can change it
              below or leave it and continue.
            </div>
          </div>
        {:else if !tools.web_search && !tools.x_search}
          <div class="rounded-lg border border-white/10 bg-white/5 p-3 text-sm text-white/60">
            Both search tools are disabled — this step is optional. You can
            still set an engine here for later.
          </div>
        {/if}

        <div class="grid grid-cols-2 gap-3">
          <label class="block">
            <span class="text-sm text-white/70">Engine</span>
            <select class="kin-field mt-1" bind:value={tools.search_engine}>
              <option value="duckduckgo">DuckDuckGo (free, no key)</option>
              <option value="exa">Exa (recommended)</option>
              <option value="searxng">SearXNG (your own server)</option>
            </select>
            <p class="text-xs text-white/40 mt-1">
              {#if tools.search_engine === 'duckduckgo'}
                Free HTML scrape, Wikipedia fallback. Social search uses
                Hacker News.
              {:else if tools.search_engine === 'searxng'}
                Metasearch from a SearXNG instance <em>you</em> run — no key, no
                cost, and your family's questions never leave your hardware.
                KinAI does not install it; see the note below. Social search
                uses Hacker News.
              {:else}
                Semantic search via
                <a class="text-teal-300" href="https://exa.ai" target="_blank" rel="noopener">exa.ai</a>.
                Requires an API key. Social search filters to X / Bluesky /
                Mastodon / Reddit / HN / Lobsters.
              {/if}
            </p>
          </label>
          {#if tools.search_engine === 'searxng'}
            <label class="block">
              <span class="text-sm text-white/70">SearXNG address</span>
              <div class="flex gap-2 mt-1">
                <input
                  class="kin-field font-mono flex-1"
                  bind:value={tools.searxng_url}
                  placeholder="http://127.0.0.1:8888"
                  autocomplete="off"
                  spellcheck="false"
                  oninput={() => (searxngTest = null)}
                />
                <button
                  type="button"
                  class="kin-btn shrink-0"
                  disabled={searxngTesting}
                  onclick={testSearxng}
                >
                  {searxngTesting ? 'Testing…' : 'Test'}
                </button>
              </div>
              {#if searxngTest}
                <p class="text-xs mt-1 {searxngTest.ok ? 'text-teal-300/90' : 'text-amber-300/90'}">
                  {searxngTest.ok ? '✓' : '⚠'} {searxngTest.message}
                  {#if searxngTest.ok && searxngTest.sample}
                    <span class="block text-white/40 mt-0.5 truncate">
                      Top hit: {searxngTest.sample}{searxngTest.engines.length
                        ? ` · via ${searxngTest.engines.join(', ')}`
                        : ''}
                    </span>
                  {/if}
                </p>
              {:else}
                <p class="text-xs text-white/40 mt-1">
                  Your instance must have the JSON format enabled
                  (<code class="font-mono">search.formats</code> in
                  <code class="font-mono">settings.yml</code>). Test to check.
                </p>
              {/if}
            </label>
          {/if}
          {#if tools.search_engine === 'exa'}
            <label class="block">
              <span class="text-sm text-white/70">Exa API key</span>
              <input
                type="password"
                class="kin-field mt-1 font-mono"
                bind:value={tools.search_api_key}
                placeholder="paste your key"
                autocomplete="off"
                spellcheck="false"
              />
              <p class="text-xs text-white/40 mt-1">
                Get one at
                <a class="text-teal-300" href="https://exa.ai/dashboard" target="_blank" rel="noopener">exa.ai/dashboard</a>.
                Stored only on this host.
              </p>
            </label>
          {/if}
        </div>

        <div class="flex justify-between mt-4">
          <button class="kin-btn" onclick={() => (step = 'tools')}>Back</button>
          <button class="kin-btn-primary" onclick={() => (step = 'vision')}>Continue</button>
        </div>
      </div>
    {:else if step === 'vision'}
      <div class="kin-card space-y-5">
        <div class="flex items-start justify-between gap-3">
          <div class="min-w-0">
            <h2 class="font-semibold text-lg">Vision (for image attachments)</h2>
            <p class="text-sm text-white/60 mt-1">
              Where KinAI sends an image when a family member drops a photo or
              screenshot into the chat. If your chat model already understands
              images (Claude Sonnet/Opus, Gemini Pro/Flash, GPT-4o, llava,
              qwen-vl…) it's used directly. Otherwise images route through the
              endpoint you configure here.
            </p>
            <p class="text-xs text-white/40 mt-1">
              Optional — you can skip this and set it up later in Settings →
              Vision.
            </p>
          </div>
          <label class="flex items-center gap-2 text-sm text-white/80 flex-shrink-0">
            <input type="checkbox" bind:checked={vision.enabled} />
            <span>Enable</span>
          </label>
        </div>

        {#if vision.enabled}
          <div class="space-y-3">
            <div class="text-[10px] uppercase tracking-wider text-white/40">Primary endpoint</div>
            <div class="rounded-lg border border-white/10 bg-white/[0.03] px-3 py-2.5 text-xs text-white/55 space-y-1.5">
              <p>
                <span class="text-white/75 font-medium">Base URL</span> — your vision
                server's OpenAI-compatible address. KinAI appends
                <code class="text-teal-300">/v1/chat/completions</code> itself, so
                don't add it. Local example:
                <code class="text-teal-300">http://192.168.1.50:8080</code>
                (llama.cpp, LM Studio, vLLM, or Ollama on your LAN). A cloud
                provider's OpenAI-compatible base URL works too.
              </p>
              <p>
                <span class="text-white/75 font-medium">Model</span> — the exact model
                id your server reports.
              </p>
              <p>
                <span class="text-white/75 font-medium">API key</span> — leave blank for
                a local server; only a cloud provider needs one.
              </p>
            </div>
            <div class="grid grid-cols-2 gap-3">
              <label class="block">
                <span class="text-sm text-white/70">Label</span>
                <input class="kin-field mt-1" bind:value={vision.primary.label} placeholder="a name for you, e.g. Local vision" />
              </label>
              <label class="block">
                <span class="text-sm text-white/70">Model</span>
                <input class="kin-field mt-1 font-mono" bind:value={vision.primary.model} placeholder="model id your server reports" />
              </label>
            </div>
            <label class="block">
              <span class="text-sm text-white/70">Base URL</span>
              <input class="kin-field mt-1 font-mono" bind:value={vision.primary.base_url}
                placeholder="http://192.168.1.50:8080" />
            </label>
            <label class="block">
              <span class="text-sm text-white/70">API key</span>
              <input
                type="password"
                class="kin-field mt-1 font-mono"
                value={vision.primary.api_key ?? ''}
                oninput={(e) => (vision.primary.api_key = (e.currentTarget as HTMLInputElement).value || null)}
                placeholder="leave blank for a local server"
                autocomplete="off"
                spellcheck="false"
              />
            </label>
            <div class="flex items-center gap-2">
              <button class="kin-btn" type="button" onclick={() => testVision('primary')} disabled={visionTestState === 'testing'}>
                {#if visionTestState === 'testing'}
                  <Loader2 size={14} class="animate-spin" /> Testing…
                {:else}
                  Test vision
                {/if}
              </button>
              {#if visionTestState === 'ok'}
                <span class="text-xs text-teal-300 truncate">✓ {visionTestMsg}</span>
              {:else if visionTestState === 'fail'}
                <span class="text-xs text-red-300 truncate">✗ {visionTestMsg}</span>
              {/if}
            </div>

            <details class="text-xs">
              <summary class="text-white/50 cursor-pointer select-none mt-2">
                Add a failover endpoint (optional)
              </summary>
              <div class="space-y-3 mt-3 pl-2 border-l border-white/10">
                <p class="text-xs text-white/45">
                  Tried only if the primary fails — e.g. a cloud backup for a local
                  primary. Same fields as above; leave empty to skip.
                </p>
                <div class="grid grid-cols-2 gap-3">
                  <label class="block">
                    <span class="text-sm text-white/70">Label</span>
                    <input class="kin-field mt-1" bind:value={vision.failover.label} placeholder="a name for you, e.g. Cloud backup" />
                  </label>
                  <label class="block">
                    <span class="text-sm text-white/70">Model</span>
                    <input class="kin-field mt-1 font-mono" bind:value={vision.failover.model} placeholder="model id" />
                  </label>
                </div>
                <label class="block">
                  <span class="text-sm text-white/70">Base URL</span>
                  <input class="kin-field mt-1 font-mono" bind:value={vision.failover.base_url} placeholder="https://provider.example/openai" />
                </label>
                <label class="block">
                  <span class="text-sm text-white/70">API key</span>
                  <input
                    type="password"
                    class="kin-field mt-1 font-mono"
                    value={vision.failover.api_key ?? ''}
                    oninput={(e) => (vision.failover.api_key = (e.currentTarget as HTMLInputElement).value || null)}
                    placeholder="leave blank to disable failover"
                    autocomplete="off"
                    spellcheck="false"
                  />
                </label>
              </div>
            </details>
          </div>
        {:else}
          <div class="rounded-lg border border-white/10 bg-white/5 p-3 text-sm text-white/60">
            Image attachments will be saved with the message but the model
            won't be able to look at them. PDFs still work (text-extracted by
            the host). Flip the toggle above to enable vision.
          </div>
        {/if}

        <div class="flex justify-between mt-4">
          <button class="kin-btn" onclick={() => (step = 'search')}>Back</button>
          <div class="flex items-center gap-2">
            <button
              class="kin-btn"
              type="button"
              onclick={() => { vision.enabled = false; commit(); }}
            >
              Skip
            </button>
            <button class="kin-btn-primary" onclick={commit}>
              <Sparkles size={14} />
              {reconfiguring ? 'Save changes' : 'Start hosting'}
            </button>
          </div>
        </div>
      </div>
    {:else}
      <div class="kin-card text-center space-y-3">
        <div class="inline-flex items-center justify-center w-12 h-12 rounded-full bg-teal-500/15 text-teal-300">
          <Check size={28} />
        </div>
        <h2 class="font-semibold text-lg">KinAI is running</h2>
        <p class="text-sm text-white/60">Sending you to the invite page…</p>
      </div>
    {/if}
  </div>
</main>
