# Security Policy

Thank you for taking the time to look at KinAI's security.

KinAI runs entirely on your own hardware — no servers, no cloud, no
accounts, no telemetry. The threat model is narrower than a cloud
service, but real: a compromised KinAI install could read your family's
chat history, control which LLM your prompts are sent to, or leak data
over your LAN. We take any vulnerability that breaks those properties
seriously.

## Reporting a vulnerability

**Preferred channel — GitHub's private vulnerability reporting:**

[Open a private advisory →](https://github.com/Gogo6969/kinai/security/advisories/new)

This sends the report directly to the maintainers, keeps it invisible
to the public until a fix ships, and lets us collaborate with you on a
patch in a private branch.

**Fallback — email:** `vidfame@me.com` with subject line starting with
`[KinAI security]`.

**Do not** open a public GitHub issue or PR for security problems.
Public disclosure before a patch lands puts existing KinAI users at
risk.

## What to include

A useful report typically has:

- A short description of the issue and its impact.
- KinAI version (`Settings → version chip`, or the SHA of the commit).
- Operating system + arch (e.g. macOS 14.5 Apple Silicon, Windows 11 x64).
- Reproduction steps. A minimal proof-of-concept is gold; even a clear
  written walkthrough is fine.
- Whether the issue is exploitable across the LAN, only locally on
  the host, or only with a forged JWT / invite code.
- Any thoughts on mitigation if you have them.

You do not need to have a CVE number, a patch, or a write-up ready.
A two-line report is better than no report.

## What happens next

- **Acknowledgement:** within 7 days, usually faster.
- **Triage:** we confirm the issue, scope the impact, and agree on a
  fix timeline with the reporter.
- **Patch:** depending on severity, fixes land in the next release
  (typical cadence is multiple releases per week). Critical issues get
  an out-of-band release.
- **Coordinated disclosure:** the advisory is published after the
  patched version is available on the Releases page. We credit
  reporters by name (or anonymously, your choice).

KinAI is a small open-source project — no bug bounty, no contractual
SLA. We will do our best, in the open, and ship fixes that everyone
can verify.

## Supported versions

Only the latest release on the
[Releases page](https://github.com/Gogo6969/kinai/releases) is
supported with security fixes. Auto-update is enabled by default and
pulls patches within the hour the host is reachable, so most users are
on the latest already.

| Version | Supported          |
| ------- | ------------------ |
| latest  | :white_check_mark: |
| older   | :x:                |

## Security properties KinAI tries to uphold

- **Per-peer privacy.** A family member's chat history must never
  surface in another member's prompt context. All DB queries scope on
  `peer_id`; any path that breaks this scope is a security bug.
- **JWT-bound transport.** Every WebSocket message between client and
  host is JWT-authenticated. Any path that lets an unauthenticated
  peer read or write another peer's data is a security bug.
- **Updater signature verification.** Tauri's minisign signature
  guards every auto-update bundle. The signing key is committed in
  `tauri.conf.json`'s `pubkey` field; the private key is held only by
  the project maintainers. Any path that lets a forged / unsigned
  bundle install on a client is a security bug.
- **Sensitive material outside the repo.** API keys, Apple notary
  credentials, the updater private key, and per-host JWT secrets all
  live in `~/.kinai/keys/` and never in version control. Leaks via
  logs, error messages, or prompt-debug snapshots are security bugs.
- **LAN-only by default.** The host binds to `0.0.0.0:4847` for LAN
  use; it is not intended to be reachable from the public internet.
  Bugs that let unauthenticated traffic from the internet reach the
  host are security bugs.

Thanks for helping keep KinAI safe for the families that run it.
