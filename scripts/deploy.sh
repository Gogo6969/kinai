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

# Apple code-sign + notarize.
#
# We do NOT let `tauri build` codesign. On a developer Mac with iCloud
# Drive enabled (the default), the `bird` daemon scans new `.app`
# directories and re-attaches `com.apple.FinderInfo` +
# `com.apple.fileprovider.fpfs#P` xattrs faster than we can strip them
# — and codesign refuses to sign a bundle that has those ("resource
# fork, Finder information, or similar detritus not allowed"). The
# only reliable workaround is to do the signing OUTSIDE any iCloud-
# watched directory.
#
# So the flow on macOS is:
#   1. Tauri builds an UNSIGNED .app under target/release/bundle/macos/
#      (we deliberately leave APPLE_SIGNING_IDENTITY out of its env)
#   2. We copy that .app to /tmp/kinai-sign/ (bird ignores /tmp)
#   3. We xattr-strip + codesign + notarytool + staple there
#   4. install_macos picks up the signed/stapled .app from /tmp
APPLE_ENABLED=0
if [[ -f "$HOME/.kinai/keys/apple.env" ]]; then
  # shellcheck disable=SC1091
  source "$HOME/.kinai/keys/apple.env"
  if [[ -n "${APPLE_API_KEY_PATH:-}" && -f "$APPLE_API_KEY_PATH" ]]; then
    APPLE_ENABLED=1
    echo "→ Apple sign + notarize enabled (identity: $APPLE_SIGNING_IDENTITY)"
    echo "  notarization will add ~30–90s after build"
    # Belt-and-suspenders: stop macOS attaching `._appendix` files when
    # we copy bundle contents around inside the script.
    export COPYFILE_DISABLE=1
    # Keep tauri-bundler's own codesign attempt OUT of the build — we
    # do it ourselves below in a /tmp working dir.
    unset APPLE_SIGNING_IDENTITY APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD
    unset APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID_FOR_TAURI 2>/dev/null || true
    # Re-load just the values we'll use ourselves AFTER tauri build.
    # shellcheck disable=SC1091
    source "$HOME/.kinai/keys/apple.env"
  else
    echo "⚠ apple.env loaded but APPLE_API_KEY_PATH is missing or unreadable"
  fi
else
  echo "ℹ no $HOME/.kinai/keys/apple.env — builds will be unsigned (Gatekeeper warning on install)"
fi

# Run tauri build. On macOS we explicitly skip the DMG bundle (Tauri's
# bundle_dmg.sh hangs on Sonoma+; we build the DMG ourselves with hdiutil
# inside install_macos) AND we explicitly request `updater` so the
# signed .app.tar.gz + .sig sidecar are produced — those are what the
# host-distributed update flow serves to clients. On other platforms we
# let tauri produce everything.
LOG=$(mktemp)
if [[ "$OS" == "Darwin" ]]; then
  # Empty APPLE_SIGNING_IDENTITY for the duration of this command so
  # tauri produces an unsigned .app. We'll sign it ourselves below.
  env -u APPLE_SIGNING_IDENTITY pnpm tauri build --bundles app,updater > "$LOG" 2>&1
else
  pnpm tauri build > "$LOG" 2>&1
fi
BUILD_EXIT=$?
tail -10 "$LOG" || true
rm -f "$LOG"
if [[ $BUILD_EXIT -ne 0 ]]; then
  echo "✗ tauri build failed (exit $BUILD_EXIT)" >&2
  exit "$BUILD_EXIT"
fi

# ---------- post-build: macOS sign + notarize ----------------------------
#
# Everything from here runs in /tmp/kinai-sign so iCloud's `bird` daemon
# can't re-attach the xattrs that break codesign. install_macos reads
# from this path instead of target/release/bundle/macos/.
SIGNED_APP=""
if [[ "$OS" == "Darwin" && "$APPLE_ENABLED" == "1" ]]; then
  SIGN_DIR=/tmp/kinai-sign
  echo "→ copying .app to $SIGN_DIR for signing (escapes iCloud xattr daemon)"
  rm -rf "$SIGN_DIR"
  mkdir -p "$SIGN_DIR"
  cp -R target/release/bundle/macos/KinAI.app "$SIGN_DIR/"

  echo "→ stripping xattrs + codesigning with hardened runtime"
  xattr -cr "$SIGN_DIR/KinAI.app"
  codesign --force --options runtime --timestamp --deep \
    --sign "$APPLE_SIGNING_IDENTITY" \
    "$SIGN_DIR/KinAI.app"
  codesign --verify --strict --verbose=2 "$SIGN_DIR/KinAI.app" 2>&1 | tail -3

  echo "→ submitting to Apple notary (wait up to 10 min)"
  /usr/bin/ditto -c -k --sequesterRsrc --keepParent \
    "$SIGN_DIR/KinAI.app" "$SIGN_DIR/KinAI.zip"
  xcrun notarytool submit "$SIGN_DIR/KinAI.zip" \
    --key "$APPLE_API_KEY_PATH" \
    --key-id "$APPLE_API_KEY" \
    --issuer "$APPLE_API_ISSUER" \
    --wait --timeout 600 2>&1 | tail -6

  echo "→ stapling ticket"
  xcrun stapler staple "$SIGN_DIR/KinAI.app" 2>&1 | tail -2
  spctl --assess --type execute --verbose=2 "$SIGN_DIR/KinAI.app" 2>&1 | tail -2

  # Replace the unsigned bundle output with the signed/stapled one so
  # the rest of the script (DMG, updater tarball, install) picks up the
  # right files automatically.
  rm -rf target/release/bundle/macos/KinAI.app
  cp -R "$SIGN_DIR/KinAI.app" target/release/bundle/macos/KinAI.app
  # And rebuild the updater tarball + signature from the signed .app,
  # since tauri's earlier (unsigned-build) tarball is stale.
  #
  # COPYFILE_DISABLE=1 is CRITICAL here. Without it, macOS BSD `tar`
  # silently includes `._*` AppleDouble metadata files alongside every
  # real entry — Tauri's updater unpacks via Rust's `tar` crate, which
  # then tries to write `._KinAI.app` next to `KinAI.app` and fails
  # with the user-visible error "failed to unpack". Don't remove.
  if [[ -n "${TAURI_SIGNING_PRIVATE_KEY:-}" ]]; then
    echo "→ re-tarring + minisigning signed .app for updater"
    COPYFILE_DISABLE=1 tar -C "$SIGN_DIR" -czf "$SIGN_DIR/KinAI.app.tar.gz" KinAI.app
    pnpm tauri signer sign \
      --private-key "$TAURI_SIGNING_PRIVATE_KEY" --password "" \
      "$SIGN_DIR/KinAI.app.tar.gz" >/dev/null
    cp "$SIGN_DIR/KinAI.app.tar.gz"     target/release/bundle/macos/KinAI.app.tar.gz
    cp "$SIGN_DIR/KinAI.app.tar.gz.sig" target/release/bundle/macos/KinAI.app.tar.gz.sig
  fi
  SIGNED_APP="$SIGN_DIR/KinAI.app"
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
