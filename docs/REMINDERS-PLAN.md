# Reminders — per family member, on their own devices

Status: plan (2026-09-09), nothing implemented yet.

## Why

A family member asked KinAI over Telegram to set a reminder. The first reply
was honest ("I can't set a reminder on your Mac"); the follow-up "can you
remind me?" got "Done — I've noted it … saved as a pending task". Nothing was
saved. KinAI has no reminder feature, and a model under pressure to be helpful
invented one. That is the history-poison class (docs of 0.2.10x): once a
standing claim sits in the thread, later turns repeat it.

Two things follow:

1. Until reminders exist, KinAI must say so — a one-line prompt rule (Phase 0).
2. Reminders are worth building. They are the first feature that acts for a
   member *later*, without a question in front of it, so the design has to be
   explicit about **whose** reminder it is and **where** it fires.

## Goals

- Each family member owns their reminders. Peer-scoped like `user_facts`:
  the host user, every invited client peer, and Telegram-paired members see
  only their own.
- A reminder fires on the member's own instances: their KinAI app (host app
  for the host user, the client app on their Windows/Linux/macOS machine) and
  their Telegram chat when paired. Never on someone else's screen.
- It fires at the member's local time, not the host's.
- Set, list, snooze, mark done, cancel — from chat (natural language), from
  a slash command, and from Settings.
- Survives host restarts; overdue reminders fire on the next tick, labelled
  overdue.
- Nothing personal in logs: reminder ids and counts only, never the text.

## Non-goals (for now)

- Location- or event-triggered reminders.
- Writing into Apple Reminders / Google Tasks. (Optional host-Mac bridge in
  Phase 3 — see open questions.)
- A shared family calendar. One member, one reminder; "remind us all" can
  be three reminders.

## Architecture

```
member (any surface) ──"remind me tomorrow at 9 to …"──▶ host tool loop
                                                            │ set_reminder(…)
                                                            ▼
                                            host kinai.db: reminders (peer-scoped)
                                                            │
                                   host scheduler task (30 s tick, leases a due row)
                                                            │
                ┌───────────────────────────┬───────────────┴──────────────┐
                ▼                           ▼                              ▼
   host app (peer = host)          client peer sessions               Telegram link
   app.emit("kinai://reminder")    Envelope::Reminder over WS         sendMessage(chat_id)
   + OS notification (plugin)      → client raises OS notification    + inline Done/Snooze
                                   + bubble in the Reminders thread
```

The host is the single source of truth (it already owns every peer's threads,
facts and Telegram links). Each instance is a *delivery agent*: it renders
the reminder, raises the OS notification, and sends the member's Done/Snooze
back. Phase 2 adds a per-instance cache so a client fires locally even while
the host is unreachable (see "Offline").

## Data model

New migration in `src/db/migrate.rs`:

```sql
CREATE TABLE IF NOT EXISTS reminders (
  id            TEXT PRIMARY KEY,
  peer_id       TEXT NOT NULL,            -- owner; 'host' or invite short code
  thread_id     TEXT,                     -- where it was set (for the chip)
  text          TEXT NOT NULL,            -- what to say
  due_at        TEXT NOT NULL,            -- UTC RFC3339, the next occurrence
  tz            TEXT NOT NULL,            -- IANA zone used to interpret local times
  due_local     TEXT NOT NULL,            -- 'YYYY-MM-DDTHH:MM' as the member said it
  repeat        TEXT NOT NULL DEFAULT '', -- '' | daily | weekdays | weekly | monthly
  status        TEXT NOT NULL,            -- scheduled | firing | fired | done | cancelled
  fired_at      TEXT,
  source_msg_id TEXT,
  created_at    TEXT NOT NULL,
  updated_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS reminders_due ON reminders(status, due_at);
CREATE INDEX IF NOT EXISTS reminders_peer ON reminders(peer_id, status, due_at);
```

`due_local` + `tz` are kept so a DST shift or a member moving zones can be
re-resolved from what they actually asked for. Snooze rewrites `due_at`
(status back to `scheduled`); a repeat computes the next `due_at` from
`due_local` in `tz` when the current one fires.

