#!/usr/bin/env bash
# scripts/deploy.sh — build KinAI for the current OS and (on macOS) install
# the .app into /Applications. Other platforms print the install command.
#
# This is a MAINTAINER convenience. End users install from the bundle they
# downloaded from the project's Releases page; they don't need this script.
#
# Usage:
#   ./scripts/deploy.sh             # patch bump + build + install
#   ./scripts/deploy.sh minor
#   ./scripts/deploy.sh major
#   ./scripts/deploy.sh skip-bump   # don't change version, just rebuild
#   ./scripts/deploy.sh 0.3.0       # explicit version

set -euo pipefail

cd "$(dirname "$0")/.."

# ---------- per-OS install functions (declared up front) ------------------

install_macos() {
  local SRC=target/release/bundle/macos/KinAI.app
  if [[ ! -d "$SRC" ]]; then
    echo "✗ build produced no .app at $SRC; aborting" >&2
    exit 1
  fi

  echo "→ stopping any running KinAI"
  pkill -9 -f '/Applications/KinAI.app' 2>/dev/null || true
  pkill -9 -f 'target/release/kinai' 2>/dev/null || true
  pkill -9 -x kinai 2>/dev/null || true
  sleep 1

  echo "→ installing to /Applications/KinAI.app"
  rm -rf /Applications/KinAI.app
  ditto "$SRC" /Applications/KinAI.app

  local INSTALLED_VER
  INSTALLED_VER=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" \
                  /Applications/KinAI.app/Contents/Info.plist)
  echo "  ✓ installed CFBundleShortVersionString = $INSTALLED_VER"
  if [[ "$INSTALLED_VER" != "$NEW_VERSION" ]]; then
    echo "  ⚠ version mismatch — built $NEW_VERSION but installed $INSTALLED_VER" >&2
    exit 1
  fi

  # Arch suffix for the DMG name so cross-arch builds don't clobber.
  local ARCH
  ARCH=$(uname -m)
  case "$ARCH" in
    arm64)   ARCH=aarch64 ;;
    x86_64)  ARCH=x86_64 ;;
  esac

  echo "→ producing DMG"
  local DMG="target/release/bundle/dmg/KinAI_${NEW_VERSION}_${ARCH}.dmg"
  mkdir -p target/release/bundle/dmg
  local STAGE
  STAGE=$(mktemp -d)
  cp -R "$SRC" "$STAGE/"
  ln -s /Applications "$STAGE/Applications"
  hdiutil create -volname "KinAI" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
  rm -rf "$STAGE"

  # Stage the signed update tarball where the host can serve it. Tauri's
  # macOS bundler emits `<app>.app.tar.gz` plus `.sig` next to the .app
  # when TAURI_SIGNING_PRIVATE_KEY[_PATH] is set. The host reads these
  # from a stable per-version directory under ~/.kinai/updates/.
  local TARBALL_SRC="target/release/bundle/macos/KinAI.app.tar.gz"
  local SIG_SRC="target/release/bundle/macos/KinAI.app.tar.gz.sig"
  if [[ -f "$TARBALL_SRC" && -f "$SIG_SRC" ]]; then
    local UPDATE_DIR="$HOME/.kinai/updates/${NEW_VERSION}/darwin-${ARCH}"
    mkdir -p "$UPDATE_DIR"
    cp "$TARBALL_SRC" "$UPDATE_DIR/KinAI.app.tar.gz"
    cp "$SIG_SRC"     "$UPDATE_DIR/KinAI.app.tar.gz.sig"
    # Also overwrite a "latest" symlink so the host's manifest endpoint
    # always points at the freshest version without scanning the tree.
    ln -sfn "${NEW_VERSION}" "$HOME/.kinai/updates/latest-darwin-${ARCH}"
    echo "  ✓ staged signed update at $UPDATE_DIR"
  else
    echo "  ⚠ no signed tarball at $TARBALL_SRC (signing key missing?). Clients won't auto-update from this host."
  fi

  # Keep a permanent copy of every released DMG in releases/ so future
  # debugging ("install 0.1.34 to compare") doesn't require rebuilding —
  # tauri-bundler clears target/release/bundle/dmg/ on each fresh build.
  mkdir -p releases
  cp "$DMG" "releases/KinAI_${NEW_VERSION}_${ARCH}.dmg"
  echo "  ✓ archived to releases/KinAI_${NEW_VERSION}_${ARCH}.dmg"

  ls -lh /Applications/KinAI.app/Contents/MacOS/kinai "$DMG"
  echo "✓ done — open /Applications/KinAI.app"
}

