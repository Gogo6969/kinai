// Streaming markdown that doesn't fight the compositor.
//
// A streamed reply is re-parsed from scratch and written back with
// `{@html …}` on every token — `app.svelte.ts:825` appends the delta and then
// reassigns the whole `streaming` object, so the render effect re-runs for
// each one. That was fine while replies were text. Once a reply contains a
// picture it is not, and the family's report was blunt: "the moment the
// thread renders with the picture, KinAI is flickering like an old damaged
// TV."
//
// Three things compound, and this action addresses the first two:
//
//   1. `{@html}` does no diffing, so every token destroys and re-creates
//      every <img> in the answer. Measured in the running app: streaming one
//      image at 25 tokens/s produced 41 distinct <img> nodes in 41 renders.
//      A fresh element carries `loading="lazy" decoding="async"` and no
//      width/height (markdown.ts:51), so it has no intrinsic size until its
//      IntersectionObserver callback and async decode land — neither of
//      which can happen on the frame that created it. The image box
//      therefore collapses to 0 and springs back to 320px at token rate.
//
//   2. Re-parsing is O(n^2): marked + KaTeX + highlight.js + a full
//      DOMPurify.sanitize over the ENTIRE accumulated answer, per token.
//      On a fast local model that starves the main thread and WKWebView
//      starts presenting partially-composited frames.
//
//   3. The collapse in (1) drives ChatWindow's autoscroll effect, which
//      reads scrollHeight against the mid-collapse layout and slams
//      scrollTop, so the column jumps by up to 320px per frame. Fixing (1)
//      removes its cause.
//
// So: coalesce the re-render to at most one per animation frame, and keep
// the already-decoded <img> elements alive across those renders instead of
// rebuilding them. Text still updates continuously; the picture is simply
// never taken away.
//
// Why not defer images to the end of the turn: the reply usually continues
// past the picture, and a picture that only appears once the model stops
// writing is broken in a different way.
//
// Both streaming chat surfaces use this — ChatWindow and Overlay. Telegram
// renders server-side and is unaffected. KinAI has three chat surfaces and a
// UI change has to cover all of them.

import { renderMarkdown } from '$lib/markdown';

/**
 * Svelte action: render streamed markdown into `node`, coalesced to one
 * paint per frame, reusing any <img> already materialised by a prior render.
 *
 * Takes the RAW markdown text, not pre-rendered HTML — the parse is the
 * expensive half, so it has to happen behind the coalescing, not in front
 * of it.
 *
 * Put it on a wrapper with `display: contents` so it adds no box of its own
 * and the prose styles (all descendant selectors) apply exactly as before.
 */
export function streamMarkdown(node: HTMLElement, text: string) {
  /** Occurrence-keyed cache of live, already-decoded image elements. */
  const kept = new Map<string, HTMLImageElement>();
  let pending = text;
  let frame = 0;

  function paint() {
    frame = 0;
    node.innerHTML = renderMarkdown(pending);

    // Count occurrences as we go: the same picture can legitimately appear
    // twice in one reply, and keying on URL alone would let the second <img>
    // steal the first one's element, so the picture would jump up the
    // message on every token.
    const seen = new Map<string, number>();

    for (const fresh of Array.from(node.querySelectorAll('img'))) {
      const src = fresh.getAttribute('src');
      if (!src) continue;

      const nth = seen.get(src) ?? 0;
      seen.set(src, nth + 1);
      const key = `${nth} ${src}`;

      const prev = kept.get(key);
      if (prev) {
        // `prev` was detached by the innerHTML write above; this puts the
        // same, already-loaded element back where the new one would have
        // gone. No load, no decode, no blank frame, no collapsed box.
        fresh.replaceWith(prev);
      } else {
        kept.set(key, fresh);
      }
    }
  }

  // First paint is synchronous so the bubble is never briefly empty.
  paint();

  return {
    update(next: string) {
      pending = next;
      // Tokens arrive faster than frames on a local model; render once per
      // frame with whatever text has accumulated by then.
      if (!frame) frame = requestAnimationFrame(paint);
    },
    destroy() {
      if (frame) cancelAnimationFrame(frame);
      kept.clear();
    },
  };
}
