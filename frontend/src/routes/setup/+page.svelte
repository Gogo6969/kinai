<script lang="ts">
  // First-run host setup wizard — the implementation of the approved
  // step-by-step mockup. Design contract:
  //   * Splash first; once setup completes (or is skipped) the
  //     `setup_completed` config flag guarantees neither splash nor wizard
  //     ever appears again (root +page.svelte gates on it).
  //   * EVERY step except the role fork and Finish is skippable; each step
  //     spells out the consequence of skipping and where to configure it
  //     later in Settings.
  //   * Steps persist their settings the moment the user hits Continue, so
  //     quitting mid-wizard loses nothing.
  //   * The Finish screen adapts: if no AI model was connected it shows a
  //     prominent "KinAI can't answer yet" warning with the exact fix path.
  import { api, events, type DetectedBackend, type Invite } from '$lib/api';
  import { app } from '$lib/stores/app.svelte';
  import Logo from '$lib/components/Logo.svelte';
  import QRCode from 'qrcode';
  import { goto } from '$app/navigation';
  import { onDestroy } from 'svelte';
  import {
    enable as enableAutostart,
    disable as disableAutostart,
  } from '@tauri-apps/plugin-autostart';
  import { Loader2, Check } from '@lucide/svelte';

  // ---------- step machinery ----------
  type StepId =
    | 'splash' | 'role' | 'family' | 'ai' | 'deep' | 'vision' | 'tools'
    | 'imagegen' | 'telegram' | 'voice' | 'overlay' | 'invite' | 'finish';

  const STEPS: { id: StepId; label: string; skippable: boolean }[] = [
    { id: 'splash', label: 'Welcome', skippable: false },
    { id: 'role', label: 'How will you use it', skippable: false },
    { id: 'family', label: 'Family & you', skippable: true },
    { id: 'ai', label: 'Connect your AI', skippable: true },
    { id: 'deep', label: 'Deep model', skippable: true },
    { id: 'vision', label: 'Vision (images)', skippable: true },
    { id: 'tools', label: 'Web search & tools', skippable: true },
    { id: 'imagegen', label: 'Image generation', skippable: true },
    { id: 'telegram', label: 'Telegram bot', skippable: true },
    { id: 'voice', label: 'Voice', skippable: true },
    { id: 'overlay', label: 'Quick Chat & looks', skippable: true },
    { id: 'invite', label: 'Invite your family', skippable: true },
    { id: 'finish', label: 'Finish', skippable: false },
  ];

  /** Where each skipped step lives in Settings — shown on the Finish screen. */
  const WHERE: Record<string, string> = {
    family: 'Settings → Host',
    ai: 'Settings → Backend & model',
    deep: 'Settings → Backend & model (deep slot)',
    vision: 'Settings → Vision',
    tools: 'Settings → Tools',
    imagegen: 'Settings → Image generation',
    telegram: 'Settings → Telegram',
    voice: 'Settings → Voice',
    overlay: 'Settings → Overlay / Appearance',
    invite: 'Sidebar → Manage family → + Invite',
  };

  let cur = $state(0);
  let stepState = $state<Record<string, 'done' | 'skipped'>>({});
  let saveError = $state('');
  let saving = $state(false);
  const step = $derived(STEPS[cur]);
  const visibleSteps = STEPS.slice(1); // splash isn't in the rail
  const stepNo = $derived(cur); // 1-based within visibleSteps since splash is 0
  let contentEl = $state<HTMLElement | null>(null);

  function advance() {
    if (cur < STEPS.length - 1) cur++;
    saveError = '';
    contentEl?.scrollTo(0, 0);
  }
  function back() {
    if (cur > 0) cur--;
    saveError = '';
    contentEl?.scrollTo(0, 0);
  }
  function skip() {
    stepState[step.id] = 'skipped';
    advance();
  }
  async function continueStep() {
    saveError = '';
    saving = true;
    try {
      const save = SAVERS[step.id];
      if (save) await save();
      stepState[step.id] = 'done';
      advance();
    } catch (e) {
      saveError = String(e).replace(/^Error:\s*/, '');
    } finally {
      saving = false;
    }
  }
  function jump(id: StepId) {
    const i = STEPS.findIndex((s) => s.id === id);
    if (i >= 0) {
      cur = i;
      saveError = '';
    }
  }

  // ---------- per-step state (seeded from config so resume shows saved values) ----------
  const cfg = () => app.config!;

  // family
  let familyName = $state('Our Family');
  let displayName = $state('');
  let port = $state(4847);
  let rateLimit = $state(60);
  let mdns = $state(true);

  // ai (fast slot)
  let backends = $state<DetectedBackend[]>([]);
  let scanning = $state(false);
  let scanNote = $state('');
  let provider = $state('ollama');
  let baseUrl = $state('http://localhost:11434');
  let model = $state('');
  let modelOptions = $state<string[]>([]);
  let contextWindow = $state(8192);
  let llmTest = $state<{ state: 'idle' | 'busy' | 'ok' | 'fail'; msg: string }>({ state: 'idle', msg: '' });

  // deep
  let deepBase = $state('');
  let deepModel = $state('');
  let deepTest = $state<{ state: 'idle' | 'busy' | 'ok' | 'fail'; msg: string }>({ state: 'idle', msg: '' });

  // vision
  let visionEnabled = $state(false);
  let vBase = $state('');
  let vModel = $state('');
  let vKey = $state('');
  let vFoBase = $state('');
  let vFoModel = $state('');
  let vFoKey = $state('');
  let visionTest = $state<{ state: 'idle' | 'busy' | 'ok' | 'fail'; msg: string }>({ state: 'idle', msg: '' });

  // tools
  let tWeb = $state(true);
  let tImage = $state(true);
  let tX = $state(true);
  let tCalc = $state(true);
  let engine = $state<'duckduckgo' | 'exa'>('duckduckgo');
  let exaKey = $state('');

  // image gen
  let comfyUrl = $state('');
  let comfyTest = $state<{ state: 'idle' | 'busy' | 'ok' | 'fail'; msg: string }>({ state: 'idle', msg: '' });

  // telegram
  let botToken = $state('');
  let tgTest = $state<{ state: 'idle' | 'busy' | 'ok' | 'fail'; msg: string }>({ state: 'idle', msg: '' });

  // voice
  let ttsOn = $state(true);
  let voiceEn = $state('Zoe (Premium)');
  let voiceDe = $state('Anna (Premium)');
  let ttsVoices = $state<{ name: string; lang: string }[]>([]);
  let sttWanted = $state(false);
  let sttDownloading = $state(false);
  let sttProgress = $state(0);
  let sttDownloaded = $state(false);
  let voicePreviewing = $state(false);
  let sttUnlisten: (() => void) | null = null;

  // overlay & appearance
  let hotkey = $state('CmdOrCtrl+Space');
  let recordingHotkey = $state(false);
  let alwaysOnTop = $state(true);
  let closeOnBlur = $state(true);
  let theme = $state<'dark' | 'light'>('dark');
  let autostartOn = $state(true);
  let autostartMin = $state(true);

  // invite
  let inviteLabel = $state('');
  let inviteTtl = $state(30);
  let invite = $state<Invite | null>(null);
  let inviteQr = $state('');
  let inviteBusy = $state(false);

  // ---------- seed from existing config once loaded ----------
  let seeded = false;
  $effect(() => {
    if (!app.config || seeded) return;
    seeded = true;
    const c = app.config;
    familyName = c.host.family_name || 'Our Family';
    displayName = c.client.display_name || '';
    port = c.host.port || 4847;
    rateLimit = c.host.rate_limit_rpm || 60;
    mdns = c.host.mdns_enabled;
    if (c.llm.base_url) baseUrl = c.llm.base_url;
    if (c.llm.model) model = c.llm.model;
    provider = c.llm.provider || 'ollama';
    contextWindow = c.llm.context_window || 8192;
    deepBase = c.llm_deep.base_url;
    deepModel = c.llm_deep.model;
    visionEnabled = c.vision.enabled;
    vBase = c.vision.primary.base_url;
    vModel = c.vision.primary.model;
    vFoBase = c.vision.failover.base_url;
    vFoModel = c.vision.failover.model;
    tWeb = c.tools.web_search;
    tImage = c.tools.image_search;
    tX = c.tools.x_search;
    tCalc = c.tools.calculator;
    engine = c.tools.search_engine as 'duckduckgo' | 'exa';
    comfyUrl = c.comfyui.base_url;
    ttsOn = c.tts.enabled;
    voiceEn = c.tts.voice_en;
    voiceDe = c.tts.voice_de;
    hotkey = c.overlay.hotkey;
    alwaysOnTop = c.overlay.always_on_top;
    closeOnBlur = c.overlay.auto_close_on_blur;
    theme = c.theme as 'dark' | 'light';
    autostartMin = c.startup?.autostart_minimized ?? true;
    void detect();
    void api.sttStatus().then((s) => {
      sttDownloaded = s.models?.some((m: { downloaded: boolean }) => m.downloaded) ?? false;
      sttWanted = s.enabled;
    }).catch(() => {});
    if (app.ttsSupported) {
      void api.listTtsVoices().then((v) => (ttsVoices = v)).catch(() => {});
    }
  });
  onDestroy(() => {
    try {
      sttUnlisten?.();
    } catch { /* best-effort */ }
  });

  // ---------- step actions ----------
  async function detect(deep = false) {
    scanning = true;
    scanNote = '';
    try {
      const found = deep ? await api.scanLocalNetwork() : await api.detectBackends();
      backends = found;
      scanNote = found.length
        ? `Found ${found.length} server${found.length > 1 ? 's' : ''}.`
        : 'Nothing found — is your AI server running? You can still enter its address below.';
    } catch (e) {
      scanNote = String(e);
    } finally {
      scanning = false;
    }
  }

  async function useBackend(b: DetectedBackend) {
    provider = b.provider;
    baseUrl = b.base_url;
    modelOptions = b.models;
    if (b.models.length && !b.models.includes(model)) model = b.models[0];
    try {
      const caps = await api.queryModelCaps({ provider, base_url: baseUrl, model, api_key: null });
      if (caps.context_length) contextWindow = caps.context_length;
    } catch { /* caps are best-effort */ }
  }

  async function testLlm(which: 'fast' | 'deep') {
    const t = which === 'fast' ? { base_url: baseUrl, model } : { base_url: deepBase, model: deepModel };
    const slot = which === 'fast' ? llmTest : deepTest;
    if (!t.base_url || !t.model) {
      slot.state = 'fail';
      slot.msg = 'Enter the server address and a model first.';
      return;
    }
    slot.state = 'busy';
    slot.msg = 'Sending a test message…';
    try {
      const r = await api.testLlmEndpoint({ base_url: t.base_url, model: t.model, api_key: null });
      if (r.ok) {
        slot.state = 'ok';
        slot.msg = `✓ “${r.reply.slice(0, 90)}” — replied in ${(r.latency_ms / 1000).toFixed(1)}s`;
      } else {
        slot.state = 'fail';
        slot.msg = `✗ ${r.error ?? 'No reply.'}`;
      }
    } catch (e) {
      slot.state = 'fail';
      slot.msg = `✗ ${String(e)}`;
    }
  }

  async function testVision() {
    if (!vBase || !vModel) {
      visionTest = { state: 'fail', msg: 'Enter the server address and model first.' };
      return;
    }
    visionTest = { state: 'busy', msg: 'Testing with a tiny image…' };
    try {
      const r = await api.testVisionEndpoint({ base_url: vBase, model: vModel, api_key: vKey || null });
      visionTest = r.ok
        ? { state: 'ok', msg: `✓ replied “${r.reply.slice(0, 60)}” in ${r.latency_ms}ms` }
        : { state: 'fail', msg: `✗ ${r.error ?? 'Test failed.'}` };
    } catch (e) {
      visionTest = { state: 'fail', msg: `✗ ${String(e)}` };
    }
  }

  async function testComfy() {
    if (!comfyUrl.trim()) {
      comfyTest = { state: 'fail', msg: 'Enter the ComfyUI address first.' };
      return;
    }
    comfyTest = { state: 'busy', msg: 'Checking…' };
    try {
      const r = await api.testComfyEndpoint({ base_url: comfyUrl.trim() });
      comfyTest = r.ok
        ? { state: 'ok', msg: '✓ ComfyUI reachable.' }
        : { state: 'fail', msg: `✗ ${r.error ?? 'Not reachable.'}` };
    } catch (e) {
      comfyTest = { state: 'fail', msg: `✗ ${String(e)}` };
    }
  }

  async function testTelegram() {
    if (!botToken.trim()) {
      tgTest = { state: 'fail', msg: 'Paste the token from @BotFather first.' };
      return;
    }
    tgTest = { state: 'busy', msg: 'Verifying with Telegram…' };
    try {
      const r = await api.testTelegramToken({ bot_token: botToken.trim() });
      tgTest = r.ok
        ? { state: 'ok', msg: `✓ Connected as @${r.bot_username ?? 'your bot'}` }
        : { state: 'fail', msg: `✗ ${r.error ?? 'Token rejected.'}` };
    } catch (e) {
      tgTest = { state: 'fail', msg: `✗ ${String(e)}` };
    }
  }

  async function downloadStt() {
    sttDownloading = true;
    sttProgress = 0;
    try {
      sttUnlisten = await events.onSttDownloadProgress((p) => (sttProgress = p.progress));
      await api.downloadSttModel('small-q5_1');
      await api.setSttConfig({ ...cfg().stt, model: 'small-q5_1', enabled: true });
      sttDownloaded = true;
      sttWanted = true;
    } catch (e) {
      saveError = String(e);
    } finally {
      try {
        sttUnlisten?.();
      } catch { /* unlisten is best-effort */ }
      sttUnlisten = null;
      sttDownloading = false;
    }
  }

  async function previewVoice(lang: 'en' | 'de') {
    voicePreviewing = true;
    try {
      await api.previewTtsVoice({ voice: lang === 'de' ? voiceDe : voiceEn, lang });
    } catch { /* best-effort */ }
    setTimeout(() => (voicePreviewing = false), 3500);
  }

  async function createInvite() {
    inviteBusy = true;
    try {
      invite = await api.generateInvite({ label: inviteLabel.trim() || 'Family device', ttl_days: inviteTtl });
      inviteQr = await QRCode.toString(invite.qr_payload, {
        type: 'svg',
        color: { dark: '#ffffff', light: '#00000000' },
        margin: 1,
        width: 168,
      });
    } catch (e) {
      saveError = String(e).replace(/^Error:\s*/, '');
    } finally {
      inviteBusy = false;
    }
  }

  function recordHotkey(e: KeyboardEvent) {
    if (!recordingHotkey) return;
    e.preventDefault();
    const parts: string[] = [];
    if (e.ctrlKey || e.metaKey) parts.push('CmdOrCtrl');
    if (e.altKey) parts.push('Alt');
    if (e.shiftKey) parts.push('Shift');
    if (e.key && !['Control', 'Alt', 'Meta', 'Shift'].includes(e.key)) {
      parts.push(e.key.length === 1 ? e.key.toUpperCase() : e.key);
      if (parts.length >= 2) {
        hotkey = parts.join('+');
        recordingHotkey = false;
      }
    }
  }

  // ---------- persistence per step (runs on Continue) ----------
  const SAVERS: Partial<Record<StepId, () => Promise<void>>> = {
    family: async () => {
      // Keep mode unchanged (still unconfigured) — the host only starts at Finish.
      await api.setMode({
        mode: cfg().mode,
        host: {
          ...cfg().host,
          family_name: familyName.trim() || 'Our Family',
          port: Number(port) || 4847,
          rate_limit_rpm: Number(rateLimit) || 60,
          mdns_enabled: mdns,
        },
        client: { ...cfg().client, display_name: displayName.trim() || cfg().client.display_name },
      });
      await app.load();
    },
    ai: async () => {
      if (!baseUrl.trim() || !model.trim()) {
        throw new Error('Enter the server address and pick a model — or use “Set up later — skip”.');
      }
      await api.setLlmSettings({
        ...cfg().llm,
        provider,
        base_url: baseUrl.trim(),
        model: model.trim(),
        context_window: Number(contextWindow) || 8192,
        enabled: true,
      });
      await app.load();
    },
    deep: async () => {
      if (!deepBase.trim() || !deepModel.trim()) return; // nothing entered = nothing saved
      await api.setLlmDeepSettings({
        ...cfg().llm_deep,
        provider: 'llamacpp',
        base_url: deepBase.trim(),
        model: deepModel.trim(),
        context_window: 32768,
        enabled: true,
      });
      await app.load();
    },
    vision: async () => {
      const on = visionEnabled && !!vBase.trim() && !!vModel.trim();
      await api.setVisionSettings({
        enabled: on,
        primary: { label: 'Primary', base_url: vBase.trim(), model: vModel.trim(), api_key: vKey.trim() || null },
        failover: { label: 'Backup', base_url: vFoBase.trim(), model: vFoModel.trim(), api_key: vFoKey.trim() || null },
      });
      await app.load();
    },
    tools: async () => {
      await api.setMode({
        mode: cfg().mode,
        tools: {
          ...cfg().tools,
          web_search: tWeb,
          image_search: tImage,
          x_search: tX,
          calculator: tCalc,
          datetime: tCalc,
          search_engine: engine,
          search_api_key: engine === 'exa' ? exaKey.trim() || null : cfg().tools.search_api_key,
        },
      });
      await app.load();
    },
    imagegen: async () => {
      if (!comfyUrl.trim()) return;
      await api.setComfyConfig({ base_url: comfyUrl.trim(), default_model: 'zimage' });
      await app.load();
    },
    telegram: async () => {
      if (!botToken.trim()) return;
      await api.setTelegramToken({ bot_token: botToken.trim() });
      await app.load();
    },
    voice: async () => {
      if (app.ttsSupported) {
        await api.setTtsConfig({ ...cfg().tts, enabled: ttsOn, voice_en: voiceEn, voice_de: voiceDe });
      }
      if (sttDownloaded) {
        await api.setSttConfig({ ...cfg().stt, enabled: sttWanted });
      }
      await app.load();
    },
    overlay: async () => {
      await api.setOverlaySettings({
        ...cfg().overlay,
        hotkey,
        always_on_top: alwaysOnTop,
        auto_close_on_blur: closeOnBlur,
      });
      await api.setTheme(theme);
      try {
        if (autostartOn) await enableAutostart();
        else await disableAutostart();
        await api.setStartupSettings({ autostart_minimized: autostartMin });
      } catch { /* autostart plugin can fail in dev — never block setup on it */ }
      await app.load();
    },
    invite: async () => { /* invite is created via its own button; Continue just moves on */ },
  };

  /** Finish: become a host, stamp setup complete, open chat. */
  async function finishSetup() {
    saving = true;
    saveError = '';
    try {
      await api.setMode({
        mode: 'host',
        host: {
          ...cfg().host,
          family_name: familyName.trim() || 'Our Family',
          port: Number(port) || 4847,
          rate_limit_rpm: Number(rateLimit) || 60,
          mdns_enabled: mdns,
        },
      });
      await api.startHost();
      await api.markSetupCompleted();
      await app.load();
      goto('/');
    } catch (e) {
      saveError = String(e).replace(/^Error:\s*/, '');
    } finally {
      saving = false;
    }
  }

  const aiConfigured = $derived(stepState['ai'] === 'done');
  const testStates: Record<string, { state: string; msg: string }> = {};
