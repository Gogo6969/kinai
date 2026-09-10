# `updates` — the updater's manifest endpoint

This branch is data, not source. It holds one file:

    latest.json

which is a verbatim copy of the `latest.json` asset from the newest published
release. KinAI's updater polls it here:

    https://raw.githubusercontent.com/Gogo6969/kinai/updates/latest.json

## Why it isn't served from the release

The updater used to poll `releases/latest/download/latest.json`. That URL
redirects to whichever release is newest, so every poll from every install
incremented that release's asset download counter — 2,245 of the repo's first
3,334 recorded downloads were update checks rather than downloads.

Serving the manifest from a plain file keeps the two apart:

| | goes to | counted as a download |
|---|---|---|
| manifest poll | this branch | no |
| the update itself | the release asset the manifest names | yes |

The manifest's payload URLs are tag-pinned (`releases/download/vX.Y.Z/...`),
never `/latest/`, so the bundles are still fetched from the release exactly as
before. An `app.tar.gz` download now means one install genuinely updated.

## Do not edit this branch by hand

`.github/workflows/publish-manifest.yml` rewrites `latest.json` whenever a
release is published, after re-checking that the manifest names all four
platform families and that every entry is signed. A hand-edit is either
overwritten by the next release or, worse, survives and points installs at
something that was never validated.

Installs built before 0.2.123 poll the old release URL and will keep working —
that endpoint remains as the fallback until every device has rolled over.
