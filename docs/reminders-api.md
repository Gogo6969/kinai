# The reminder API

Since 0.2.125 a program can add a reminder without going through a
conversation. Until then the only way in was to ask KinAI, which means a
model had to understand you first — fine for a person typing a sentence,
useless for a cron job, where the same words can produce a reminder, a
clarifying question, or nothing at all. These three routes are
deterministic.

It is **not** a calendar API. There are no events, no durations, no
all-day, no location, no attendees. A reminder is one piece of text and one
moment, belonging to one person. "Calendar" is the name of the page that
lists them.

---

## Getting a key

**Invites page → Reminder API key.** Give it a name, press Create, copy the
token. That token is the credential — there is no code to type, and the
key's own six-character code is deliberately *not* redeemable over the
network, because a guessed code resolving to it would grant more than any
family invite does.

The key lands in the same table as your invites, so the **Revoke** button
on that page covers it like anything else.

What a key can and cannot do:

| | |
|---|---|
| write reminders to **your own** calendar | yes |
| read and act on **your own** reminders | yes |
| open a chat socket | **no** — refused at the handshake |
| read anyone's messages | **no** |
| write to another family member's calendar | **no**, and the request body has no field for it |

The peer written to comes from the token, never from the request. There is
no way to name somebody else: one member scheduling notifications onto
another's phone is not something this household wanted.

---

## Endpoints

Base URL is the host on your LAN, e.g. `http://YOUR-HOST:4847`. Every
request needs `Authorization: Bearer <your key>`.

### `POST /v1/reminders`

```bash
curl -X POST http://HOST:4847/v1/reminders \
  -H "Authorization: Bearer $KINAI_KEY" \
  -H "Content-Type: application/json" \
  -d '{"text":"Bins out","in_minutes":540}'
```

| field | required | notes |
|---|---|---|
| `text` | yes | up to 600 characters |
| `in_minutes` | one of | minutes from now, 1 to 366 days |
| `due_local` | one of | `YYYY-MM-DDTHH:MM` in your own timezone |
| `repeat` | no | `daily`, `weekdays`, `weekly`, `monthly` |

Give exactly one of `in_minutes` or `due_local`. `201` returns the stored
reminder, including the `id` you need for the action route.

A repeating reminder keeps its **clock time** through a daylight-saving
change — 9am stays 9am. If you set one for a time the clocks skip, it fires
at the first minute that exists that morning and is back to its usual time
the next day.

### `GET /v1/reminders`

Your own live reminders, soonest first.

### `POST /v1/reminders/:id/action`

```bash
curl -X POST http://HOST:4847/v1/reminders/$ID/action \
  -H "Authorization: Bearer $KINAI_KEY" \
  -H "Content-Type: application/json" \
  -d '{"action":"delete"}'
```

| action | meaning |
|---|---|
| `ack` | done with this one. On a repeating reminder the next occurrence stays scheduled. |
| `snooze` | come back in `snooze_minutes` (default 10). Never moves a repeating series. |
| `stop` | end a repeating series. Different from `ack`. |
| `delete` | remove the row entirely. Idempotent. |

Note `cancel` is **not** a verb here — the one that removes a row is
`delete`, and the one that ends a series is `stop`.

---

## When it says no

Refusals are `422` with a stable `code` and a sentence. Branch on the code;
the sentence is for a human reading a log.

| code | meaning |
|---|---|
| `text_too_long` | over 600 characters |
| `empty` | nothing but whitespace |
| `too_soon` | less than a minute away |
| `too_far` | more than a year out |
| `in_past` | already gone, and not a repeating one |
| `bad_due_local` | unreadable time, including an hour the clocks skip |
| `no_when` | neither `in_minutes` nor `due_local` |
| `ambiguous_when` | both of them |
| `bad_repeat` | not one of the four words |

These codes are an API surface. Changing one is a breaking change.

Other statuses: `401` no or bad key, `404` no such reminder,
`429` you hold 200 unfinished reminders already. That last one is a
runaway guard rather than a quota — every reminder that fires is a
notification on your devices *and* a Telegram message, so a script stuck in
a loop spends the household's attention. Finish or delete some.

---

## Worth knowing

**LAN only.** The host listens on your local network. Do not port-forward
4847 to reach this from outside; use Telegram or a VPN.

**Validation is shared with the chat path.** The same code checks a
reminder whether it arrives from a cron job or from someone asking out
loud, so this route cannot be the careless way in.

**The log records that you wrote something, never what.** Reminder ids and
peer ids only — the text is yours.
