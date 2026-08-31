<script lang="ts">
  import '../app.css';
  // DEV-only: installs an in-memory Tauri IPC mock when running in a plain
  // browser (no-op inside the real app; eliminated from production builds).
  import '$lib/dev-tauri-mock';
  import { onMount, onDestroy } from 'svelte';
  import { events } from '$lib/api';
  import { app } from '$lib/stores/app.svelte';
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { open as shellOpen } from '@tauri-apps/plugin-shell';
  import { save as saveDialog } from '@tauri-apps/plugin-dialog';
  import { invoke } from '@tauri-apps/api/core';
  import ChangelogModal from '$lib/components/ChangelogModal.svelte';

  let { children } = $props();
  const cleanups: Array<() => void> = [];

  // Route every external http(s) link through the OS default browser instead
  // of letting it navigate inside the Tauri webview (which would hijack the
  // app shell). Capture-phase listener so it pre-empts SvelteKit's anchor
  // handling too.
  function interceptExternalLinks(e: MouseEvent) {
    if (e.defaultPrevented || e.metaKey || e.shiftKey || e.altKey || e.button !== 0) return;
    const target = e.target as HTMLElement | null;
    if (!target) return;

    // Image-action buttons (`[data-kin-action]`) ship next to every
    // chat image. Tauri's WebView blocks the built-in right-click
    // → "Save Image As…" / "Open Image in New Window", so we expose
    // explicit Open + Download buttons that route through the shell
    // and fs plugins.
    const actionBtn = target.closest('[data-kin-action]') as HTMLButtonElement | null;
    if (actionBtn) {
      const action = actionBtn.getAttribute('data-kin-action');
      const url = actionBtn.getAttribute('data-kin-url');
      if (action && url) {
        e.preventDefault();
        e.stopPropagation();
        if (action === 'open') {
          shellOpen(url).catch((err) => console.warn('shell open failed', err));
        } else if (action === 'download') {
          void downloadImage(url);
        }
        return;
      }
    }

    const anchor = target.closest('a') as HTMLAnchorElement | null;
    if (!anchor) return;
    const href = anchor.getAttribute('href');
    if (!href) return;
    if (!/^https?:\/\//i.test(href)) return;
    e.preventDefault();
    e.stopPropagation();
    shellOpen(href).catch((err) => console.warn('shell open failed', err));
  }

  /** Save the image at `url` to a path the user picks via Save dialog.
   *  The actual HTTP fetch + file write are done in Rust (Tauri IPC
   *  command) — the webview's `fetch()` can't reach
   *  `http://192.168.1.x:4847` on macOS because WebKit's ATS blocks
   *  plain HTTP requests from JavaScript, even though <img src> loads
   *  the same URL fine via a different code path. Rust's reqwest has
   *  no such restriction. */
  async function downloadImage(url: string) {
    let stage = 'dialog';
    try {
      const defaultName =
        url.split('/').pop()?.split('?')[0] || `kinai-image-${Date.now()}.png`;
      const path = await saveDialog({
        defaultPath: defaultName,
        filters: [{ name: 'Images', extensions: ['png', 'jpg', 'jpeg', 'webp'] }],
      });
      if (!path) return; // user cancelled
      stage = 'download';
      await invoke('download_url_to_path', { url, destPath: path });
      showToast(`✓ Saved to ${path.split('/').pop() || path}`);
    } catch (err) {
      const msg = String(err).replace(/^Error:\s*/, '');
      console.warn('image download failed at', stage, err);
      showToast(`✗ Download failed (${stage}): ${msg}`, 6000);
    }
  }

  // Brief auto-fade toast at the top of the window. Used by the
  // download flow above and any future user-facing acknowledgements.
  let toastMsg = $state('');
  let toastTimer: ReturnType<typeof setTimeout> | undefined;
  function showToast(msg: string, ms = 3000) {
    toastMsg = msg;
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toastMsg = ''), ms);
  }

  // Components that want to surface a toast dispatch a `kin-toast`
  // CustomEvent on window with `{ msg, ms? }` in detail. Keeps the
  // toast machinery in one place — components don't import showToast
  // directly. Used by Sidebar's newChat error path, image download
  // success/failure, etc.
  function onKinToast(e: Event) {
    const ce = e as CustomEvent<{ msg?: string; ms?: number }>;
    if (ce.detail?.msg) showToast(ce.detail.msg, ce.detail.ms ?? 3000);
  }

  onMount(() => {
    if (!page.url.pathname.startsWith('/overlay')) {
      void app.load().then(() => app.startListening());
    }
    (async () => {
      cleanups.push(await events.onOpenRoute((r) => goto(r)));
    })();
    document.addEventListener('click', interceptExternalLinks, { capture: true });
    window.addEventListener('kin-toast', onKinToast);
    cleanups.push(() =>
      document.removeEventListener('click', interceptExternalLinks, { capture: true } as any)
    );
    cleanups.push(() => window.removeEventListener('kin-toast', onKinToast));
  });

  onDestroy(() => {
    app.stopListening();
    cleanups.forEach((u) => u());
  });

  // Apply theme to <html>. "system" follows the OS preference.
  $effect(() => {
    const theme = app.config?.theme ?? 'dark';
    const root = document.documentElement;
    let useLight = false;
    if (theme === 'light') useLight = true;
    else if (theme === 'system') {
      useLight = !window.matchMedia('(prefers-color-scheme: dark)').matches;
    }
    root.classList.toggle('light', useLight);
    root.classList.toggle('dark', !useLight);
  });

  // Apply overlay font size as a CSS variable + global font scale.
  $effect(() => {
    const px = app.config?.overlay?.font_size ?? 14;
    document.documentElement.style.setProperty('--kin-overlay-font-size', `${px}px`);
    // Apply to the entire UI as well — KinAI's "font size" affects all text.
    document.documentElement.style.fontSize = `${px}px`;
  });

  /** A text/URL drag over an input, textarea, or contenteditable keeps its
   *  native default (insert the text). File drags never qualify — a file's
   *  default is navigation, the thing the window guard exists to stop. */
  function nativeTextDropAllowed(e: DragEvent): boolean {
    if (e.dataTransfer?.types?.includes('Files')) return false;
    const t = e.target;
    return (
      t instanceof HTMLElement &&
      (t.isContentEditable || t.tagName === 'INPUT' || t.tagName === 'TEXTAREA')
    );
  }
