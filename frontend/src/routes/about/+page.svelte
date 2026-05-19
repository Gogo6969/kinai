<script lang="ts">
  import Logo from '$lib/components/Logo.svelte';
  import { api } from '$lib/api';
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';

  let info = $state<{
    version: string;
    build_time: number;
    git_commit: string;
    target: string;
    repository: string;
  } | null>(null);

  onMount(() => {
    api.kinaiVersion().then((v) => (info = v)).catch(() => {});
  });
</script>

<main class="h-screen w-screen bg-ink-950 overflow-y-auto">
  <div class="max-w-2xl mx-auto px-8 py-14 text-center space-y-6">
    <Logo size={64} />
    <h1 class="text-3xl font-bold tracking-tight">
      KinAI {info ? `v${info.version}` : ''}
    </h1>
    <p class="text-white/70 text-lg">Your family's private AI — running at home.</p>
    <p class="text-sm text-white/50 max-w-md mx-auto">
      MIT-licensed, fully open-source, zero telemetry. Your conversations never
      leave your hardware.
    </p>
    <div class="flex justify-center gap-3 pt-4 flex-wrap">
      <a
        class="kin-btn"
        href="https://kin-ai.replit.app"
        target="_blank"
        rel="noreferrer"
        title="kin-ai.replit.app"
      >
        Website
      </a>
      {#if info?.repository}
        <a class="kin-btn" href={info.repository} target="_blank" rel="noreferrer">
          GitHub
        </a>
      {/if}
      <button class="kin-btn-primary" onclick={() => goto('/')}>Back</button>
    </div>
    <p class="text-xs text-white/40 pt-2">
      Project home: <a
        href="https://kin-ai.replit.app"
        target="_blank"
        rel="noreferrer"
        class="text-teal-300 underline underline-offset-2">kin-ai.replit.app</a>
    </p>
  </div>
</main>
