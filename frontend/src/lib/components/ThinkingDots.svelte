<script lang="ts">
  let {
    label = 'Thinking',
    size = 'sm',
  }: { label?: string; size?: 'sm' | 'md' } = $props();

  const dotClass = $derived(
    size === 'md' ? 'w-2 h-2' : 'w-1.5 h-1.5'
  );
</script>

<span class="inline-flex items-center gap-1.5 text-xs text-white/60">
  <span class="inline-flex items-center gap-1">
    <span class="kin-dot {dotClass}" style="animation-delay: 0ms"></span>
    <span class="kin-dot {dotClass}" style="animation-delay: 160ms"></span>
    <span class="kin-dot {dotClass}" style="animation-delay: 320ms"></span>
  </span>
  {#if label}
    <span class="font-medium tracking-tight">{label}…</span>
  {/if}
</span>

<style>
  .kin-dot {
    display: inline-block;
    background-color: rgb(0 212 200); /* teal-500 */
    border-radius: 9999px;
    animation: kin-wave 1.1s ease-in-out infinite;
    transform-origin: center;
    will-change: transform, opacity;
  }
  @keyframes kin-wave {
    0%,
    80%,
    100% {
      transform: translateY(0) scale(0.85);
      opacity: 0.5;
    }
    40% {
      transform: translateY(-3px) scale(1);
      opacity: 1;
    }
  }
  :global(html.light) .kin-dot {
    background-color: rgb(8 145 178); /* teal-600 — darker for light bg */
  }
</style>
