#!/usr/bin/env bash
#
# Build a single self-contained Claude-Buddy-x86_64.AppImage: one file the user
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

APPDIR="$(mktemp -d)/Claude Buddy.AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/claude-buddy"
install -m 0755 "$GUI"    "$APPDIR/usr/bin/claude-buddy-app"
install -m 0755 "$DAEMON" "$APPDIR/usr/bin/claude-buddy"

if [ -n "$FW_DIR" ] && [ -d "$FW_DIR" ]; then
  shopt -s nullglob
  for f in "$FW_DIR"/firmware*.bin "$FW_DIR"/firmware*.version; do
    cp "$f" "$APPDIR/usr/bin/"   # beside the binaries — where the app looks
    echo "    bundled $(basename "$f")"
  done
  shopt -u nullglob
fi

# .desktop + icon are mandatory for AppImage. Icon name must match the .desktop
# Icon= key and a file at the AppDir root.
cat > "$APPDIR/claude-buddy.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Claude Buddy
Comment=Control panel for your Claude hardware buddy
Exec=claude-buddy-app
Icon=claude-buddy
Categories=Utility;
Terminal=false
DESKTOP

# Icon: a simple generated square, or a 1x1 placeholder if ImageMagick is absent
# (appimagetool only requires the file to exist and match the name).
if command -v convert >/dev/null 2>&1; then
  convert -size 256x256 xc:'#C15F3C' "$APPDIR/claude-buddy.png"
else
  base64 -d > "$APPDIR/claude-buddy.png" <<'PNG'
iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNgYGAAAAAEAAEnNCcKAAAAAElFTkSuQmCC
PNG
fi

# AppRun: launch the GUI. The AppImage runtime exports $APPIMAGE (the path of the
# .AppImage file itself); pass it through so the app can register a launcher that
# survives across runs rather than pointing at the ephemeral mount.
cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/claude-buddy-app" "$@"
APPRUN
chmod +x "$APPDIR/AppRun"

mkdir -p "$OUT"
OUTFILE="$OUT/Claude-Buddy-$VERSION-x86_64.AppImage"
echo "==> building $OUTFILE"
# --appimage-extract-and-run avoids needing FUSE in CI containers.
ARCH=x86_64 appimagetool --appimage-extract-and-run "$APPDIR" "$OUTFILE"
echo "==> done: $OUTFILE"
