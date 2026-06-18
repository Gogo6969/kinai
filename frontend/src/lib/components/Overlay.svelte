<script lang="ts">
  import { api, events, type Attachment } from '$lib/api';
  import { renderMarkdown } from '$lib/markdown';
  import { onMount, onDestroy } from 'svelte';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { LogicalSize } from '@tauri-apps/api/dpi';
  import Logo from './Logo.svelte';
  import MicButton from './MicButton.svelte';
  import ThinkingDots from './ThinkingDots.svelte';
  import ThinkingPanel from './ThinkingPanel.svelte';
  import ToolPill from './ToolPill.svelte';
  import { Download, X, Square } from '@lucide/svelte';

  let input = $state('');
  let busy = $state(false);
  let activeThreadId = $state<string | null>(null);
  let streamingContent = $state('');
  let reasoningContent = $state('');
  let tools = $state<{ name: string; ok?: boolean }[]>([]);
  let inputEl: HTMLTextAreaElement | undefined = $state();
  let containerEl: HTMLDivElement | undefined = $state();
  let currentClientId = $state<string | null>(null);
  /** Image / PDF attachments staged for the next overlay send. Mirrors
   *  the main ChatWindow's `pendingAttachments` semantics so a user can
   *  paste a screenshot into the quick-chat (Cmd+Space) overlay and
   *  have it ride along with the prompt, same as in the main window. */
  let pendingAttachments = $state<Attachment[]>([]);
  let attachmentError = $state('');
  // Update affordance. The main update banner lives only in the main
  // window — someone who only ever uses this quick-chat bar would never
  // be told about (or able to install) a new version. So the overlay
  // listens for the same update event and shows its own install pill.
  let updateInfo = $state<{ version: string; current: string; source: 'host' | 'github' } | null>(null);
  let updateInstalling = $state(false);
  let updateProgress = $state<number | null>(null);
  let updateError = $state('');
  let lastUpdateCheck = 0;
  const cleanups: Array<() => void> = [];

  // Same limits as ChatWindow so the user sees a consistent rejection
  // message whether they paste in the overlay or the main window.
  const MAX_BYTES = 25 * 1024 * 1024;
  const SUPPORTED_MIMES = /^(application\/pdf|image\/(png|jpe?g|gif|webp))$/i;

  function fileToDataUrl(file: File): Promise<string> {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(reader.result as string);
      reader.onerror = () => reject(reader.error ?? new Error('read failed'));
      reader.readAsDataURL(file);
    });
  }

  async function ingestFiles(files: FileList | File[]) {
    attachmentError = '';
    const accepted: Attachment[] = [];
    for (const file of Array.from(files)) {
      if (file.size > MAX_BYTES) {
        attachmentError = `${file.name || 'attachment'} is too large (max 25 MB).`;
        continue;
      }
      if (file.type && !SUPPORTED_MIMES.test(file.type)) {
        attachmentError = `${file.name || 'attachment'}: only PDFs and images are supported.`;
        continue;
      }
      try {
        const dataUrl = await fileToDataUrl(file);
        accepted.push({
          kind: file.type.startsWith('image/') ? 'image' : 'file',
          mime: file.type || null,
          name: file.name || null,
          data_url: dataUrl,
        });
      } catch (e) {
        attachmentError = `Couldn't read ${file.name || 'attachment'}: ${String(e)}`;
      }
    }
    if (accepted.length) {
      pendingAttachments = [...pendingAttachments, ...accepted];
    }
  }

  /** Handle Cmd+V of an image (or PDF) into the overlay's textarea.
   *  Without this, the paste produced literal text content for an image
   *  blob (i.e. nothing useful) — the main ChatWindow had this support
   *  but I missed adding it to the overlay when the quick-chat surface
   *  was introduced. */
  function onPaste(e: ClipboardEvent) {
    const items = e.clipboardData?.items;
    if (!items) return;
    const files: File[] = [];
    for (const item of Array.from(items)) {
      if (item.kind === 'file') {
        const f = item.getAsFile();
        if (f) files.push(f);
      }
    }
    if (files.length > 0) {
      e.preventDefault();
      void ingestFiles(files);
    }
  }

  function removeAttachment(idx: number) {
    pendingAttachments = pendingAttachments.filter((_, i) => i !== idx);
  }

  const renderedHtml = $derived(streamingContent ? renderMarkdown(streamingContent) : '');

  // Resize the overlay window to fit its content. Tauri's overlay window
  // starts at ~80px; without this, anything past the input (reasoning,
  // streamed answer) gets clipped.
  $effect(() => {
    void streamingContent;
    void reasoningContent;
    void tools.length;
    // Resize when attachment count changes too — pasting an image
    // adds a thumbnail row beneath the input that the window needs
    // to fit, otherwise the preview is clipped.
    void pendingAttachments.length;
    // …and when the update pill shows/hides.
    void updateInfo;
    void updateInstalling;
    if (!containerEl) return;
    queueMicrotask(() => {
      const h = Math.min(Math.max(containerEl!.offsetHeight + 24, 96), 720);
      getCurrentWindow().setSize(new LogicalSize(720, h)).catch(() => {});
    });
  });

  // Cache the user's overlay preferences so the blur handler doesn't have
  // to hit the Tauri command layer for every focus change.
  let autoCloseOnBlur = $state(true);
  let alwaysOnTop = $state(true);

  onMount(() => {
    (async () => {
      const cfg = await api.getConfig();
      autoCloseOnBlur = cfg.overlay.auto_close_on_blur;
      alwaysOnTop = cfg.overlay.always_on_top;

      const win = getCurrentWindow();
      const unlistenFocus = await win.onFocusChanged(({ payload: focused }) => {
        if (!focused && autoCloseOnBlur) {
          win.hide().catch(() => {});
        }
      });
      cleanups.push(unlistenFocus);

      // One handler per overlay re-open: focus + select the input, refresh
      // prefs (the user may have toggled them while hidden), and re-check
      // for updates (throttled) so an overlay-only user still hears about
      // new versions. (Previously this was two separate onOverlayFocus
      // subscriptions doing duplicate work.)
      const unlistenOverlay = await events.onOverlayFocus(async () => {
        inputEl?.focus();
        inputEl?.select();
        try {
          const fresh = await api.getConfig();
          autoCloseOnBlur = fresh.overlay.auto_close_on_blur;
          alwaysOnTop = fresh.overlay.always_on_top;
        } catch {}
        void maybeCheckUpdate();
      });
      cleanups.push(unlistenOverlay);

      const threads = await api.listThreads();
      activeThreadId = threads[0]?.id ?? (await api.createThread('Quick chat')).id;
      inputEl?.focus();
      cleanups.push(
        await events.onToken(({ client_msg_id, delta }) => {
          if (client_msg_id === currentClientId) {
            streamingContent += delta;
          }
        })
      );
      cleanups.push(
        await events.onReasoning(({ client_msg_id, delta }) => {
          if (client_msg_id === currentClientId) {
            reasoningContent += delta;
          }
        })
      );
      cleanups.push(
        await events.onTool(({ client_msg_id, event }) => {
          if (client_msg_id !== currentClientId) return;
          if (event.kind === 'Started') tools = [...tools, { name: event.name }];
          if (event.kind === 'Finished') {
            tools = tools.map((t) =>
              t.name === event.name && t.ok === undefined ? { ...t, ok: event.ok } : t
            );
          }
        })
      );
      cleanups.push(
        await events.onAssistantDone(({ client_msg_id, message }) => {
          if (client_msg_id !== currentClientId) return;
          // Slash commands (/help, /pic, /picHQ) and any non-streaming
          // reply path never fire `onToken`, so `streamingContent` is
          // still empty when AssistantDone arrives. The final content
          // lives in `message.content`. Backfill from there so the
          // overlay actually shows the reply instead of going blank.
          // For streaming replies `streamingContent` already has the
          // text — we only fill it when empty to avoid clobbering a
          // partial stream if AssistantDone races the last token.
          if (!streamingContent && message?.content) {
            streamingContent = message.content;
          }
          busy = false;
        })
      );
      cleanups.push(
        await events.onClientStatus((s) => {
          // If the link to the host drops mid-turn, the AssistantDone that
          // clears `busy` never arrives — don't spin "Thinking…" forever.
          if (!s.connected && busy) {
            busy = false;
            currentClientId = null;
            if (!streamingContent) {
              streamingContent = '⚠️ Lost connection to the host — try again.';
            }
          }
        })
      );
      cleanups.push(
        await events.onError((msg) => {
          // Backend errors surfaced via the error channel (not the send
          // promise) would otherwise leave the bar stuck spinning.
          if (!busy) return;
          busy = false;
          currentClientId = null;
          streamingContent = `Error: ${msg}`;
        })
      );
      cleanups.push(
        await events.onUpdateAvailable((u) => {
          updateInfo = u;
        })
      );
      cleanups.push(
        await events.onUpdateProgress(({ progress }) => {
          updateProgress = progress;
        })
      );
      // Kick an initial check now that the listener is registered (the
      // periodic backend check may have already fired before mount).
      void maybeCheckUpdate();
    })();
  });

  onDestroy(() => cleanups.forEach((u) => u()));

  async function submit(e: Event) {
    e.preventDefault();
    await send(input);
  }

  // `/newchat` → returns the trailing question ('' if bare), or null if
  // the text isn't the command. Case-insensitive; `/newchatx` is normal text.
  function parseNewchat(text: string): string | null {
    const m = /^\/newchat(?:\s+([\s\S]*))?$/i.exec(text.trim());
    return m ? (m[1] ?? '').trim() : null;
  }

  async function send(text: string) {
    // /newchat — start a fresh conversation (new thread, no prior
    // context). Earlier messages won't be used; persistent memory stays.
    const fresh = parseNewchat(text);
    if (fresh !== null) {
      // Must have a NEW thread before routing the question, otherwise the
      // "fresh" question would go into the OLD thread (with its context) —
      // the exact thing /newchat exists to prevent. Abort on failure.
      try {
        const t = await api.createThread('Quick chat');
        activeThreadId = t.id;
      } catch (e) {
        streamingContent = `Couldn't start a new chat: ${String(e).replace(/^Error:\s*/, '')}`;
        return;
      }
      // Reset the in-flight turn id so a late token/done from the previous
      // turn can't write into the new (empty) overlay.
      currentClientId = null;
      reasoningContent = '';
      tools = [];
      pendingAttachments = [];
      attachmentError = '';
      input = '';
      if (fresh) {
        streamingContent = '';
        await send(fresh); // answer the trailing question in the fresh thread
      } else {
        streamingContent = "🆕 Started a new chat — earlier messages won't be used as context.";
      }
      return;
    }

    // Allow sending an attachment with no text (e.g. "what's in this
    // image?" UX where the user just drags / pastes a screenshot and
    // hits Enter without typing).
    if (!activeThreadId || busy) return;
    if (!text.trim() && pendingAttachments.length === 0) return;
    busy = true;
    streamingContent = '';
    reasoningContent = '';
    tools = [];
    currentClientId = crypto.randomUUID();
    const attachmentsForTurn = pendingAttachments;
    input = '';
    pendingAttachments = [];
    attachmentError = '';
    try {
      await api.sendMessage({
        thread_id: activeThreadId,
        content: text,
        client_msg_id: currentClientId,
        attachments: attachmentsForTurn,
      });
    } catch (err) {
      streamingContent = `Error: ${err}`;
      busy = false;
    }
  }

  // Stop the in-flight turn (Stop button). Clears the local busy state
  // immediately so the overlay is responsive, and tells the backend to
  // cancel — which, in client mode, forwards a StopGeneration to the
  // host so a runaway loop is actually aborted (not just visually).
  async function stop() {
    const id = currentClientId;
    busy = false;
    currentClientId = null;
    try {
      await api.stopGeneration(id ?? undefined);
    } catch (e) {
      console.warn('overlay stop failed', e);
    }
  }

  // Ask the backend to re-check for updates, at most once a minute so
  // rapid open/close of the overlay doesn't spam the host/GitHub. A hit
  // re-emits `kinai://update-available`, which our listener turns into
  // the pill.
  async function maybeCheckUpdate() {
    const now = Date.now();
    if (now - lastUpdateCheck < 60_000) return;
    lastUpdateCheck = now;
    try {
      await api.checkUpdates();
    } catch {}
  }

  async function installUpdate() {
    if (updateInstalling) return;
    updateInstalling = true;
    updateError = '';
    updateProgress = 0;
    try {
      // Relaunches the app on success — this normally never returns.
      await api.installUpdate();
    } catch (e) {
      updateInstalling = false;
      updateProgress = null;
      updateError = String(e).replace(/^Error:\s*/, '');
    }
  }

  function dismiss() {
    api.toggleOverlay();
  }