Backups: `~/.kinai` is mirrored daily by `kinai-backup.sh`; the table rides
along.

## Time zones

Today every timestamp KinAI shows is the host's `Local` (`tools/datetime.rs`).
Reminders need the member's zone:

1. `Envelope::Hello` gains `tz: String` (serde default, empty on old clients)
   — the client sends `iana-time-zone` at connect. The host stores it in a new
   `peers.tz` column and refreshes it every connect (laptops travel).
2. Telegram-only members have no client. Fallback order: `peers.tz` →
   `user_facts` key `timezone` (the `remember` tool already suggests that key
   with an IANA example value) → host zone.
3. `chrono-tz` is added to convert `due_local` in `tz` to UTC (the crate is
   not a dependency yet; `chrono` 0.4 is).
4. The clock the model sees on the newest user message (`now_pretty`) is
   rendered in the member's zone, so "in 20 minutes" and "tomorrow at 9" are
   computed from the right "now".

## Tools (the chat path)

Three tool definitions in `tools/registry.rs`, offered to every slot the way
`remember`/`forget` are (they need `ToolRuntime.db` + `peer_id`):

| tool | parameters | returns |
|---|---|---|
| `set_reminder` | `text`; exactly one of `due_local` (`YYYY-MM-DDTHH:MM`) or `in_minutes`; optional `repeat` | the resolved time in the member's zone, the zone name, where it will fire ("this app, Telegram") |
| `list_reminders` | — | the member's scheduled reminders, ids included |
| `cancel_reminder` | `id` | confirmation |

Rules baked into the descriptions and enforced by the tool:

- The model must never say a reminder is set unless the tool returned ok;
  the tool's own result text *is* the confirmation and is what the model
  should relay. (Same discipline as the "every lookup failed" banner in the
  loop pipeline — the truth is stated by code, not narrated by the model.)
- A time in the past is rejected with a hint ("did you mean tomorrow?");
  more than a year out is rejected.
- Text is capped (200 chars) and stored verbatim; the tool never logs it.
- `in_minutes` exists because relative phrasing is the common case and the
  model is more reliable at "20" than at date arithmetic.

Slash commands (`slash.rs`, all three surfaces): `/remind 18:00 take the
pills`, `/remind tomorrow 9:00 …`, `/reminders`, `/done <n>`, `/snooze <n>
30m`. Deterministic, no model turn, useful when the model is slow or the
member prefers commands on Telegram.

## Scheduler (host only)

A tokio task started from `network::server::start` (host mode) next to the
Telegram poller:

- Every 30 s: `UPDATE reminders SET status='firing' WHERE status='scheduled'
  AND due_at <= now RETURNING …` — the row transition is the lease, so a
  slow delivery and the next tick cannot fire the same reminder twice.
- Deliver (below), then `fired` (or `scheduled` with the next `due_at` for a
  repeat). A delivery error leaves the row `firing` with `fired_at` unset;
  the next tick retries rows in `firing` older than two minutes.
- On start: anything due in the past fires immediately with an "overdue
  since …" prefix — a host reboot must not swallow reminders.
- Tick and outcomes are logged as counts and ids only.

## Delivery — per member, per instance

The fired reminder becomes a persisted assistant message
"⏰ Reminder: <text>" in a dedicated per-member **Reminders** thread (created
on first use, like the Telegram thread), not in whatever thread happens to be
open. A reminder line inside a research thread would sit in that thread's
context forever; a dedicated thread keeps the chat model's history clean and
gives the member one place to scroll back. Replies in that thread ("done",
"snooze an hour") run as normal turns with the tools above.

Then, per instance:

- **Host app** (peer `host`): `app.emit("kinai://message")` (existing fan-out)
  plus a new `kinai://reminder` event; the frontend raises an OS notification
  through `tauri-plugin-notification` (already registered in `lib.rs`) and
  shows the bubble.
- **Client app**: `fan_out_message` already pushes `Envelope::Message` to
  every WS session of that peer. Add `Envelope::Reminder { id, text, due_at }`
  so the client can raise the OS notification and offer Done/Snooze without
  parsing the bubble. Clicking the notification opens the Reminders thread.
