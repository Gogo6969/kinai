import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vite';

const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  // SvelteKit 3 takes its options here; `svelte.config.js` is gone.
  plugins: [
    sveltekit({
      preprocess: vitePreprocess(),
      adapter: adapter({
        pages: 'build',
        assets: 'build',
        fallback: 'index.html',
        precompress: false,
        strict: false,
      }),
      // Deprecated in SvelteKit 3 in favour of `#lib` subpath imports, but
      // still honoured. Kept so the move to 3 does not also rewrite every
      // import in the app; that rename can be its own change.
      alias: {
        $lib: 'src/lib',
      },
      compilerOptions: {
        runes: true,
      },
    }),
  ],
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
    // down-leveling: the webview handles it, and the output is smaller.
    target:
      process.env.TAURI_ENV_PLATFORM === 'windows' ? 'chrome105' : 'safari16',
    // Vite 8 minifies with Oxc; esbuild is no longer part of the build.
    minify: !process.env.TAURI_ENV_DEBUG,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
  },
});
