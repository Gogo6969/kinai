# Announcing maintenance of a model server

A model server (a Mac, a Linux box, a GPU machine) gets restarted for tests,
updates and engine changes. If KinAI is not told, it finds out the hard way:
a failed probe, a failed request, a slower first answer, or an error.

This call tells the host **before** the server goes away. For the window you
give, every model slot (and the image reader) on that server is treated as
down at once, the family is answered by the next model at home, and each
answer starts with one line saying so. When the window ends, KinAI goes back
to the server by itself.

It only touches runtime state. It never edits `config.toml`, never changes a
slot's settings, and never needs KinAI to restart.

## What the family sees

* **A backup model answers:** the first line of the reply reads, for example,
  "The main model is being serviced right now (planned maintenance, back
  around 16:45). Answering with the backup model — … — instead."
* **No model at home answers:** one message says so and explains that the
  online model can be chosen by hand with `/online` (it answers from the
  internet, so what is typed leaves the home network). KinAI never switches to
  the online model by itself.
* **A picture arrives during the window:** it goes to the backup model only if
  that model can read pictures; otherwise the reply says so instead of
  answering blind.

## Getting a key

Open **Manage family → Reminder API key → Create API key** on the host. The
same kind of key works here. A family chat token is refused (403).

## Calls

All calls need `Authorization: Bearer <key>`. A script on the host machine can
use `http://127.0.0.1:4847`.

**Start (or renew) a window**

```
POST /v1/maintenance
{"endpoint": "http://<server-host>:<port>", "minutes": 45, "reason": "engine test"}
```

* `endpoint`: any URL or `host:port` of the server — a slot's base URL is the
  natural choice. Matched by host and port, so every slot and the image reader
  on that server move together.
* `minutes`: 1 to 240, default 30. Or send `"until": "<RFC 3339 time>"`
  instead (not both).
* `reason`: free text for the status call and the log; never shown to the
  family.
* An address that no slot or vision endpoint uses is refused with 422, and the
  answer lists the addresses that are in use.
* The reply lists what the window affects (`fast`, `balanced`, …, `vision`)
  and the minutes left.

**Look**

```
GET /v1/maintenance
```

**End early**

```
DELETE /v1/maintenance?endpoint=http://<server-host>:<port>
DELETE /v1/maintenance            # every window
```

A window ends by itself at its deadline even if nobody calls "end", so a
forgotten call cannot leave the house on the backup model.

## Example: around a restart

```sh
KINAI=http://127.0.0.1:4847
SERVER=http://<server-host>:<port>
AUTH="Authorization: Bearer $KINAI_API_KEY"

curl -fsS -X POST "$KINAI/v1/maintenance" -H "$AUTH" \
  -H 'Content-Type: application/json' \
  -d "{\"endpoint\": \"$SERVER\", \"minutes\": 45, \"reason\": \"restart\"}"

# ... restart the server, wait until it answers ...

curl -fsS -X DELETE "$KINAI/v1/maintenance?endpoint=$SERVER" -H "$AUTH"
```

Give the window more time than the work needs. If the work runs long, call
start again to renew it.

## Limits

* Runtime only: if KinAI itself restarts during a window, the window is gone
  (the next probe finds the server down anyway).
* A server that accepts connections but never answers is not detected by this
  call; announce it, or wait for the health probe.
