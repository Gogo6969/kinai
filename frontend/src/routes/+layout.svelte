<script lang="ts">
  import '../app.css';
  import { onMount, onDestroy } from 'svelte';
  import { events } from '$lib/api';
  import { app } from '$lib/stores/app.svelte';
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { open as shellOpen } from '@tauri-apps/plugin-shell';
  import { save as saveDialog } from '@tauri-apps/plugin-dialog';
  import { writeFile } from '@tauri-apps/plugin-fs';

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

  /** Fetch the image bytes and offer the user a Save dialog. The host
   *  serves `/v1/pic/<uuid>.png` over plain HTTP on the LAN, so the
   *  fetch is reachable from every paired family device.
   *
   *  Errors surface as a visible toast so the user knows what
   *  happened instead of having to open devtools. */
  async function downloadImage(url: string) {
    let stage = 'fetch';
    try {
      stage = 'fetch';
      const resp = await fetch(url);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const buf = new Uint8Array(await resp.arrayBuffer());
      const defaultName =
        url.split('/').pop()?.split('?')[0] || `kinai-image-${Date.now()}.png`;
      stage = 'dialog';
      const path = await saveDialog({
        defaultPath: defaultName,
        filters: [{ name: 'Images', extensions: ['png', 'jpg', 'jpeg', 'webp'] }],
      });
      if (!path) return; // user cancelled
      stage = 'write';
      await writeFile(path, buf);
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

  onMount(() => {
    if (!page.url.pathname.startsWith('/overlay')) {
      void app.load().then(() => app.startListening());
    }
    (async () => {
      cleanups.push(await events.onOpenRoute((r) => goto(r)));
    })();
    document.addEventListener('click', interceptExternalLinks, { capture: true });
    cleanups.push(() =>
      document.removeEventListener('click', interceptExternalLinks, { capture: true } as any)
    );
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
</script>

{@render children?.()}

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
