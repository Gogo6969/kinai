# KinAI Release Checklist

Every release follows these steps **in order, no skipping**. If any step
fails, stop — fix, then restart from step 1. One release in flight at a
time: never start version N+1's pipeline while N is between "tag pushed"
and "published".

## 1 — Before building

- [ ] All changes committed to the working tree you intend to ship (no
      "I'll add one more fix mid-pipeline"; a new fix = restart checklist).
- [ ] `cargo test --lib` — all green.
- [ ] `cargo test --no-run` — exit 0. `--lib` does NOT compile `tests/*.rs`,
      and neither does release.yml (`cargo test --lib --quiet`), so both
      gates stayed green from 0.2.107 to 0.2.111 while the integration
      tests had not compiled since a `system_prompt` signature change.
      `build.yml` (push to main) is the only job that catches it, and its
      failures are easy to miss because no release depends on them.
- [ ] `cd frontend && pnpm check` — no **new** errors vs main.
- [ ] New/changed features exercised end-to-end in a running instance
      (dev mock scenario, live test, or the installed app — "tests pass"
      alone is not "tested"). Client-visible features must be verified in
      a client-mode simulation (`?mock=…` scenarios) or on a real client.
- [ ] No stray dev servers or builds running (`lsof -iTCP:1420`); build
      on a quiet machine.
- [ ] Version bumped in `Cargo.toml`, `tauri.conf.json`,
      `frontend/package.json`, **`Cargo.lock`** + CHANGELOG entry dated
      today. `bump-version.sh` now refreshes the lock; verify it did
      (`awk '/^name = "kinai"$/{f=1} f&&/^version/{print; exit}' Cargo.lock`).
      Before 0.2.95 the lock was updated only as a side effect of the next
      build, so a tag could ship naming the previous version.
- [ ] Every gate must show **positive evidence it ran** — an exit code and
      a completion line, never the mere absence of matches. `pnpm check`
      was reported clean for several releases while never executing: it
      was wrapped in `timeout`, which does not exist on macOS, so it
      exited 127 and the grep for "ERROR" then found nothing. Two real
      type errors sat behind that. Record `exit=$?` and the tool's own
      summary line ("N ERRORS"), not a grep count.

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
- [ ] **Test EVERY configured model slot, not just `fast`.** Send one
      real message on each of `/fast`, `/balanced`, `/deep` and
      `/online` (skip only the slots that are genuinely unconfigured)
      and get an answer on each. Then send **one live-data question**
      (e.g. "what are the top 10 holdings in QQQ today?") on every slot
      that is configured, because that path differs: it forces a web
      search, and forcing is the part providers disagree about.

      the owner's rule, 2026-08-12, after 0.2.98 shipped with `/online`
      broken for exactly this: the forced-search round sends
      `tool_choice: "required"`, which llama.cpp and vLLM honour and
      DeepSeek's thinking mode rejects with a 400. Every live-data
      question on `/online` failed, and testing `fast` alone could never
      have found it — the slots run against different providers, so one
      slot answering proves nothing about the others.
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

- [ ] **Intel Mac has no `stage-*` command — stage it by hand.** There is
      a `stage-windows` and a `stage-linux`, and nothing for
      `darwin-x86_64`, so it is the one family that silently keeps serving
      the *previous* version while everything else moves:
      ```
      gh release download vX.Y.Z -p "KinAI_x64.app.tar.gz" -p "KinAI_x64.app.tar.gz.sig" -D /tmp/i
      mkdir -p ~/.kinai/updates/X.Y.Z/darwin-x86_64
      cp /tmp/i/KinAI_x64.app.tar.gz     ~/.kinai/updates/X.Y.Z/darwin-x86_64/KinAI.app.tar.gz
      cp /tmp/i/KinAI_x64.app.tar.gz.sig ~/.kinai/updates/X.Y.Z/darwin-x86_64/KinAI.app.tar.gz.sig
      ```
- [ ] `./scripts/deploy.sh stage-windows` and `stage-linux`.
      **Stage BEFORE bumping to the next version.** Both commands read the
      version from the working tree, so once you bump, they target the new
      (unpublished) version and silently warn instead of staging — the
      release you just published never reaches Windows/Linux family
      devices. If that happens, don't back-stage: ship the newer version
      and let those platforms jump to it.
- [ ] Confirm the host actually serves the new version to **all four**
      targets — this is the check that catches a family nobody staged:
      ```
      for t in darwin-aarch64 darwin-x86_64 windows-x86_64 linux-x86_64; do
        printf "  %-16s %s\n" "$t" \
          "$(curl -s "localhost:4847/v1/update/manifest?target=$t" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("version","-"))')"
      done
      ```
- [ ] X post on @Gogo6969 (short + catchy + screenshot; GitHub/website
      links go in a **reply**, never in tweet 1) — only after step 5's
      public-endpoint check passed. **At most ONE post per day**: on
      multi-release days, skip per-release posts and publish a single
      EVENING post covering the day's newest version (the owner's rule,
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
