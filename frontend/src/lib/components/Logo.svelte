<script lang="ts">
  let { size = 28, glow = true }: { size?: number; glow?: boolean } = $props();
</script>

<svg
  xmlns="http://www.w3.org/2000/svg"
  viewBox="0 0 64 64"
  width={size}
  height={size}
  aria-label="KinAI"
  role="img"
>
  <defs>
    <linearGradient id="kin-house" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0%" stop-color="#00D4C8" />
      <stop offset="100%" stop-color="#1E3A8A" />
    </linearGradient>
    {#if glow}
      <radialGradient id="kin-glow" cx="0.5" cy="0.5" r="0.5">
        <stop offset="0%" stop-color="#00D4C8" stop-opacity="0.9" />
        <stop offset="100%" stop-color="#00D4C8" stop-opacity="0" />
      </radialGradient>
    {/if}
  </defs>
  <!-- house outline -->
  <path
    d="M32 6 L58 26 L58 54 a4 4 0 0 1 -4 4 L10 58 a4 4 0 0 1 -4 -4 L6 26 Z"
    fill="none"
    stroke="url(#kin-house)"
    stroke-width="3.5"
    stroke-linejoin="round"
  />
  <!-- neural nodes -->
  <g fill="#00D4C8">
    <circle cx="22" cy="34" r="2.5" />
    <circle cx="42" cy="34" r="2.5" />
    <circle cx="32" cy="44" r="2.5" />
    <circle cx="32" cy="26" r="2.5" />
  </g>
  <!-- connections -->
  <g stroke="#00D4C8" stroke-width="1.5" stroke-linecap="round" opacity="0.75">
    <line x1="22" y1="34" x2="32" y2="26" />
    <line x1="42" y1="34" x2="32" y2="26" />
    <line x1="22" y1="34" x2="32" y2="44" />
    <line x1="42" y1="34" x2="32" y2="44" />
    <line x1="22" y1="34" x2="42" y2="34" />
  </g>
  {#if glow}
    <circle cx="32" cy="35" r="22" fill="url(#kin-glow)" opacity="0.15" />
  {/if}
</svg>
