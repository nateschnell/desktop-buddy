#!/usr/bin/env sh
# claude-buddy installer (macOS / Linux)
#
# One command:
#   curl -fsSL https://raw.githubusercontent.com/nateschnell/desktop-buddy/main/install.sh | sh
#
# Downloads the right prebuilt binary for this machine, installs it, and wires
# it into Claude Code (writes hooks + installs the background service).
#
# Env overrides:
#   CLAUDE_BUDDY_REPO    GitHub owner/repo to fetch releases from
#   CLAUDE_BUDDY_VERSION release tag (default: latest)
#   CLAUDE_BUDDY_BINDIR  install dir (default: ~/.local/bin)
#   CLAUDE_BUDDY_NO_SETUP=1  install the binary only; skip `setup`
set -eu

REPO="${CLAUDE_BUDDY_REPO:-nateschnell/desktop-buddy}"
VERSION="${CLAUDE_BUDDY_VERSION:-latest}"
BINDIR="${CLAUDE_BUDDY_BINDIR:-$HOME/.local/bin}"

say()  { printf '\033[1m%s\033[0m\n' "$*"; }
err()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# --- detect platform -> Rust target triple --------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin) case "$arch" in
            arm64|aarch64) target="aarch64-apple-darwin" ;;
            x86_64)        target="x86_64-apple-darwin" ;;
            *) err "unsupported macOS arch: $arch" ;;
          esac ;;
  Linux)  case "$arch" in
            x86_64)        target="x86_64-unknown-linux-gnu" ;;
            aarch64|arm64) target="aarch64-unknown-linux-gnu" ;;
            *) err "unsupported Linux arch: $arch" ;;
          esac ;;
  *) err "unsupported OS: $os (use install.ps1 on Windows)" ;;
esac

# --- resolve download URL --------------------------------------------------
asset="claude-buddy-${target}.tar.gz"
if [ "$VERSION" = "latest" ]; then
  url="https://github.com/${REPO}/releases/latest/download/${asset}"
else
  url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
fi

say "Installing claude-buddy for ${target}…"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

if ! curl -fsSL "$url" -o "$tmp/$asset"; then
  err "download failed: $url
(If you haven't published a release yet, build locally:
   cd bridge && cargo build --release && cp target/release/claude-buddy \"$BINDIR/\")"
fi

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$BINDIR"
install -m 0755 "$tmp/claude-buddy" "$BINDIR/claude-buddy"
say "✓ installed $BINDIR/claude-buddy"

# The per-board firmware images + their versions ride along in the archive
# (firmware-<board>.bin / .version, plus a legacy firmware.bin = CYD). Drop them
# all next to the binary so `setup` (and the desktop app) can bundle them and
# offer over-the-air updates to whichever board is connected.
for f in "$tmp"/firmware*.bin "$tmp"/firmware*.version; do
  [ -f "$f" ] && cp "$f" "$BINDIR/$(basename "$f")"
done

# --- PATH hint -------------------------------------------------------------
case ":$PATH:" in
  *":$BINDIR:"*) ;;
  *) say "  note: add $BINDIR to your PATH:  export PATH=\"$BINDIR:\$PATH\"" ;;
esac

# --- wire into Claude Code -------------------------------------------------
if [ "${CLAUDE_BUDDY_NO_SETUP:-0}" != "1" ]; then
  say "Wiring into Claude Code…"
  "$BINDIR/claude-buddy" setup
fi

say "Done. Power on your buddy, then run: claude-buddy pair"
