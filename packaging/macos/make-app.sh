#!/usr/bin/env bash
#
# Assemble a self-contained "Agent Buddy.app" and wrap it in a .dmg.
#
# The .app carries BOTH binaries — the GUI (Contents/MacOS/agent-buddy-app, the
# bundle's main executable) and the daemon (Contents/MacOS/agent-buddy, its
# sibling) — plus every board's firmware image in Contents/Resources. That's the
# layout the app's own self-install (setup.rs::install_desktop_launcher) expects:
# the user double-clicks the app, it copies itself into /Applications, registers
# the daemon as a launchd service, and wires the Claude Code hooks. So this
# script produces the single download; the app does the rest on first open.
#
# Usage:
#   make-app.sh --bin-dir DIR --fw-dir DIR --out DIR --version vX.Y.Z
#
#   --bin-dir   dir holding the (ideally universal) `agent-buddy` and
#               `agent-buddy-app` binaries
#   --fw-dir    dir holding firmware*.bin / firmware*.version (optional)
#   --out       output dir for "Agent Buddy.app" and the .dmg
#   --version   version string baked into Info.plist + the dmg name
#
# Signing: ad-hoc by default. Set MACOS_SIGN_IDENTITY to a "Developer ID
# Application: …" identity to sign for real (Gatekeeper-friendly); the rest of
# the layout is identical, so notarization can be layered on in CI without
# touching this script.
set -euo pipefail

BIN_DIR="" FW_DIR="" OUT="" VERSION="dev"
while [ $# -gt 0 ]; do
  case "$1" in
    --bin-dir) BIN_DIR="$2"; shift 2;;
    --fw-dir)  FW_DIR="$2";  shift 2;;
    --out)     OUT="$2";     shift 2;;
    --version) VERSION="$2"; shift 2;;
    *) echo "unknown arg: $1" >&2; exit 2;;
  esac
done
[ -n "$BIN_DIR" ] && [ -n "$OUT" ] || { echo "need --bin-dir and --out" >&2; exit 2; }

