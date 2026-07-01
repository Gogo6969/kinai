<script lang="ts">
  import {
    api,
    events,
    type ClientSettings,
    type ComfyConfig,
    type LlmSettings,
    type OverlaySettings,
    type TelegramLinkStatus,
    type Theme,
    type ToolSettings,
    type SttStatus,
    type TtsConfig,
    type TtsVoice,
    type VisionEndpoint,
    type VisionSettings,
  } from '$lib/api';
  import { app } from '$lib/stores/app.svelte';
  import { onMount, onDestroy } from 'svelte';
  import { goto } from '$app/navigation';
  import QRCode from 'qrcode';
  import { Loader2, RefreshCw, ArrowDownUp } from '@lucide/svelte';
  import {
    enable as enableAutostart,
    disable as disableAutostart,
    isEnabled as isAutostartEnabled,
  } from '@tauri-apps/plugin-autostart';

  const isClient = $derived(app.config?.mode === 'client');
  const isHost = $derived(app.config?.mode === 'host');

  // Fast model — the existing single slot. Reachable as `/fast` once
  // the deep slot is also configured; the default routing target for
  // plain (non-slash) messages.
  let llm = $state<LlmSettings>({
    provider: 'ollama',
    base_url: 'http://localhost:11434',
    model: 'llama3.1:8b',
    context_window: 8192,
    api_key: null,
    temperature: 0.7,
    max_tokens: 1024,
    system_addendum: '',
    enabled: true,
  });
  // Deep model — new secondary slot. Disabled + empty by default so
  // existing installs keep their single-model behaviour. Reachable as
  // `/deep` once base_url + model are filled and `enabled` is true.
  let llmDeep = $state<LlmSettings>({
    provider: 'ollama',
    base_url: '',
    model: '',
    context_window: 8192,
    api_key: null,
    temperature: 0.7,
    max_tokens: 0,
    system_addendum: '',
    enabled: false,
  });
  let llmDeepSaveState = $state<'idle' | 'saving' | 'ok' | 'fail'>('idle');
  let overlay = $state<OverlaySettings>({
    hotkey: 'CmdOrCtrl+Space',
    always_on_top: true,
    font_size: 14,
    auto_close_on_blur: true,
  });
  // Global-hotkey recorder UI state. `recordingHotkey` = field focused and
  // listening for a key combo; `hotkeyManual` = user opted into raw text entry.
  let recordingHotkey = $state(false);
  let hotkeyManual = $state(false);
  let theme = $state<Theme>('dark');
  let tools = $state<ToolSettings>({
    web_search: true,
    x_search: true,
    image_search: true,
    calculator: true,
    datetime: true,
    search_engine: 'duckduckgo',
    search_api_key: null,
  });
  const emptyEndpoint = (): VisionEndpoint => ({
    label: '',
    base_url: '',
    model: '',
    api_key: null,
  });
  let vision = $state<VisionSettings>({
    enabled: false,
    primary: emptyEndpoint(),
    failover: emptyEndpoint(),
  });
  // Per-slot so the primary and failover test results show independently
  // (testing one must not overwrite the other's ✓/✗ line).
  let visionTestState = $state<
    Record<'primary' | 'failover', 'idle' | 'testing' | 'ok' | 'fail'>
  >({ primary: 'idle', failover: 'idle' });
  let visionTestMsg = $state<Record<'primary' | 'failover', string>>({
    primary: '',
    failover: '',
  });

  // Image generation (ComfyUI) — empty base_url means feature disabled.
  let comfyui = $state<ComfyConfig>({ base_url: '', default_model: 'zimage' });
  let comfyTestState = $state<'idle' | 'testing' | 'ok' | 'fail'>('idle');
  let comfyTestMsg = $state<string>('');

  // Voice replies (TTS) — host-only, macOS `say` engine.
  let tts = $state<TtsConfig>({ enabled: false, voice_en: 'Zoe (Premium)', voice_de: 'Anna (Premium)', auto_speak: false });
  let ttsVoices = $state<TtsVoice[]>([]);
  let ttsVoicesLoading = $state(false);
  let ttsPreviewing = $state<'' | 'en' | 'de'>('');
  const ttsVoicesEn = $derived(ttsVoices.filter((v) => v.lang.startsWith('en')));
  const ttsVoicesDe = $derived(ttsVoices.filter((v) => v.lang.startsWith('de')));

  async function refreshTtsVoices() {
    ttsVoicesLoading = true;
    try {
      ttsVoices = await api.listTtsVoices();
    } catch (e) {
      console.warn('listTtsVoices failed', e);
    } finally {
      ttsVoicesLoading = false;
    }
  }

  async function previewTtsVoice(lang: 'en' | 'de') {
    ttsPreviewing = lang;
    try {
      await api.previewTtsVoice({
        voice: lang === 'de' ? tts.voice_de : tts.voice_en,
        lang,
      });
    } catch (e) {
      console.warn('preview failed', e);
    } finally {
      // The sample is ~4s; release the button shortly after.
      setTimeout(() => (ttsPreviewing = ''), 4000);
    }
  }

  // Voice input (STT) — Whisper model download + status.
  let stt = $state<SttStatus | null>(null);
  let sttDownloading = $state<string | null>(null); // model id mid-download
  let sttProgress = $state(0);
  let sttError = $state('');
  let sttUnlisten: (() => void) | null = null;

  async function refreshStt() {
    try {
      stt = await api.sttStatus();
    } catch (e) {
      console.warn('sttStatus failed', e);
    }
  }

  async function downloadStt(modelId: string) {
    sttError = '';
    sttDownloading = modelId;
    sttProgress = 0;
    try {
      sttUnlisten = await events.onSttDownloadProgress((p) => {
        if (p.model_id === modelId) sttProgress = p.progress;
      });
      await api.downloadSttModel(modelId);
      // Downloading a voice model IS the intent to use voice input — flip it
      // on (and select the model just downloaded) so the user doesn't have to
      // hunt for a separate enable toggle. Without this, the model sat
      // downloaded-but-disabled and Telegram voice messages were rejected.
      if (app.config) {
        await api.setSttConfig({ ...app.config.stt, model: modelId, enabled: true });
      }
      await app.load();
      await refreshStt();
    } catch (e) {
      sttError = String(e).replace(/^Error:\s*/, '');
    } finally {
      sttUnlisten?.();
      sttUnlisten = null;
      sttDownloading = null;
    }
  }

  async function setSttEnabled(enabled: boolean) {
    if (!app.config) return;
    try {
      await api.setSttConfig({ ...app.config.stt, enabled });
      await app.load();
      await refreshStt();
    } catch (e) {
      sttError = String(e).replace(/^Error:\s*/, '');
    }
  }

  async function selectSttModel(model: string) {
    if (!app.config) return;
    try {
      await api.setSttConfig({ ...app.config.stt, model });
      await app.load();
      await refreshStt();
    } catch (e) {
      sttError = String(e).replace(/^Error:\s*/, '');
    }
  }

  // Telegram bot integration.
  let telegramTokenInput = $state('');
  let telegramTestState = $state<'idle' | 'testing' | 'ok' | 'fail'>('idle');
  let telegramTestMsg = $state<string>('');
  let telegramSaveState = $state<'idle' | 'saving' | 'ok' | 'fail'>('idle');
  let telegramStatus = $state<TelegramLinkStatus | null>(null);
  let telegramPairUrl = $state<string>('');
  let telegramPairQrDataUrl = $state<string>('');
  // Resolve the family-bot username from two sources: the local status
  // (host knows it directly) OR the host's Welcome (clients get it on
  // connect). `botConfigured` falls back to the username presence so
  // the pair sub-card appears for clients before their first status
  // round-trip lands.
  const telegramBotUsername = $derived(
    telegramStatus?.bot_username || app.hostInfo?.host_telegram_bot || ''
  );
  const telegramBotConfigured = $derived(
    telegramStatus?.bot_configured || !!telegramBotUsername
  );
  let telegramPairing = $state(false);
  // Pairing status poll handle — cleared on unmount so it can't outlive
  // the component (it runs up to 11 min). Not $state; it's just a timer id.
  let telegramPoll: ReturnType<typeof setInterval> | null = null;

  async function refreshTelegramStatus() {
    try {
      telegramStatus = await api.telegramLinkStatus();
    } catch (e) {
      console.warn('telegram status:', e);
      telegramStatus = null;
    }
  }

  async function testTelegramToken() {
    if (!telegramTokenInput.trim()) {
      telegramTestState = 'fail';
      telegramTestMsg = 'Paste a bot token from @BotFather first.';
      return;
    }
    telegramTestState = 'testing';
    telegramTestMsg = '';
    try {
      const r = await api.testTelegramToken({ bot_token: telegramTokenInput.trim() });
      if (r.ok) {
        telegramTestState = 'ok';
        telegramTestMsg = `Reachable — bot is @${r.bot_username ?? '?'}`;
      } else {
        telegramTestState = 'fail';
        telegramTestMsg = r.error ?? 'Test failed';
      }
    } catch (e) {
      telegramTestState = 'fail';
      telegramTestMsg = String(e).replace(/^Error:\s*/, '');
    }
  }

  async function saveTelegramToken() {
    telegramSaveState = 'saving';
    try {
      await api.setTelegramToken({ bot_token: telegramTokenInput.trim() });
      telegramSaveState = 'ok';
      // Pull fresh status so the rest of the card adapts (paired
      // section if applicable, "Pair this device" button now active).
      await refreshTelegramStatus();
      // Don't keep the token in the input — the field is write-only.
      telegramTokenInput = '';
      setTimeout(() => {
        if (telegramSaveState === 'ok') telegramSaveState = 'idle';
      }, 2000);
    } catch (e) {
      telegramSaveState = 'fail';
      telegramTestMsg = String(e).replace(/^Error:\s*/, '');
    }
  }

  async function startTelegramPair() {
    telegramPairing = true;
    telegramPairUrl = '';
    telegramPairQrDataUrl = '';
    try {
      const r = await api.requestTelegramPair();
      telegramPairUrl = r.url;
      // Generate a QR code as a data URL. 256 px, medium error-
      // correction is plenty for a t.me link.
      telegramPairQrDataUrl = await QRCode.toDataURL(r.url, {
        width: 256,
        margin: 1,
        color: { dark: '#0f172a', light: '#ffffff' },
      });
    } catch (e) {
      telegramPairUrl = '';
      telegramPairQrDataUrl = '';
      window.dispatchEvent(
        new CustomEvent('kin-toast', {
          detail: {
            msg: `✗ Couldn't start pairing: ${String(e).replace(/^Error:\s*/, '')}`,
            ms: 6000,
          },
        })
      );
    } finally {
      telegramPairing = false;
    }
    // Poll the status briefly so when the user finishes scanning + the
    // bot confirms pairing, the card auto-updates without a manual refresh.
    // Tracked in `telegramPoll` (cleared on unmount + before re-starting)
    // so navigating away mid-pair doesn't leave an interval firing IPCs at
    // a dead component for up to 11 minutes.
    if (telegramPoll) clearInterval(telegramPoll);
    const start = Date.now();
    telegramPoll = setInterval(async () => {
      if (Date.now() - start > 11 * 60 * 1000) {
        clearInterval(telegramPoll!);
        telegramPoll = null;
        return;
      }
      await refreshTelegramStatus();
      if (telegramStatus?.paired) {
        clearInterval(telegramPoll!);
        telegramPoll = null;
        telegramPairUrl = '';
        telegramPairQrDataUrl = '';
      }
    }, 3000);
  }

  async function unpairTelegram() {
    try {
      await api.unpairTelegram();
      await refreshTelegramStatus();
    } catch (e) {
      console.warn('unpair:', e);
    }
  }

  let displayName = $state('');
  let models = $state<string[]>([]);
  let saving = $state(false);
  let refreshing = $state(false);
  // Autostart toggle state. `null` while we hydrate from the OS; the
  // toggle is rendered disabled until we know the truth (avoids the
  // user toggling-twice race during the first paint).
  let autostartOn = $state<boolean | null>(null);
  let autostartBusy = $state(false);
  let autostartError = $state<string>('');
  /** Sub-preference: when launched via autostart, should the window
   *  show or stay hidden in the tray? Hydrated from config; persisted
   *  via setStartupSettings. Only visible in the UI when the parent
   *  autostart toggle is on (it's irrelevant otherwise). */
  let autostartMinimized = $state<boolean>(true);
  /** UI feedback after a successful save: "saved" → fades to "" after 2.5s.
   *  Empty string for the initial state and after the fade. */
  let saveStatus = $state<'' | 'saved' | 'error'>('');
  let saveError = $state('');

  /**
   * Unsaved-changes tracking. `settingsSnapshot()` serializes exactly the
   * fields the main `save()` persists — nothing more. Everything that saves
   * itself immediately via its own handler (autostart, Telegram token, STT,
   * the autostart-minimized sub-toggle) is deliberately excluded, so toggling
   * those does NOT make the form look dirty.
   *
   * `savedBaseline` is stamped after hydration and after every successful
   * save; `dirty` is true whenever the live form diverges from it. The
   * header button reads `dirty` to turn "Back to chat" into "Save changes",
   * so the user can't navigate away on unsaved edits.
   */
  function settingsSnapshot(): string {
    return JSON.stringify({
      llm,
      llmDeep,
      overlay,
      theme,
      tools,
      vision,
      comfyui,
      tts,
      displayName: displayName.trim(),
    });
  }
  let savedBaseline = $state('');
  const dirty = $derived(savedBaseline !== '' && settingsSnapshot() !== savedBaseline);
  let refreshMessage = $state('');
  let testResult = $state<string>('');
  let versionInfo = $state<{
    version: string;
    build_time: number;
    git_commit: string;
    target: string;
  } | null>(null);

  onDestroy(() => {
    if (telegramPoll) {
      clearInterval(telegramPoll);
      telegramPoll = null;
    }
  });

  onMount(() => {
    if (app.config) {
      // Hydrate the autostart sub-preference (window vs tray). Default
      // matches the Rust struct's Default::default — `true` (minimized
      // to tray) when the section is absent in older configs.
      autostartMinimized = app.config.startup?.autostart_minimized ?? true;
      llm = { ...app.config.llm };
      if (app.config.llm_deep) {
        llmDeep = { ...app.config.llm_deep };
      }
      overlay = { ...app.config.overlay };
      theme = app.config.theme;
      tools = { ...app.config.tools };
      displayName = app.config.client.display_name;
      if (app.config.vision) {
        vision = {
          enabled: app.config.vision.enabled,
          primary: { ...app.config.vision.primary },
          failover: { ...app.config.vision.failover },
        };
      }
      if (app.config.comfyui) {
        comfyui = { ...app.config.comfyui };
      }
      if (app.config.tts) {
        tts = { ...app.config.tts };
      }
    }
    // Stamp the unsaved-changes baseline once hydration is done. The async
    // refreshes below (TTS voices, STT, Telegram, autostart) don't touch any
    // tracked field, so the form starts clean.
    savedBaseline = settingsSnapshot();
    if (app.config?.mode === 'host') {
      void refreshTtsVoices();
      void refreshStt();
    } else if (app.ttsSupported) {
      void refreshTtsVoices();
    }
    // Pull current Telegram pairing state regardless of mode — host
    // sees the bot-token card + their own pair status; client peers
    // (in a later iteration) see just their pair status.
    void refreshTelegramStatus();
    // Only the host knows local model lists; in Client mode the LLM lives
    // on another machine and there's nothing to refresh here.
    if (app.config?.mode === 'host') {
      void refreshModels({ silent: true });
    }
    api.kinaiVersion().then((v) => (versionInfo = v)).catch(() => {});
    // Hydrate the autostart toggle from the OS. The plugin reads the
    // ~/Library/LaunchAgents plist (macOS) or HKCU\...\Run key
    // (Windows) so the rendered switch always reflects ground truth,
    // not a cached guess in our config. If the query fails (rare —
    // shouldn't fail in dev/release builds; can fail in `tauri dev`
    // before the plugin is set up), default to OFF and surface the
    // error to the user instead of pretending it's working.
    isAutostartEnabled()
      .then((on) => {
        autostartOn = on;
      })
      .catch((e) => {
        autostartOn = false;
        autostartError = String(e).replace(/^Error:\s*/, '');
      });
  });

  /**
   * Flip the autostart state on the OS. The toggle is bound to
   * `autostartOn`; this handler runs on `onchange` and reconciles.
   * Disable inputs during the IPC so a fast double-click can't issue
   * enable + disable racing.
   */
  /**
   * Persist the "show window vs minimize to tray" choice. Updates
   * local state optimistically (the radio reflects the click
   * immediately) and pushes the config to the host. On error we
   * revert to whatever the config still says, so a failed write
   * doesn't strand the UI in a wrong state.
   */
  async function setAutostartMinimized(next: boolean) {
    const previous = autostartMinimized;
    autostartMinimized = next;
    try {
      const updated = await api.setStartupSettings({
        autostart_minimized: next,
      });
      app.config = updated;
    } catch (e) {
      autostartMinimized = previous;
      autostartError = String(e).replace(/^Error:\s*/, '');
    }
  }

  async function toggleAutostart(next: boolean) {
    if (autostartBusy) return;
    autostartBusy = true;
    autostartError = '';
    try {
      if (next) {
        await enableAutostart();
      } else {
        await disableAutostart();
      }
      // Re-read from OS so we never display a stale optimistic state
      // — the user wants to trust this toggle.
      autostartOn = await isAutostartEnabled();
    } catch (e) {
      autostartError = String(e).replace(/^Error:\s*/, '');
      // Revert the visible toggle to whatever the OS actually says.
      try {
        autostartOn = await isAutostartEnabled();
      } catch {
        /* swallow — we already have an error to show */
      }
    } finally {
      autostartBusy = false;
    }
  }

  function copyVersion() {
    if (!versionInfo) return;
    const built = new Date(versionInfo.build_time * 1000).toISOString();
    const commitSegment =
      versionInfo.git_commit && versionInfo.git_commit !== 'unknown'
        ? ` · ${versionInfo.git_commit}`
        : '';
    navigator.clipboard.writeText(
      `KinAI ${versionInfo.version} · ${versionInfo.target}${commitSegment} · built ${built}`
    );
  }

  // Friendly platform label for the footer — the raw build target
  // ("macos-aarch64") reads like a CI artifact; users recognize
  // "macOS · Apple silicon".
  function prettyPlatform(target: string): string {
    switch (target) {
      case 'macos-aarch64':
        return 'macOS · Apple silicon';
      case 'macos-x86_64':
        return 'macOS · Intel';
      case 'windows':
        return 'Windows';
      case 'linux':
        return 'Linux';
      default:
        return target;
    }
  }

  async function refreshModels({ silent = false }: { silent?: boolean } = {}) {
    refreshing = true;
    refreshMessage = '';
    try {
      const next = await api.listLocalModels();
      models = next;
      if (next.length > 0) {
        if (!next.includes(llm.model)) llm.model = next[0];
        // Auto-fill context_window from the selected model when supported.
        try {
          const caps = await api.queryModelCaps({
            provider: llm.provider,
            base_url: llm.base_url,
            api_key: llm.api_key,
            model: llm.model,
          });
          if (caps.context_length && caps.context_length > 0) {
            llm.context_window = caps.context_length;
          }
        } catch {
          /* best effort */
        }
        if (!silent) {
          refreshMessage = `Found ${next.length} model${next.length === 1 ? '' : 's'} · context ${llm.context_window.toLocaleString()} tokens`;
        }
      } else if (!silent) {
        refreshMessage = 'Server reported zero models.';
      }
    } catch (e) {
      models = [];
      if (!silent) refreshMessage = `Failed: ${String(e)}`;
    } finally {
      refreshing = false;
    }
  }

  async function save() {
    saving = true;
    saveStatus = '';
    saveError = '';
    try {
      // Host-only settings — the client can't change what model or
      // search engine the host runs.
      if (isHost) {
        await api.setLlmSettings(llm);
        // Save the deep slot whether or not it's configured — an
        // empty base_url + enabled=false is a valid "disabled"
        // state. The backend's `is_active()` check handles the
        // visibility/routing implications.
        await api.setLlmDeepSettings(llmDeep);
        if (app.config) {
          await api.setMode({ mode: app.config.mode, tools });
        }
        await api.setVisionSettings(vision);
        await api.setComfyConfig({
          base_url: comfyui.base_url.trim(),
          default_model: comfyui.default_model || 'zimage',
        });
        await api.setTtsConfig({ ...tts });
      }
      if (!isHost && app.ttsSupported) {
        // Mac client: voice replies are a local-machine preference.
        await api.setTtsConfig({ ...tts });
      }
      // Always-applicable settings.
      await api.setOverlaySettings(overlay);
      await api.setTheme(theme);
      if (app.config) {
        const nextClient: ClientSettings = {
          ...app.config.client,
          display_name: displayName.trim() || app.config.client.display_name,
        };
        await api.setMode({
          mode: app.config.mode,
          client: nextClient,
        });
      }
      await app.load();
      // Re-baseline: the live form now equals what we persisted, so the
      // header button drops back from "Save changes" to "Back to chat".
      savedBaseline = settingsSnapshot();
      saveStatus = 'saved';
      // Show "saved" briefly, then return to the chat — settings are
      // done, no reason to leave the user parked here.
      setTimeout(() => {
        if (saveStatus === 'saved') void goto('/');
      }, 900);
    } catch (e) {
      saveStatus = 'error';
      saveError = String(e).replace(/^Error:\s*/, '');
    } finally {
      saving = false;
    }
  }

  let updateStatus = $state<'idle' | 'checking' | 'uptodate' | 'available' | 'error'>('idle');
  let updateInfo = $state<{ version: string } | null>(null);

  async function checkUpdates() {
    updateStatus = 'checking';
    updateInfo = null;
    let listener: undefined | (() => void);
    try {
      const { listen } = await import('@tauri-apps/api/event');
      listener = await listen<{ version: string }>('kinai://update-available', (e) => {
        updateInfo = { version: e.payload.version };
        updateStatus = 'available';
      });
      await api.checkUpdates();
      // Give the event a beat to land before resolving "up to date".
      await new Promise((r) => setTimeout(r, 1500));
      if (updateStatus === 'checking') updateStatus = 'uptodate';
    } catch (e) {
      updateStatus = 'error';
    } finally {
      listener?.();
    }
  }

  async function testTool(name: string, argsJson: string) {
    try {
      testResult = await api.testTool(name, argsJson);
    } catch (e) {
      testResult = String(e);
    }
  }

  /** Swap the primary and failover vision endpoints so the user can flip
   *  which model is the regular one and which is the backup without
   *  retyping anything. `VisionEndpoint` is flat (label/base_url/model/
   *  api_key), so a shallow clone is a full copy. The ✓/✗ test results
   *  belonged to the old slots, so reset them. Mutating `vision` marks the
   *  form dirty, so the header "Save & close" button appears. */
  function swapVisionEndpoints() {
    const oldPrimary = { ...vision.primary };
    vision.primary = { ...vision.failover };
    vision.failover = oldPrimary;
    visionTestState = { primary: 'idle', failover: 'idle' };
    visionTestMsg = { primary: '', failover: '' };
  }

  async function testVision(slot: 'primary' | 'failover') {
    const ep = vision[slot];
    if (!ep.base_url || !ep.model) {
      visionTestState[slot] = 'fail';
      visionTestMsg[slot] = 'Set base URL and model first.';
      return;
    }
    visionTestState[slot] = 'testing';
    visionTestMsg[slot] = '';
    try {
      const res = await api.testVisionEndpoint({
        base_url: ep.base_url,
        model: ep.model,
        api_key: ep.api_key,
      });
      if (res.ok) {
        visionTestState[slot] = 'ok';
        visionTestMsg[slot] = `${ep.label || ep.model} replied in ${res.latency_ms}ms: "${res.reply.slice(0, 80)}"`;
      } else {
        visionTestState[slot] = 'fail';
        visionTestMsg[slot] = res.error ?? 'Test failed.';
      }
    } catch (e) {
      visionTestState[slot] = 'fail';
      visionTestMsg[slot] = String(e).replace(/^Error:\s*/, '');
    }
  }

  // Physical key codes for the modifier keys themselves — pressing one of
  // these alone shouldn't end the recording; we wait for the real key.
  const HOTKEY_MOD_CODES = new Set([
    'ShiftLeft', 'ShiftRight', 'ControlLeft', 'ControlRight',
    'AltLeft', 'AltRight', 'MetaLeft', 'MetaRight',
  ]);

  // Map a KeyboardEvent's physical `code` to an accelerator key token the
  // backend's global-shortcut parser accepts. Uses `code` (not `key`) so a
  // held Option/Alt on macOS doesn't turn the letter into a dead char.
  // Returns null for keys we don't capture (use "Type manually" for those).
  function hotkeyKeyToken(code: string): string | null {
    if (/^Key[A-Z]$/.test(code)) return code.slice(3); // KeyK -> K
    if (/^Digit[0-9]$/.test(code)) return code.slice(5); // Digit1 -> 1
    if (/^Numpad[0-9]$/.test(code)) return code.slice(6); // Numpad1 -> 1
    if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return code; // F1..F24
    switch (code) {
      case 'Space': return 'Space';
      case 'Enter':
      case 'NumpadEnter': return 'Enter';
      case 'Tab': return 'Tab';
      case 'Backspace': return 'Backspace';
      case 'Delete': return 'Delete';
      case 'ArrowUp': return 'Up';
      case 'ArrowDown': return 'Down';
      case 'ArrowLeft': return 'Left';
      case 'ArrowRight': return 'Right';
      case 'Home': return 'Home';
      case 'End': return 'End';
      case 'PageUp': return 'PageUp';
      case 'PageDown': return 'PageDown';
      default: return null;
    }
  }

  // Capture a pressed key combo into `overlay.hotkey`. Swallows the event so
  // the keystroke doesn't leak into the page (or insert a stray character).
  function captureHotkey(e: KeyboardEvent) {
    e.preventDefault();
    e.stopPropagation();

    if (e.key === 'Escape') {
      recordingHotkey = false;
      (e.currentTarget as HTMLElement | null)?.blur();
      return;
    }
    // Modifier-only press → keep listening for the actual key.
    if (HOTKEY_MOD_CODES.has(e.code) ||
        ['Shift', 'Control', 'Alt', 'Meta'].includes(e.key)) {
      return;
    }

    const key = hotkeyKeyToken(e.code);
    if (!key) return; // unsupported key — keep waiting

    const mods: string[] = [];
    if (e.metaKey) mods.push('CmdOrCtrl');
    if (e.ctrlKey) mods.push('Ctrl');
    if (e.altKey) mods.push('Alt');
    if (e.shiftKey) mods.push('Shift');

    const isFunctionKey = /^F([1-9]|1[0-9]|2[0-4])$/.test(key);
    // A bare letter/space would hijack that key globally — require a
    // modifier, except for standalone function keys (e.g. F8).
    if (mods.length === 0 && !isFunctionKey) return;

    overlay.hotkey = [...mods, key].join('+');
    recordingHotkey = false;
    (e.currentTarget as HTMLElement | null)?.blur();
  }

  async function testComfy() {
    const url = comfyui.base_url.trim();
    if (!url) {
      comfyTestState = 'fail';
      comfyTestMsg = 'Enter a ComfyUI URL first (e.g. http://192.168.1.50:8188).';
      return;
    }
    comfyTestState = 'testing';
    comfyTestMsg = '';
    try {
      const res = await api.testComfyEndpoint({ base_url: url });
      if (res.ok) {
        comfyTestState = 'ok';
        comfyTestMsg = `Reachable (${res.latency_ms}ms). /pic and /picHQ will work for the whole family.`;
      } else {
        comfyTestState = 'fail';
        comfyTestMsg = res.error ?? 'Test failed.';
      }
    } catch (e) {
      comfyTestState = 'fail';
      comfyTestMsg = String(e).replace(/^Error:\s*/, '');
    }
  }
