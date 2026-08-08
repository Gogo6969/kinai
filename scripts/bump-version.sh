#!/usr/bin/env bash
# scripts/bump-version.sh — bump KinAI's version in every manifest.
#
# Portable: works with BSD sed (macOS default) AND GNU sed (Linux default).
# The two flavours have incompatible `-i` syntax; we side-step that with a
# tiny perl-based in-place edit.
#
# Usage:
#   ./scripts/bump-version.sh             # patch bump (0.1.5 → 0.1.6)
#   ./scripts/bump-version.sh minor       # 0.1.5 → 0.2.0
#   ./scripts/bump-version.sh major       # 0.1.5 → 1.0.0
#   ./scripts/bump-version.sh 0.3.0       # explicit version

set -euo pipefail

cd "$(dirname "$0")/.."

CARGO=Cargo.toml
TAURI=tauri.conf.json
PKG=frontend/package.json

current=$(grep -E '^version = "' "$CARGO" | head -1 | sed -E 's/.*"([^"]+)".*/\1/')
IFS='.' read -r major minor patch <<< "$current"

case "${1:-patch}" in
  major)  new="$((major + 1)).0.0" ;;
  minor)  new="${major}.$((minor + 1)).0" ;;
  patch)  new="${major}.${minor}.$((patch + 1))" ;;
  *)      if [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
            new="$1"
          else
            echo "error: '$1' is not a valid semver or bump-kind (patch|minor|major)" >&2
            exit 1
          fi
          ;;
esac

echo "→ $current → $new"

# Use perl for in-place edits — works identically on macOS and Linux without
# the -i '' vs -i quirk of sed.
perl -pi -e "s/^version = \"\Q$current\E\"/version = \"$new\"/" "$CARGO"
perl -pi -e "s/\"version\": \"\Q$current\E\"/\"version\": \"$new\"/" "$TAURI"
perl -pi -e "s/\"version\": \"\Q$current\E\"/\"version\": \"$new\"/" "$PKG"

echo "✓ $CARGO"
echo "✓ $TAURI"
echo "✓ $PKG"

# Cargo.lock records the workspace crate's own version, so it goes stale the
# moment Cargo.toml moves. Nothing here used to update it: it was corrected
# only as a side effect of whoever ran the next `cargo build`, which meant a
# tag could carry a lock still naming the previous version (and CI would then
# build with a dirty tree). Refresh it here so the bump commit is complete.
if cargo update -p kinai --precise "$new" --offline >/dev/null 2>&1; then
  echo "✓ Cargo.lock"
else
  # --offline fails on a cold registry cache; a metadata read rewrites the
  # lock just as well and needs no network for a path-local version bump.
  cargo metadata --format-version 1 >/dev/null 2>&1 || true
  if grep -q "^version = \"$new\"" <(awk '/^name = "kinai"$/{f=1} f&&/^version/{print; exit}' Cargo.lock); then
    echo "✓ Cargo.lock"
  else
    echo "⚠ Cargo.lock still not at $new — run 'cargo check' before committing" >&2
  fi
fi
