# Per-peer encryption — implementation plan

Companion to `PRIVACY-ENCRYPTION-DESIGN.md`. **Awaiting go-ahead — no
code written.**

## Ground rule

**Nobody loses access at any point, in any slice.** Encryption is opt-in
per person, so an unsealed peer takes exactly the code path it takes
today. Every slice below ships with the mixed household working: some
peers sealed, some not, all of them chatting normally. If a slice cannot
guarantee that, it does not ship.

The rollout is people-by-people, not all-at-once:

1. Version ships. Everyone updates. Nothing changes for anyone — the
   first-start question appears and "skip" leaves you where you are.
2. the owner opts in first, on his own account. If sealing has a bug, it is
   his data and he is the one who can diagnose it.
3. A few days of ordinary use: chat, memory, search, reconnect, a second
   device.
4. Then whoever else wants it. Anyone who never opts in is never
   affected, permanently.

**Opt-out stays available.** Turning encryption off writes the content
back in plain form using the key the client still holds, so an early
adopter is never trapped by a decision made before the feature proved
itself.

---

## Slice 1 — identity and keys (no content sealed yet)

Nothing is encrypted in this slice. It exists so that key handling can
be proven before any data depends on it.

* Client generates an X25519 keypair on opt-in; private key into the OS
  secret store (Keychain / DPAPI / Secret Service), public key sent to
  the host.
* New table `peer_keys(peer_id, public_key, created_at, algo)`.
* Per-peer `encrypted` flag, default off.
* First-start prompt with the plain-language choice, and the same choice
  reachable later from Settings.
* Password path: key derived with Argon2id from a passphrase the person
  chooses; a sealed copy of the private key stored host-side so a new
  device can fetch and unlock it. Trivial passwords refused.
* Host UI shows, per family member, whether they are sealed.

**Ships when:** a peer can opt in, restart, reinstall on another machine
and recover the same key — with all conversation still plaintext and
every client working normally.

**Risk:** key storage differs on three OSes. Linux is the awkward one
(Secret Service may be absent on a headless or minimal desktop) — needs
a fallback and an honest error, not a crash.

## Slice 2 — sealed messages

* `messages.content` sealed for peers with `encrypted = 1`: per-row
  content key under XChaCha20-Poly1305, content key sealed to the peer's
  public key, `peer_id` + message id as associated data.
* Host writes sealed rows without being able to read them (that is what
  the keypair buys).
* `LoadThread` → `ThreadMessages` returns sealed payloads for those
  peers; the client decrypts for display.
* **Prompt assembly changes** for sealed peers only: the client sends
  the context with the turn, since the host can no longer read history.
  `src/context/builder.rs` grows a path that accepts client-supplied
  context; the existing path stays for unsealed peers.
* `messages_fts` rows suppressed for sealed peers — an index over
  plaintext would defeat the whole exercise. Their search moves
  client-side over local decrypted data.

**Ships when:** a sealed peer and an unsealed peer are both chatting
normally, and `sqlite3` on the host shows readable rows for one and
ciphertext for the other.

**Risk:** this is where data can be lost. Sealing must be write-ahead —
never delete plaintext until the sealed row is verified readable by a
round-trip through the client. Migration of the 2,291 existing messages
is a separate, explicit, resumable step, not a side effect of the
upgrade.

## Slice 3 — derived content

* `memory_notes` and `user_facts` sealed the same way. The host keeps
  deriving them during a turn, seals them, forgets the plaintext.
* Client sends its own decrypted facts with the turn's context, so
  answer quality is unchanged.
* `memory_fts` suppressed for sealed peers.
* Settings → Memory becomes the person's own window onto what KinAI has
  recorded about them, which for a sealed peer is the only window.

## Slice 4 — edges

* **Telegram**: no client, no key. Those threads stay unsealed by
  nature, labelled as such in the UI rather than silently different.
* **Log redaction**: drop the verbatim `web_search` query text from
  tool-call log lines; keep tool name, duration, result size, success or
  failure and error text. Plus retention (delete files older than N
  days). This lives here because it closes the same hole in the one
  place database sealing cannot reach.
* **Reported answers** keep working — the reporter is explicitly handing
  that message over.
* **Recovery phrase** offered as the third layer.
* **Migration** of existing history, resumable, per peer, opt-in.

---

## Testing

* Unit: sealing round-trip, associated-data rejection (a row moved
  between peers must fail to open), Argon2id parameters, key-store
  read/write per platform behind a trait so it can be faked in tests.
* Mixed-household integration: one sealed peer and one unsealed peer in
  the same host, both chatting, both loading history.
* The failure that matters: **kill the host mid-seal** and confirm no
  message is lost and none is left unreadable.
* Manual, on real machines: macOS host, the Fedora client, the Windows
  client — key storage is exactly the kind of thing that works on one
  OS and fails on another.

## What this does not protect

Unchanged from the design doc, and worth restating in the UI: metadata
(who, when, how many), Telegram threads, explicitly reported answers,
and live traffic passing through the host's memory during a turn.
