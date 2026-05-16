<script lang="ts">
  import '../app.css';
  import { onMount, onDestroy } from 'svelte';
  import { events } from '$lib/api';
  import { app } from '$lib/stores/app.svelte';
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { open as shellOpen } from '@tauri-apps/plugin-shell';

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
    const anchor = target.closest('a') as HTMLAnchorElement | null;
    if (!anchor) return;
    const href = anchor.getAttribute('href');
    if (!href) return;
    if (!/^https?:\/\//i.test(href)) return;
    e.preventDefault();
    e.stopPropagation();
    shellOpen(href).catch((err) => console.warn('shell open failed', err));
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
