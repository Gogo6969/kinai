<script lang="ts">
  import { app } from '$lib/stores/app.svelte';
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { Check, RefreshCw, Trash2, Undo2, Flag } from '@lucide/svelte';

  onMount(() => {
    void app.loadReports();
  });

  const open = $derived(app.reports.filter((r) => !r.reviewed_at));
  const done = $derived(app.reports.filter((r) => r.reviewed_at));

  function when(iso: string): string {
    const d = new Date(iso);
    return `${d.toLocaleDateString()} ${d.toLocaleTimeString([], {
      hour: '2-digit',
      minute: '2-digit',
    })}`;
  }
</script>

<main class="h-screen w-screen bg-ink-950 overflow-y-auto">
  <div class="max-w-3xl mx-auto px-8 py-10 space-y-6">
    <header class="flex items-center justify-between gap-3">
      <div class="flex items-center gap-2">
        <button class="kin-btn" onclick={() => goto('/')} title="Back to chat">←</button>
        <h1 class="text-2xl font-bold tracking-tight">Reported answers</h1>
      </div>
      <div class="flex gap-2">
        {#if done.length > 0}
          <button
            class="kin-btn text-white/60"
            onclick={() => app.deleteReviewedReports()}
            title="Delete every report already marked reviewed"
          >
            <Trash2 size={14} /> Clear reviewed ({done.length})
          </button>
        {/if}
        <button class="kin-btn" onclick={() => app.loadReports()}>
          <RefreshCw size={14} /> Refresh
        </button>
      </div>
    </header>

    <p class="text-sm text-white/50 -mt-2">
      Answers your family flagged as wrong or confusing. Each report shows
      only the question and answer the person chose to send — the rest of
      their conversation stays private to them.
    </p>

    {#if app.reports.length === 0}
      <div class="kin-card text-center text-white/50">
        Nothing reported. When someone taps <span class="text-white/70">report</span>
        under an answer, it lands here.
      </div>
    {/if}

    {#snippet card(r: (typeof app.reports)[number])}
      <div class="kin-card space-y-3 {r.reviewed_at ? 'opacity-60' : ''}">
        <div class="flex items-start justify-between gap-3">
          <div class="text-sm">
            <span class="font-semibold">{r.reporter}</span>
            <span class="text-white/40"> · {when(r.created_at)}</span>
            {#if r.model}
              <span class="text-white/40"> · </span>
              <span class="font-mono text-xs text-teal-300/70">
                {r.slot ? r.slot + ' · ' : ''}{r.model}
              </span>
            {/if}
          </div>
          <div class="flex gap-2 shrink-0">
            {#if r.reviewed_at}
              <button
                class="kin-btn-ghost text-white/60"
                onclick={() => app.setReportReviewed(r.id, false)}
                title="Move back to open"
              >
                <Undo2 size={14} /> Reopen
              </button>
            {:else}
              <button
                class="kin-btn-ghost text-teal-300/80 hover:text-teal-300"
                onclick={() => app.setReportReviewed(r.id, true)}
                title="Mark as handled"
              >
                <Check size={14} /> Reviewed
              </button>
            {/if}
            <button
              class="kin-btn-ghost text-red-300/70 hover:text-red-300"
              onclick={() => app.deleteReport(r.id)}
              title="Delete this report"
            >
              <Trash2 size={14} />
            </button>
          </div>
        </div>

        <div>
          <div class="text-[11px] uppercase tracking-wider text-white/40 mb-1">Question</div>
          <div class="rounded-lg bg-teal-500/10 border border-teal-300/20 px-3 py-2 text-sm whitespace-pre-wrap break-words
                      {r.question ? '' : 'text-white/40 italic'}">
            {r.question || '(question not captured)'}
          </div>
        </div>

        <div>
          <div class="text-[11px] uppercase tracking-wider text-white/40 mb-1">
            Answer that didn't work
          </div>
          <!-- Rendered as PLAIN TEXT on purpose. This is the only place
               peer-authored content reaches the host's window, and
               markdown would let a reported "answer" pull remote images
               (a read receipt / IP beacon) or plant clickable open and
               download affordances. Formatting isn't worth that. -->
          <div class="rounded-lg bg-white/5 border border-white/10 px-3 py-2 text-sm
                      whitespace-pre-wrap break-words">
            {r.answer}
          </div>
        </div>
      </div>
    {/snippet}

    {#if open.length > 0}
      <section class="space-y-2">
        <h2 class="text-sm font-semibold text-amber-300/90 flex items-center gap-1.5">
          <Flag size={14} /> Needs a look ({open.length})
        </h2>
        {#each open as r (r.id)}
          {@render card(r)}
        {/each}
      </section>
    {/if}

    {#if done.length > 0}
      <section class="space-y-2">
        <h2 class="text-sm font-semibold text-white/50">Reviewed ({done.length})</h2>
        {#each done as r (r.id)}
          {@render card(r)}
        {/each}
      </section>
    {/if}
  </div>
</main>
