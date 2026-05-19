<script lang="ts">
  import { api, events } from '$lib/api';
  import { renderMarkdown } from '$lib/markdown';
  import { onMount, onDestroy } from 'svelte';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { LogicalSize } from '@tauri-apps/api/dpi';
  import Logo from './Logo.svelte';
  import MicButton from './MicButton.svelte';
  import ThinkingDots from './ThinkingDots.svelte';
  import ThinkingPanel from './ThinkingPanel.svelte';
  import ToolPill from './ToolPill.svelte';

  let input = $state('');
  let busy = $state(false);
  let activeThreadId = $state<string | null>(null);
  let streamingContent = $state('');
  let reasoningContent = $state('');
  let tools = $state<{ name: string; ok?: boolean }[]>([]);
  let inputEl: HTMLTextAreaElement | undefined = $state();
  let containerEl: HTMLDivElement | undefined = $state();
  let currentClientId = $state<string | null>(null);
  const cleanups: Array<() => void> = [];

  const renderedHtml = $derived(streamingContent ? renderMarkdown(streamingContent) : '');

  // Resize the overlay window to fit its content. Tauri's overlay window
  // starts at ~80px; without this, anything past the input (reasoning,
  // streamed answer) gets clipped.
  $effect(() => {
    void streamingContent;
    void reasoningContent;
    void tools.length;
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

      // Refresh prefs each time the overlay re-opens (the user may have
      // toggled the setting while the overlay was hidden).
      const unlistenOverlay = await events.onOverlayFocus(async () => {
        try {
          const fresh = await api.getConfig();
          autoCloseOnBlur = fresh.overlay.auto_close_on_blur;
          alwaysOnTop = fresh.overlay.always_on_top;
        } catch {}
      });
      cleanups.push(unlistenOverlay);

      const threads = await api.listThreads();
      activeThreadId = threads[0]?.id ?? (await api.createThread('Quick chat')).id;
      inputEl?.focus();

      cleanups.push(
        await events.onOverlayFocus(() => {
          inputEl?.focus();
          inputEl?.select();
        })
      );
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
    })();
  });

  onDestroy(() => cleanups.forEach((u) => u()));

  async function submit(e: Event) {
    e.preventDefault();
    await send(input);
  }

  async function send(text: string) {
    if (!activeThreadId || !text.trim() || busy) return;
    busy = true;
    streamingContent = '';
    reasoningContent = '';
    tools = [];
    currentClientId = crypto.randomUUID();
    input = '';
    try {
      await api.sendMessage({
        thread_id: activeThreadId,
        content: text,
        client_msg_id: currentClientId,
      });
    } catch (err) {
      streamingContent = `Error: ${err}`;
      busy = false;
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
        <ThinkingDots
          label={tools.length > 0 ? tools[tools.length - 1].name.replace('_', ' ') : 'Thinking'}
        />
      {/if}
    </form>
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
