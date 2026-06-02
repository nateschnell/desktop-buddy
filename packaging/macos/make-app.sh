#!/usr/bin/env bash
#
# Assemble a self-contained "Claude Buddy.app" and wrap it in a .dmg.
#
# The .app carries BOTH binaries — the GUI (Contents/MacOS/claude-buddy-app, the
# bundle's main executable) and the daemon (Contents/MacOS/claude-buddy, its
# sibling) — plus every board's firmware image in Contents/Resources. That's the
# layout the app's own self-install (setup.rs::install_desktop_launcher) expects:
# the user double-clicks the app, it copies itself into /Applications, registers
# the daemon as a launchd service, and wires the Claude Code hooks. So this
# script produces the single download; the app does the rest on first open.
#
# Usage:
#   make-app.sh --bin-dir DIR --fw-dir DIR --out DIR --version vX.Y.Z
#
#   --bin-dir   dir holding the (ideally universal) `claude-buddy` and
#               `claude-buddy-app` binaries
#   --fw-dir    dir holding firmware*.bin / firmware*.version (optional)
#   --out       output dir for "Claude Buddy.app" and the .dmg
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

GUI="$BIN_DIR/claude-buddy-app"
DAEMON="$BIN_DIR/claude-buddy"
[ -f "$GUI" ]    || { echo "missing GUI binary: $GUI" >&2; exit 1; }
[ -f "$DAEMON" ] || { echo "missing daemon binary: $DAEMON" >&2; exit 1; }

# CFBundleVersion wants a dotted number; strip the leading v and any -gHASH tail
# from `git describe` (e.g. v0.1.0-2-g86cb615 -> 0.1.0). Keep $VERSION verbatim
# for the human-facing dmg name and short-version string.
SHORT="$(printf '%s' "$VERSION" | sed -E 's/^v//; s/-[0-9]+-g[0-9a-f]+$//; s/-dirty$//')"
[ -n "$SHORT" ] || SHORT="0.0.0"

APP="$OUT/Claude Buddy.app"
CONTENTS="$APP/Contents"
echo "==> assembling $APP (version $VERSION)"
rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"

install -m 0755 "$GUI"    "$CONTENTS/MacOS/claude-buddy-app"
install -m 0755 "$DAEMON" "$CONTENTS/MacOS/claude-buddy"

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

# Info.plist mirrors setup.rs::install_desktop_launcher so the bundle the user
# downloads is identical to the one the app would build for itself. The daemon
# (not the GUI) opens CoreBluetooth, but it runs from its own helper bundle the
# app creates at install time, so the BT usage string lives there; the GUI only
# needs the Local-Network string for the espota OTA flow.
cat > "$CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Claude Buddy</string>
  <key>CFBundleDisplayName</key><string>Claude Buddy</string>
  <key>CFBundleIdentifier</key><string>com.anthropic.claude-buddy-app</string>
  <key>CFBundleExecutable</key><string>claude-buddy-app</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$SHORT</string>
  <key>CFBundleShortVersionString</key><string>$SHORT</string>
  <key>LSMinimumSystemVersion</key><string>10.13</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>Claude Buddy flashes firmware updates to your buddy over your local Wi-Fi network.</string>
  <key>NSBluetoothAlwaysUsageDescription</key><string>Claude Buddy connects to your hardware buddy over Bluetooth.</string>
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
  # Inner Mach-O binaries first (the sibling daemon), then the bundle itself —
  # signing the bundle covers its main executable (claude-buddy-app).
  codesign --force "${HARDENED[@]}" --sign "$IDENTITY" \
    "$CONTENTS/MacOS/claude-buddy"
  codesign --force "${HARDENED[@]}" --sign "$IDENTITY" \
    --identifier com.anthropic.claude-buddy-app "$APP"
  codesign --verify --strict --verbose=2 "$APP"
else
  codesign --force --deep --sign "$IDENTITY" \
    --identifier com.anthropic.claude-buddy-app "$APP" || \
    echo "    (codesign failed; bundle still runs unsigned)" >&2
fi

# Build the .dmg. A plain read-only image of a folder containing the app + an
# /Applications symlink is the conventional drag-to-install layout.
DMG="$OUT/Claude-Buddy-$VERSION.dmg"
echo "==> building $DMG"
STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "Claude Buddy" -srcfolder "$STAGE" \
  -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"

echo "==> done: $APP"
echo "==> done: $DMG"
