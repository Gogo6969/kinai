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
   *  capture`, `network`, …) mean nothing to a normal user. */
  function humanizeError(code: string): string {
    switch (code) {
      case 'not-allowed':
      case 'service-not-allowed':
        permissionDenied = true;
        return "Microphone access is blocked. Open System Settings → Privacy & Security → Microphone and turn KinAI on.";
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

  /** Open macOS System Settings to the Microphone privacy pane so the
   *  user can flip KinAI on with one click. On Windows / Linux there's
   *  no equivalent deep-link; do nothing there. */
  async function openMicSettings() {
    try {
      const isMac =
        typeof navigator !== 'undefined' &&
        /mac/i.test(navigator.platform);
      if (isMac) {
        // macOS 13+ deep-link to Privacy → Microphone pane.
        await shellOpen('x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone');
      }
    } catch {}
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
  <div class="text-xs text-red-300 px-2 flex items-center gap-2 max-w-[260px]">
    <span class="flex-1">{lastError}</span>
    {#if permissionDenied}
      <button
        type="button"
        class="kin-btn !text-xs !px-2 !py-0.5 shrink-0"
        onclick={openMicSettings}
        title="Open System Settings → Privacy & Security → Microphone"
      >
        Fix
      </button>
    {/if}
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
{/if}
