// DEV-ONLY Tauri IPC mock.
//
// When the frontend runs in a plain browser (`pnpm dev` + localhost:1420,
// no Tauri shell), `window.__TAURI_INTERNALS__` doesn't exist and every
// invoke() rejects — the UI is untestable outside the app. This module
// installs a minimal in-memory backend so the whole UI (most importantly
// the /setup wizard) can be driven end-to-end in a browser: config
// mutations persist for the session, test buttons return canned successes,
// and every invoke is recorded on `window.__mockCalls` so an automated
// walkthrough can assert what would have been persisted.
//
// Guarded by import.meta.env.DEV — Vite eliminates the whole module from
// production builds — and by the __TAURI_INTERNALS__ check, so it never
// interferes inside the real app shell.

/* eslint-disable @typescript-eslint/no-explicit-any */

function defaultConfig() {
  return {
    mode: 'unconfigured',
    theme: 'dark',
    setup_completed: false,
    llm: {
      provider: 'ollama', base_url: 'http://localhost:11434', model: '',
      context_window: 8192, api_key: null, temperature: 0.7, max_tokens: 0,
      system_addendum: '', enabled: true,
    },
    llm_deep: {
      provider: 'llamacpp', base_url: '', model: '', context_window: 32768,
      api_key: null, temperature: 0.7, max_tokens: 0, system_addendum: '', enabled: false,
    },
    llm_balanced: {
      provider: 'llamacpp', base_url: '', model: '', context_window: 32768,
      api_key: null, temperature: 0.7, max_tokens: 0, system_addendum: '', enabled: false,
    },
    llm_factcheck: {
      provider: 'openai-compat', base_url: '', model: '', context_window: 65536,
      api_key: null, temperature: 0.2, max_tokens: 0, system_addendum: '', enabled: false,
    },
    host: { bind_addr: '0.0.0.0', port: 4847, family_name: 'Our Family', mdns_enabled: true, rate_limit_rpm: 60 },
    client: { host_url: null, host_token: null, host_label: null, display_name: 'You', pinned_hosts: [] },
    overlay: { hotkey: 'CmdOrCtrl+Space', always_on_top: true, font_size: 14, auto_close_on_blur: true },
    tools: { web_search: true, x_search: true, calculator: true, datetime: true, image_search: true, search_engine: 'duckduckgo', search_api_key: null, searxng_url: 'http://127.0.0.1:8888', search_fallback_searxng: true },
    vision: {
      enabled: false,
      primary: { label: '', base_url: '', model: '', api_key: null },
      failover: { label: '', base_url: '', model: '', api_key: null },
    },
    comfyui: { base_url: '', default_model: 'zimage' },
    telegram: { bot_token: '', bot_username: '' },
    tts: { enabled: true, voice_en: 'Zoe (Premium)', voice_de: 'Anna (Premium)', auto_speak: false },
    stt: { enabled: false, model: 'small-q5_1' },
    startup: { autostart_minimized: true },
    last_seen_changelog_version: '0.0.0',
  };
}

