<script lang="ts">
  import { api, events, type UserFact } from '$lib/api';
  import { goto } from '$app/navigation';
  import { onMount, onDestroy } from 'svelte';
  import { ChevronLeft, Plus, Trash2, Pencil, Check, X, Brain, AlertTriangle } from '@lucide/svelte';

  let facts = $state<UserFact[]>([]);
  let loading = $state(true);
  let error = $state('');

  // Inline-edit state. Only one row editable at a time.
  let editingId = $state<string | null>(null);
  let editKey = $state('');
  let editValue = $state('');

  // New-fact entry row at the top of the list.
  let newKey = $state('');
  let newValue = $state('');
  let adding = $state(false);

  let unlistenUpdated: (() => void) | null = null;

  async function load() {
    loading = true;
    error = '';
    try {
      facts = await api.listUserFacts();
    } catch (e) {
      error = `Couldn't load memory: ${String(e).replace(/^Error:\s*/, '')}`;
    } finally {
      loading = false;
    }
  }

  async function add() {
    const key = newKey.trim();
    const value = newValue.trim();
    if (!key || !value) return;
    adding = true;
    try {
      await api.saveUserFact({ key, value });
      newKey = '';
      newValue = '';
      await load();
    } catch (e) {
      error = `Couldn't save: ${String(e).replace(/^Error:\s*/, '')}`;
    } finally {
      adding = false;
    }
  }

  function startEdit(f: UserFact) {
    editingId = f.id;
    editKey = f.key;
    editValue = f.value;
  }

  function cancelEdit() {
    editingId = null;
    editKey = '';
    editValue = '';
  }

  async function commitEdit(original: UserFact) {
    const key = editKey.trim();
    const value = editValue.trim();
    if (!key || !value) {
      cancelEdit();
      return;
    }
    try {
      // If the key changed, delete the old row first so we don't leave
      // an orphan under the old name. Save under the new key.
      if (key !== original.key) {
        await api.deleteUserFact(original.id);
      }
      await api.saveUserFact({ key, value });
      cancelEdit();
      await load();
    } catch (e) {
      error = `Couldn't update: ${String(e).replace(/^Error:\s*/, '')}`;
    }
  }

  async function remove(f: UserFact) {
    if (!confirm(`Forget "${f.key}: ${f.value}"?`)) return;
    try {
      await api.deleteUserFact(f.id);
      await load();
    } catch (e) {
      error = `Couldn't delete: ${String(e).replace(/^Error:\s*/, '')}`;
    }
  }

  async function clearAll() {
    if (
      !confirm(
        `Delete ALL ${facts.length} stored fact${facts.length === 1 ? '' : 's'}? This can't be undone.`
      )
    )
      return;
    try {
      await api.clearUserFacts();
      await load();
    } catch (e) {
      error = `Couldn't clear: ${String(e).replace(/^Error:\s*/, '')}`;
    }
  }

  function formatRelative(iso: string): string {
    const t = new Date(iso).getTime();
    const diff = (Date.now() - t) / 1000;
    if (diff < 60) return 'just now';
    if (diff < 3600) return `${Math.floor(diff / 60)} min ago`;
    if (diff < 86400) return `${Math.floor(diff / 3600)} h ago`;
    if (diff < 2592000) return `${Math.floor(diff / 86400)} d ago`;
    return new Date(iso).toLocaleDateString();
  }

  function sourceLabel(source: string): string {
    switch (source) {
      case 'tool':
        return 'said in chat';
      case 'extractor':
        return 'inferred';
      case 'manual':
        return 'you typed';
      default:
        return source;
    }
  }

  function sourceClass(source: string): string {
    switch (source) {
      case 'tool':
        return 'bg-teal-500/15 text-teal-300';
      case 'extractor':
        return 'bg-amber-500/15 text-amber-300';
      case 'manual':
        return 'bg-violet-500/15 text-violet-300';
      default:
        return 'bg-white/10 text-white/60';
    }
  }

  onMount(async () => {
    await load();
    unlistenUpdated = await events.onUserFactsUpdated(() => {
      void load();
    });
  });
  onDestroy(() => {
    unlistenUpdated?.();
  });
</script>

