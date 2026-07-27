# KinAI Release Checklist

Every release follows these steps **in order, no skipping**. If any step
fails, stop — fix, then restart from step 1. One release in flight at a
time: never start version N+1's pipeline while N is between "tag pushed"
and "published".

## 1 — Before building

- [ ] All changes committed to the working tree you intend to ship (no
      "I'll add one more fix mid-pipeline"; a new fix = restart checklist).
- [ ] `cargo test --lib` — all green.
- [ ] `cd frontend && pnpm check` — no **new** errors vs main.
- [ ] New/changed features exercised end-to-end in a running instance
      (dev mock scenario, live test, or the installed app — "tests pass"
      alone is not "tested"). Client-visible features must be verified in
      a client-mode simulation (`?mock=…` scenarios) or on a real client.
- [ ] No stray dev servers or builds running (`lsof -iTCP:1420`); build
      on a quiet machine.
- [ ] Version bumped in `Cargo.toml`, `tauri.conf.json`,
      `frontend/package.json` + CHANGELOG entry dated today.

## 2 — Build + install on host

- [ ] `./scripts/deploy.sh skip-bump` — wait for it to **fully exit**
      before touching anything it writes (no relaunching the app while
      the installer is still copying files — this caused the 0.2.80
      "asset not found: index.html" window).
- [ ] **Never pipe deploy.sh.** Run it unpiped and check the status:
      `./scripts/deploy.sh skip-bump > /tmp/deploy.log 2>&1; echo "exit=$?"`.
      The script itself is careful (`set -euo pipefail`, and it aborts
      before installing anything), but a piped invocation such as
      `deploy.sh | tail -40` returns the **pipe's** exit status, not the
      script's — on 2026-07-25 a notarization that failed on a network
      timeout was reported as "exit code 0" and only caught by reading
      the log. Confirm the last line of the log is `✓ done`.
- [ ] `PlistBuddy -c 'Print :CFBundleShortVersionString' /Applications/KinAI.app/Contents/Info.plist`
      shows the new version.
- [ ] `spctl --assess -vv --type execute /Applications/KinAI.app` says
      `Notarized Developer ID`.

## 3 — Smoke-test the installed app BEFORE the family sees it

- [ ] Relaunch host: `pkill -x kinai; sleep 2; open -a /Applications/KinAI.app`.
- [ ] Window renders (no blank window, no asset errors) — look at it.
- [ ] Host is listening: `lsof -nP -iTCP:4847 -sTCP:LISTEN`.
- [ ] Send one test message on `fast` in the host app and get an answer.
- [ ] Staged client bundle matches the build:
      `shasum -a 256 ~/.kinai/updates/<ver>/darwin-aarch64/KinAI.app.tar.gz`
      == `shasum -a 256 target/release/bundle/macos/KinAI.app.tar.gz`.
- [ ] Only now is the update allowed to reach family clients. After a
      client updates, confirm the release's headline feature works there.

## 4 — Tag + CI

- [ ] `git commit` + `git tag vX.Y.Z` + `git push origin main vX.Y.Z`.
- [ ] Find the run by branch — `gh run list --workflow=release.yml --json databaseId,headBranch --jq '.[] | select(.headBranch=="vX.Y.Z") | .databaseId'`
      — and watch **that** run. Never trust `gh run watch`'s exit code alone.

## 5 — Publish gate (all must hold)

- [ ] Run conclusion == `success` (assets present ≠ run succeeded).
- [ ] Draft release's `latest.json` contains **all four** platform
      families: `darwin-aarch64`, `darwin-x86_64`, `windows-x86_64`,
      `linux-x86_64`.
- [ ] `gh release edit vX.Y.Z --draft=false --latest`.
- [ ] Public endpoint serves the new version:
      `curl -sL https://github.com/Gogo6969/kinai/releases/latest/download/latest.json`.

## 6 — After publish

- [ ] `./scripts/deploy.sh stage-windows` and `stage-linux`.
      **Stage BEFORE bumping to the next version.** Both commands read the
      version from the working tree, so once you bump, they target the new
      (unpublished) version and silently warn instead of staging — the
      release you just published never reaches Windows/Linux family
      devices. If that happens, don't back-stage: ship the newer version
      and let those platforms jump to it.
- [ ] X post on @Gogo6969 (short + catchy + screenshot; GitHub/website
      links go in a **reply**, never in tweet 1) — only after step 5's
      public-endpoint check passed. **At most ONE post per day**: on
      multi-release days, skip per-release posts and publish a single
      EVENING post covering the day's newest version (Wolf's rule,
      2026-07-23).

## If a release must be amended

A tag may only be re-pointed while the release is still a draft **and**
no client anywhere (including family Macs via host staging) has
installed that version number. Once any machine has it, the fix ships
as a new version — the updater never re-offers an installed version.

## Keeping the project map current

`docs/kinai-map.html` + `docs/kinai-map.json` are generated, never edited
by hand:

```
python3 scripts/build-map.py
```

Re-run it when the shape of the app changes (new module, new table, new
protocol message) and after a release, so the map's history matches
CHANGELOG.md. Module descriptions come from each file's own `//!` doc
comment — the way to improve the map is to improve those.