- **Telegram**: `api.send_message(chat_id, …)` when `telegram_links` has the
  peer, with an inline keyboard (Done / Snooze 10m / Snooze 1h) — callback
  queries are new to the router; text fallbacks `/done`, `/snooze` work
  without them.
- A member with several instances gets the notification on each; the first
  Done/Snooze wins and the others update on their next message load.

### Offline (Phase 2)

A client that is disconnected when the reminder fires still gets the message
on reconnect, but not the notification at the right moment. Phase 2: the
client asks `ListReminders` on connect and after every change, keeps the next
few in `tauri-plugin-store`, and fires the OS notification itself at
`due_at`. The host still owns the state; the client reports "fired locally"
when it reconnects, and the host de-duplicates on `(id, due_at)`.

## UI

- Chat: the `set_reminder` tool chip reads "⏰ Tue 9:00 · your time · this
  app + Telegram" — the member sees at a glance that it is real and where.
- Reminder bubble: Done / Snooze (10 min · 1 h · tomorrow 9:00).
- Settings → Reminders (host and client): list with local times, edit time
  and text, toggle repeat, delete. Wire shape mirrors the user-facts
  envelopes (`ListReminders` / `Reminders` / `SaveReminder` /
  `DeleteReminder`), peer-scoped on the host exactly like `user_facts`.
- `Welcome.host_reminders: bool` gates all of it on the client, the same
  way `host_reports` and `host_thread_ops` gate older features — an old
  host shows no reminder UI instead of a wire error.

## Privacy

- Reminder text stays in `kinai.db` on the host and in the member's own
  client store. Logs carry ids and counts. The privacy guard already blocks
  log lines and household words from reaching the repo; test fixtures use
  invented text.
- Telegram delivery sends the text to Telegram's servers — that is the
  member's existing choice when they paired; the Settings panel says so
  next to the Telegram toggle per reminder ("also on Telegram", default on
  when paired).

## Testing

Unit (Rust):
- local→UTC resolution across a DST boundary and a zone change; past and
  far-future rejection; `in_minutes` vs `due_local` exclusivity.
- scheduler lease: two ticks racing on one due row fire it once; repeat
  advances `due_at` from `due_local`; overdue rows fire on start.
- peer scoping: a peer cannot list, snooze or cancel another peer's row.
- `Hello` without `tz` resolves to the fallback chain.

End-to-end (release checklist rows, on the running host):
- host app: set "in 2 minutes" in chat → notification + bubble at T+2.
- client (Windows or Linux — both are in daily use): same, in the client's
  zone, with the host in another zone.
- Telegram: set via caption-free text, receive the message with buttons,
  press Snooze, receive again.
- restart the host with one reminder overdue → it fires on start with the
  overdue prefix, once.

## Phases

| phase | scope | size |
|---|---|---|
| 0 | System prompt: "You cannot set reminders, timers or alarms yet; say so and suggest the member's phone." Stops the fabricated confirmation today. | one line, ships with the next release |
| 1 | Migration, three tools, scheduler, Reminders thread, delivery to host app / client WS / Telegram text, `/remind` + `/reminders`, `Hello.tz`, `Welcome.host_reminders` | core; one release |
| 2 | Bubble actions, Settings panel on host + client, Telegram inline buttons, client-side offline firing, snooze | one to two releases |
| 3 | Repeats beyond daily/weekly, natural-language edits ("move it to Friday"), optional host-Mac bridge to Apple Reminders for the host user | later |

Phase 0 can ride with the image-follow-up fix already in the working tree.

## Open questions

1. Dedicated Reminders thread (proposed) or the thread the member set it in?
2. Snooze defaults: 10 min / 1 h / tomorrow 9:00 — or fewer buttons?
3. Telegram-paired members: deliver to both app and Telegram by default, or
   Telegram only when the app has not connected in the last day?
4. Host Mac bridge: should "set a reminder on my Mac" from the host user also
   create an Apple Reminders entry (macOS only, host user only)? It is a
   Phase 3 add-on; the in-app reminder works regardless.
