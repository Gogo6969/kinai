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
- [ ] **Persistent memory still treats one fact as one fact.** On any
      one slot: ask KinAI to remember something under a key written with
      capitals and a space, then ask it to remember a *different* value
      for the same thing spelled another way (lowercase, or hyphenated).
      Settings → Memory must show **one** row holding the second value,
      not two rows. Then ask it to forget it using a third spelling and
      confirm the row is gone.

      `(peer_id, key)` is the uniqueness constraint on `user_facts`, and
      for a long time only the passive extractor normalized the key: the
      `remember` / `forget` tools and both Settings → Memory paths (the
      host command and the client's WS handler) wrote whatever they were
      handed, so three spellings of one fact became three rows. The
      `remember` tool's own description promises the opposite — "calling
      remember with the same key OVERWRITES" — so the breakage was
      silent: contradictory values all got injected into the prompt as
      authoritative, and `forget` cleared only the one spelling it was
      given. Normalization now lives in `db::user_facts`, the single
      point of write, and unit tests cover it; this step is what catches
      a caller that bypasses it again, which is the only way it can come
      back.

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
- [ ] **Mirror the manifest to the `updates` branch — by hand, every time.**
      Do not assume publishing did it for you:
      ```
      gh workflow run publish-manifest.yml
      ```
      Safe to run unconditionally: it defaults to the latest published
      release, refuses to roll backwards, and stops with "branch already
      serves this manifest" if there is nothing to do. So run it even when
      the `release: published` trigger looks like it fired.

      *Why by hand.* A `release` event runs the workflow file **as it exists
      at the tagged commit** — for a release, `GITHUB_SHA` is the last commit
      in the tagged release, not the tip of `main`. A release cut from a
      commit that does not contain `.github/workflows/publish-manifest.yml`
      therefore cannot start that run, and GitHub says nothing about it: no
      run, no failure, no notice. That is exactly how 0.2.122 and 0.2.123
      published with the branch still serving 0.2.121. Landing a workflow on
      `main` does **not** put it on a tag that branched earlier.
- [ ] **Primary endpoint** serves the new version. Watch the dispatched run
      to `success`, then:
      `curl -sL https://raw.githubusercontent.com/Gogo6969/kinai/updates/latest.json | jq -r .version`
      It must print the version you just published. A short lag here is
      raw.githubusercontent's CDN (~5 min TTL), not a failure — the
      workflow's last step already retries for two minutes. A version that
      never moves is a real problem: nothing built from 0.2.124 on will see
      the release until the branch does.
- [ ] **Fallback endpoint** serves the new version — installs built before
      0.2.124 poll this one, and will until every device has rolled over:
      `curl -sL https://github.com/Gogo6969/kinai/releases/latest/download/latest.json | jq -r .version`

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
