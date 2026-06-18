<script lang="ts">
  import { app } from '$lib/stores/app.svelte';
  import Sidebar from '$lib/components/Sidebar.svelte';
  import ChatWindow from '$lib/components/ChatWindow.svelte';
  import Logo from '$lib/components/Logo.svelte';
  import UpdateBanner from '$lib/components/UpdateBanner.svelte';
  import { goto } from '$app/navigation';
  import { Home, LogIn } from '@lucide/svelte';
</script>

{#if app.config === null}
  <div class="h-screen w-screen grid place-items-center text-white/50">
    Loading KinAI…
  </div>
{:else if app.config.mode === 'unconfigured'}
  <main class="h-screen w-screen grid place-items-center bg-ink-950 p-8">
    <div class="max-w-3xl w-full">
      <header class="text-center mb-10">
        <div class="inline-flex items-center gap-3 mb-4">
          <Logo size={44} />
          <span class="text-3xl font-semibold tracking-tight">Welcome to KinAI</span>
        </div>
        <p class="text-white/60 text-lg">
          Your family's private AI — running on your own hardware.
        </p>
      </header>

      <div class="grid grid-cols-2 gap-5">
        <button
          class="kin-glass rounded-2xl p-7 text-left hover:bg-white/10 transition-colors group"
          onclick={() => goto('/host')}
        >
          <div class="mb-3 text-teal-300 group-hover:scale-110 transition-transform"><Home size={40} strokeWidth={1.5} /></div>
          <h2 class="font-semibold text-lg mb-2">Host KinAI here</h2>
          <p class="text-sm text-white/60 leading-relaxed">
            This machine becomes the family's KinAI server. Auto-detects Ollama,
            LM Studio, vLLM, and llama.cpp on localhost — or
            <span class="text-teal-300">configure manually</span>
            to point at any OpenAI-compatible server on your network
            (Olares One, Minisforum, another Mac, etc.).
          </p>
        </button>
        <button
          class="kin-glass rounded-2xl p-7 text-left hover:bg-white/10 transition-colors group"
          onclick={() => goto('/client')}
        >
          <div class="mb-3 text-teal-300 group-hover:scale-110 transition-transform"><LogIn size={40} strokeWidth={1.5} /></div>
          <h2 class="font-semibold text-lg mb-2">Join an existing host</h2>
          <p class="text-sm text-white/60 leading-relaxed">
            Already have a KinAI host at home? Paste the invite link, type the
            6-character code, or pick from the network.
          </p>
        </button>
      </div>

      <p class="text-center text-xs text-white/40 mt-8">
        100% local · 100% private · 100% free · MIT licensed
      </p>
    </div>
  </main>
{:else}
  <main class="h-screen w-screen bg-ink-950 flex flex-col">
    <UpdateBanner />
    <div class="flex-1 flex min-h-0">
      <Sidebar />
      <div class="flex-1 min-w-0">
        <ChatWindow />
      </div>
    </div>
  </main>
{/if}