install_linux() {
  echo "→ bundle output:"
  ls -lh target/release/bundle/deb/*.deb 2>/dev/null || true
  ls -lh target/release/bundle/appimage/*.AppImage 2>/dev/null || true
  ls -lh target/release/bundle/rpm/*.rpm 2>/dev/null || true
  echo ""
  echo "  Install with one of:"
  echo "    Debian/Ubuntu: sudo dpkg -i target/release/bundle/deb/*.deb"
  echo "    AppImage:      chmod +x target/release/bundle/appimage/*.AppImage && ./<name>.AppImage"
  echo "    RPM:           sudo rpm -i target/release/bundle/rpm/*.rpm"
}

install_windows() {
  echo "→ bundle output:"
  ls -lh target/release/bundle/msi/*.msi 2>/dev/null || true
  ls -lh target/release/bundle/nsis/*.exe 2>/dev/null || true
  echo ""
  echo "  Double-click the .msi or .exe under target/release/bundle/ to install."
}

# ---------- main ----------------------------------------------------------

bump="${1:-patch}"
if [[ "$bump" != "skip-bump" ]]; then
  ./scripts/bump-version.sh "$bump"
fi

NEW_VERSION=$(grep -E '^version = "' Cargo.toml | head -1 | sed -E 's/.*"([^"]+)".*/\1/')
OS="$(uname -s)"
echo "→ building KinAI v$NEW_VERSION on $OS"

# Updater signing — Tauri reads these env vars at build time. The private
# key lives outside the repo (~/.kinai/keys/updater.key); the public key
# is committed in tauri.conf.json. Without these, tauri build still
# produces the .app but won't write the .sig sidecar needed by host-
# distributed updates.
if [[ -f "$HOME/.kinai/keys/updater.key" ]]; then
  # Tauri reads the key from the *contents* of TAURI_SIGNING_PRIVATE_KEY
  # — passing the path won't sign the bundle (silent failure at the
  # very end of the build, after the .tar.gz is already written).
  export TAURI_SIGNING_PRIVATE_KEY
  TAURI_SIGNING_PRIVATE_KEY="$(cat "$HOME/.kinai/keys/updater.key")"
  export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
  echo "→ updater signing enabled (key file: $HOME/.kinai/keys/updater.key)"
else
  echo "⚠ no updater signing key at ~/.kinai/keys/updater.key — bundles won't be signed"
fi

# Run tauri build. On macOS we explicitly skip the DMG bundle (Tauri's
# bundle_dmg.sh hangs on Sonoma+; we build the DMG ourselves with hdiutil
# inside install_macos) AND we explicitly request `updater` so the
# signed .app.tar.gz + .sig sidecar are produced — those are what the
# host-distributed update flow serves to clients. On other platforms we
# let tauri produce everything.
LOG=$(mktemp)
if [[ "$OS" == "Darwin" ]]; then
  pnpm tauri build --bundles app,updater > "$LOG" 2>&1
else
  pnpm tauri build > "$LOG" 2>&1
fi
BUILD_EXIT=$?
tail -10 "$LOG" || true
rm -f "$LOG"
if [[ $BUILD_EXIT -ne 0 ]]; then
  echo "✗ tauri build failed (exit $BUILD_EXIT)" >&2
fi

case "$OS" in
  Darwin)  install_macos ;;
  Linux)   install_linux ;;
  MINGW*|MSYS*|CYGWIN*)
           install_windows ;;
  *)       echo "Unsupported OS for auto-install: $OS"
           echo "Bundle output lives under target/release/bundle/; ship that manually."
           ;;
esac