<div class="max-w-3xl mx-auto p-6 pb-12">
  <button
    class="kin-btn-ghost mb-4 inline-flex items-center gap-1"
    onclick={() => goto('/settings')}
  >
    <ChevronLeft size={14} /> Back to settings
  </button>

  <header class="mb-6">
    <h1 class="text-2xl font-semibold flex items-center gap-2">
      <Brain size={22} class="text-teal-300" /> Memory
    </h1>
    <p class="text-sm text-white/60 mt-2 leading-relaxed">
      Persistent facts KinAI knows about you. These survive across every conversation —
      the model can read them at the start of any chat, and uses them naturally when
      relevant.
    </p>
    <p class="text-sm text-white/60 mt-2 leading-relaxed">
      Facts get here three ways: the model calls
      <code class="bg-white/10 px-1 rounded text-xs">remember()</code> when you state
      something stable ("I live in Berlin"), a background extractor scans messages for
      anything fact-shaped, or you add one manually below. Edit or delete any of them
      anytime — nothing is locked.
    </p>
  </header>

  {#if error}
    <div
      class="mb-4 rounded-lg border border-red-400/30 bg-red-400/10 text-red-200 px-3 py-2 text-sm flex items-start gap-2"
    >
      <AlertTriangle size={16} class="shrink-0 mt-0.5" />
      <span>{error}</span>
    </div>
  {/if}

  <section class="kin-card p-4 mb-4">
    <h2 class="text-sm font-medium text-white/80 mb-3">Add a fact</h2>
    <div class="flex gap-2 flex-wrap">
      <input
        type="text"
        placeholder="key (e.g. city)"
        class="kin-field flex-[1_1_180px] min-w-0"
        bind:value={newKey}
        maxlength="80"
        onkeydown={(e) => {
          if (e.key === 'Enter') void add();
        }}
      />
      <input
        type="text"
        placeholder="value (e.g. Berlin)"
        class="kin-field flex-[2_1_240px] min-w-0"
        bind:value={newValue}
        maxlength="500"
        onkeydown={(e) => {
          if (e.key === 'Enter') void add();
        }}
      />
      <button
        type="button"
        class="kin-btn-primary inline-flex items-center gap-1"
        onclick={() => void add()}
        disabled={adding || !newKey.trim() || !newValue.trim()}
      >
        <Plus size={14} /> Add
      </button>
    </div>
    <p class="text-xs text-white/40 mt-2">
      Use a short snake_case key. Saving an existing key overwrites — that's how you
      update a fact.
    </p>
  </section>

  <section class="kin-card p-4">
    <div class="flex items-center justify-between mb-3">
      <h2 class="text-sm font-medium text-white/80">
        Stored facts {facts.length > 0 ? `(${facts.length})` : ''}
      </h2>
      {#if facts.length > 0}
        <button
          type="button"
          class="text-xs text-red-300/80 hover:text-red-200 inline-flex items-center gap-1"
          onclick={() => void clearAll()}
        >
          <Trash2 size={12} /> Forget everything
        </button>
      {/if}
    </div>

    {#if loading}
      <div class="text-sm text-white/50">Loading…</div>
    {:else if facts.length === 0}
      <div class="text-sm text-white/50 py-6 text-center">
        Nothing remembered yet. Mention something about yourself in a chat
        ("I live in Berlin", "I'm vegetarian") and it'll appear here, or use the form
        above to add a fact directly.
      </div>
    {:else}
      <ul class="divide-y divide-white/5">
        {#each facts as f (f.id)}
          <li class="py-3 first:pt-0 last:pb-0">
            {#if editingId === f.id}
              <div class="flex gap-2 flex-wrap items-start">
                <input
                  type="text"
                  class="kin-field flex-[1_1_180px] min-w-0"
                  bind:value={editKey}
                  maxlength="80"
                  onkeydown={(e) => {
                    if (e.key === 'Enter') void commitEdit(f);
                    if (e.key === 'Escape') cancelEdit();
                  }}
                />
                <input
                  type="text"
                  class="kin-field flex-[2_1_240px] min-w-0"
                  bind:value={editValue}
                  maxlength="500"
                  onkeydown={(e) => {
                    if (e.key === 'Enter') void commitEdit(f);
                    if (e.key === 'Escape') cancelEdit();
                  }}
                />
                <button
                  type="button"
                  class="kin-btn-primary !px-2"
                  onclick={() => void commitEdit(f)}
                  aria-label="Save"
                  title="Save"
                >
                  <Check size={14} />
                </button>
                <button
                  type="button"
                  class="kin-btn !px-2"
                  onclick={cancelEdit}
                  aria-label="Cancel"
                  title="Cancel"
                >
                  <X size={14} />
                </button>
              </div>
            {:else}
              <div class="flex items-start justify-between gap-3 group">
                <div class="min-w-0 flex-1">
                  <div class="flex items-baseline gap-2 flex-wrap">
                    <span class="font-mono text-sm text-teal-200">{f.key}</span>
                    <span class="text-white/30">·</span>
                    <span class="text-sm text-white/90 break-words">{f.value}</span>
                  </div>
                  <div class="text-[11px] text-white/40 mt-1 flex items-center gap-2 flex-wrap">
                    <span class="px-1.5 py-0.5 rounded text-[10px] {sourceClass(f.source)}">
                      {sourceLabel(f.source)}
                    </span>
                    <span>{formatRelative(f.updated_at)}</span>
                  </div>
                </div>
                <div class="flex items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
                  <button
                    type="button"
                    class="kin-btn-ghost !px-1.5 !py-1"
                    onclick={() => startEdit(f)}
                    aria-label="Edit"
                    title="Edit"
                  >
                    <Pencil size={13} />
                  </button>
                  <button
                    type="button"
                    class="kin-btn-ghost !px-1.5 !py-1 hover:!text-red-300"
                    onclick={() => void remove(f)}
                    aria-label="Forget"
                    title="Forget"
                  >
                    <Trash2 size={13} />
                  </button>
                </div>
              </div>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </section>
</div>
