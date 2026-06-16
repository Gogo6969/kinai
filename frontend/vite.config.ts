import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [sveltekit()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: 'ws',
          host,
          port: 1421,
        }
      : undefined,
    watch: { ignored: ['**/src-tauri/**', '**/target/**'] },
  },
  envPrefix: ['VITE_', 'TAURI_ENV_*'],
  build: {
    // The runtime is a Tauri WKWebView (macOS 11+, our minimum) /
    // WebView2 (Windows) — both modern, so we emit modern JS instead of
    // down-leveling. esbuild 0.28 dropped its old (buggy) transforms for
    // lowering destructuring to Safari 13/14, so an older target now
    // hard-errors; targeting safari16 tells esbuild "don't lower, the
    // webview handles it" — which is true and yields smaller output.
    target:
      process.env.TAURI_ENV_PLATFORM === 'windows' ? 'chrome105' : 'safari16',
    minify: !process.env.TAURI_ENV_DEBUG ? 'esbuild' : false,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
  },
});
