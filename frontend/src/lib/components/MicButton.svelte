<script lang="ts">
  import { Mic, MicOff } from '@lucide/svelte';
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
  let rec: SpeechRecognitionLike | null = null;
  let accumulated = '';
  // Set when the user releases the button so the onend handler knows to
  // fire `onsend` (vs. a programmatic / error-driven stop, where we
  // shouldn't auto-send).
  let releaseRequested = false;

  const supported = speechRecognitionAvailable();

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
      lastError = `Speech recognition error: ${e.error}`;
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
      lastError = `Couldn't start recognition: ${String(err)}`;
    }
  }

  function stopRecording(send: boolean) {
    releaseRequested = send;
    rec?.stop();
  }

  function onPointerDown(e: PointerEvent) {
    if (!supported) {
      lastError =
        "This Mac's WebView doesn't expose speech recognition. macOS 13+ required, or use the keyboard.";
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
  <div class="text-xs text-red-300 px-2">{lastError}</div>
{/if}
