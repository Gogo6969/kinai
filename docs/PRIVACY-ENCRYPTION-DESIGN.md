# Per-peer encryption — making "I can't read your questions" true

Status: **proposal, not implemented.** Written 2026-08-12.

## 1. Why do this

Wolf told the family he does not read their questions, and he does not.
Nothing here is a remediation for anything that happened; the point is
to make the software enforce what he already practises, so the promise
rests on the design instead of on his restraint.

Today it rests on restraint. Everything lives in one unencrypted SQLite
file, `~/.kinai/kinai.db` (80 MB, 2,291 messages, all peers, back to
2026-05-15), so reading any member's history would take one `SELECT`.
Nobody has. The goal is that nobody *can* — including a future owner of
this machine, a future maintainer, or a version of KinAI that changes
hands.

This is not about a stolen laptop, and the app's existing wording stays
as it is: it describes the interface accurately and always has.

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

**One practical consequence:** `~/.kinai/logs/` records each
`web_search` query verbatim in its tool-call lines. That is user text
living outside the database, so sealing the database alone would leave
it behind. Redaction belongs in the same phase — keeping everything the
logs are actually for (tool, timing, result size, errors).

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

**Keys — asymmetric, not symmetric.** Each peer generates a **keypair**
on the client at pairing. The *private* key never leaves the device and
lives in the OS secret store — Keychain (macOS), DPAPI / Credential
Manager (Windows), Secret Service / KWallet (Linux). The *public* key is
given to the host.

This asymmetry is the point, and a symmetric key would not do: the host
must be able to **write** things it can never **read**. It derives
memory summaries and personal facts during a turn, and it stores the
assistant's reply — all of that has to be encrypted at rest even when
the client is offline or the write happens after the client disconnects.
With the public key the host can seal any of it; only the family
member's device can open it.

**Storage.** Hybrid sealing per row (the age/ECIES pattern): a fresh
random content key per row, the payload under AEAD
(XChaCha20-Poly1305), the content key sealed to the peer's public key.
`peer_id` + row id as associated data, so a row cannot be moved between
peers or replayed into another thread.

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

**Memory and personality facts — kept, still used, no longer visible.**
This is a feature worth protecting, not a liability: `user_facts` is what
lets KinAI answer someone the way that person needs to be answered.
Nothing about it is removed.

* The host derives a fact during a turn exactly as it does today, seals
  it to the peer's public key, stores the ciphertext, and forgets the
  plaintext.
* On the next turn the client decrypts its own facts and sends them as
  part of the context, so the model receives the same information it
  receives today.
* The host owner cannot read them at rest — including the inferred
  `personality_traits` judgements, which is the case that prompted this
  design.

Cost: memory only travels with a client that can decrypt it. A
Telegram-only member has no key, so their facts stay host-readable (see
the Telegram carve-out) — and that should be stated in the UI rather
than glossed over.

## 5. Decisions taken

**Encryption is opt-in, per person** (Wolf, 2026-08-12). On first
start of the new version each family member is asked once:

* **Set a password** → their conversations, summaries and remembered
  facts are sealed; nobody, including the host owner, can read them at
  rest.
* **Skip it** → everything is stored as it is today, readable on the
  family computer.

Rationale: not everyone wants this. Wolf's wife would decline, and
forcing a password on someone who does not care about it buys nothing
and costs support calls. A mixed household is expected and supported —
the database will hold plaintext rows for some peers and sealed rows for
others.

Consequences to build:

* A per-peer `encrypted` flag, set at that first prompt.
* The host UI must show which members are sealed and which are not, so
  the host owner is never confused about what he can and cannot see, and
  so the promise made to each person is visible rather than assumed.
* Turning it **on later** must be possible: the client holds the key, so
  it can re-seal its own existing history. Turning it **off** likewise.
* Copy at the prompt has to be plain: "If you set a password, nobody —
  not even the family computer's owner — can read your chats. If you
  skip it, your chats are stored readable on the family computer."

**Keys are per person, not per device.** A person's laptop and phone
share one identity and one history.

**Key recovery — three layers, none of which put the key on the host:**

1. **A password they choose** (default). The key is derived from it; a
   sealed copy sits on the host so any new device can fetch and unlock
   it. Trivial passwords must be refused — the sealed copy is offline-
   guessable by anyone holding the database.
2. **Another of their own paired devices.** A new laptop is paired from
   the phone that already holds the key. This is the route most people
   will actually use when a machine dies.
3. **A printed recovery phrase**, offered and optional, for anyone who
   wants a backup that depends on nothing else.

## 5b. Still open
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

* **Phase 0 — folded into Phase 1, not shipped separately.** The only
  item worth doing is redacting user text from `~/.kinai/logs/`: the
  tool-call lines record each `web_search` query verbatim, which is
  content sitting outside the database that encryption would otherwise
  miss. Keep everything needed to debug — tool name, duration, result
  size, success or failure, error text — and drop the query string
  itself. Log retention (delete files older than N days) is ordinary
  housekeeping and rides along.

  No UI wording change. The sentence on the host chat screen describes
  that screen and is accurate.
* **Phase 1 — keys and storage.** Key generation at pairing, OS secret
  store on three platforms, encrypted `messages`, FTS removed,
  client-side search.
* **Phase 2 — context and memory.** Client-supplied context; encrypted
  `memory_notes` and `user_facts`.
* **Phase 3 — migration and recovery.** Re-encrypt existing history,
  recovery phrase, Telegram labelling.

The sequence is deliberate: log redaction lands with Phase 1 because it
closes the same hole — family content readable at rest — in the one
place the database encryption cannot reach.