if (import.meta.env.DEV && typeof window !== 'undefined' && !('__TAURI_INTERNALS__' in window)) {
  const w = window as any;
  const cfg = defaultConfig();
  const stats: any = { model_loaded: null, peers_connected: 0, host_url: null, last_first_token_ms: null, client_connected: false, client_error: null, host_info: null };
  // Scenario preset: a test can set `window.__mockPreset` from an
  // init script (before the bundle runs) to shape the mock — e.g.
  // simulate a CLIENT install (config.mode 'client' + stats.host_info
  // with host_slots) for slash-menu tests. Shallow merge on purpose:
  // presets replace whole top-level sections.
  if (w.__mockPreset?.config) Object.assign(cfg, w.__mockPreset.config);
  if (w.__mockPreset?.stats) Object.assign(stats, w.__mockPreset.stats);
  // URL scenarios for manual/browser-pane testing, where nothing can run
  // before the bundle: `?mock=client-slots` simulates a CLIENT install
  // connected to a host that serves all three model slots. The local LLM
  // config stays empty on purpose — proving client UI (e.g. the slash
  // menu) reads the host-advertised slot list, not local settings.
  const scenario = new URLSearchParams(location.search).get('mock');
  if (scenario === 'client-slots') {
    cfg.mode = 'client';
    cfg.setup_completed = true;
    stats.client_connected = true;
    stats.host_info = {
      family_name: 'Mock Family',
      host_version: 'dev-mock',
      host_model: 'mock-fast-20b',
      host_search_engine: 'exa',
      host_vision: 'off',
      host_telegram_bot: '',
      host_slots: [
        { slug: 'fast', model: 'mock-fast-20b', alive: true },
        { slug: 'balanced', model: 'mock-balanced-33b', alive: true },
        // One offline slot so the client menu's "offline" marker is
        // visible in this scenario.
        { slug: 'deep', model: 'mock-deep-35b', alive: false },
      ],
      host_fact_check: true,
      host_reports: true,
      host_thread_ops: true,
    };
  } else if (scenario === 'client-old-host') {
    // A client connected to a host that predates a capability (here:
    // reports). Everything version-gated must degrade to hidden, never
    // to a button that hangs until it times out.
    cfg.mode = 'client';
    cfg.setup_completed = true;
    stats.client_connected = true;
    stats.host_info = {
      family_name: 'Old Host',
      host_version: '0.2.84',
      host_model: 'mock-fast-20b',
      host_search_engine: 'exa',
      host_vision: 'off',
      host_telegram_bot: '',
      host_slots: [{ slug: 'fast', model: 'mock-fast-20b', alive: true }],
      host_fact_check: false,
      // host_reports deliberately absent — an older host never sends it.
    };
  } else if (scenario === 'host-one-slot' || scenario === 'host-three-slots') {
    // Host-mode scenarios for the slash menu's local-config path:
    // one active slot → no model switches (≥2 gate); three → all shown
    // (with `balanced` marked offline via the mocked slot_health).
    cfg.mode = 'host';
    cfg.setup_completed = true;
    cfg.llm.model = 'local-fast-8b';
    if (scenario === 'host-three-slots') {
      Object.assign(cfg.llm_balanced, { base_url: 'http://192.168.1.50:8084', model: 'local-balanced-33b', enabled: true });
      Object.assign(cfg.llm_deep, { base_url: 'http://192.168.1.50:8081', model: 'local-deep-35b', enabled: true });
      w.__mockSlotHealth = { fast: true, balanced: false, deep: true };
      // Credentials present -> the per-message "fact check" button shows.
      Object.assign(cfg.llm_factcheck, {
        base_url: 'https://api.deepseek.com', model: 'deepseek-chat', api_key: 'sk-mock', enabled: true,
      });
    }
  }
  // Both scenarios get one canned Q/A so message-level UI (fact-check
  // button, panel) is exercisable in a plain browser.
  const cannedMessages = scenario
    ? [
        {
          id: 'mock-msg-user-1', thread_id: 'mock-thread-1', role: 'user', sender: 'You',
          content: 'When did the Eiffel Tower open?', attachments: [],
          created_at: new Date(Date.now() - 60000).toISOString(), summarized_into: null,
        },
        {
          id: 'mock-msg-assistant-1', thread_id: 'mock-thread-1', role: 'assistant', sender: 'KinAI',
          content: 'The Eiffel Tower opened in 1887 and is 330 m tall.', attachments: [],
          created_at: new Date(Date.now() - 55000).toISOString(), summarized_into: null,
          metrics: { first_token_ms: 300, total_ms: 1800, output_tokens: 18, tps: 12, model: 'local-fast-8b', slot: 'fast' },
        },
      ]
    : [];
  const calls: { cmd: string; args: any }[] = [];
  w.__mockCalls = calls;
  w.__mockConfig = cfg;
  w.__mockStats = stats;

  // Presettable "What's new" payload — lets a test/screenshot script
  // drive the real ChangelogModal with real release notes via
  // `__mockPreset.changelog` (same init-script mechanism as above).
  let changelogPayload: any = { version: 'dev', markdown: null, should_show: false };
  if (w.__mockPreset?.changelog) changelogPayload = w.__mockPreset.changelog;

  const handlers: Record<string, (args: any) => any> = {
    get_config: () => cfg,
    runtime_stats: () => stats,
    // Host-mode slot liveness; override via __mockPreset.slotHealth or a
    // URL scenario to test the menu's offline markers.
    slot_health: () => w.__mockSlotHealth ?? {},
    // Reports: an in-memory list so the button, the badge and the
    // host page can all be exercised in a plain browser.
    report_answer: (a) => {
      if (w.__mockReportFail) throw new Error(w.__mockReportFail);
      w.__mockReports = [
        {
          id: 'rep-' + (w.__mockReports?.length ?? 0),
          peer_id: cfg.mode === 'host' ? 'host' : 'peer-1',
          reporter: cfg.mode === 'host' ? 'You' : 'Mom',
          message_id: a.messageId,
          question: a.question,
          answer: a.answer,
          model: a.model ?? '',
          slot: a.slot ?? '',
          created_at: new Date().toISOString(),
          reviewed_at: null,
        },
        ...(w.__mockReports ?? []),
      ];
      return 'Thanks — the host can see this answer now.';
    },
    list_reports: () => w.__mockReports ?? [],
    open_report_count: () => (w.__mockReports ?? []).filter((r: any) => !r.reviewed_at).length,
    set_report_reviewed: (a) => {
      w.__mockReports = (w.__mockReports ?? []).map((r: any) =>
        r.id === a.id ? { ...r, reviewed_at: a.reviewed ? new Date().toISOString() : null } : r
      );
      return null;
    },
    delete_reviewed_reports: () => {
      const before = (w.__mockReports ?? []).length;
      w.__mockReports = (w.__mockReports ?? []).filter((r: any) => !r.reviewed_at);
      return before - w.__mockReports.length;
    },
    delete_report: (a) => {
      w.__mockReports = (w.__mockReports ?? []).filter((r: any) => r.id !== a.id);
      return null;
    },
    // Set `window.__mockSendFail = "message"` to make the next send
    // reject — exercises the inline turn-error bubble path.
    send_message: () => {
      if (w.__mockSendFail) throw new Error(w.__mockSendFail);
      return null;
    },
    kinai_version: () => ({ version: 'dev-mock', build_time: 0, git_commit: 'mock', target: 'browser', repository: '' }),
    get_changelog_payload: () => changelogPayload,
    tts_supported: () => true,
    list_tts_voices: () => [
      { name: 'Zoe (Premium)', lang: 'en_US' }, { name: 'Samantha', lang: 'en_US' },
      { name: 'Anna (Premium)', lang: 'de_DE' }, { name: 'Markus', lang: 'de_DE' },
    ],
    stt_status: () => ({ enabled: cfg.stt.enabled, model: cfg.stt.model, ready: cfg.stt.enabled, models: [
      { id: 'small-q5_1', label: 'Standard (good accuracy, fast)', size_mb: 190, downloaded: false },
      { id: 'medium-q5_0', label: 'High accuracy (bigger, slower)', size_mb: 574, downloaded: false },
    ] }),
    download_stt_model: () => { cfg.stt.enabled = true; return null; },
    set_stt_config: (a) => { Object.assign(cfg.stt, a.stt); return cfg; },
    set_tts_config: (a) => { Object.assign(cfg.tts, a.tts); return cfg; },
    detect_backends: () => [
      { provider: 'ollama', base_url: 'http://localhost:11434', label: 'Ollama (this computer)', models: ['llama3.1:8b', 'qwen2.5:14b'] },
    ],
    scan_local_network: () => [
      { provider: 'ollama', base_url: 'http://localhost:11434', label: 'Ollama (this computer)', models: ['llama3.1:8b', 'qwen2.5:14b'] },
      { provider: 'lmstudio', base_url: 'http://192.168.1.50:1234', label: 'LM Studio (192.168.1.50)', models: ['qwen2.5-14b-instruct'] },
    ],
    query_model_caps: () => ({ context_length: 32768 }),
    test_llm_endpoint: () => ({ ok: true, latency_ms: 850, reply: 'Hello! I’m ready to help your family.', error: null }),
    test_llm_connection: () => ({ ok: true, models: ['llama3.1:8b', 'qwen2.5:14b'], error: null }),
    test_vision_endpoint: () => ({ ok: true, latency_ms: 1200, reply: 'Red', error: null }),
    test_comfy_endpoint: () => ({ ok: true, error: null }),
    test_telegram_token: () => ({ ok: true, bot_username: 'mock_family_bot', error: null }),
    set_telegram_token: (a) => { cfg.telegram.bot_token = a.args.bot_token; cfg.telegram.bot_username = 'mock_family_bot'; return cfg; },
    set_llm_settings: (a) => { Object.assign(cfg.llm, a.llm); return cfg; },
    set_llm_deep_settings: (a) => { Object.assign(cfg.llm_deep, a.llm); return cfg; },
    set_llm_balanced_settings: (a) => { Object.assign(cfg.llm_balanced, a.llm); return cfg; },
    test_searxng: async (a: any) => {
      await new Promise((r) => setTimeout(r, 400));
      if (!/^https?:\/\//.test(a?.args?.url ?? ''))
        return { ok: false, latency_ms: 3, message: 'The address needs a scheme — try http://…', sample: '', engines: [] };
      return { ok: true, latency_ms: 812, message: 'Connected — 10 results in 812 ms.',
               sample: 'Technical specifications | Olares Docs', engines: ['brave', 'duckduckgo'] };
    },
    set_llm_factcheck_settings: (a) => { Object.assign(cfg.llm_factcheck, a.llm); return cfg; },
    // Canned fact-check report after a short delay; set
    // `window.__mockFactCheckFail = "reason"` to exercise the error path.
    fact_check_message: async () => {
      await new Promise((r) => setTimeout(r, 1200));
      if (w.__mockFactCheckFail) throw new Error(w.__mockFactCheckFail);
      // The second bullet deliberately carries TWO "~" approximations in
      // one line: GFM used to pair them into a strikethrough and eat the
      // numbers between them (0.2.87 field report). Keep them here so the
      // regression shows up on sight in this scenario.
      return [
        '⚠️ Partly accurate — one date is off.',
        '',
        '- "The Eiffel Tower opened in 1887" — construction STARTED 1887; it opened **31 March 1889**. (Source: web_search — britannica.com)',
        '- "is 330 m tall" — right: ~330 m to the tip, ~300 m to the roof. (Source: web_search — toureiffel.paris)',
      ].join('\n');
    },
    set_vision_settings: (a) => { Object.assign(cfg.vision, a.vision); return cfg; },
    set_comfy_config: (a) => { Object.assign(cfg.comfyui, a.comfyui); return cfg; },
    set_overlay_settings: (a) => { Object.assign(cfg.overlay, a.overlay); return cfg; },
    set_theme: (a) => { cfg.theme = a.theme; return cfg; },
    set_startup_settings: (a) => { Object.assign(cfg.startup, a.startup); return cfg; },
    set_mode: (a) => {
      cfg.mode = a.args.mode;
      if (a.args.host) Object.assign(cfg.host, a.args.host);
      if (a.args.client) Object.assign(cfg.client, a.args.client);
      if (a.args.tools) Object.assign(cfg.tools, a.args.tools);
      if (a.args.overlay) Object.assign(cfg.overlay, a.args.overlay);
      return cfg;
    },
    start_host: () => null,
    mark_setup_completed: () => { cfg.setup_completed = true; return cfg; },
    generate_invite: (a) => ({
      id: 'mock-invite', short_code: 'k7m3px', jwt: 'mock', host_url: 'ws://192.168.1.50:4847/kin',
      label: a.args.label, join_url: 'kinai://join?host=mock&code=k7m3px&token=mock',
      qr_payload: 'kinai://join?host=mock&code=k7m3px&token=mock',
      created_at: new Date().toISOString(), expires_at: new Date(Date.now() + 30 * 864e5).toISOString(), revoked: false,
    }),
    list_threads: () =>
      scenario
        ? [{ id: 'mock-thread-1', title: 'Eiffel Tower', created_at: new Date().toISOString(), updated_at: new Date().toISOString(), peer_id: 'host' }]
        : [],
    create_thread: (a: any) => ({
      id: 'mock-thread-1', title: a?.title ?? 'New conversation',
      created_at: new Date().toISOString(), updated_at: new Date().toISOString(), peer_id: 'host',
    }),
    load_messages: () => cannedMessages,
    load_thread: () => cannedMessages,
    thread_active_slot: () => null,
    list_invites: () => [],
    list_peers: () => [],
    stt_download_progress: () => null,
  };

  w.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: 'main' }, currentWebview: { label: 'main' } },
    plugins: {},
    convertFileSrc: (p: string) => p,
    transformCallback: (cb: (r: any) => void) => {
      const id = Math.floor(Math.random() * 1e9);
      w[`_${id}`] = cb;
      return id;
    },
    invoke: async (cmd: string, args: any = {}) => {
      calls.push({ cmd, args });
      // Plugin-namespaced commands (events, autostart, dialogs…) all no-op.
      if (cmd.startsWith('plugin:')) {
        if (cmd === 'plugin:autostart|is_enabled') return false;
        if (cmd === 'plugin:event|listen') return 0;
        return null;
      }
      const h = handlers[cmd];
      if (h) return h(args);
      console.info('[dev-tauri-mock] unhandled command:', cmd, args);
      return null;
    },
  };
  console.info('[dev-tauri-mock] installed — browser session uses in-memory backend');
}

export {};
