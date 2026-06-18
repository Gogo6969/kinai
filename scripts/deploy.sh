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

  # Pull the other platforms' updater bundles from the matching GitHub
  # release so the host serves every client (Mac/Windows/Linux) the same
  # version over the LAN. No-ops with a hint if the release CI hasn't
  # published yet — re-run `stage-windows` / `stage-linux` afterward.
  stage_windows_update
  stage_linux_update
}

# Pull the latest Windows updater bundle from GitHub and stage it
# alongside the Mac bundle, so Windows clients connecting to this host
# get updates pushed to them automatically — same flow Mac clients get.
#
# Source preference order:
#   1. GitHub Release with matching tag (vX.Y.Z) — permanent, ideal
#   2. Latest successful test-windows.yml workflow artifact — ephemeral
#      (14-day expiry), used during the "Windows not yet released" phase
#
# Skipped silently if neither source has anything for this version.
stage_windows_update() {
  local TARGET=windows-x86_64
  local STAGE_DIR="$HOME/.kinai/updates/${NEW_VERSION}/${TARGET}"
  local BUNDLE_NAME=KinAI.msi.zip
  local DEST="$STAGE_DIR/$BUNDLE_NAME"
  local SIG_DEST="$DEST.sig"

  if ! command -v gh >/dev/null 2>&1; then
    echo "ℹ skipping Windows update stage (gh CLI not installed)"
    return
  fi

  local REPO="${KINAI_REPO:-Gogo6969/kinai}"
  local TMP
  TMP=$(mktemp -d)

  echo "→ staging Windows updater bundle from $REPO"

  # Pull the Windows installer from the GitHub Release WITH THE MATCHING
  # TAG. This is the ONLY source we trust: the asset attached to
  # `vX.Y.Z` is, by construction, that exact version.
  #
  # We deliberately do NOT fall back to the latest test-windows.yml
  # artifact anymore. That fallback was the root cause of "Windows stuck
  # on an old version": test-windows.yml runs only on manual dispatch, so
  # its newest artifact lagged the release. deploy.sh then staged that
  # STALE .msi under the CURRENT version dir, and the host advertised
  # (say) 0.2.55 while serving a 0.2.48 installer — Windows Installer saw
  # the same version already installed and no-op'd, so the client looped
  # forever on the update banner.
  #
  # Since release.yml now builds + signs the Windows installer for every
  # tag, the matching-tag asset always exists AFTER the release CI runs.
  # deploy.sh itself runs BEFORE the push, so on the release machine this
  # will usually skip (the tag isn't published yet) — that's fine: it
  # just means no Windows bundle is advertised for the new version until
  # you re-stage. Re-run `./scripts/deploy.sh stage-windows` once the
  # release CI has published, or let a connected Windows client fall back
  # to GitHub.
  if gh release download "v${NEW_VERSION}" -R "$REPO" \
       --pattern '*-setup.exe' --pattern '*-setup.exe.sig' \
       --pattern '*_x64_en-US.msi' --pattern '*_x64_en-US.msi.sig' \
       --dir "$TMP" 2>/dev/null
  then
    echo "  ↳ pulled Windows installer from Release v${NEW_VERSION}"
  else
    echo "  ⚠ Release v${NEW_VERSION} has no Windows installer yet."
    echo "    (deploy runs before the push; re-run './scripts/deploy.sh stage-windows'"
    echo "     after the release CI publishes the Windows asset.)"
    rm -rf "$TMP"
    return
  fi

  # Tauri-bundler produces these Windows artifacts:
  #   *.msi        + *.msi.sig          (raw MSI + signature)
  #   *.msi.zip    + *.msi.zip.sig      (zipped wrapper + signature)
  #   *.exe        + *.exe.sig          (NSIS installer + signature)
  #   *.nsis.zip   + *.nsis.zip.sig     (zipped NSIS + signature)
  #
  # We used to stage the .msi.zip wrapper because the Tauri 1.x updater
  # required it. The Tauri 2.x updater (tauri-plugin-updater 2.10.x)
  # accepts the raw .msi directly via its extract_exe → infer::is_msi
  # path — and crucially, its zip-extraction path CANNOT decompress
  # DEFLATE because the plugin declares `zip = { default-features =
  # false }` with no compression backends enabled. Every Windows client
  # trying to install a .msi.zip update tripped "Unsupported Zip
  # Archive: Compression method not supported".
  #
  # So we stage the raw .msi now. Smaller change footprint, no zip
  # crate involved, works on every Tauri 2.x version regardless of
  # plugin internals.
  # Prefer the NSIS `-setup.exe` — it's the installer the Tauri updater
  # is built around on Windows, and (unlike an MSI) it always installs
  # over the existing version rather than consulting the MSI upgrade
  # table. Fall back to the raw `.msi` only if no NSIS artifact exists.
  local SRC_MSI SRC_MSI_SIG DEST_NAME
  SRC_MSI=$(find "$TMP" -name '*-setup.exe' -type f 2>/dev/null | head -1)
  SRC_MSI_SIG=$(find "$TMP" -name '*-setup.exe.sig' -type f 2>/dev/null | head -1)
  DEST_NAME="KinAI.exe"
  if [[ -z "$SRC_MSI" || -z "$SRC_MSI_SIG" ]]; then
    SRC_MSI=$(find "$TMP" -name '*.msi' -type f -not -name '*.zip' 2>/dev/null | head -1)
    SRC_MSI_SIG=$(find "$TMP" -name '*.msi.sig' -type f -not -name '*.zip.sig' 2>/dev/null | head -1)
    DEST_NAME="KinAI.msi"
  fi
  if [[ -z "$SRC_MSI" || -z "$SRC_MSI_SIG" ]]; then
    echo "  ⚠ Windows artifacts don't contain a .msi/.exe + .sig pair. Skipping."
    rm -rf "$TMP"
    return
  fi
  DEST="$STAGE_DIR/$DEST_NAME"
  SIG_DEST="$DEST.sig"

  mkdir -p "$STAGE_DIR"
  # Clear any previously-staged installer so the host serves exactly the
  # one we just fetched (the manifest resolver prefers .msi over .exe by
  # filename order — a leftover stale .msi would otherwise win).
  rm -f "$STAGE_DIR"/KinAI.msi "$STAGE_DIR"/KinAI.msi.sig \
        "$STAGE_DIR"/KinAI.exe "$STAGE_DIR"/KinAI.exe.sig
  cp "$SRC_MSI" "$DEST"
  cp "$SRC_MSI_SIG" "$SIG_DEST"
  ln -sfn "${NEW_VERSION}" "$HOME/.kinai/updates/latest-${TARGET}"
  rm -rf "$TMP"

  echo "  ✓ staged Windows update at $STAGE_DIR"
  echo "    ($(ls -lh "$DEST" | awk '{print $5}') raw $DEST_NAME + signature)"
}

