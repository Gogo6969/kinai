/**
 * Which slot is answering *right now*, for the composer prompt label.
 *
 * Wolf's ask: "show in the prompt which model is currently set — so that
 * the user always knows which model is right now working for him", and
 * then: "the chat entry in the app should just show fast: balanced: or
 * deep:". So this resolves the slot, not the model id — the per-message
 * footer already carries the full model name and stays as it is.
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
 *      wins — the switch takes effect on the next turn, and the label
 *      should show it immediately rather than after the next answer.
 *      This reads the user's own text, never the model's prose, so
 *      there is nothing fragile to parse.
 *   4. Nothing found (fresh thread) → the default slot.
 *
 * Known limit: switching slots on one device does not update another
 * device's label until a reply arrives there. Accepted — the alternative
 * is a protocol round-trip on every thread load.
 */

import type { Message, TurnMetrics } from './api';

export type SlotSlug = 'fast' | 'balanced' | 'deep';

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

export interface ActiveSlot {
  slot: SlotSlug;
  /** Full model id, for the tooltip only — the label itself is the slot. */
  model: string;
  /** false only when the host has probed the slot as unreachable, in
   *  which case the label would otherwise name a model that is NOT going
   *  to answer (KinAI fails over). */
  alive: boolean;
}

/**
 * Resolve the label. `hostSlots` is the client-mode list from the Welcome
 * envelope; in host mode it is undefined and `configuredModel` supplies
 * the name for the tooltip.
 */
export function resolveActiveSlot(
  messages: Message[] | undefined,
  metrics: Record<string, TurnMetrics>,
  configuredModel: (slot: SlotSlug) => string | undefined,
  hostSlots?: Array<{ slug: string; model: string; alive?: boolean | null }>,
): ActiveSlot {
  const slot = activeSlot(messages, metrics);
  const hit = hostSlots?.find((s) => s.slug === slot);
  return {
    slot,
    model: hit?.model || configuredModel(slot) || '',
    alive: hit ? hit.alive !== false : true,
  };
}
