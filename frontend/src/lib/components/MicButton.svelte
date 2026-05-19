<script lang="ts">
  import { Mic, MicOff } from '@lucide/svelte';
  import { open as shellOpen } from '@tauri-apps/plugin-shell';
  import {
    createRecognition,
    speechRecognitionAvailable,
    type SpeechRecognitionLike,
  } from '$lib/voice';

  let {
    /** Called with the current transcript on every interim/final result. */
    ontranscript,
    /** Called when the user releases the button (push-to-talk) with the
     *  finalized transcript. If non-empty, callers usually fire their
     *  message-send here. */
    onsend,
    /** Compact mode hides the text label. */
    compact = false,
  }: {
    ontranscript: (text: string) => void;
    onsend?: (text: string) => void;
    compact?: boolean;
  } = $props();

  let listening = $state(false);
  let lastError = $state<string | null>(null);
  let permissionDenied = $state(false);
  let errorClearTimer: ReturnType<typeof setTimeout> | undefined;
  let rec: SpeechRecognitionLike | null = null;
  let accumulated = '';
  // Set when the user releases the button so the onend handler knows to
  // fire `onsend` (vs. a programmatic / error-driven stop, where we
  // shouldn't auto-send).
  let releaseRequested = false;

  const supported = speechRecognitionAvailable();

  /** Translate Web Speech API errors into actionable human text. The
   *  raw error codes (`not-allowed`, `service-not-allowed`, `audio-
   *  capture`, `network`, …) mean nothing to a normal user.
   *
   *  Note on macOS: even with the Microphone toggle ON, KinAI ALSO
   *  needs Speech Recognition enabled separately (Privacy → Speech
   *  Recognition). The Web Speech API can't distinguish which one is
   *  missing — both surface as `not-allowed` — so the message has to
   *  mention both, and we expose two deep-link buttons. */
  function humanizeError(code: string): string {
    switch (code) {
      case 'not-allowed':
      case 'service-not-allowed':
        permissionDenied = true;
        return "Speech recognition is blocked. macOS needs BOTH \"Microphone\" AND \"Speech Recognition\" enabled for KinAI in Privacy & Security — enabling just one is not enough. Open both panes below, toggle KinAI on, then quit and relaunch KinAI.";
      case 'audio-capture':
        return "No microphone detected. Plug one in (or enable your built-in mic) and try again.";
      case 'network':
        return "Speech recognition is offline. Try again in a moment.";
      default:
        return `Speech recognition error: ${code}`;
    }
  }

  function setError(msg: string | null) {
    lastError = msg;
    if (errorClearTimer) clearTimeout(errorClearTimer);
    if (msg) {
      // Auto-clear after 8 seconds so the error doesn't stick around
      // forever in the overlay's tiny UI.
      errorClearTimer = setTimeout(() => {
        lastError = null;
      }, 8000);
    }
  }

  /** True when the renderer is on macOS — gates the Privacy-pane
   *  deep-link buttons (Windows / Linux have no equivalent URL). */
  const isMac =
    typeof navigator !== 'undefined' && /mac/i.test(navigator.platform);

  /** Open a macOS Privacy pane via the `x-apple.systempreferences:` URL
   *  scheme. On Ventura+ the older `com.apple.preference.security`
   *  bundle path is silently routed by System Settings to the new
   *  `com.apple.settings.PrivacySecurity.extension`, so the same URL
   *  works on both Monterey and Sonoma/Sequoia. Logging real errors
   *  surfaces the Tauri shell-plugin scope rejection we saw before
   *  (plugins.shell.open regex must whitelist the scheme). */
  async function openPrivacyPane(anchor: 'Microphone' | 'SpeechRecognition') {
    if (!isMac) return;
    const url = `x-apple.systempreferences:com.apple.preference.security?Privacy_${anchor}`;
    try {
      await shellOpen(url);
    } catch (err) {
      console.warn(`shellOpen(${url}) failed:`, err);
      setError(
        `Couldn't open System Settings automatically. Open it yourself: System Settings → Privacy & Security → ${anchor === 'Microphone' ? 'Microphone' : 'Speech Recognition'} and toggle KinAI on.`
      );
    }
  }

  async function openMicSettings() {
    await openPrivacyPane('Microphone');
  }

  async function openSpeechRecognitionSettings() {
    await openPrivacyPane('SpeechRecognition');
  }

  function startRecording() {
    if (!supported || listening) return;
    rec = createRecognition();
    if (!rec) return;
    accumulated = '';
    lastError = null;
    releaseRequested = false;

    rec.onresult = (e) => {
      let interim = '';
      let final_ = '';
      for (let i = e.resultIndex; i < e.results.length; i++) {
        const r = e.results[i];
        if (r.isFinal) final_ += r[0].transcript;
        else interim += r[0].transcript;
      }
      if (final_) accumulated += final_;
      const composed = (accumulated + interim).trim();
      if (composed) ontranscript(composed);
    };
    rec.onerror = (e) => {
      if (e.error === 'no-speech' || e.error === 'aborted') return;
      setError(humanizeError(e.error));
    };
    rec.onend = () => {
      listening = false;
      const composed = accumulated.trim();
      if (releaseRequested && composed && onsend) {
        onsend(composed);
      }
      releaseRequested = false;
    };
    rec.onstart = () => {
      listening = true;
    };

    try {
      rec.start();
    } catch (err) {
      setError(`Couldn't start recognition: ${String(err)}`);
    }
  }

  function stopRecording(send: boolean) {
    releaseRequested = send;
    rec?.stop();
  }

  function onPointerDown(e: PointerEvent) {
    if (!supported) {
      setError(
        "This Mac's WebView doesn't expose speech recognition. macOS 13+ required, or use the keyboard."
      );
      return;
    }
    // Capture the pointer so we still get pointerup even if the cursor
    // drags off the button mid-talk.
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    startRecording();
  }

  function onPointerUp(_e: PointerEvent) {
    if (listening) stopRecording(true);
  }

  function onPointerCancel(_e: PointerEvent) {
    if (listening) stopRecording(false);
  }

  // Keyboard accessibility: Space / Enter while focused = push-to-talk.
  function onKeyDown(e: KeyboardEvent) {
    if (listening) return;
    if (e.key === ' ' || e.key === 'Enter') {
      e.preventDefault();
      startRecording();
    }
  }
  function onKeyUp(e: KeyboardEvent) {
    if (!listening) return;
    if (e.key === ' ' || e.key === 'Enter') {
      e.preventDefault();
      stopRecording(true);
    }
  }