</script>

<!-- With Tauri's dragDropEnabled off (the composer needs HTML5 drag events),
     the webview's DEFAULT drop behavior applies wherever the app doesn't
     intercept — and that default is "navigate to the dropped file/URL",
     which replaced the whole UI with the dropped page. Swallow drag/drop
     at the window level on every route; drop zones that WANT the drop
     (the chat composer) run first and take it. One exception: a TEXT drag
     onto an editable field keeps its native default (inserting the text) —
     only editable targets, and never file drags, get that pass, so
     navigation stays impossible. -->
<svelte:window
  ondragover={(e) => {
    if (nativeTextDropAllowed(e)) return;
    e.preventDefault();
    // "none" keeps the cursor honest on routes where a drop is a no-op
    // (Settings, setup). The chat window's own dragover handler runs
    // after this one and flips it back to "copy" for file drags there.
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'none';
  }}
  ondrop={(e) => {
    if (nativeTextDropAllowed(e)) return;
    e.preventDefault();
  }}
/>

{@render children?.()}

<!--
  Changelog modal — self-mounting. Decides internally whether to open
  based on `get_changelog_payload`. Skip on the overlay route (a small
  popover, the modal would dwarf it).
-->
{#if !page.url.pathname.startsWith('/overlay')}
  <ChangelogModal />
{/if}

{#if toastMsg}
  <div
    class="fixed top-4 left-1/2 -translate-x-1/2 z-50 px-4 py-2 rounded-lg
           bg-ink-900/95 border border-white/15 shadow-2xl text-sm text-white
           max-w-[90vw] truncate animate-fade-in"
    role="status"
    aria-live="polite"
  >
    {toastMsg}
  </div>
{/if}