# Pull the Linux updater bundle (gzipped AppImage) from the matching
# GitHub Release and stage it so the macOS host serves Linux clients
# their updates over the LAN — same model + same matching-tag-only trust
# rule as stage_windows_update. Run after the release CI publishes:
#   ./scripts/deploy.sh stage-linux
stage_linux_update() {
  local TARGET=linux-x86_64
  local STAGE_DIR="$HOME/.kinai/updates/${NEW_VERSION}/${TARGET}"

  if ! command -v gh >/dev/null 2>&1; then
    echo "ℹ skipping Linux update stage (gh CLI not installed)"
    return
  fi

  local REPO="${KINAI_REPO:-Gogo6969/kinai}"
  local TMP
  TMP=$(mktemp -d)

  echo "→ staging Linux updater bundle from $REPO"
  # Tauri's Linux updater artifact is the raw AppImage + its minisig:
  #   *_amd64.AppImage  +  *_amd64.AppImage.sig
  # (.deb/.rpm are install-only, not used by the in-app updater.)
  if gh release download "v${NEW_VERSION}" -R "$REPO" \
       --pattern '*.AppImage' --pattern '*.AppImage.sig' \
       --dir "$TMP" 2>/dev/null
  then
    echo "  ↳ pulled Linux AppImage updater bundle from Release v${NEW_VERSION}"
  else
    echo "  ⚠ Release v${NEW_VERSION} has no Linux updater bundle yet."
    echo "    (re-run './scripts/deploy.sh stage-linux' after the release CI publishes it)"
    rm -rf "$TMP"
    return
  fi

  local SRC SRC_SIG
  SRC=$(find "$TMP" -name '*.AppImage' -type f 2>/dev/null | head -1)
  SRC_SIG=$(find "$TMP" -name '*.AppImage.sig' -type f 2>/dev/null | head -1)
  if [[ -z "$SRC" || -z "$SRC_SIG" ]]; then
    echo "  ⚠ Linux artifacts don't contain an AppImage + .sig pair. Skipping."
    rm -rf "$TMP"
    return
  fi

  mkdir -p "$STAGE_DIR"
  cp "$SRC" "$STAGE_DIR/KinAI.AppImage"
  cp "$SRC_SIG" "$STAGE_DIR/KinAI.AppImage.sig"
  ln -sfn "${NEW_VERSION}" "$HOME/.kinai/updates/latest-${TARGET}"
  rm -rf "$TMP"

  echo "  ✓ staged Linux update at $STAGE_DIR"
  echo "    ($(ls -lh "$STAGE_DIR/KinAI.AppImage" | awk '{print $5}') + signature)"
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

# Re-stage ONLY the Windows updater bundle for the current version, then
# exit. deploy.sh itself runs before the git push, so its first pass
# can't fetch the Windows installer (the release tag isn't published
# yet). Run this once the release CI has attached the Windows asset:
#   ./scripts/deploy.sh stage-windows
if [[ "$bump" == "stage-windows" ]]; then
  NEW_VERSION=$(grep -E '^version = "' Cargo.toml | head -1 | sed -E 's/.*"([^"]+)".*/\1/')
  echo "→ re-staging Windows update for v$NEW_VERSION"
  stage_windows_update
  echo "✓ done"
  exit 0
fi

if [[ "$bump" == "stage-linux" ]]; then
  NEW_VERSION=$(grep -E '^version = "' Cargo.toml | head -1 | sed -E 's/.*"([^"]+)".*/\1/')
  echo "→ re-staging Linux update for v$NEW_VERSION"
  stage_linux_update
  echo "✓ done"
  exit 0
fi

if [[ "$bump" != "skip-bump" ]]; then
  ./scripts/bump-version.sh "$bump"
fi

NEW_VERSION=$(grep -E '^version = "' Cargo.toml | head -1 | sed -E 's/.*"([^"]+)".*/\1/')
OS="$(uname -s)"
echo "→ building KinAI v$NEW_VERSION on $OS"

# Belt-and-suspenders: force build.rs to re-run so the embedded git
# short-hash + build time are always fresh. build.rs already watches
# .git/HEAD and its ref, but packed refs (.git/packed-refs) can dodge
# that; touching a src file guarantees the rerun-if-changed=src trigger
# fires. Cheap insurance against shipping a binary that mislabels its
# own commit (the 458a300-on-a-0.2.47-build confusion).
touch src/main.rs 2>/dev/null || true

# Run the Rust test suite BEFORE any expensive build/sign/notarize work.
# Skip with KINAI_DEPLOY_SKIP_TESTS=1 for genuinely emergency hotfix
# situations (don't use this casually — silent breakage shipped this
# way is what got us into the v0.2.36 JWT regression where every
# encode()/decode() panicked at runtime because a feature flag
# wasn't enabled, and the test suite added afterwards would have
# caught it in seconds).
#
# tauri::generate_context!() evaluates at compile time and checks
# that `frontend/build/` exists. On a fresh checkout that dir isn't
# there yet, so we build the frontend first — pnpm tauri build
# would do this anyway later via beforeBuildCommand, doing it now
# means cargo test can actually compile the lib.
if [[ "${KINAI_DEPLOY_SKIP_TESTS:-0}" != "1" ]]; then
  if [[ ! -d frontend/build ]]; then
    echo "→ building frontend (so cargo test can compile the lib)"
    pnpm --filter kinai-frontend build > /dev/null
  fi
  echo "→ running cargo test (set KINAI_DEPLOY_SKIP_TESTS=1 to skip)"
  if ! cargo test --lib --quiet 2>&1 | tail -15; then
    echo "✗ tests failed — aborting deploy. Fix the failures or set KINAI_DEPLOY_SKIP_TESTS=1 if this is truly an emergency."
    exit 1
  fi
  echo "  ✓ tests passed"
fi

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

  echo "→ stripping xattrs + codesigning with hardened runtime + entitlements"
  xattr -cr "$SIGN_DIR/KinAI.app"
  # The --entitlements file enables the hardened-runtime exemptions we
  # need: audio-input (microphone), camera (future), and allow-jit
  # (WKWebView JS performance). Without this flag the binary signs
  # cleanly but EVERY mic access is denied at the kernel level even
  # if the user grants Privacy permission — that's the v0.2.x mic
  # regression. See entitlements.plist for full rationale.
  ENTITLEMENTS_FILE="$PWD/entitlements.plist"
  if [[ ! -f "$ENTITLEMENTS_FILE" ]]; then
    echo "  ✗ entitlements.plist missing — mic/camera will be denied at runtime" >&2
    exit 1
  fi
  codesign --force --options runtime --timestamp --deep \
    --entitlements "$ENTITLEMENTS_FILE" \
    --sign "$APPLE_SIGNING_IDENTITY" \
    "$SIGN_DIR/KinAI.app"
  codesign --verify --strict --verbose=2 "$SIGN_DIR/KinAI.app" 2>&1 | tail -3
  echo "  ✓ entitlements attached:"
  codesign -d --entitlements - "$SIGN_DIR/KinAI.app" 2>&1 | grep -E "audio-input|camera|allow-jit|Bool" | head -10

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