</script>

<button
  type="button"
  onpointerdown={onPointerDown}
  onpointerup={onPointerUp}
  onpointercancel={onPointerCancel}
  onkeydown={onKeyDown}
  onkeyup={onKeyUp}
  class="kin-btn !px-2 relative select-none
         {listening ? '!bg-red-500/20 !border-red-400/40' : ''}
         {!supported ? 'opacity-50' : ''}"
  title={!supported
    ? 'Speech recognition not available in this WebView'
    : listening
      ? 'Release to send'
      : 'Hold to speak'}
  aria-label="Hold to speak"
>
  {#if listening}
    <Mic size={14} class="text-red-300" />
    {#if !compact}
      <span class="text-red-300">listening — release to send</span>
    {/if}
    <span class="absolute -top-1 -right-1 w-2 h-2 rounded-full bg-red-400 animate-pulse"></span>
  {:else if !supported}
    <MicOff size={14} />
  {:else}
    <Mic size={14} />
  {/if}
</button>

{#if lastError}
  <div class="text-xs text-red-300 px-2 max-w-[420px] space-y-1.5">
    <div class="flex items-start gap-2">
      <span class="flex-1 leading-relaxed">{lastError}</span>
      <button
        type="button"
        class="text-red-300/60 hover:text-red-300 shrink-0"
        onclick={() => setError(null)}
        title="Dismiss"
        aria-label="Dismiss error"
      >
        ✕
      </button>
    </div>
    {#if permissionDenied && isMac}
      <div class="flex items-center gap-2 flex-wrap">
        <button
          type="button"
          class="kin-btn !text-xs !px-2 !py-0.5 shrink-0"
          onclick={openMicSettings}
          title="Open System Settings → Privacy & Security → Microphone"
        >
          Open Mic settings
        </button>
        <button
          type="button"
          class="kin-btn !text-xs !px-2 !py-0.5 shrink-0"
          onclick={openSpeechRecognitionSettings}
          title="Open System Settings → Privacy & Security → Speech Recognition"
        >
          Open Speech Recognition settings
        </button>
      </div>
    {/if}
  </div>
{/if}