</script>

<svelte:window onkeydown={recordHotkey} />

{#if !app.config}
  <div class="h-screen w-screen grid place-items-center text-white/50 bg-ink-950">Loading…</div>
{:else if step.id === 'splash'}
  <!-- ======================= SPLASH ======================= -->
  <main class="h-screen w-screen bg-ink-950 flex flex-col items-center justify-center text-center p-8">
    <div class="mb-7"><Logo size={96} /></div>
    <h1 class="text-4xl font-bold tracking-tight mb-3">Your family’s AI. Nobody else’s.</h1>
    <p class="text-white/60 text-lg max-w-xl mb-7">
      KinAI lives on your own computer and works for the whole house — no cloud,
      no accounts, no one listening in.
    </p>
    <div class="max-w-lg w-full rounded-2xl border border-teal-400/35 bg-teal-400/10 px-6 py-4 text-left flex gap-4 items-start mb-9">
      <div class="text-2xl leading-none">🔒</div>
      <div>
        <div class="font-bold text-[15px] mb-0.5">What Mom asks stays Mom’s.</div>
        <div class="text-sm text-white/65">
          Every family member’s conversations are sealed to them — invisible to
          everyone else, <b class="text-white/90">including whoever runs this computer</b>. Ask anything.
        </div>
      </div>
    </div>
    <button class="kin-btn-primary !text-base !px-9 !py-3" onclick={advance}>Get started</button>
    <p class="text-xs text-white/35 mt-9 max-w-md">
      This welcome appears only while setup is unfinished — once you complete
      (or skip) setup, KinAI opens straight into chat.
    </p>
  </main>
{:else}
  <!-- ======================= WIZARD FRAME ======================= -->
  <main class="h-screen w-screen bg-ink-950 flex">
    <nav class="w-60 flex-shrink-0 border-r border-white/10 p-4 flex flex-col gap-0.5 overflow-y-auto">
      <div class="text-[10px] uppercase tracking-wider text-white/35 px-2.5 pb-2.5">KinAI Setup</div>
      {#each visibleSteps as s}
        {@const i = STEPS.indexOf(s)}
        <button
          class="flex items-center gap-2.5 px-2.5 py-1.5 rounded-lg text-[13px] text-left transition-colors
                 {i === cur ? 'bg-teal-400/15 text-white border border-teal-400/25' : 'text-white/50 border border-transparent hover:text-white/75'}"
          onclick={() => jump(s.id)}
        >
          <span class="w-[18px] h-[18px] rounded-full grid place-items-center text-[11px] font-bold flex-shrink-0
                 {stepState[s.id] === 'done' ? 'bg-emerald-400 text-ink-950' : stepState[s.id] === 'skipped' ? 'border border-white/30 text-white/40' : i === cur ? 'bg-teal-400 text-ink-950' : 'border border-white/25'}">
            {stepState[s.id] === 'done' ? '✓' : stepState[s.id] === 'skipped' ? '–' : ''}
          </span>
          <span class={stepState[s.id] === 'skipped' ? 'line-through decoration-white/25' : ''}>{s.label}</span>
        </button>
      {/each}
      <div class="mt-auto pt-3 px-2.5 text-[11px] text-white/35 border-t border-white/10">
        Every step is optional. Skipped items show exactly where to set them up later.
      </div>
    </nav>

    <div class="flex-1 flex flex-col min-w-0">
      <div class="flex-1 overflow-y-auto px-12 py-10" bind:this={contentEl}>
        <div class="max-w-3xl mx-auto space-y-4">
          <div class="text-[11px] uppercase tracking-wider text-teal-300 font-semibold">
            Step {stepNo} of {visibleSteps.length}
          </div>

          {#if step.id === 'role'}
            <h1 class="text-2xl font-bold tracking-tight">How will this computer be used?</h1>
            <p class="text-white/60 max-w-2xl">
              KinAI works as a family: <b class="text-white/85">one</b> computer is the Host — it runs the
              AI and stores every conversation. Everyone else joins that host from their own devices.
            </p>
            <div class="grid grid-cols-2 gap-4 pt-2">
              <button class="kin-card text-left hover:bg-white/10 transition-colors !border-teal-400/40" onclick={advance}>
                <h3 class="font-semibold mb-1">🏠 Set up for my family <span class="text-teal-300 text-xs">(Host — this wizard)</span></h3>
                <p class="text-sm text-white/55">
                  Choose this on the computer that will run the AI — usually your most powerful
                  machine, one that stays at home and switched on. This wizard walks you through
                  connecting an AI model, then inviting everyone else.
                </p>
              </button>
              <button class="kin-card text-left hover:bg-white/10 transition-colors" onclick={() => goto('/client')}>
                <h3 class="font-semibold mb-1">📨 I got an invite code <span class="text-white/40 text-xs">(Join a family)</span></h3>
                <p class="text-sm text-white/55">
                  Someone already set up a KinAI host and sent you a code or QR. This skips
                  everything here — paste the code and start chatting.
                </p>
              </button>
            </div>
            <div class="rounded-xl border border-teal-400/25 bg-teal-400/10 px-4 py-3 text-sm max-w-2xl mt-2">
              <b>What “host” actually means:</b> the host keeps the family’s chats in a private
              database on this machine and talks to an AI model you choose next. If this computer
              sleeps or shuts down, family members can’t chat until it’s back — a desktop or Mac mini
              that stays plugged in works best.
            </div>

          {:else if step.id === 'family'}
            <h1 class="text-2xl font-bold tracking-tight">Name your family — and yourself</h1>
            <p class="text-white/60 max-w-2xl">These names appear everywhere: on family members’ screens when they connect, in invites, and next to your messages.</p>
            <div class="kin-card space-y-4">
              <label class="block">
                <span class="text-sm text-white/70">Family name — what everyone sees when they join this host</span>
                <input class="kin-field mt-1" bind:value={familyName} placeholder="e.g. The Millers" />
              </label>
              <label class="block">
                <span class="text-sm text-white/70">Your display name — shown next to your messages</span>
                <input class="kin-field mt-1" bind:value={displayName} placeholder="e.g. Alex" />
                <p class="text-xs text-white/40 mt-1">Each family member picks their own name on their own device — the AI keeps everyone’s chats and memory separate.</p>
              </label>
            </div>
            <div class="kin-card space-y-3">
              <h3 class="font-semibold">Network details <span class="text-white/35 text-xs font-normal">— the defaults are right for almost everyone</span></h3>
              <div class="grid grid-cols-2 gap-4">
                <label class="block">
                  <span class="text-sm text-white/70">Port</span>
                  <input class="kin-field mt-1 font-mono" type="number" bind:value={port} />
                  <p class="text-xs text-white/40 mt-1">The “door number” family devices connect to. Change only if another app uses it.</p>
                </label>
                <label class="block">
                  <span class="text-sm text-white/70">Requests per minute, per person</span>
                  <input class="kin-field mt-1 font-mono" type="number" bind:value={rateLimit} />
                  <p class="text-xs text-white/40 mt-1">A safety brake so one device can’t overload the AI for everyone.</p>
                </label>
              </div>
              <label class="flex items-start gap-3 cursor-pointer pt-1">
                <input type="checkbox" bind:checked={mdns} class="accent-teal-400 mt-1" />
                <span class="text-sm">
                  <span class="text-white/85">Let family devices find this host automatically</span><br />
                  <span class="text-xs text-white/45">Announces “KinAI — {familyName || 'Our Family'}” on your home network so joining devices pick it from a list instead of typing an address. Recommended.</span>
                </span>
              </label>
            </div>
            <p class="text-xs text-white/40"><b class="text-white/60">If you skip:</b> KinAI uses “Our Family”, your computer account’s name, and standard network settings — changeable later in {WHERE.family}.</p>

          {:else if step.id === 'ai'}
            <h1 class="text-2xl font-bold tracking-tight">Connect your AI</h1>
            <p class="text-white/60 max-w-2xl">
              The one step that matters most. KinAI is the family layer — the intelligence comes from
              an AI model server you run yourself, for free, on your own hardware.
              <b class="text-white/85">Without one connected, KinAI cannot answer anything.</b>
            </p>
            <div class="kin-card space-y-3">
              <div class="flex items-center justify-between">
                <h3 class="font-semibold">Found on your network</h3>
                <div class="flex gap-2">
                  <button class="kin-btn !text-xs" onclick={() => detect(false)} disabled={scanning}>Scan this computer</button>
                  <button class="kin-btn !text-xs" onclick={() => detect(true)} disabled={scanning}>Scan home network</button>
                </div>
              </div>
              {#if scanning}
                <p class="text-sm text-white/50 flex items-center gap-2"><Loader2 size={14} class="animate-spin" /> Scanning…</p>
              {:else if backends.length === 0}
                <p class="text-sm text-white/45">{scanNote || 'No AI servers found yet.'}</p>
              {:else}
                {#each backends as b}
                  <div class="flex items-center justify-between gap-3 py-1.5 border-b border-white/5 last:border-0">
                    <div class="min-w-0">
                      <div class="text-sm">🟢 {b.label} <span class="font-mono text-xs text-white/40">{b.base_url}</span></div>
                      <div class="text-xs text-white/45 truncate">{b.models.length ? b.models.join(', ') : 'models list unavailable'}</div>
                    </div>
                    <button class="kin-btn !text-xs flex-shrink-0 {baseUrl === b.base_url ? '!border-teal-400/50 !text-teal-300' : ''}" onclick={() => useBackend(b)}>
                      {baseUrl === b.base_url ? '✓ Selected' : 'Use this'}
                    </button>
                  </div>
                {/each}
              {/if}
            </div>
            <div class="kin-card space-y-4">
              <h3 class="font-semibold">Or point KinAI at a server yourself</h3>
              <p class="text-sm text-white/55">Any OpenAI-compatible server works: Ollama, LM Studio, llama.cpp (<code class="text-teal-300">llama-server</code>), vLLM — on this machine or any box on your network.</p>
              <div class="grid grid-cols-2 gap-4">
                <label class="block">
                  <span class="text-sm text-white/70">Server address</span>
                  <input class="kin-field mt-1 font-mono" bind:value={baseUrl} placeholder="http://localhost:11434" />
                </label>
                <label class="block">
                  <span class="text-sm text-white/70">Model</span>
                  {#if modelOptions.length}
                    <select class="kin-field mt-1 font-mono" bind:value={model}>
                      {#each modelOptions as m}<option value={m}>{m}</option>{/each}
                    </select>
                  {:else}
                    <input class="kin-field mt-1 font-mono" bind:value={model} placeholder="e.g. llama3.1:8b" />
                  {/if}
                </label>
              </div>
              <label class="block max-w-[220px]">
                <span class="text-sm text-white/70">Context window (tokens)</span>
                <input class="kin-field mt-1 font-mono" type="number" bind:value={contextWindow} />
                <p class="text-xs text-white/40 mt-1">How much conversation the model “holds in its head”. Auto-filled when detected.</p>
              </label>
              <div class="flex items-center gap-3">
                <button class="kin-btn !text-sm" onclick={() => testLlm('fast')} disabled={llmTest.state === 'busy'}>
                  {#if llmTest.state === 'busy'}<Loader2 size={14} class="animate-spin" /> Testing…{:else}Send a test message{/if}
                </button>
                {#if llmTest.state === 'ok'}<span class="text-xs text-emerald-300">{llmTest.msg}</span>{/if}
                {#if llmTest.state === 'fail'}<span class="text-xs text-red-300">{llmTest.msg}</span>{/if}
              </div>
            </div>
            <div class="rounded-xl border border-teal-400/25 bg-teal-400/10 px-4 py-3 text-sm max-w-2xl">
              <b>Don’t have an AI server yet?</b> Easiest start: install
              <a class="text-teal-300 underline underline-offset-2" href="https://ollama.com">Ollama</a>
              (free, 2 minutes), run <code class="text-teal-300">ollama pull llama3.1:8b</code> in
              Terminal, then hit “Scan this computer”. A Mac with 16&nbsp;GB of memory runs an 8B model comfortably.
            </div>
            <p class="text-xs text-white/40"><b class="text-white/60">If you skip:</b> KinAI installs fine but <b class="text-amber-300">cannot answer a single question</b> until you connect a model in {WHERE.ai}. The final screen will remind you.</p>

          {:else if step.id === 'deep'}
            <h1 class="text-2xl font-bold tracking-tight">A second, deeper model <span class="text-white/40 text-base font-normal">(optional)</span></h1>
            <p class="text-white/60 max-w-2xl">
              KinAI can run two model “slots”: <b class="text-white/85">fast</b> — the everyday one you just set up —
              and <b class="text-white/85">deep</b> — a bigger, slower model for hard questions. Anyone in the family
              switches by typing <code class="text-teal-300">/deep</code> or <code class="text-teal-300">/fast</code> in chat.
            </p>
            <div class="kin-card space-y-2 text-sm text-white/65">
              <p><b class="text-white/85">Fast</b> answers greetings, homework help, recipes — instantly.</p>
              <p><b class="text-white/85">Deep</b> (a reasoning model like Qwen or DeepSeek-R1) thinks step-by-step through math, planning, and tricky research — taking a minute or more, but getting further.</p>
              <p>The deep model can live on a <b class="text-white/85">different computer</b> — for example a gaming PC with a big graphics card.</p>
            </div>
            <div class="kin-card space-y-4">
              <div class="grid grid-cols-2 gap-4">
                <label class="block">
                  <span class="text-sm text-white/70">Server address</span>
                  <input class="kin-field mt-1 font-mono" bind:value={deepBase} placeholder="http://192.168.1.50:8081" />
                </label>
                <label class="block">
                  <span class="text-sm text-white/70">Model</span>
                  <input class="kin-field mt-1 font-mono" bind:value={deepModel} placeholder="e.g. qwen3:32b" />
                </label>
              </div>
              <div class="flex items-center gap-3">
                <button class="kin-btn !text-sm" onclick={() => testLlm('deep')} disabled={deepTest.state === 'busy'}>
                  {#if deepTest.state === 'busy'}<Loader2 size={14} class="animate-spin" /> Testing…{:else}Send a test message{/if}
                </button>
                {#if deepTest.state === 'ok'}<span class="text-xs text-emerald-300">{deepTest.msg}</span>{/if}
                {#if deepTest.state === 'fail'}<span class="text-xs text-red-300">{deepTest.msg}</span>{/if}
              </div>
            </div>
            <p class="text-xs text-white/40"><b class="text-white/60">If you skip:</b> everything works with just the fast model — <code>/deep</code> simply isn’t offered. Add it any time in {WHERE.deep}.</p>

          {:else if step.id === 'vision'}
            <h1 class="text-2xl font-bold tracking-tight">Let KinAI see photos <span class="text-white/40 text-base font-normal">(optional)</span></h1>
            <p class="text-white/60 max-w-2xl">
              With vision set up, anyone can drop a photo into chat — or send one to the Telegram bot —
              and ask “what is this?”, “translate this menu”, “is this plant sick?”. Without it, images
              are saved but the AI can’t look at them.
            </p>
            <div class="kin-card space-y-4">
              <label class="flex items-center gap-3 cursor-pointer">
                <input type="checkbox" bind:checked={visionEnabled} class="accent-teal-400" />
                <span class="text-sm text-white/85">Enable vision</span>
              </label>
              <div class="text-sm text-white/60 space-y-1.5">
                <p><b class="text-white/85">On your own hardware (recommended):</b> a vision model (Qwen-VL, llava) served by llama.cpp, LM Studio, Ollama or vLLM. Photos never leave your network.</p>
                <p><b class="text-white/85">A cloud provider:</b> paste an API key (e.g. Gemini) — easier, but <b>images are sent to that provider</b>.</p>
              </div>
              <div class="grid grid-cols-2 gap-4">
                <label class="block">
                  <span class="text-sm text-white/70">Server address</span>
                  <input class="kin-field mt-1 font-mono" bind:value={vBase} placeholder="http://192.168.1.50:8083" />
                  <p class="text-xs text-white/40 mt-1">KinAI adds <code>/v1/chat/completions</code> itself.</p>
                </label>
                <label class="block">
                  <span class="text-sm text-white/70">Model id</span>
                  <input class="kin-field mt-1 font-mono" bind:value={vModel} placeholder="what /v1/models reports" />
                </label>
              </div>
              <label class="block">
                <span class="text-sm text-white/70">API key — cloud providers only</span>
                <input class="kin-field mt-1 font-mono" type="password" bind:value={vKey} placeholder="leave blank for a local server" autocomplete="off" />
              </label>
              <div class="flex items-center gap-3">
                <button class="kin-btn !text-sm" onclick={testVision} disabled={visionTest.state === 'busy'}>
                  {#if visionTest.state === 'busy'}<Loader2 size={14} class="animate-spin" /> Testing…{:else}Test vision{/if}
                </button>
                {#if visionTest.state === 'ok'}<span class="text-xs text-emerald-300">{visionTest.msg}</span>{/if}
                {#if visionTest.state === 'fail'}<span class="text-xs text-red-300">{visionTest.msg}</span>{/if}
              </div>
            </div>
            <div class="kin-card space-y-3">
              <h3 class="font-semibold">Backup endpoint <span class="text-white/35 text-xs font-normal">(optional — tried only when the primary fails)</span></h3>
              <div class="grid grid-cols-2 gap-4">
                <input class="kin-field font-mono" bind:value={vFoBase} placeholder="server address" />
                <input class="kin-field font-mono" bind:value={vFoModel} placeholder="model id" />
              </div>
              <input class="kin-field font-mono" type="password" bind:value={vFoKey} placeholder="API key (if cloud)" autocomplete="off" />
            </div>
            <p class="text-xs text-white/40"><b class="text-white/60">If you skip:</b> photos still attach and store fine — the AI just says it can’t view them. Enable later in {WHERE.vision}.</p>

          {:else if step.id === 'tools'}
            <h1 class="text-2xl font-bold tracking-tight">Web search &amp; tools</h1>
            <p class="text-white/60 max-w-2xl">
              A local AI’s knowledge stops at its training date. Tools let it <b class="text-white/85">look
              things up while answering</b> — today’s weather, sports results, news — and do exact math
              instead of guessing.
            </p>
            <div class="kin-card divide-y divide-white/5">
              <label class="flex items-start justify-between gap-4 py-3 cursor-pointer">
                <span class="text-sm"><span class="text-white/85">Web search</span><br /><span class="text-xs text-white/45">Searches the internet when a question needs current facts, and cites sources. Only the search query leaves your network — never your conversation.</span></span>
                <input type="checkbox" bind:checked={tWeb} class="accent-teal-400 mt-1" />
              </label>
              <label class="flex items-start justify-between gap-4 py-3 cursor-pointer">
                <span class="text-sm"><span class="text-white/85">Image search</span><br /><span class="text-xs text-white/45">“Show me a picture of the Eiffel Tower” returns real photos — in chat and on Telegram.</span></span>
                <input type="checkbox" bind:checked={tImage} class="accent-teal-400 mt-1" />
              </label>
              <label class="flex items-start justify-between gap-4 py-3 cursor-pointer">
                <span class="text-sm"><span class="text-white/85">X / Twitter search</span><br /><span class="text-xs text-white/45">Recent posts for “what’s happening” questions. Needs the Exa engine below.</span></span>
                <input type="checkbox" bind:checked={tX} class="accent-teal-400 mt-1" />
              </label>
              <label class="flex items-start justify-between gap-4 py-3 cursor-pointer">
                <span class="text-sm"><span class="text-white/85">Calculator &amp; date/time</span><br /><span class="text-xs text-white/45">Exact arithmetic and a real clock — small models are famously bad at both.</span></span>
                <input type="checkbox" bind:checked={tCalc} class="accent-teal-400 mt-1" />
              </label>
            </div>
            <div class="kin-card space-y-3">
              <h3 class="font-semibold">Search engine</h3>
              <div class="flex gap-2 flex-wrap">
                <button class="kin-btn !text-xs {engine === 'duckduckgo' ? '!border-teal-400/50 !text-teal-300' : ''}" onclick={() => (engine = 'duckduckgo')}>DuckDuckGo — free, no key, works now</button>
                <button class="kin-btn !text-xs {engine === 'exa' ? '!border-teal-400/50 !text-teal-300' : ''}" onclick={() => (engine = 'exa')}>Exa — better results, needs an API key (paid)</button>
              </div>
              {#if engine === 'exa'}
                <label class="block">
                  <span class="text-sm text-white/70">Exa API key <span class="text-white/40">(dashboard.exa.ai)</span></span>
                  <input class="kin-field mt-1 font-mono" type="password" bind:value={exaKey} placeholder="exa_…" autocomplete="off" />
                </label>
              {/if}
            </div>
            <p class="text-xs text-white/40"><b class="text-white/60">If you skip:</b> tools default to on with the free DuckDuckGo engine — skipping is safe. Fine-tune later in {WHERE.tools}.</p>

          {:else if step.id === 'imagegen'}
            <h1 class="text-2xl font-bold tracking-tight">Image generation <span class="text-white/40 text-base font-normal">(optional)</span></h1>
            <p class="text-white/60 max-w-2xl">
              Type <code class="text-teal-300">/pic a watercolor fox</code> and get a picture back —
              generated entirely on your own hardware by a
              <a class="text-teal-300 underline underline-offset-2" href="https://www.comfy.org">ComfyUI</a> server.
              Great fun for kids; zero cloud involved.
            </p>
            <div class="kin-card space-y-3">
              <label class="block">
                <span class="text-sm text-white/70">ComfyUI server address</span>
                <input class="kin-field mt-1 font-mono" bind:value={comfyUrl} placeholder="http://192.168.1.50:8188" />
                <p class="text-xs text-white/40 mt-1">ComfyUI is a separate free app, usually on a PC with a decent graphics card. No ComfyUI? Skip — this is the most “power user” feature here.</p>
              </label>
              <div class="flex items-center gap-3">
                <button class="kin-btn !text-sm" onclick={testComfy} disabled={comfyTest.state === 'busy'}>
                  {#if comfyTest.state === 'busy'}<Loader2 size={14} class="animate-spin" /> Checking…{:else}Test connection{/if}
                </button>
                {#if comfyTest.state === 'ok'}<span class="text-xs text-emerald-300">{comfyTest.msg}</span>{/if}
                {#if comfyTest.state === 'fail'}<span class="text-xs text-red-300">{comfyTest.msg}</span>{/if}
              </div>
            </div>
            <p class="text-xs text-white/40"><b class="text-white/60">If you skip:</b> <code>/pic</code> politely explains it isn’t set up. Add it later in {WHERE.imagegen}.</p>

          {:else if step.id === 'telegram'}
            <h1 class="text-2xl font-bold tracking-tight">Your family’s phone access: Telegram <span class="text-white/40 text-base font-normal">(optional)</span></h1>
            <p class="text-white/60 max-w-2xl">
              This is how KinAI reaches everyone’s <b class="text-white/85">phone</b>. One bot for the whole
              family: each person pairs once, then texts questions from anywhere — photos and even voice
              messages included. Replies come from <b class="text-white/85">your host at home</b>; the AI stays local.
            </p>
            <div class="kin-card space-y-3">
              <h3 class="font-semibold">Create the bot — 2 minutes, once, free</h3>
              <ol class="list-decimal pl-5 text-sm text-white/65 space-y-1.5">
                <li>Open Telegram and message <code class="text-teal-300">@BotFather</code>.</li>
                <li>Send <code class="text-teal-300">/newbot</code> and follow the two prompts (pick any name, e.g. “Miller Family AI”).</li>
                <li>BotFather replies with a <b class="text-white/85">token</b> — a long code like <code class="text-teal-300">7213…:AAH…</code>. Paste it below.</li>
              </ol>
              <label class="block">
                <span class="text-sm text-white/70">Bot token</span>
                <input class="kin-field mt-1 font-mono" type="password" bind:value={botToken} placeholder="paste the token from @BotFather" autocomplete="off" />
                <p class="text-xs text-white/40 mt-1">The token is a secret — KinAI stores it only on this computer.</p>
              </label>
              <div class="flex items-center gap-3">
                <button class="kin-btn !text-sm" onclick={testTelegram} disabled={tgTest.state === 'busy'}>
                  {#if tgTest.state === 'busy'}<Loader2 size={14} class="animate-spin" /> Verifying…{:else}Verify token{/if}
                </button>
                {#if tgTest.state === 'ok'}<span class="text-xs text-emerald-300">{tgTest.msg}</span>{/if}
                {#if tgTest.state === 'fail'}<span class="text-xs text-red-300">{tgTest.msg}</span>{/if}
              </div>
            </div>
            <div class="rounded-xl border border-teal-400/25 bg-teal-400/10 px-4 py-3 text-sm max-w-2xl">
              <b>How family members pair, later:</b> each person opens Settings → Telegram on their own
              device and scans a QR code — one tap links their Telegram to <em>their</em> KinAI identity,
              so chats stay personal. Nothing else to do now.
            </div>
            <p class="text-xs text-white/40"><b class="text-white/60">If you skip:</b> KinAI works fully on computers; there’s just no phone access. Set it up later in {WHERE.telegram}.</p>

          {:else if step.id === 'voice'}
            <h1 class="text-2xl font-bold tracking-tight">Voice <span class="text-white/40 text-base font-normal">(optional{app.ttsSupported ? '' : ' · not available on this platform'})</span></h1>
            <p class="text-white/60 max-w-2xl">Two independent halves — KinAI speaking, and KinAI listening. Both run fully on this computer; no audio ever goes to a cloud.</p>
            {#if app.ttsSupported}
              <div class="kin-card space-y-3">
                <label class="flex items-start justify-between gap-4 cursor-pointer">
                  <span class="text-sm"><span class="text-white/85">🔈 Voice replies (speaking)</span><br /><span class="text-xs text-white/45">A speak button on every answer, and — for family members who opt in with <code>/voice</code> — Telegram answers as voice notes.</span></span>
                  <input type="checkbox" bind:checked={ttsOn} class="accent-teal-400 mt-1" />
                </label>
                <div class="grid grid-cols-2 gap-4">
                  <label class="block">
                    <span class="text-sm text-white/70">English voice</span>
                    {#if ttsVoices.filter((v) => v.lang.startsWith('en')).length}
                      <select class="kin-field mt-1" bind:value={voiceEn}>
                        {#each ttsVoices.filter((v) => v.lang.startsWith('en')) as v}<option value={v.name}>{v.name}</option>{/each}
                      </select>
                    {:else}
                      <input class="kin-field mt-1" bind:value={voiceEn} />
                    {/if}
                  </label>
                  <label class="block">
                    <span class="text-sm text-white/70">German voice</span>
                    {#if ttsVoices.filter((v) => v.lang.startsWith('de')).length}
                      <select class="kin-field mt-1" bind:value={voiceDe}>
                        {#each ttsVoices.filter((v) => v.lang.startsWith('de')) as v}<option value={v.name}>{v.name}</option>{/each}
                      </select>
                    {:else}
                      <input class="kin-field mt-1" bind:value={voiceDe} />
                    {/if}
                  </label>
                </div>
                <button class="kin-btn !text-sm" onclick={() => previewVoice('en')} disabled={voicePreviewing}>
                  {voicePreviewing ? 'Playing…' : 'Preview voice'}
                </button>
              </div>
              <div class="kin-card space-y-3">
                <div class="flex items-start justify-between gap-4">
                  <span class="text-sm"><span class="text-white/85">🎙 Voice input (listening)</span><br /><span class="text-xs text-white/45">Family members can send <b>voice messages</b> to the Telegram bot and get real answers — transcribed on this computer by Whisper, German and English auto-detected.</span></span>
                </div>
                {#if sttDownloaded}
                  <label class="flex items-center gap-3 cursor-pointer">
                    <input type="checkbox" bind:checked={sttWanted} class="accent-teal-400" />
                    <span class="text-sm text-white/85">Voice input enabled <span class="text-emerald-300 text-xs">✓ model downloaded</span></span>
                  </label>
                {:else if sttDownloading}
                  <p class="text-sm text-teal-300 font-mono">↓ downloading… {sttProgress.toFixed(0)}%</p>
                {:else}
                  <button class="kin-btn !text-sm" onclick={downloadStt}>Download voice model (190 MB) — enables voice input</button>
                {/if}
              </div>
            {:else}
              <div class="kin-card text-sm text-white/55">
                Voice runs on the Mac host. This platform doesn’t support it — skip ahead; everything else works normally.
              </div>
            {/if}
            <p class="text-xs text-white/40"><b class="text-white/60">If you skip:</b> everything stays text — add either half later in {WHERE.voice}.</p>

          {:else if step.id === 'overlay'}
            <h1 class="text-2xl font-bold tracking-tight">Quick Chat &amp; appearance</h1>
            <p class="text-white/60 max-w-2xl">Comfort settings for <b class="text-white/85">this computer</b> — the floating Quick Chat window, the theme, and whether KinAI starts with the system.</p>
            <div class="kin-card space-y-3">
              <h3 class="font-semibold">⚡ Quick Chat overlay</h3>
              <p class="text-sm text-white/55">A small floating window that opens over any app with one keystroke — ask, read, Esc, back to work.</p>
              <label class="block max-w-[280px]">
                <span class="text-sm text-white/70">Global shortcut</span>
                <input
                  class="kin-field mt-1 font-mono cursor-pointer"
                  value={recordingHotkey ? 'press keys…' : hotkey}
                  readonly
                  onclick={() => (recordingHotkey = true)}
                />
                <p class="text-xs text-white/40 mt-1">Click, then press the combination you want (e.g. <span class="font-mono">Ctrl+Alt+K</span>).</p>
              </label>
              <label class="flex items-center gap-3 cursor-pointer">
                <input type="checkbox" bind:checked={alwaysOnTop} class="accent-teal-400" />
                <span class="text-sm text-white/85">Keep overlay above other windows</span>
              </label>
              <label class="flex items-center gap-3 cursor-pointer">
                <input type="checkbox" bind:checked={closeOnBlur} class="accent-teal-400" />
                <span class="text-sm text-white/85">Close it when you click elsewhere</span>
              </label>
            </div>
            <div class="kin-card space-y-3">
              <h3 class="font-semibold">Appearance &amp; startup</h3>
              <div class="flex gap-2">
                <button class="kin-btn !text-xs {theme === 'dark' ? '!border-teal-400/50 !text-teal-300' : ''}" onclick={() => (theme = 'dark')}>🌙 Dark</button>
                <button class="kin-btn !text-xs {theme === 'light' ? '!border-teal-400/50 !text-teal-300' : ''}" onclick={() => (theme = 'light')}>☀️ Light</button>
              </div>
              <label class="flex items-start gap-3 cursor-pointer">
                <input type="checkbox" bind:checked={autostartOn} class="accent-teal-400 mt-1" />
                <span class="text-sm">
                  <span class="text-white/85">Start KinAI when this computer starts</span><br />
                  <span class="text-xs text-white/45"><b>Recommended for a host</b> — after a reboot or power cut, the family’s AI comes back by itself.</span>
                </span>
              </label>
              <label class="flex items-center gap-3 cursor-pointer pl-7">
                <input type="checkbox" bind:checked={autostartMin} class="accent-teal-400" disabled={!autostartOn} />
                <span class="text-sm text-white/70">…minimized to the menu bar</span>
              </label>
            </div>
            <p class="text-xs text-white/40"><b class="text-white/60">If you skip:</b> sensible defaults — dark theme, the standard shortcut, no autostart. Change in {WHERE.overlay}.</p>

          {:else if step.id === 'invite'}
            <h1 class="text-2xl font-bold tracking-tight">Invite your family</h1>
            <p class="text-white/60 max-w-2xl">
              Each family member gets their own invite — it’s their identity. Their chats, memory, and
              preferences stay <b class="text-white/85">theirs</b>: private from everyone else, including you.
            </p>
            <div class="kin-card space-y-4">
              <div class="grid grid-cols-2 gap-4">
                <label class="block">
                  <span class="text-sm text-white/70">Who is this invite for?</span>
                  <input class="kin-field mt-1" bind:value={inviteLabel} placeholder="e.g. Mom’s MacBook" />
                </label>
                <label class="block">
                  <span class="text-sm text-white/70">Expires</span>
                  <select class="kin-field mt-1" bind:value={inviteTtl}>
                    <option value={7}>7 days</option>
                    <option value={30}>30 days</option>
                    <option value={90}>90 days</option>
                    <option value={365}>1 year</option>
                    <option value={0}>Never</option>
                  </select>
                </label>
              </div>
              <button class="kin-btn-primary !text-sm" onclick={createInvite} disabled={inviteBusy || !!invite}>
                {#if inviteBusy}<Loader2 size={14} class="animate-spin" /> Creating…{:else if invite}<Check size={14} /> Invite created{:else}Create invite{/if}
              </button>
              {#if invite}
                <div class="flex gap-6 items-center flex-wrap pt-2">
                  <div class="bg-black/40 rounded-lg p-3">{@html inviteQr}</div>
                  <div>
                    <div class="text-[10px] uppercase text-white/40 tracking-wider mb-1.5">Or type this code</div>
                    <span class="font-mono text-2xl tracking-[0.3em] bg-black/40 px-4 py-2 rounded-lg">{invite.short_code}</span>
                    <p class="text-xs text-white/45 mt-3 max-w-xs">On their computer: install KinAI → “I got an invite code” → scan or type. Done.</p>
                  </div>
                </div>
              {/if}
            </div>
            <div class="rounded-xl border border-teal-400/25 bg-teal-400/10 px-4 py-3 text-sm max-w-2xl">
              <b>Treat codes like house keys:</b> anyone on your network with this code can join as this
              person. Send it over a private channel, and revoke lost ones in Manage family.
            </div>
            <p class="text-xs text-white/40"><b class="text-white/60">If you skip:</b> chat solo right away and invite people whenever you like via {WHERE.invite}.</p>

          {:else if step.id === 'finish'}
            <h1 class="text-2xl font-bold tracking-tight">
              {aiConfigured ? 'Setup complete' : 'Setup finished — one thing missing'}
            </h1>
            {#if !aiConfigured}
              <div class="rounded-xl border border-red-400/40 bg-red-400/10 px-5 py-4 text-sm max-w-2xl space-y-2">
                <div class="font-bold text-base text-red-200">⚠️ KinAI has no AI connected — it can’t answer yet</div>
                <p class="text-red-100/85">
                  You skipped “Connect your AI”, so right now KinAI is a beautiful empty shell:
                  family members can install and connect, but <b>every question will fail</b> until a
                  model is connected.
                </p>
                <p class="text-red-100/85">
                  <b>To make it work:</b> open <b>{WHERE.ai}</b>, point KinAI at an AI server
                  (easiest: install <a class="underline" href="https://ollama.com">Ollama</a> and run
                  <code>ollama pull llama3.1:8b</code>), and press “Send a test message”.
                </p>
                <button class="kin-btn !text-sm mt-1" onclick={() => jump('ai')}>← Go back and connect an AI now</button>
              </div>
            {:else}
              <div class="rounded-xl border border-teal-400/30 bg-teal-400/10 px-5 py-4 text-sm max-w-2xl">
                <div class="font-bold text-base mb-1">🎉 Your family’s AI is ready</div>
                Your model answered its test, and everything you skipped can be added later — nothing
                here is a one-shot decision. Try it: ask <em>“What can you do?”</em> in the chat.
              </div>
            {/if}
            <div class="kin-card divide-y divide-white/5">
              {#each STEPS.filter((s) => s.skippable) as s}
                <div class="flex items-baseline gap-3 py-2 text-sm">
                  {#if stepState[s.id] === 'done'}
                    <span class="text-emerald-300 text-xs font-semibold w-16 flex-shrink-0">SET UP</span>
                    <span class="flex-1">{s.label}</span>
                  {:else}
                    <span class="text-white/35 text-xs font-semibold w-16 flex-shrink-0">SKIPPED</span>
                    <span class="flex-1 text-white/60">{s.label}</span>
                    <span class="text-xs text-white/35">later in: {WHERE[s.id]}</span>
                  {/if}
                </div>
              {/each}
            </div>
            <p class="text-xs text-white/40">This wizard won’t appear again — every setting above lives in Settings from now on.</p>
          {/if}

          {#if saveError}
            <div class="rounded-lg border border-red-400/30 bg-red-400/10 text-red-200 px-3 py-2 text-sm">
              {saveError}
            </div>
          {/if}
        </div>
      </div>

      <!-- footer nav -->
      <div class="border-t border-white/10 px-12 py-3.5 flex items-center gap-3 bg-ink-950/90">
        {#if cur > 1}
          <button class="kin-btn-ghost" onclick={back}>← Back</button>
        {/if}
        <span class="flex-1"></span>
        {#if step.skippable}
          <button class="kin-btn-ghost text-white/55" onclick={skip}>Set up later — skip</button>
        {/if}
        {#if step.id === 'finish'}
          <button class="kin-btn-primary" onclick={finishSetup} disabled={saving}>
            {#if saving}<Loader2 size={14} class="animate-spin" /> Starting…{:else}Open KinAI →{/if}
          </button>
        {:else if step.id !== 'role'}
          <button class="kin-btn-primary" onclick={continueStep} disabled={saving}>
            {#if saving}<Loader2 size={14} class="animate-spin" /> Saving…{:else}Continue →{/if}
          </button>
        {/if}
      </div>
    </div>
  </main>
{/if}
