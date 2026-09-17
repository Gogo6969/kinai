# Host guide

Becoming the household's KinAI host. Estimated time: **3 minutes**, plus the time to download a model.

## 1. Prerequisites

- A laptop or desktop on your home network (Mac, Windows, or Linux).
- One of: [Ollama](https://ollama.com/), [LM Studio](https://lmstudio.ai/), [vLLM](https://docs.vllm.ai/), [llama.cpp server](https://github.com/ggerganov/llama.cpp), [Open WebUI](https://openwebui.com/), or any OpenAI-compatible local server.
- ≥ 8 GB free RAM if you plan to run a 7B-class model. 16 GB+ is comfortable. GPU strongly recommended for 13B+.

## 2. Install your LLM backend

The fastest path is Ollama:

```bash
# macOS
brew install ollama && ollama serve &
ollama pull llama3.1:8b

# Windows / Linux
# → https://ollama.com/download
ollama pull llama3.1:8b
```

LM Studio has a GUI: download, click "Start Server" on a chat model.

## 3. Install KinAI

Download the right bundle for your OS from the [Releases page](https://github.com/Gogo6969/kinai/releases):

- macOS: `KinAI_x.y.z_universal.dmg`
- Windows: `KinAI_x.y.z_x64.msi`
- Linux: `kinai_x.y.z_amd64.deb` or `KinAI_x.y.z_amd64.AppImage`

Open it. The welcome screen will appear.

## 4. Run the setup wizard

1. Click **"Host KinAI here"**.
2. KinAI auto-scans `localhost:11434`, `:1234`, `:8000`, `:8080`, `:8888` for known backends and lists them. Pick the one you want.
3. **Family settings** — pick a friendly name ("Smith Family"), keep the default port (`4847`), leave mDNS enabled so other devices on your network can discover you.
4. **Model settings** — confirm the model name and context window. Add a "system addendum" if you want house rules like *"Always answer in French"*.
5. **Tools** — toggle which tools the LLM can call. All four (web, X, calculator, datetime) are on by default.
6. Click **"Start hosting"**.

KinAI minimizes to the tray. The tooltip shows `KinAI · Host · 0 connected · llama3.1:8b`.

## 5. Invite the family

From the tray menu choose **"Create Invite…"** (or go to the Invite page in the app):

1. Type a label — "Mom's iPad", "Living-room TV", etc.
2. Set the expiry (default 30 days).
3. Click **Create invite**.

You'll see three ways to share:

- **6-character code** — easy for SMS or in-person.
- **QR code** — for phones.
- **`kinai://join?…` link** — for AirDrop / email / Slack.

All three contain the same signed JWT.

## 6. Use it

Press **`Cmd+Space`** (macOS) / **`Ctrl+Space`** (Win/Linux) from anywhere → KinAI overlay drops down → ask → done.

Open the main window to browse conversation history per family member.

## 7. Managing your family

Tray menu → **Manage Family** lists everyone who can reach your household — not only the devices switched on at that moment. Each row says whether it is connected, paused, or simply not on right now, so somebody whose laptop has been shut for a month is still there and still actionable.

Two buttons, and they mean different things:

- **Pause** ends the session and keeps that device out until you press Resume. The invite still works, so letting them back in needs no new code. Use it to stop a device for an afternoon.
- **Disconnect** ends the session *and* revokes the invite code. Coming back needs a brand-new invite. It cannot be undone, and the confirmation says so.

Invite codes are not shown on this page. They are working credentials, and that page tends to get screenshotted — the full list lives on the **Invites** page.

That page opens on the invites that still work. Press **Show all** to
include the revoked and expired ones, which are kept for about a week as a
record; a line under the list always says how many are hidden. Reminder
API keys stay in the list either way, marked **API key** — they are live
credentials, and this is where you revoke them.

The pencil beside a device's name renames it — your own note about whose
device it is, not anything the device knows about itself. Renaming changes
nothing about the credential: the 6-character code, the link and the QR
stay exactly as they were, so one already scanned or stuck on the fridge
keeps working. Once a device has connected, Manage family shows the name
that device calls itself and yours beside it; before it ever connects,
your name is all there is to show.

## 8. Things to know

- **Rate limit:** default 60 requests/min per peer. Adjust in Settings.
- **Encryption:** every connection presents an RS256-signed JWT; the keys live in `~/.kinai/keys/` with `0600` permissions. The transport is plain `ws://` on the local network for performance — put KinAI behind Tailscale or wireguard if you want WAN access.
- **Updates:** KinAI checks GitHub Releases every 6 hours and shows a notification when a new version is out. Click to install.
- **Storage:** everything lives in `~/.kinai/`. Backing that folder up = backing up KinAI.
- **Reminders from a script:** a cron job, a Shortcut or anything else can add reminders to your own calendar without going through a conversation. Create a key on the Invites page — see [the reminder API](reminders-api.md). It cannot open a chat, read anyone's messages, or write to another member's calendar.
