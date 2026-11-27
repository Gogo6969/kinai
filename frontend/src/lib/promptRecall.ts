// ↑/↓ recall of earlier prompts for a message box, shared by the chat window
// and the quick-chat overlay.
//
// The rule that matters: nothing the writer typed is ever thrown away by an
// arrow key. The prompt being written is one more position at the end of the
// list. Going up from it keeps it; coming back down shows it again — also
// after a recalled prompt was edited, undone, or was a bare slash command.
// Edits made to a recalled prompt are kept the same way until a message is
// sent.

export class PromptRecall {
  /** Position in the history list; null = the prompt being written. */
  index: number | null = null;
  /** True right after an arrow key put a prompt in the box and before the
   *  writer touched it: the next arrow keeps walking the list without
   *  needing the caret at the edge. */
  pristine = false;
  private draft = '';
  private edits = new Map<number, string>();

  /** Move one step (-1 older, 1 newer). `current` is the text in the box
   *  now. Returns the text to show, or null when there is nowhere to go. */
  move(dir: -1 | 1, current: string, history: readonly string[]): string | null {
    if (history.length === 0) return null;
    // The list can grow or be capped while a recall is open.
    if (this.index !== null && this.index >= history.length) this.index = history.length - 1;

    let target: number | null;
    if (dir === -1) {
      if (this.index === 0) return null;
      target = this.index === null ? history.length - 1 : this.index - 1;
    } else {
      if (this.index === null) return null;
      target = this.index < history.length - 1 ? this.index + 1 : null;
    }

    // Keep what is in the box before leaving it.
    if (this.index === null) this.draft = current;
    else if (current !== history[this.index]) this.edits.set(this.index, current);
    else this.edits.delete(this.index);

    this.index = target;
    this.pristine = target !== null;
    return target === null ? this.draft : (this.edits.get(target) ?? history[target]);
  }

  /** The writer typed, clicked or moved the caret: arrows go back to moving
   *  the caret until it reaches the edge of the text. */
  touched() {
    this.pristine = false;
  }

  /** A message was sent: start over. */
  reset() {
    this.index = null;
    this.pristine = false;
    this.draft = '';
    this.edits.clear();
  }
}

/** Decide whether an arrow key should walk the prompt list instead of moving
 *  the caret. Up needs the caret at the very start, down at the very end —
 *  unless the box still shows an untouched recalled prompt. */
export function recallDirection(
  key: string,
  recall: PromptRecall,
  el: { selectionStart: number; selectionEnd: number; value: string },
): -1 | 1 | null {
  if (key !== 'ArrowUp' && key !== 'ArrowDown') return null;
  if (recall.index !== null && recall.pristine) return key === 'ArrowUp' ? -1 : 1;
  if (key === 'ArrowUp') return el.selectionStart === 0 && el.selectionEnd === 0 ? -1 : null;
  if (recall.index === null) return null;
  const end = el.value.length;
  return el.selectionStart === end && el.selectionEnd === end ? 1 : null;
}