</script>

<svelte:window
  onkeydown={(e) => {
    if (e.key === 'Escape') dismiss();
  }}
/>

<div class="h-screen w-screen grid place-items-start justify-center pt-2">
  <div bind:this={containerEl} class="kin-glass rounded-2xl w-[720px] animate-slide-down">
    <form onsubmit={submit} class="flex items-center gap-3 px-5 py-3">
      <Logo size={26} glow={false} />
      <textarea
        bind:this={inputEl}
        bind:value={input}
        rows="1"
        placeholder="Ask KinAI…"
        class="kin-input resize-none overflow-y-auto text-base leading-snug py-1"
        style="max-height: 200px;"
        oninput={(e) => {
          const el = e.currentTarget as HTMLTextAreaElement;
          el.style.height = 'auto';
          el.style.height = Math.min(el.scrollHeight, 200) + 'px';
        }}
        onkeydown={(e) => {
          if (e.key === 'Enter' && !e.shiftKey) submit(e);
        }}
        onpaste={onPaste}
      ></textarea>
      <MicButton
        compact
        ontranscript={(t) => {
          input = t;
          if (inputEl) {
            inputEl.style.height = 'auto';
            inputEl.style.height = Math.min(inputEl.scrollHeight, 200) + 'px';
          }
        }}
        onsend={(t) => send(t)}
      />
      {#if busy}
        <div class="flex items-center justify-between gap-3">
          <ThinkingDots
            label={tools.length > 0 ? tools[tools.length - 1].name.replace('_', ' ') : 'Thinking'}
          />
          <button
            type="button"
            onclick={stop}
            class="inline-flex items-center gap-1.5 shrink-0 rounded-md px-2.5 py-1 text-xs
                   text-white/70 hover:text-white bg-white/5 hover:bg-white/10 transition-colors"
            title="Stop generating"
            aria-label="Stop generating"
          >
            <Square size={12} /> Stop
          </button>
        </div>
      {/if}
    </form>
    {#if updateInfo}
      <button
        type="button"
        onclick={installUpdate}
        disabled={updateInstalling}
        class="w-full flex items-center gap-2 border-t border-teal-500/20 bg-teal-500/5
               hover:bg-teal-500/10 px-5 py-2 text-xs text-teal-200 transition-colors
               disabled:cursor-default disabled:hover:bg-teal-500/5"
        title={updateInstalling ? 'Installing…' : `Install KinAI v${updateInfo.version} and restart`}
      >
        <Download size={13} class="flex-shrink-0" />
        {#if updateInstalling}
          <span>Updating{updateProgress !== null ? ` · ${updateProgress.toFixed(0)}%` : '…'}</span>
        {:else}
          <span class="font-medium">Update available — v{updateInfo.version}</span>
          <span class="text-teal-200/60">· click to install &amp; restart</span>
        {/if}
        {#if updateError}
          <span class="text-red-300 ml-auto truncate">{updateError}</span>
        {/if}
      </button>
    {/if}
    {#if pendingAttachments.length > 0 || attachmentError}
      <div class="px-5 pb-2 flex flex-wrap items-center gap-2">
        {#each pendingAttachments as att, idx}
          <div class="relative group">
            {#if att.kind === 'image' && att.data_url}
              <img
                src={att.data_url}
                alt={att.name ?? 'pasted image'}
                class="h-12 w-12 rounded-md object-cover border border-white/10"
              />
            {:else}
              <div class="h-12 px-2 flex items-center rounded-md bg-white/5 border border-white/10 text-xs text-white/70 max-w-[160px] truncate">
                {att.name ?? 'file'}
              </div>
            {/if}
            <button
              type="button"
              class="absolute -top-1.5 -right-1.5 w-4 h-4 rounded-full bg-ink-950 border border-white/20 grid place-items-center text-white/70 hover:text-white opacity-0 group-hover:opacity-100 transition-opacity"
              onclick={() => removeAttachment(idx)}
              aria-label="Remove attachment"
            >
              <X size={10} />
            </button>
          </div>
        {/each}
        {#if attachmentError}
          <span class="text-xs text-red-300/80">{attachmentError}</span>
        {/if}
      </div>
    {/if}
    {#if tools.length}
      <div class="px-5 pb-2 flex flex-wrap gap-2">
        {#each tools as t}
          <ToolPill name={t.name} ok={t.ok} />
        {/each}
      </div>
    {/if}
    {#if reasoningContent}
      <div class="border-t border-white/5 px-5 pt-3 pb-1">
        <ThinkingPanel text={reasoningContent} live={!streamingContent} />
      </div>
    {/if}
    {#if streamingContent}
      <div class="border-t border-white/5 px-5 py-3 max-h-96 overflow-y-auto kin-prose text-ink-50">
        {@html renderedHtml}
      </div>
    {/if}
  </div>
</div>
