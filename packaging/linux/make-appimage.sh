#!/usr/bin/env bash
#
# Build a single self-contained Agent-Buddy-x86_64.AppImage: one file the user
# downloads, marks executable, and double-clicks. It carries BOTH binaries (GUI
# + daemon, side by side so the GUI finds the daemon as its sibling) plus every
# board's firmware image. On first open the app writes its .desktop entry,
# copies the daemon to a stable path, and registers the systemd --user service
# (setup.rs) — so the AppImage is the only thing the user handles.
#
# Usage:
#   make-appimage.sh --bin-dir DIR --fw-dir DIR --out DIR --version vX.Y.Z
#
# Requires `appimagetool` on PATH (CI downloads it). ImageMagick `convert` is
# used for the icon if present; otherwise a tiny embedded placeholder is used.
set -euo pipefail

BIN_DIR="" FW_DIR="" OUT="" VERSION="dev" ARCH="x86_64"
while [ $# -gt 0 ]; do
  case "$1" in
    --bin-dir) BIN_DIR="$2"; shift 2;;
    --fw-dir)  FW_DIR="$2";  shift 2;;
    --out)     OUT="$2";     shift 2;;
    --version) VERSION="$2"; shift 2;;
    --arch)    ARCH="$2";    shift 2;;  # x86_64 (default) or aarch64
    *) echo "unknown arg: $1" >&2; exit 2;;
  esac
done
[ -n "$BIN_DIR" ] && [ -n "$OUT" ] || { echo "need --bin-dir and --out" >&2; exit 2; }

# Completeness guard — the AppImage must carry EVERY binary the crate declares
# (daemon + each GUI bin). Derived from bridge/Cargo.toml's [[bin]] entries so it
# auto-covers new binaries and hard-fails (rather than silently shipping an
# incomplete bundle) if a staging step drops one. See make-app.sh for the
# rationale (the v0.2.2 widget-omission this prevents).
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO="$ROOT/bridge/Cargo.toml"
if [ -f "$CARGO" ]; then
  EXPECTED_BINS="$(awk '
    /^\[\[bin\]\]/ { inbin=1; next }
    /^\[/          { inbin=0 }
    inbin && /^[[:space:]]*name[[:space:]]*=/ { l=$0; sub(/.*=[[:space:]]*"/,"",l); sub(/".*/,"",l); print l }
  ' "$CARGO")"
else
  EXPECTED_BINS="agent-buddy agent-buddy-app agent-buddy-widget"
fi
[ -n "$EXPECTED_BINS" ] || { echo "could not determine expected binaries from $CARGO" >&2; exit 1; }
missing=""
for b in $EXPECTED_BINS; do
  [ -f "$BIN_DIR/$b" ] || missing="$missing $b"
done
if [ -n "$missing" ]; then
  echo "::error::AppImage is missing binaries from --bin-dir ($BIN_DIR):$missing" >&2
  echo "  expected (from $CARGO):" $EXPECTED_BINS >&2
  exit 1
fi

APPDIR="$(mktemp -d)/Agent Buddy.AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/agent-buddy"
# Install every declared binary side by side (agent-buddy-app is the launcher;
# agent-buddy the daemon; agent-buddy-widget the floating desktop buddy).
for b in $EXPECTED_BINS; do
  install -m 0755 "$BIN_DIR/$b" "$APPDIR/usr/bin/$b"
done

if [ -n "$FW_DIR" ] && [ -d "$FW_DIR" ]; then
  shopt -s nullglob
  for f in "$FW_DIR"/firmware*.bin "$FW_DIR"/firmware*.version; do
    cp "$f" "$APPDIR/usr/bin/"   # beside the binaries — where the app looks
    echo "    bundled $(basename "$f")"
  done
  shopt -u nullglob
fi

# Ship the bundled-font license notices (Lucide/Feather icon font ISC + MIT, and
# IBM Plex Sans OFL 1.1) so the required copyright notices travel with every copy,
# not only compiled into the GUI binary. Resolved relative to this script's repo.
ASSETS="$(cd "$(dirname "$0")/../.." && pwd)/bridge/assets"
if [ -f "$ASSETS/LICENSE" ]; then
  TPL="$APPDIR/usr/share/agent-buddy/THIRD_PARTY_LICENSES"
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

# .desktop + icon are mandatory for AppImage. Icon name must match the .desktop
# Icon= key and a file at the AppDir root.
cat > "$APPDIR/agent-buddy.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Agent Buddy
Comment=Control panel for your Claude hardware buddy
Exec=agent-buddy-app
Icon=agent-buddy
Categories=Utility;
Terminal=false
DESKTOP

# Icon: a simple generated square, or a 1x1 placeholder if ImageMagick is absent
# (appimagetool only requires the file to exist and match the name).
if command -v convert >/dev/null 2>&1; then
  convert -size 256x256 xc:'#C15F3C' "$APPDIR/agent-buddy.png"
else
  base64 -d > "$APPDIR/agent-buddy.png" <<'PNG'
iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNgYGAAAAAEAAEnNCcKAAAAAElFTkSuQmCC
PNG
fi

# AppRun: launch the GUI. The AppImage runtime exports $APPIMAGE (the path of the
# .AppImage file itself); pass it through so the app can register a launcher that
# survives across runs rather than pointing at the ephemeral mount.
cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/agent-buddy-app" "$@"
APPRUN
chmod +x "$APPDIR/AppRun"

mkdir -p "$OUT"
OUTFILE="$OUT/Agent-Buddy-$VERSION-$ARCH.AppImage"
echo "==> building $OUTFILE"
# --appimage-extract-and-run avoids needing FUSE in CI containers. ARCH tells
# appimagetool which runtime to embed (x86_64 / aarch64); the caller passes the
# arch matching the binaries in --bin-dir.
ARCH="$ARCH" appimagetool --appimage-extract-and-run "$APPDIR" "$OUTFILE"
echo "==> done: $OUTFILE"
