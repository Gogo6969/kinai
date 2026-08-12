# Per-peer encryption — making "I can't read your questions" true

Status: **proposal, not implemented.** Written 2026-08-12.

## 1. The promise, and where it currently fails

Wolf told the family that he cannot read their questions. The app repeats
it: *"Their chats are private — you only see your own here"*
(`frontend/src/lib/components/ChatWindow.svelte:539`).

The UI keeps that promise. The storage does not. Everything lives in one
unencrypted SQLite file, `~/.kinai/kinai.db` (80 MB, 2,291 messages, all
peers, back to 2026-05-15). Reading any family member's complete history
takes one `SELECT`. No password, no deliberate act, no code change.

That gap is the whole problem. It is not about a stolen laptop.

## 2. Threat model (Wolf's, stated precisely)

**In scope — must become impossible:**

* Opening the database and reading a family member's conversations.
* Doing so *accidentally, casually, or in a weak moment* — the file is
  right there, and curiosity is a normal human failing. Prevention has to
  be structural, not a matter of self-restraint.
* Anything derived from their content being readable the same way:
  summaries, remembered facts, search indexes.

**Explicitly out of scope, by the host owner's own decision:**

* The host process necessarily sees each message in RAM while it builds
  the prompt and streams the answer. Wolf could add logging and capture
  it. He has decided he never will. That is an intention-based boundary
  he accepts, and encryption is not expected to cover it.

This is a coherent line: it removes the easy, passive path entirely and
leaves only a path that requires deliberately subverting your own server.

**Consequence:** the existing log already crosses that line by accident —
`~/.kinai/logs/` records every `web_search` query verbatim, e.g.
`args={"query":"Proton cancel subscription from another account…"}`.
Those are user questions in plaintext. Redacting them is part of the
work, not an optional extra.

## 3. What must be encrypted (inventory, measured)

Encrypting `messages.content` alone is **not sufficient**. Verified in
the live database:

| Location | What it exposes | Rows today |
|---|---|---|
| `messages.content` | the questions and answers themselves | 2,291 |
| `messages_fts` (+ `_data`, `_idx`, `_docsize`) | a full plaintext copy of every message, for search | 2,291 |
| `memory_notes.summary` / `.keywords` | AI-written summaries of each conversation | 233 |
| `memory_fts` (+ shadow tables) | plaintext copy of those summaries | — |
| `user_facts.key` / `.value` | inferred personal facts — including a stored `personality_traits` assessment of a family member | 16 |
| `threads.title` | auto-generated titles derived from the first question | — |
| `~/.kinai/logs/*` | every web_search query, verbatim | 4 days retained, unbounded |

The `user_facts` row is the clearest illustration of why this matters:
the model inferred a personality judgement about a child and stored it
where his father can read it. He never wrote it and does not know it
exists.

## 4. Design

**Keys.** Each peer holds a symmetric key. It is generated on the client
at pairing and stored in the OS secret store — Keychain (macOS), DPAPI /
Credential Manager (Windows), Secret Service / KWallet (Linux). The host
never receives it.

**Storage.** The host stores ciphertext blobs it can serve back to the
owning peer and cannot read. AEAD (XChaCha20-Poly1305 or AES-GCM), one
nonce per row, with `peer_id` + row id as associated data so rows cannot
be moved between peers.

**Reading.** `LoadThread` → `ThreadMessages` returns ciphertext; the
client decrypts for display. Unchanged protocol shape, different payload.

**Writing a turn.** This is the real change. The host currently builds
the prompt from its own stored history (`src/context/builder.rs`
`build_context`). It will no longer be able to. The client must send the
context it wants included along with the new message. The host holds that
plaintext in RAM for the turn, sends it to the model, encrypts the new
user message and the reply, stores both, and forgets.

**Search.** `messages_fts` must be dropped for encrypted peers — an FTS
index over ciphertext is useless and an FTS index over plaintext defeats
the whole exercise. Search becomes client-side over decrypted local data.

**Memory.** `memory_notes` and `user_facts` are encrypted with the same
peer key and travel to the client with the context. The host can no
longer use them to build prompts on its own — the client supplies them.

## 5. Decisions required before implementation

1. **Person or device?** Today a peer *is* a device: the invites are
   `Quentin`, `Kris`, `Rafael`, `Family device`. Per-device keys are
   simplest but give one person two disjoint histories across laptop and
   phone. Per-person keys mean the key must reach a second device — the
   existing invite/QR flow can carry it. **Recommendation: per person**,
   with the invite carrying the key.
2. **Recovery.** Lose the key, lose the history. Offer a recovery phrase
   at pairing, or accept the loss and say so plainly in the UI.
3. **Migration of the 2,291 existing messages.** Either (a) each client,
   on first run of the new version, fetches its plaintext history,
   encrypts it, sends the ciphertext back and the host deletes the
   plaintext, or (b) draw a line at the switchover and encrypt only new
   messages. (a) is the honest one; it needs care, and it is the only
   path that removes the `personality_traits` row and its relatives from
   the host's reach.
4. **Telegram.** Those messages arrive with no client and no key. Those
   threads stay host-readable by nature. The UI must say so, per-thread,
   rather than implying blanket privacy.

## 6. What the host owner can still see afterwards (state this openly)

* Metadata: who sent how many messages, when, thread counts, timings.
* Telegram conversations, entirely.
* Anything a family member explicitly reports via "report answer".
* Live traffic, if he ever chose to add logging — the boundary he has
  undertaken not to cross.

Family members should be told exactly this. A precise promise that holds
is worth more than a broad one that does not.

## 7. Phases

* **Phase 0 — honesty (hours).** Fix the UI sentence so it states what is
  actually true today. Redact `web_search` queries from logs. Add log
  retention. None of this waits for the crypto.
* **Phase 1 — keys and storage.** Key generation at pairing, OS secret
  store on three platforms, encrypted `messages`, FTS removed,
  client-side search.
* **Phase 2 — context and memory.** Client-supplied context; encrypted
  `memory_notes` and `user_facts`.
* **Phase 3 — migration and recovery.** Re-encrypt existing history,
  recovery phrase, Telegram labelling.

Phase 0 is worth doing regardless of whether Phases 1–3 ever happen,
because the current UI text promises something the software does not do.
