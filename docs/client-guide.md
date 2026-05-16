# Client guide

Joining a KinAI host as a family member. Estimated time: **60 seconds**.

## 1. Get an invite

Ask whoever runs the household host for one of:

- A **6-character code** like `pq3kx7`
- A **QR code** (open it on your phone camera)
- A **`kinai://join?…` link**

Invites are usually good for 30 days. Hosts can revoke at any time.

## 2. Install KinAI

Download the right bundle from the [Releases page](https://github.com/Gogo6969/kinai/releases). No model required — Clients are lightweight.

## 3. Connect

1. Open KinAI → **"Join an existing host"**.
2. Type your name (this is what shows up in the family conversation history).
3. Paste the code or link, or — if you're on the same Wi-Fi — pick the host from the **"hosts on this network"** list (mDNS magic).
4. Click **Connect**.

You should land in the main chat window immediately.

## 4. Use it

Two ways to chat:

- **Global hotkey** — press `Cmd+Space` (macOS) or `Ctrl+Space` (Win/Linux) from any app. A Spotlight-style overlay drops down. Ask. Get an answer. Press Esc to dismiss.
- **Full window** — open KinAI from the dock / start menu. Conversations grouped by thread on the left.

## 5. Switching hosts (e.g., between family + friend's KinAI)

Coming in v1.0: a multi-host switcher. For now, **Settings → Disconnect** and paste the other invite.

## 6. Privacy

The Client app stores **only** your invite JWT, your display name, and your local conversation cache. The host has the full conversation history; if you want to wipe yours locally, delete `~/.kinai/kinai.db` on your device.

## 7. Troubleshooting

| Symptom | Likely cause |
|---|---|
| "invite rejected: invite revoked" | Host revoked your access. Ask for a new invite. |
| "invite rejected: jwt: ExpiredSignature" | Invite is past its expiry. Ask for a new one. |
| Overlay doesn't appear on hotkey | Another app stole the shortcut. Pick a new one in Settings. |
| "host not reachable" | The host's machine is asleep or the LAN connection dropped. Wake it / reconnect to the Wi-Fi. |
| Tool button stays spinning | Tool ran but the model didn't follow up — usually a too-small model. Ask the host to switch to a bigger one. |
