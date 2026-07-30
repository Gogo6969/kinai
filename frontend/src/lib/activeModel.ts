/**
 * Which model is answering *right now*, for the composer badge.
 *
 * Wolf's ask: "show in the prompt which model is currently set — so that
 * the user always knows which model is right now working for him."
 *
 * The truth lives server-side in `threads.active_slot` (the sticky
 * per-thread slot a `/fast` `/balanced` `/deep` command sets). Reading it
 * would mean a new host command AND a new client protocol message plus a
 * capability flag for older hosts — so instead this derives the same
 * answer from what the client already holds, which works identically in
 * host and client mode and cannot go stale against an old host:
 *
 *   1. Walk the thread newest → oldest.
 *   2. The first assistant reply carrying `metrics.slot` tells us which
 *      slot actually served a turn.
 *   3. A `/fast|/balanced|/deep` the USER typed more recently than that
 *      wins — the switch takes effect on the next turn, and the badge
 *      should show it immediately rather than after the next answer.
 *      This reads the user's own text, never the model's prose, so
 *      there is nothing fragile to parse.
 *   4. Nothing found (fresh thread) → the default slot.
 *
 * Known limit: switching slots on one device does not update another
 * device's badge until a reply arrives there. Accepted — the alternative
 * is a protocol round-trip on every thread load.
 */

import type { AppConfig, LlmSettings, Message, TurnMetrics } from './api';

export type SlotSlug = 'fast' | 'balanced' | 'deep';

export const SLOT_GLYPH: Record<SlotSlug, string> = {
  fast: '⚡',
  balanced: '⚖️',
  deep: '🧠',
};

/** Leading slash command, if the message is one. */
export function slotFromCommand(text: string): SlotSlug | null {
  const m = /^\s*\/(fast|balanced|deep)\b/i.exec(text ?? '');
  return m ? (m[1].toLowerCase() as SlotSlug) : null;
}

/**
 * The slot serving this thread. `metrics` maps assistant message id →
 * metrics, exactly as the store holds it.
 */
export function activeSlot(
  messages: Message[] | undefined,
  metrics: Record<string, TurnMetrics>,
  fallback: SlotSlug = 'fast',
): SlotSlug {
  if (!messages?.length) return fallback;
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i];
    if (m.role === 'user') {
      const cmd = slotFromCommand(m.content);
      if (cmd) return cmd;
      continue;
    }
    if (m.role === 'assistant') {
      const slot = metrics[m.id]?.slot;
      if (slot === 'fast' || slot === 'balanced' || slot === 'deep') return slot;
    }
  }
  return fallback;
}

/** Host mode: the slot's configured model, from AppConfig. */
function configuredSlot(cfg: AppConfig | null, slot: SlotSlug): LlmSettings | null {
  if (!cfg) return null;
  if (slot === 'fast') return cfg.llm ?? null;
  if (slot === 'balanced') return cfg.llm_balanced ?? null;
  return cfg.llm_deep ?? null;
}

/** Drop the vendor prefix and a `.gguf` suffix — badges are narrow. */
export function shortModelName(model: string | undefined | null): string {
  if (!model) return '';
  let name = model.split('/').pop() ?? model;
  name = name.replace(/\.gguf$/i, '');
  return name;
}

export interface ActiveModel {
  slot: SlotSlug;
  glyph: string;
  /** Abbreviated model id, or '' when we genuinely don't know it. */
  model: string;
  /** false only when the host has told us the slot is unreachable. */
  alive: boolean;
}

/**
 * Resolve the badge contents. `hostSlots` is the client-mode list from
 * the Welcome envelope; in host mode it is undefined and the config
 * supplies the name.
 */
export function resolveActiveModel(
  messages: Message[] | undefined,
  metrics: Record<string, TurnMetrics>,
  cfg: AppConfig | null,
  hostSlots?: Array<{ slug: string; model: string; alive?: boolean | null }>,
): ActiveModel {
  const slot = activeSlot(messages, metrics);
  let model = '';
  let alive = true;

  if (hostSlots?.length) {
    const hit = hostSlots.find((s) => s.slug === slot);
    if (hit) {
      model = shortModelName(hit.model);
      alive = hit.alive !== false;
    }
  }
  if (!model) {
    model = shortModelName(configuredSlot(cfg, slot)?.model);
  }
  return { slot, glyph: SLOT_GLYPH[slot], model, alive };
}