# Completeness guard. The desktop bundle must carry EVERY binary the crate
# declares — the daemon plus each GUI bin (agent-buddy-app, agent-buddy-widget,
# …). We derive that list from bridge/Cargo.toml's [[bin]] entries (single source
# of truth) and hard-fail if any is absent from --bin-dir, so a packaging or
# staging step that drops a binary breaks the build LOUDLY instead of silently
# shipping an incomplete app. This is the guard that would have caught the
# v0.2.2 release shipping without agent-buddy-widget.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO="$ROOT/bridge/Cargo.toml"
expected_bins() {
  if [ -f "$CARGO" ]; then
    awk '
      /^\[\[bin\]\]/ { inbin=1; next }
      /^\[/          { inbin=0 }
      inbin && /^[[:space:]]*name[[:space:]]*=/ {
        l=$0; sub(/.*=[[:space:]]*"/, "", l); sub(/".*/, "", l); print l
      }
    ' "$CARGO"
  else
    # Cargo.toml not locatable — fall back to the known set so the guard still
    # has teeth. Keep in sync with bridge/Cargo.toml [[bin]] entries.
    printf '%s\n' agent-buddy agent-buddy-app agent-buddy-widget
  fi
}
EXPECTED_BINS="$(expected_bins)"
[ -n "$EXPECTED_BINS" ] || { echo "could not determine expected binaries from $CARGO" >&2; exit 1; }
missing=""
for b in $EXPECTED_BINS; do
  [ -f "$BIN_DIR/$b" ] || missing="$missing $b"
done
if [ -n "$missing" ]; then
  echo "::error::desktop bundle is missing binaries from --bin-dir ($BIN_DIR):$missing" >&2
  echo "  expected (from $CARGO):" $EXPECTED_BINS >&2
  echo "  present:" $(cd "$BIN_DIR" && ls 2>/dev/null) >&2
  exit 1
fi
DAEMON="$BIN_DIR/agent-buddy"

# CFBundleVersion wants a dotted number; strip the leading v and any -gHASH tail
# from `git describe` (e.g. v0.1.0-2-g86cb615 -> 0.1.0). Keep $VERSION verbatim
# for the human-facing dmg name and short-version string.
SHORT="$(printf '%s' "$VERSION" | sed -E 's/^v//; s/-[0-9]+-g[0-9a-f]+$//; s/-dirty$//')"
[ -n "$SHORT" ] || SHORT="0.0.0"

APP="$OUT/Agent Buddy.app"
CONTENTS="$APP/Contents"
echo "==> assembling $APP (version $VERSION)"
rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"

# Install every declared binary as a sibling in Contents/MacOS/ (the guard above
# already verified they're all present). agent-buddy-app is the bundle's main
# executable (CFBundleExecutable); agent-buddy is the daemon; agent-buddy-widget
# is the floating desktop buddy the app spawns.
for b in $EXPECTED_BINS; do
  install -m 0755 "$BIN_DIR/$b" "$CONTENTS/MacOS/$b"
done

# Bring along every firmware image + version so the app's one-click OTA has an
# image to flash for whichever board connects (see ota::bundled_firmware_path).
if [ -n "$FW_DIR" ] && [ -d "$FW_DIR" ]; then
  shopt -s nullglob
  for f in "$FW_DIR"/firmware*.bin "$FW_DIR"/firmware*.version; do
    cp "$f" "$CONTENTS/Resources/"
    echo "    bundled $(basename "$f")"
  done
  shopt -u nullglob
fi

# Ship the bundled-font license notices (Lucide/Feather icon font ISC + MIT, and
# IBM Plex Sans OFL 1.1) alongside the binary so the required copyright notices
# travel with every copy, not only compiled into the GUI. Resolved relative to
# this script's repo path.
ASSETS="$(cd "$(dirname "$0")/../.." && pwd)/bridge/assets"
if [ -f "$ASSETS/app-icon.icns" ]; then
  cp "$ASSETS/app-icon.icns" "$CONTENTS/Resources/AgentBuddy.icns"
  echo "    bundled AgentBuddy.icns"
fi
if [ -f "$ASSETS/LICENSE" ]; then
  TPL="$CONTENTS/Resources/THIRD_PARTY_LICENSES"
  {
    echo "=== Lucide icon font (lucide.ttf) ==="; echo
    cat "$ASSETS/LICENSE"
    if [ -f "$ASSETS/IBMPlexSans-LICENSE.txt" ]; then
      echo; echo "=== IBM Plex Sans (IBMPlexSans-*.ttf) ==="; echo
      cat "$ASSETS/IBMPlexSans-LICENSE.txt"
    fi
  } > "$TPL"
  echo "    bundled THIRD_PARTY_LICENSES"
fi

# Info.plist mirrors setup.rs::install_desktop_launcher so the bundle the user
# downloads is identical to the one the app would build for itself. The daemon
# (not the GUI) opens CoreBluetooth, but it runs from its own helper bundle the
# app creates at install time, so the BT usage string lives there; the GUI only
# needs the Local-Network string for the espota OTA flow.
cat > "$CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Agent Buddy</string>
  <key>CFBundleDisplayName</key><string>Agent Buddy</string>
  <key>CFBundleIdentifier</key><string>com.nateschnell.agent-buddy-app</string>
  <key>CFBundleExecutable</key><string>agent-buddy-app</string>
  <key>CFBundleIconFile</key><string>AgentBuddy</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$SHORT</string>
  <key>CFBundleShortVersionString</key><string>$SHORT</string>
  <key>LSMinimumSystemVersion</key><string>10.13</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>Agent Buddy flashes firmware updates to your buddy over your local Wi-Fi network.</string>
  <key>NSBluetoothAlwaysUsageDescription</key><string>Agent Buddy connects to your hardware buddy over Bluetooth.</string>
</dict></plist>
PLIST

# Sign innermost-first. A real "Developer ID Application: …" identity (set via
# MACOS_SIGN_IDENTITY) makes the bundle Gatekeeper-friendly AND notarizable;
# absent that we ad-hoc sign so the bundle at least runs (one-time
# right-click->Open). Notarization requires the hardened runtime (--options
# runtime) and a secure timestamp (--timestamp), neither of which apply to an
# ad-hoc signature — so we only add them when a real identity is present, and
# sign the nested daemon explicitly rather than leaning on the deprecated --deep.
IDENTITY="${MACOS_SIGN_IDENTITY:--}"
echo "==> codesigning (identity: $IDENTITY)"
if [ "$IDENTITY" != "-" ]; then
  HARDENED=(--options runtime --timestamp)
  # Inner Mach-O binaries first (every sibling except the main executable), then
  # the bundle itself — signing the bundle covers its main exe (agent-buddy-app).
  for b in $EXPECTED_BINS; do
    [ "$b" = "agent-buddy-app" ] && continue
    codesign --force "${HARDENED[@]}" --sign "$IDENTITY" "$CONTENTS/MacOS/$b"
  done
  codesign --force "${HARDENED[@]}" --sign "$IDENTITY" \
    --identifier com.nateschnell.agent-buddy-app "$APP"
  codesign --verify --strict --verbose=2 "$APP"
else
  codesign --force --deep --sign "$IDENTITY" \
    --identifier com.nateschnell.agent-buddy-app "$APP" || \
    echo "    (codesign failed; bundle still runs unsigned)" >&2
fi

# Build the .dmg. A plain read-only image of a folder containing the app + an
# /Applications symlink is the conventional drag-to-install layout.
DMG="$OUT/Agent-Buddy-$VERSION.dmg"
echo "==> building $DMG"
STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "Agent Buddy" -srcfolder "$STAGE" \
  -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"

# Sign the .dmg container itself (the app inside is already signed). Without
# this the notarization ticket still staples, but `spctl -a -t open` reports
# "no usable signature" because the dmg carries none — so we sign it (with a
# secure timestamp) whenever a real identity is present. Canonical order is
# sign-app -> build-dmg -> sign-dmg -> notarize -> staple (notarize/staple run
# in CI after this script).
if [ "$IDENTITY" != "-" ]; then
  echo "==> codesigning dmg"
  codesign --force --timestamp --sign "$IDENTITY" "$DMG"
  codesign --verify --verbose=2 "$DMG"
fi

echo "==> done: $APP"
echo "==> done: $DMG"
