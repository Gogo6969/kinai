<script lang="ts">
  /**
   * Changelog modal — pops once per upgrade.
   *
   * Mounted by `+layout.svelte`. On first paint it calls
   * `get_changelog_payload` to see if the running binary version has a
   * matching `## [x.y.z]` entry in the embedded CHANGELOG.md AND the
   * user hasn't acknowledged that version yet. If both conditions hold,
   * the modal opens with the rendered markdown.
   *
   * Closing the modal calls `mark_changelog_seen`, stamping the current
   * version into `AppConfig.last_seen_changelog_version` so the modal
   * stays closed until the next upgrade.
   *
   * The "What's new" view also remains openable on demand via the
   * "What's new" link in Settings → About (added in a follow-up; the
   * `open()` method is exposed for that use case).
   */
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { renderMarkdown } from '$lib/markdown';

  interface ChangelogPayload {
    version: string;
    markdown: string | null;
    should_show: boolean;
  }

  let payload = $state<ChangelogPayload | null>(null);
  let visible = $state(false);
  let dismissing = $state(false);

  const renderedHtml = $derived.by(() => {
    if (!payload?.markdown) return '';
    // Drop the leading `## [version]` heading line — we already render
    // it as the modal's title, so showing it twice looks silly.
    const body = payload.markdown.replace(/^## \[.*?\][^\n]*\n+/, '');
    return renderMarkdown(body);
  });

  async function refresh() {
    try {
      payload = await invoke<ChangelogPayload>('get_changelog_payload');
      if (payload.should_show) {
        // Stagger the open by ~600ms so the chat shell finishes painting
        // first — sliding a modal over a half-loaded screen feels jank.
        setTimeout(() => {
          visible = true;
        }, 600);
      }
    } catch (err) {
      console.warn('changelog payload fetch failed', err);
    }
  }

  async function dismiss() {
    if (dismissing) return;
    dismissing = true;
    try {
      await invoke('mark_changelog_seen');
    } catch (err) {
      console.warn('mark_changelog_seen failed', err);
    } finally {
      visible = false;
      dismissing = false;
    }
  }

  function onKeyDown(e: KeyboardEvent) {
    if (e.key === 'Escape' && visible) {
      e.preventDefault();
      void dismiss();
    }
  }

  /** Public hook for the "What's new" link in Settings → About. */
  export function open() {
    if (payload?.markdown) visible = true;
  }

  onMount(() => {
    void refresh();
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  });
</script>

{#if visible && payload}
  <!-- Backdrop (div, not button: HTML doesn't let buttons nest, and the
       card has its own buttons). Click outside the card to dismiss. -->
  <div
    class="fixed inset-0 z-[100] bg-black/80 backdrop-blur-sm flex items-center justify-center p-4 animate-fade-in"
    role="presentation"
    onclick={dismiss}
    onkeydown={(e) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        void dismiss();
      }
    }}
  >
    <!-- Card. Stop click propagation so clicks inside don't dismiss. -->
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="changelog-title"
      class="kin-card kin-changelog-card max-w-2xl w-full max-h-[85vh] flex flex-col p-0 cursor-default text-left"
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => e.stopPropagation()}
      tabindex="-1"
    >
      <header class="px-5 py-4 border-b border-white/10 flex items-start justify-between gap-3 shrink-0">
        <div class="min-w-0">
          <div class="text-xs uppercase tracking-wider text-white/40">
            What's new
          </div>
          <h2 id="changelog-title" class="text-lg font-semibold text-white truncate">
            KinAI {payload.version}
          </h2>
        </div>
        <button
          type="button"
          class="kin-btn !px-2 text-white/60 hover:text-white"
          onclick={dismiss}
          aria-label="Close"
        >
          ✕
        </button>
      </header>

      <div class="px-5 py-4 overflow-y-auto kin-changelog-body">
        {@html renderedHtml}
      </div>

      <footer class="px-5 py-3 border-t border-white/10 flex items-center justify-between gap-3 shrink-0">
        <span class="text-xs text-white/40">
          You'll see these notes again next time KinAI updates.
        </span>
        <button
          type="button"
          class="kin-btn-primary"
          onclick={dismiss}
          disabled={dismissing}
        >
          {dismissing ? 'Saving…' : 'Got it'}
        </button>
      </footer>
    </div>
  </div>
{/if}

<style>
  /* The modal card must be OPAQUE. `kin-card` is only `bg-white/5` and
     leans on the backdrop's `backdrop-blur-sm` to stay legible — but
     WebKitGTK (Linux Tauri) doesn't render backdrop-blur, so the chat
     bled through and the release notes were unreadable. Force a solid,
     theme-aware surface (ink-900 dark / white light); `!important` beats
     kin-card's translucent background regardless of stylesheet order. */
  :global(.kin-changelog-card) {
    background-color: #0f172a !important;
  }
  :global(html.light .kin-changelog-card) {
    background-color: #ffffff !important;
  }

  /* Tighter typography for the markdown body — the chat-bubble defaults
     are too airy when applied to a long-form release note. */
  :global(.kin-changelog-body h3) {
    @apply text-base font-semibold text-white mt-4 mb-1;
  }
  :global(.kin-changelog-body h2) {
    @apply text-base font-semibold text-white/90 mt-4 mb-2;
  }
  :global(.kin-changelog-body p) {
    @apply text-sm text-white/80 leading-relaxed mb-2;
  }
  :global(.kin-changelog-body ul) {
    @apply list-disc list-outside pl-5 text-sm text-white/80 space-y-1 mb-3;
  }
  :global(.kin-changelog-body li) {
    @apply leading-relaxed;
  }
  :global(.kin-changelog-body strong) {
    @apply text-white font-semibold;
  }
  :global(.kin-changelog-body code) {
    @apply text-teal-300 font-mono text-xs bg-white/5 px-1 py-0.5 rounded;
  }
  :global(.kin-changelog-body a) {
    @apply text-teal-300 underline underline-offset-2;
  }
</style>