</script>

<main class="h-screen w-screen bg-ink-950 overflow-y-auto">
  <div class="max-w-2xl mx-auto px-8 py-10 space-y-6">
    <header class="flex items-center justify-between gap-3">
      <div class="min-w-0">
        <h1 class="text-2xl font-bold tracking-tight">Settings</h1>
        {#if versionInfo}
          <button
            type="button"
            onclick={copyVersion}
            class="group mt-1 block text-left transition-colors"
            title="Click to copy full build details"
          >
            <span class="block text-xs text-white/55 group-hover:text-white/80 transition-colors">
              v{versionInfo.version} · {prettyPlatform(versionInfo.target)}
            </span>
            {#if versionInfo.git_commit && versionInfo.git_commit !== 'unknown'}
              <span
                class="block text-[11px] text-white/30 group-hover:text-white/50 font-mono transition-colors"
              >
                Build {versionInfo.git_commit}{#if versionInfo.build_time} · {new Date(
                    versionInfo.build_time * 1000,
                  ).toLocaleDateString([], {
                    year: 'numeric',
                    month: 'short',
                    day: 'numeric',
                  })}{/if}
              </span>
            {/if}
          </button>
        {/if}
        <!-- Project links: keep the maintainer's X handle reachable from
             every install so users always have a public channel to flag
             bugs or follow releases, without us shipping an analytics or
             phone-home system. The link opens in the OS default browser
             (intercepted by the global handler in +layout.svelte). -->
        <div class="text-xs text-white/40 mt-2 flex items-center gap-2.5 flex-wrap">
          <a
            href="https://kin-ai.replit.app"
            class="text-teal-300/80 hover:text-teal-200 underline-offset-2 hover:underline transition-colors"
          >Website</a>
          <span class="text-white/20">·</span>
          <a
            href="https://github.com/Gogo6969/kinai"
            class="text-teal-300/80 hover:text-teal-200 underline-offset-2 hover:underline transition-colors"
          >GitHub</a>
          <span class="text-white/20">·</span>
          <a
            href="https://x.com/gogo6969"
            title="Follow on X"
            class="text-teal-300/80 hover:text-teal-200 underline-offset-2 hover:underline transition-colors"
          >@gogo6969</a>
        </div>
      </div>
      {#if dirty}
        <!-- Unsaved edits: the only exit button saves first, so changes
             can't be lost by clicking away. save() returns to chat itself. -->
        <button class="kin-btn-primary flex-shrink-0" disabled={saving} onclick={save}>
          {#if saving}<Loader2 size={14} class="animate-spin" /> Saving…{:else}Save &amp; close{/if}
        </button>
      {:else}
        <button class="kin-btn flex-shrink-0" onclick={() => goto('/')}>Back to chat</button>
      {/if}
    </header>

    <div class="kin-card space-y-3">
      <h2 class="font-semibold text-lg">You</h2>
      <label class="block">
        <span class="text-sm text-white/70">Your display name</span>
        <input class="kin-field mt-1" bind:value={displayName} placeholder="e.g. Alex" />
        <p class="text-xs text-white/40 mt-1">
          {#if isClient}
            How the host sees you. Your chat history stays private — other
            family members don't see your conversations.
          {:else}
            How you appear in this thread on the host UI.
          {/if}
        </p>
      </label>
      <div class="pt-2 border-t border-white/5">
        <button
          type="button"
          class="kin-btn w-full justify-between text-left"
          onclick={() => goto('/settings/memory')}
        >
          <span class="flex flex-col gap-0.5 min-w-0 items-start">
            <span class="text-sm">Memory</span>
            <span class="text-xs text-white/50">
              Review and edit what KinAI remembers about you across sessions
            </span>
          </span>
          <span class="text-white/40 text-sm">›</span>
        </button>
      </div>
      <div class="pt-2 border-t border-white/5">
        <label class="flex items-start gap-3 text-sm cursor-pointer">
          <input
            type="checkbox"
            class="mt-0.5"
            checked={autostartOn === true}
            disabled={autostartOn === null || autostartBusy}
            onchange={(e) => void toggleAutostart(e.currentTarget.checked)}
          />
          <span class="flex-1">
            <span class="font-medium text-white/90">Launch KinAI at login</span>
            <span class="block text-xs text-white/50 mt-0.5">
              KinAI starts every time you sign in to this computer.
              Per-machine setting — toggling here only affects this
              {#if app.config?.mode === 'host'}host{:else}client{/if}.
            </span>
            {#if autostartOn === null && !autostartError}
              <span class="block text-xs text-white/40 mt-1">Checking…</span>
            {/if}
            {#if autostartError}
              <span class="block text-xs text-red-300/80 mt-1">
                Couldn't read autostart state: {autostartError}
              </span>
            {/if}
          </span>
        </label>

        {#if autostartOn === true}
          <fieldset class="mt-3 ml-7 pl-3 border-l border-white/10 space-y-2">
            <legend class="text-xs text-white/50 uppercase tracking-wider">
              When KinAI auto-launches
            </legend>
            <label class="flex items-start gap-2 text-sm cursor-pointer">
              <input
                type="radio"
                name="autostart-mode"
                class="mt-0.5"
                checked={autostartMinimized === true}
                onchange={() => void setAutostartMinimized(true)}
              />
              <span>
                <span class="text-white/90">Minimize to tray</span>
                <span class="block text-xs text-white/50">
                  Window stays hidden; click the tray icon to open it.
                  Typical "background daemon" behaviour.
                </span>
              </span>
            </label>
            <label class="flex items-start gap-2 text-sm cursor-pointer">
              <input
                type="radio"
                name="autostart-mode"
                class="mt-0.5"
                checked={autostartMinimized === false}
                onchange={() => void setAutostartMinimized(false)}
              />
              <span>
                <span class="text-white/90">Show the chat window</span>
                <span class="block text-xs text-white/50">
                  Window comes up immediately, just like a manual launch.
                </span>
              </span>
            </label>
          </fieldset>
        {/if}
      </div>
    </div>

    {#if isClient}
      <div class="kin-card space-y-3">
        <h2 class="font-semibold text-lg">Host</h2>
        <p class="text-xs text-white/50">
          The host machine runs the LLM and the search tools for the whole
          family. These settings can only be changed on the host itself.
        </p>
        <div class="grid grid-cols-2 gap-3 text-sm">
          <div>
            <div class="text-[10px] uppercase text-white/40 tracking-wider mb-1">Family</div>
            <div class="text-white/80">{app.hostInfo?.family_name ?? app.config?.client.host_label ?? '—'}</div>
          </div>
          <div>
            <div class="text-[10px] uppercase text-white/40 tracking-wider mb-1">Model</div>
            <div class="text-white/80 font-mono truncate">{app.hostInfo?.host_model || '—'}</div>
          </div>
          <div>
            <div class="text-[10px] uppercase text-white/40 tracking-wider mb-1">Search engine</div>
            <div class="text-white/80 capitalize">{app.hostInfo?.host_search_engine || '—'}</div>
          </div>
          <div>
            <div class="text-[10px] uppercase text-white/40 tracking-wider mb-1">Host version</div>
            <div class="text-white/80 font-mono">{app.hostInfo?.host_version || '—'}</div>
          </div>
          <div class="col-span-2">
            <div class="text-[10px] uppercase text-white/40 tracking-wider mb-1">Image attachments</div>
            <div class="text-white/80 truncate">{app.hostInfo?.host_vision || '—'}</div>
          </div>
        </div>
        <div class="flex justify-end pt-1">
          <button class="kin-btn" type="button" onclick={() => goto('/client')}>
            Reconnect / change host →
          </button>
        </div>
      </div>
    {/if}

    {#if isHost}
    <!--
      Two LLM cards, fast + deep, share the same form layout via a
      snippet. The fast slot is the long-standing default (reachable
      as `/fast` once both are configured); the deep slot is new
      (`/deep`). Either can be paused individually via its toggle.
      When only one slot is active, plain-text messages always route
      there regardless of which slot is "fast" or "deep" — see
      `slash::route_for` server-side for the actual decision logic.
    -->
    {#snippet modelCard(
      title: string,
      subtitle: string,
      slug: 'fast' | 'deep',
      get: () => LlmSettings,
      set: (v: LlmSettings) => void
    )}
      {@const m = get()}
      <div class="kin-card space-y-4">
        <div class="flex items-center justify-between gap-3">
          <div>
            <h2 class="font-semibold text-lg">{title}</h2>
            <p class="text-xs text-white/50">{subtitle}</p>
          </div>
          <label class="flex items-center gap-2 text-sm text-white/70 cursor-pointer select-none shrink-0">
            <input
              type="checkbox"
              checked={m.enabled}
              onchange={(e) => {
                const next = { ...m, enabled: (e.currentTarget as HTMLInputElement).checked };
                set(next);
              }}
              class="accent-teal-400"
            />
            <span>{m.enabled ? 'Active' : 'Paused'}</span>
          </label>
        </div>
        {#if slug === 'fast'}
          <div class="flex items-center justify-end -mt-2">
            <button class="kin-btn" type="button" onclick={() => goto('/host')}>
              Scan & pick a backend →
            </button>
          </div>
          <p class="text-xs text-white/50 -mt-2">
            Use the scan wizard to discover providers on your machine or LAN and
            auto-fill all the fields below. Or edit manually here.
          </p>
        {/if}
        <div class="grid grid-cols-2 gap-3">
          <label class="block">
            <span class="text-sm text-white/70">Provider</span>
            <select
              class="kin-field mt-1"
              value={m.provider}
              onchange={(e) => set({ ...m, provider: (e.currentTarget as HTMLSelectElement).value })}
            >
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
              value={m.base_url}
              oninput={(e) => set({ ...m, base_url: (e.currentTarget as HTMLInputElement).value })}
              placeholder={slug === 'deep' ? '(empty = no deep model)' : ''}
            />
          </label>
        </div>
        <div class="grid grid-cols-2 gap-3">
          <label class="block">
            <span class="text-sm text-white/70">Model</span>
            <div class="flex gap-2 mt-1">
              {#if slug === 'fast' && models.length > 0}
                <select
                  class="kin-field font-mono flex-1"
                  value={m.model}
                  onchange={(e) => set({ ...m, model: (e.currentTarget as HTMLSelectElement).value })}
                >
                  {#each models as opt}
                    <option value={opt}>{opt}</option>
                  {/each}
                  {#if m.model && !models.includes(m.model)}
                    <option value={m.model}>{m.model} (custom)</option>
                  {/if}
                </select>
              {:else}
                <input
                  class="kin-field font-mono flex-1"
                  value={m.model}
                  oninput={(e) => set({ ...m, model: (e.currentTarget as HTMLInputElement).value })}
                  placeholder={slug === 'deep' ? 'e.g. qwen2.5:72b-instruct-q4_K_M' : 'llama3.1:8b'}
                />
              {/if}
              {#if slug === 'fast'}
                <button
                  class="kin-btn !px-2"
                  type="button"
                  onclick={() => refreshModels()}
                  disabled={refreshing}
                  aria-label="Refresh model list"
                  title="Refresh model list from server"
                >
                  {#if refreshing}
                    <Loader2 size={14} class="animate-spin" />
                  {:else}
                    <RefreshCw size={14} />
                  {/if}
                </button>
              {/if}
            </div>
            {#if slug === 'fast' && refreshMessage}
              <p class="text-xs text-white/50 mt-1">{refreshMessage}</p>
            {/if}
          </label>
          <label class="block">
            <span class="text-sm text-white/70">Context window</span>
            <input
              type="number"
              class="kin-field mt-1"
              value={m.context_window}
              oninput={(e) => set({ ...m, context_window: Number((e.currentTarget as HTMLInputElement).value) || 0 })}
            />
          </label>
        </div>
        <div class="grid grid-cols-2 gap-3">
          <label class="block">
            <span class="text-sm text-white/70">Temperature</span>
            <input
              type="number"
              step="0.05"
              min="0"
              max="2"
              class="kin-field mt-1"
              value={m.temperature}
              oninput={(e) => set({ ...m, temperature: Number((e.currentTarget as HTMLInputElement).value) || 0 })}
            />
          </label>
          <label class="block">
            <span class="text-sm text-white/70">Max reply tokens</span>
            <input
              type="number"
              min="0"
              class="kin-field mt-1"
              value={m.max_tokens}
              oninput={(e) => set({ ...m, max_tokens: Number((e.currentTarget as HTMLInputElement).value) || 0 })}
            />
            <p class="text-xs text-white/40 mt-1">
              <code class="bg-black/40 px-1 rounded">0</code> = auto (use full remaining
              context, recommended for reasoning models). Otherwise a hard cap.
            </p>
          </label>
        </div>
        <label class="block">
          <span class="text-sm text-white/70">API key (optional)</span>
          <input
            type="password"
            class="kin-field mt-1"
            value={m.api_key ?? ''}
            oninput={(e) => set({ ...m, api_key: (e.currentTarget as HTMLInputElement).value || null })}
          />
        </label>
        <label class="block">
          <span class="text-sm text-white/70">System addendum</span>
          <textarea
            class="kin-field mt-1 font-mono text-sm"
            rows="3"
            value={m.system_addendum}
            oninput={(e) => set({ ...m, system_addendum: (e.currentTarget as HTMLTextAreaElement).value })}
          ></textarea>
        </label>
      </div>
    {/snippet}

    {@render modelCard(
      'Fast model',
      'Your default model. Reachable as `/fast` in chat when the Deep model is also configured.',
      'fast',
      () => llm,
      (v) => (llm = v)
    )}

    {@render modelCard(
      'Deep model (optional)',
      'A second, typically larger model. Reachable as `/deep` in chat. Leave Base URL empty to disable.',
      'deep',
      () => llmDeep,
      (v) => (llmDeep = v)
    )}

    <div class="kin-card space-y-4">
      <h2 class="font-semibold text-lg">Search engine</h2>
      <p class="text-xs text-white/50 -mt-2">
        Used by <strong>every</strong> search-style tool (<code>web_search</code>
        + <code>x_search</code>) regardless of which model the host is running.
        DuckDuckGo is free / no-key but scrapes HTML and gets throttled
        (Hacker News fallback for social).
        <a class="text-teal-300" href="https://exa.ai" target="_blank" rel="noopener">Exa</a>
        is a semantic-search API purpose-built for AI agents — high quality,
        no rate-limiting. Social search restricts results to X / Bluesky /
        Mastodon / Reddit / HN / Lobsters via domain allowlist (works on every
        Exa plan).
      </p>
      <div class="grid grid-cols-2 gap-3">
        <label class="block">
          <span class="text-sm text-white/70">Engine</span>
          <select class="kin-field mt-1" bind:value={tools.search_engine}>
            <option value="duckduckgo">DuckDuckGo (free)</option>
            <option value="exa">Exa (recommended)</option>
          </select>
        </label>
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
              Looks like a long alphanumeric string — paste whatever
              <a class="text-teal-300" href="https://exa.ai/dashboard" target="_blank" rel="noopener">exa.ai/dashboard</a>
              shows you, no prefix needed. Stored only on this host.
            </p>
          </label>
        {/if}
      </div>
    </div>

    <div class="kin-card space-y-4">
      <div class="flex items-start justify-between gap-3">
        <div class="min-w-0">
          <h2 class="font-semibold text-lg">Vision (for image attachments)</h2>
          <p class="text-xs text-white/50 mt-1">
            Where image attachments get analyzed. If your chat model already
            understands images (Claude Sonnet/Opus, Gemini Pro/Flash, GPT-4o,
            llava, qwen-vl…) KinAI uses it directly. Otherwise images route
            here. Failover catches transient cloud errors.
          </p>
        </div>
        <label class="flex items-center gap-2 text-sm text-white/80 flex-shrink-0">
          <input type="checkbox" bind:checked={vision.enabled} />
          <span>Enabled</span>
        </label>
      </div>

      <div class="space-y-3">
        <div class="text-[10px] uppercase tracking-wider text-white/40">Primary endpoint</div>
        <div class="rounded-lg border border-white/10 bg-white/[0.03] px-3 py-2.5 text-xs text-white/55 space-y-1.5">
          <p>
            <span class="text-white/75 font-medium">Base URL</span> — your vision
            server's OpenAI-compatible address. KinAI appends
            <code class="text-teal-300">/v1/chat/completions</code> itself, so don't
            add it. Local example:
            <code class="text-teal-300">http://192.168.1.50:8080</code>
            (llama.cpp, LM Studio, vLLM, or Ollama on your LAN). A cloud provider's
            OpenAI-compatible base URL works too.
          </p>
          <p>
            <span class="text-white/75 font-medium">Model</span> — the exact model id
            your server reports (e.g. what <code class="text-teal-300">/v1/models</code> lists).
          </p>
          <p>
            <span class="text-white/75 font-medium">API key</span> — leave blank for a
            local server; only a cloud provider needs one.
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
            placeholder="paste your key (leave blank for local endpoints)"
            autocomplete="off"
            spellcheck="false"
          />
        </label>
        <div class="flex items-center gap-2">
          <button class="kin-btn" type="button" onclick={() => testVision('primary')} disabled={visionTestState.primary === 'testing'}>
            {#if visionTestState.primary === 'testing'}
              <Loader2 size={14} class="animate-spin" /> Testing…
            {:else}
              Test vision
            {/if}
          </button>
          {#if visionTestState.primary === 'ok'}
            <span class="text-xs text-teal-300 truncate">✓ {visionTestMsg.primary}</span>
          {:else if visionTestState.primary === 'fail'}
            <span class="text-xs text-red-300 truncate">✗ {visionTestMsg.primary}</span>
          {/if}
        </div>
      </div>

      <div class="space-y-3 border-t border-white/5 pt-4">
        <div class="flex items-center justify-between gap-2">
          <div class="text-[10px] uppercase tracking-wider text-white/40">
            Failover endpoint <span class="text-white/30 normal-case">(optional)</span>
          </div>
          <button
            type="button"
            class="kin-btn !text-xs"
            onclick={swapVisionEndpoints}
            title="Swap the primary and failover endpoints — make the backup your main model, or vice versa"
          >
            <ArrowDownUp size={13} /> Swap with primary
          </button>
        </div>
        <p class="text-xs text-white/45">
          A second endpoint KinAI tries only if the primary fails — e.g. a cloud
          backup for a local primary. Same fields as above; leave it empty to skip.
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
        <div class="flex items-center gap-2">
          <button class="kin-btn" type="button" onclick={() => testVision('failover')} disabled={visionTestState.failover === 'testing'}>
            {#if visionTestState.failover === 'testing'}
              <Loader2 size={14} class="animate-spin" /> Testing…
            {:else}
              Test vision
            {/if}
          </button>
          {#if visionTestState.failover === 'ok'}
            <span class="text-xs text-teal-300 truncate">✓ {visionTestMsg.failover}</span>
          {:else if visionTestState.failover === 'fail'}
            <span class="text-xs text-red-300 truncate">✗ {visionTestMsg.failover}</span>
          {/if}
        </div>
      </div>
    </div>
    {/if}

    {#if isHost}
    <div class="kin-card space-y-4">
      <div class="flex items-center justify-between gap-3">
        <div class="min-w-0">
          <h2 class="font-semibold text-lg">Image generation</h2>
          <p class="text-xs text-white/50">
            Optional. When set, every family member gets <code>/pic</code> and
            <code>/picHQ</code> slash commands in chat. Leave empty to disable.
          </p>
        </div>
      </div>

      <label class="block">
        <span class="text-sm text-white/70">ComfyUI URL</span>
        <input
          class="kin-field mt-1 font-mono"
          bind:value={comfyui.base_url}
          placeholder="http://192.168.1.50:8188"
          autocomplete="off"
          spellcheck="false"
        />
        <p class="text-xs text-white/50 mt-1">
          Point at any ComfyUI server reachable from this Mac — it can run on
          Linux, Windows, or another Mac (ComfyUI is cross-platform), or on the
          same machine. Default port is <code>8188</code>.
        </p>

        <details class="mt-2 text-xs text-white/60">
          <summary class="cursor-pointer text-white/70 hover:text-white select-none">
            What needs to be installed? →
          </summary>
          <div class="mt-2 space-y-2 border-l border-white/10 pl-3">
            <p>
              <strong class="text-white/80">1. ComfyUI</strong> (a recent
              version) on any machine on your network. Launch it so this Mac can
              reach it by binding to all interfaces, not just localhost:
              <code>python main.py --listen 0.0.0.0 --port 8188</code>. Without
              <code>--listen</code> it only answers on its own machine.
            </p>
            <p>
              <strong class="text-white/80">2. Four model files</strong> (the
              Z-Image family), each placed in its standard ComfyUI folder:
            </p>
            <ul class="list-disc space-y-1 pl-4">
              <li>
                <code>models/diffusion_models/z_image_turbo_bf16.safetensors</code>
                — drives <code>/pic</code> (fast, ~8 steps)
              </li>
              <li>
                <code>models/diffusion_models/z_image_bf16.safetensors</code>
                — drives <code>/picHQ</code> (higher quality, ~25 steps)
              </li>
              <li>
                <code>models/text_encoders/qwen_3_4b.safetensors</code>
                — shared text encoder
              </li>
              <li>
                <code>models/vae/ae.safetensors</code> — shared VAE
              </li>
            </ul>
            <p>
              <strong class="text-white/80">3. No custom nodes</strong> are
              required, but ComfyUI must be current enough to include the core
              nodes this workflow uses (<code>ModelSamplingAuraFlow</code>,
              <code>EmptySD3LatentImage</code>, the <code>res_multistep</code>
              sampler). If <em>Test image-gen</em> reports node errors, update
              ComfyUI.
            </p>
            <p>
              <strong class="text-white/80">4.</strong> Paste the URL above
              (e.g. <code>http://192.168.1.50:8188</code>) and hit
              <em>Test image-gen</em>.
            </p>
            <p class="text-white/40">
              Both commands share the same text encoder and VAE; only the
              diffusion model differs. Folder names above are current ComfyUI —
              older builds call <code>diffusion_models</code> →
              <code>unet</code> and <code>text_encoders</code> →
              <code>clip</code>.
            </p>
          </div>
        </details>
      </label>

      <div class="flex items-center gap-2 flex-wrap">
        <button
          class="kin-btn"
          type="button"
          onclick={testComfy}
          disabled={comfyTestState === 'testing'}
        >
          {#if comfyTestState === 'testing'}
            <Loader2 size={14} class="animate-spin" /> Testing…
          {:else}
            Test image-gen
          {/if}
        </button>
        {#if comfyTestState === 'ok'}
          <span class="text-xs text-emerald-300">✓ {comfyTestMsg}</span>
        {:else if comfyTestState === 'fail'}
          <span class="text-xs text-red-300">✗ {comfyTestMsg}</span>
        {/if}
      </div>
    </div>
    {/if}

    {#if isHost || (isClient && app.ttsSupported)}
    <div class="kin-card space-y-4">
      <div class="flex items-center justify-between gap-3">
        <div class="min-w-0">
          <h2 class="font-semibold text-lg">Voice replies</h2>
          {#if isHost}
            <p class="text-xs text-white/50">
              Optional. KinAI speaks its answers. <strong class="text-white/70">Telegram:</strong>
              every family member switches voice notes on/off for themselves by
              sending <code>/voice</code> to the bot — from their phone, from
              anywhere. <strong class="text-white/70">This Mac:</strong> replies
              in the chat window can be read aloud here. Fully local, powered by
              macOS speech synthesis.
            </p>
          {:else}
            <p class="text-xs text-white/50">
              Optional. KinAI reads its replies aloud <strong class="text-white/70">on this
              Mac</strong>, with the voices installed here — type <code>/voice</code> in the
              chat or use the options below. (Telegram voice notes are separate:
              send <code>/voice</code> to the family bot.) Fully local, powered by
              macOS speech synthesis.
            </p>
          {/if}
        </div>
        <label class="flex items-center gap-2 shrink-0 cursor-pointer">
          <input type="checkbox" bind:checked={tts.enabled} class="accent-teal-400" />
          <span class="text-sm text-white/70">Enabled</span>
        </label>
      </div>

      {#if tts.enabled}
        <div class="grid grid-cols-1 sm:grid-cols-2 gap-3">
          <label class="block">
            <span class="text-sm text-white/70">English voice</span>
            <div class="flex gap-2 mt-1">
              <select class="kin-field flex-1" bind:value={tts.voice_en}>
                {#if !ttsVoicesEn.some((v) => v.name === tts.voice_en)}
                  <option value={tts.voice_en}>{tts.voice_en} (not installed)</option>
                {/if}
                {#each ttsVoicesEn as v (v.name)}
                  <option value={v.name}>{v.name}</option>
                {/each}
              </select>
              <button
                class="kin-btn !px-3"
                type="button"
                title="Play a sample"
                onclick={() => previewTtsVoice('en')}
                disabled={ttsPreviewing !== ''}
              >
                {ttsPreviewing === 'en' ? '🔊' : '▶'}
              </button>
            </div>
          </label>
          <label class="block">
            <span class="text-sm text-white/70">German voice</span>
            <div class="flex gap-2 mt-1">
              <select class="kin-field flex-1" bind:value={tts.voice_de}>
                {#if !ttsVoicesDe.some((v) => v.name === tts.voice_de)}
                  <option value={tts.voice_de}>{tts.voice_de} (not installed)</option>
                {/if}
                {#each ttsVoicesDe as v (v.name)}
                  <option value={v.name}>{v.name}</option>
                {/each}
              </select>
              <button
                class="kin-btn !px-3"
                type="button"
                title="Play a sample"
                onclick={() => previewTtsVoice('de')}
                disabled={ttsPreviewing !== ''}
              >
                {ttsPreviewing === 'de' ? '🔊' : '▶'}
              </button>
            </div>
          </label>
        </div>
        <p class="text-xs text-white/50">
          The language of each reply is detected automatically — German
          answers use the German voice, everything else the English one.
        </p>

        <div class="space-y-1.5">
          <span class="text-sm text-white/70">In the chat window on this Mac
            <span class="text-white/40">(shortcut: type <code>/voice</code> in the chat)</span></span>
          <label class="flex items-start gap-2 cursor-pointer">
            <input
              type="radio"
              name="tts-desktop-mode"
              class="accent-teal-400 mt-0.5"
              checked={!tts.auto_speak}
              onchange={() => (tts.auto_speak = false)}
            />
            <span class="text-xs text-white/60">
              <strong class="text-white/80">Speak on demand</strong> — replies are
              read aloud only when you press the speak button.
            </span>
          </label>
          <label class="flex items-start gap-2 cursor-pointer">
            <input
              type="radio"
              name="tts-desktop-mode"
              class="accent-teal-400 mt-0.5"
              checked={tts.auto_speak}
              onchange={() => (tts.auto_speak = true)}
            />
            <span class="text-xs text-white/60">
              <strong class="text-white/80">Speak automatically</strong> — every
              finished reply is read aloud; press the reply's stop button to
              silence it.
            </span>
          </label>
        </div>

        <details class="text-xs text-white/60">
          <summary class="cursor-pointer text-white/70 hover:text-white select-none">
            Get better voices (free, highly recommended) →
          </summary>
          <div class="mt-2 space-y-2 border-l border-white/10 pl-3">
            <p>
              The default compact voices sound robotic. Apple ships
              much better <strong class="text-white/80">Premium</strong> and
              <strong class="text-white/80">Enhanced</strong> voices as free
              one-time downloads — they just aren't installed out of the box.
              On <em>this Mac</em> (the host — voices are generated here):
            </p>
            <ol class="list-decimal space-y-1 pl-4">
              <li>
                Open <strong class="text-white/80">System Settings →
                Accessibility → Spoken Content</strong>
              </li>
              <li>
                Next to <strong class="text-white/80">System Voice</strong>,
                open the dropdown and click
                <strong class="text-white/80">Manage Voices…</strong>
              </li>
              <li>
                Search the voice name — recommended:
                <code>Zoe (Premium)</code> for English,
                <code>Anna (Premium)</code> or <code>Petra (Premium)</code>
                for German — and click the
                <strong class="text-white/80">⬇ download</strong> icon
              </li>
              <li>
                Come back here and press
                <strong class="text-white/80">Refresh voices</strong> — the
                new voice appears in the dropdowns above
              </li>
            </ol>
            <p class="text-white/40">
              Downloading a voice in System Settings does NOT change your
              Mac's system voice — KinAI just uses it for speech synthesis.
            </p>
          </div>
        </details>

        <div class="flex items-center gap-2 flex-wrap">
          <button
            class="kin-btn"
            type="button"
            onclick={refreshTtsVoices}
            disabled={ttsVoicesLoading}
          >
            {#if ttsVoicesLoading}
              <Loader2 size={14} class="animate-spin" /> Refreshing…
            {:else}
              <RefreshCw size={14} /> Refresh voices
            {/if}
          </button>
          <span class="text-xs text-white/40">
            {ttsVoicesEn.length} English · {ttsVoicesDe.length} German voices installed
          </span>
        </div>

        <div class="border-t border-white/10 pt-4 space-y-3">
          <div class="flex items-center justify-between gap-3">
            <div class="min-w-0">
              <span class="text-sm font-medium text-white/80">🎙 Voice input (Telegram voice messages)</span>
              <p class="text-xs text-white/50 mt-0.5">
                Family members can <em>speak</em> to the bot: voice messages are
                transcribed on this Mac (Whisper, fully local — German and
                English auto-detected) and answered like typed questions.
                One-time model download, nothing else to install.
              </p>
            </div>
            <!-- Show the Enable toggle whenever a model is DOWNLOADED — not
                 only when STT is already `ready` (= enabled && downloaded).
                 Gating on `ready` was a catch-22: while disabled, the toggle
                 was hidden, so there was no way to turn it on. -->
            {#if stt && stt.models.some((m) => m.downloaded)}
              <label class="flex items-center gap-2 shrink-0 cursor-pointer">
                <input
                  type="checkbox"
                  checked={stt.enabled}
                  onchange={(e) => setSttEnabled(e.currentTarget.checked)}
                  class="accent-teal-400"
                />
                <span class="text-sm text-white/70">Enabled</span>
              </label>
            {/if}
          </div>

          {#if stt}
            <div class="space-y-2">
              {#each stt.models as m (m.id)}
                <div class="flex items-center gap-3 text-sm">
                  <label class="flex items-center gap-2 flex-1 min-w-0 {m.downloaded ? 'cursor-pointer' : 'opacity-80'}">
                    <input
                      type="radio"
                      name="stt-model"
                      class="accent-teal-400"
                      checked={stt.model === m.id}
                      disabled={!m.downloaded}
                      onchange={() => selectSttModel(m.id)}
                    />
                    <span class="truncate text-white/75">{m.label}</span>
                    <span class="text-xs text-white/40 shrink-0">{m.size_mb} MB</span>
                  </label>
                  {#if m.downloaded}
                    <span class="text-xs text-emerald-300 shrink-0">✓ downloaded</span>
                  {:else if sttDownloading === m.id}
                    <span class="text-xs text-teal-300 shrink-0 font-mono">
                      ↓ {sttProgress.toFixed(0)}%
                    </span>
                  {:else}
                    <button
                      class="kin-btn !py-1 !px-2.5 text-xs shrink-0"
                      type="button"
                      onclick={() => downloadStt(m.id)}
                      disabled={sttDownloading !== null}
                    >
                      Download
                    </button>
                  {/if}
                </div>
              {/each}
              {#if sttError}
                <p class="text-xs text-red-300">✗ {sttError}</p>
              {/if}
              {#if stt.ready}
                <p class="text-xs text-white/40">
                  Voice input is active — anyone in the family can hold the mic
                  button in Telegram and just talk.
                </p>
              {/if}
            </div>
          {/if}
        </div>
      {/if}
    </div>
    {/if}

    <!--
      Telegram card.
      Visible on BOTH host and client peers — each family member pairs
      their own Telegram independently. Host owner additionally sees
      the bot-token sub-card (only one bot in the family; only the
      host runs it).
    -->
    <div class="kin-card space-y-4">
      <div class="flex items-start justify-between gap-3">
        <div class="min-w-0">
          <h2 class="font-semibold text-lg">Telegram</h2>
          <p class="text-xs text-white/50">
            Chat with KinAI from your phone via Telegram. The family owner
            sets up <strong>one shared bot</strong> via @BotFather (just
            once, on the host machine). Each family member then pairs
            their own Telegram via QR code below.
          </p>
          <p class="text-xs text-white/40 mt-1">
            Every family member has their own private 1:1 chat with the
            bot — others can't see your messages. Slash commands
            (<code>/pic</code>, <code>/picHQ</code>, <code>/help</code>)
            work the same as in KinAI.
          </p>
        </div>
      </div>

      <!-- Bot-token sub-card — host owner only. Sets up the one shared
           family bot. Hidden from client peers since they don't run
           the long-poll loop. -->
      {#if isHost}
        <div class="border border-white/5 rounded-lg p-3 space-y-3 bg-white/[0.02]">
          <div class="text-xs uppercase tracking-wider text-white/40">
            Bot setup (host only)
          </div>
          <label class="block">
            <span class="text-sm text-white/70">Bot token</span>
            <input
              type="password"
              class="kin-field mt-1 font-mono"
              bind:value={telegramTokenInput}
              placeholder={telegramStatus?.bot_configured
                ? `(currently configured as @${telegramStatus.bot_username || '?'} — leave blank to keep, paste new to replace)`
                : 'Paste the token from @BotFather (e.g. 1234567890:AAH…)'}
              autocomplete="off"
              spellcheck="false"
            />
            <p class="text-xs text-white/50 mt-1">
              On your phone, message <strong>@BotFather</strong> →
              <code>/newbot</code> → follow the prompts → copy the token →
              paste here. Empty token disables Telegram entirely for the
              whole family.
            </p>
          </label>

          <div class="flex items-center gap-2 flex-wrap">
            <button
              class="kin-btn"
              type="button"
              onclick={testTelegramToken}
              disabled={telegramTestState === 'testing'}
            >
              {telegramTestState === 'testing' ? 'Testing…' : 'Test token'}
            </button>
            <button
              class="kin-btn-primary"
              type="button"
              onclick={saveTelegramToken}
              disabled={telegramSaveState === 'saving' || !telegramTokenInput.trim()}
            >
              {telegramSaveState === 'saving' ? 'Saving…' : 'Save token'}
            </button>
            {#if telegramSaveState === 'ok'}
              <span class="text-xs text-emerald-300">✓ Saved</span>
            {/if}
            {#if telegramTestState === 'ok'}
              <span class="text-xs text-emerald-300">✓ {telegramTestMsg}</span>
            {:else if telegramTestState === 'fail'}
              <span class="text-xs text-red-300">✗ {telegramTestMsg}</span>
            {/if}
          </div>
        </div>
      {/if}

      <!-- "Connect MY Telegram" sub-card — visible to everyone (host +
           client peers). On client peers, the bot username comes from
           the host's Welcome envelope (RuntimeStats.host_info); on the
           host, it comes from the local telegram_link_status. The two
           sources are merged into the `telegramBot*` derived values. -->
      {#if telegramBotConfigured}
        <div class="space-y-3">
          {#if telegramStatus?.paired}
            <div class="flex items-start justify-between gap-3">
              <div class="text-sm">
                <div class="text-white/80">
                  ✓ Your Telegram is paired
                  {#if telegramStatus.username}
                    as <span class="font-mono text-teal-300">@{telegramStatus.username}</span>
                  {:else if telegramStatus.first_name}
                    as <span class="text-teal-300">{telegramStatus.first_name}</span>
                  {/if}
                </div>
                {#if telegramStatus.paired_at}
                  <div class="text-xs text-white/40 mt-0.5">
                    since {new Date(telegramStatus.paired_at).toLocaleDateString()}
                  </div>
                {/if}
                <div class="text-xs text-white/40 mt-1">
                  Messages from you to <span class="font-mono">@{telegramBotUsername}</span>
                  land in your KinAI thread. Other family members have
                  their own separate chats.
                </div>
              </div>
              <button class="kin-btn !text-red-300/80 hover:!text-red-300" type="button" onclick={unpairTelegram}>
                Disconnect
              </button>
            </div>
          {:else if telegramPairUrl}
            <div class="space-y-3">
              <p class="text-sm text-white/70">
                Scan this code with your phone's camera (or tap the
                link). Telegram will open with a "Start" button — tap it,
                and your phone will be linked to <span class="font-mono">your</span>
                KinAI account.
              </p>
              <div class="flex items-start gap-4">
                {#if telegramPairQrDataUrl}
                  <img
                    src={telegramPairQrDataUrl}
                    alt="Telegram pairing QR"
                    class="rounded-lg border border-white/10 bg-white"
                    width="200"
                    height="200"
                  />
                {/if}
                <div class="flex-1 min-w-0 text-xs space-y-2">
                  <div class="text-white/60">Or open directly:</div>
                  <a
                    href={telegramPairUrl}
                    class="font-mono text-teal-300 underline break-all"
                    target="_blank"
                    rel="noreferrer"
                  >
                    {telegramPairUrl}
                  </a>
                  <div class="text-white/40">
                    Code expires in 10 minutes. This card auto-updates
                    once you complete the scan.
                  </div>
                </div>
              </div>
            </div>
          {:else}
            <div class="flex items-center justify-between gap-3">
              <p class="text-sm text-white/60">
                Family bot:
                <span class="font-mono text-teal-300">@{telegramBotUsername}</span>.
                Pair your phone so messages you send to it land in your
                KinAI chat.
              </p>
              <button class="kin-btn" type="button" onclick={startTelegramPair} disabled={telegramPairing}>
                {telegramPairing ? 'Generating…' : 'Connect my Telegram'}
              </button>
            </div>
          {/if}
        </div>
      {:else if isClient}
        <p class="text-xs text-white/50 italic">
          Telegram isn't set up for this family yet. Ask the host owner to
          configure it in Settings → Telegram on their machine, then come
          back here to pair your phone.
        </p>
      {/if}
    </div>

    <div class="kin-card space-y-4">
      <h2 class="font-semibold text-lg">Overlay</h2>
      <label class="block">
        <span class="text-sm text-white/70">Global hotkey</span>
        {#if hotkeyManual}
          <input
            class="kin-field mt-1 font-mono"
            bind:value={overlay.hotkey}
            autocomplete="off"
            spellcheck="false"
          />
          <p class="text-xs text-white/50 mt-1">
            Type an accelerator, e.g. <code>CmdOrCtrl+Space</code>,
            <code>Alt+Space</code>, <code>CmdOrCtrl+Shift+K</code>.
            <button
              type="button"
              class="underline hover:text-white"
              onclick={() => (hotkeyManual = false)}
            >Record instead</button>
          </p>
        {:else}
          <input
            class="kin-field mt-1 font-mono cursor-pointer {recordingHotkey ? 'ring-2 ring-sky-400/70' : ''}"
            readonly
            value={recordingHotkey ? 'Press your shortcut…  (Esc to cancel)' : overlay.hotkey}
            onfocus={() => (recordingHotkey = true)}
            onblur={() => (recordingHotkey = false)}
            onkeydown={captureHotkey}
          />
          <p class="text-xs text-white/50 mt-1">
            Click the field and press your key combination — it's captured
            automatically (needs a modifier like ⌘/⌥/⌃, or a function key).
            <button
              type="button"
              class="underline hover:text-white"
              onclick={() => { hotkeyManual = true; recordingHotkey = false; }}
            >Type manually</button>
          </p>
        {/if}
      </label>
      <div class="grid grid-cols-2 gap-3">
        <label class="block">
          <span class="text-sm text-white/70">Font size</span>
          <input type="number" min="10" max="24" class="kin-field mt-1" bind:value={overlay.font_size} />
        </label>
        <label class="block">
          <span class="text-sm text-white/70">Theme</span>
          <select class="kin-field mt-1" bind:value={theme}>
            <option value="dark">Dark</option>
            <option value="light">Light</option>
            <option value="system">System</option>
          </select>
        </label>
      </div>
      <div class="space-y-2">
        <label class="flex items-start gap-2 text-sm text-white/80">
          <input type="checkbox" class="mt-0.5" bind:checked={overlay.always_on_top} />
          <span>
            <span class="font-medium">Float above other windows</span>
            <span class="block text-xs text-white/50">The overlay stays on top of whatever app you're using.</span>
          </span>
        </label>
        <label class="flex items-start gap-2 text-sm text-white/80">
          <input type="checkbox" class="mt-0.5" bind:checked={overlay.auto_close_on_blur} />
          <span>
            <span class="font-medium">Auto-hide when clicking elsewhere</span>
            <span class="block text-xs text-white/50">
              The overlay disappears the moment it loses focus — like Spotlight.
              Turn off to keep the overlay open while you switch apps.
            </span>
          </span>
        </label>
      </div>
    </div>

    {#if isHost}
      <div class="kin-card space-y-3">
        <h2 class="font-semibold text-lg">Test a tool</h2>
        <div class="flex flex-wrap gap-2">
          <button class="kin-btn" onclick={() => testTool('datetime', '{}')}>datetime</button>
          <button class="kin-btn" onclick={() => testTool('calculator', '{"expression":"2^10"}')}>calculator</button>
          <button class="kin-btn" onclick={() => testTool('web_search', '{"query":"Anthropic"}')}>web_search</button>
          <button class="kin-btn" onclick={() => testTool('x_search', '{"query":"AI safety","mode":"keyword"}')}>social_search</button>
        </div>
        {#if testResult}
          <pre class="text-xs bg-black/40 p-3 rounded-lg whitespace-pre-wrap max-h-48 overflow-y-auto">{testResult}</pre>
        {/if}
      </div>
    {/if}

    <div class="flex justify-between items-center gap-3">
      <div class="flex items-center gap-2">
        <button class="kin-btn" type="button" onclick={checkUpdates} disabled={updateStatus === 'checking'}>
          {#if updateStatus === 'checking'}
            <Loader2 size={14} class="animate-spin" /> Checking…
          {:else}
            Check for updates
          {/if}
        </button>
        {#if updateStatus === 'uptodate'}
          <span class="text-xs text-teal-300">✓ You're on the latest version.</span>
        {:else if updateStatus === 'available' && updateInfo}
          <span class="text-xs text-teal-300 font-medium">
            🎉 Update v{updateInfo.version} available — install via the system dialog.
          </span>
        {:else if updateStatus === 'error'}
          <span class="text-xs text-red-300">Could not reach the update server.</span>
        {/if}
      </div>
      <div class="flex items-center gap-2">
        {#if saveStatus === 'saved'}
          <span class="text-sm text-emerald-300 font-medium animate-fade-in flex items-center gap-1">
            <span class="inline-block w-2 h-2 rounded-full bg-emerald-400"></span>
            Saved
          </span>
        {:else if saveStatus === 'error'}
          <span class="text-sm text-red-300 max-w-[280px] truncate" title={saveError}>
            ✗ {saveError || 'Failed to save'}
          </span>
        {/if}
        <button class="kin-btn-primary" disabled={saving} onclick={save}>
          {#if saving}<Loader2 size={14} class="animate-spin" /> Saving…{:else}Save changes{/if}
        </button>
      </div>
    </div>
  </div>
</main>
