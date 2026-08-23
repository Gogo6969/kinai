/**
 * The model id, the way a human should read it.
 *
 * llama-server registers whatever `-m` it was launched with unless an
 * alias was set — so a slot's id can be
 * `/home/olares/models/Qwen3.6-35B-A3B-heretic-MTP-Q4_K_M.gguf`, a
 * filesystem path from another machine, and that string was rendering
 * verbatim in the Settings model dropdown and the composer's picker.
 *
 * Mirror of the Rust rules in slash.rs `display_model_name` (basename →
 * strip .gguf → cap at 40 chars) so every surface abbreviates the same
 * way. Display only: anything that WRITES the model id (field values,
 * option values, API calls) must keep the real string, or requests
 * would name a model the server never registered.
 */
export function displayModelName(model: string | undefined | null): string {
  const s = baseModelName(model);
  return [...s].length > 40 ? [...s].slice(0, 39).join('') + '…' : s;
}

/**
 * Canonical form for "is this the same model?" comparisons: basename,
 * `.gguf` stripped, case-folded. A llama-server without an alias reports
 * a full path from its own filesystem (`C:\models\Foo-Q6_K.gguf`) while
 * the config holds the bare filename — same model, not a mismatch.
 */
export function modelIdKey(model: string | undefined | null): string {
  return baseModelName(model).toLowerCase();
}

function baseModelName(model: string | undefined | null): string {
  let s = (model ?? '').trim();
  const cut = Math.max(s.lastIndexOf('/'), s.lastIndexOf('\\'));
  if (cut >= 0) s = s.slice(cut + 1);
  return s.replace(/\.gguf$/i, '');
}
