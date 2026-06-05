#!/usr/bin/env sh
# agent-buddy installer (macOS / Linux)
#
# One command:
#   curl -fsSL https://raw.githubusercontent.com/nateschnell/agent-buddy/main/install.sh | sh
#
# Downloads the right prebuilt binary for this machine, installs it, and wires
# it into Claude Code (writes hooks + installs the background service).
#
# Env overrides:
#   AGENT_BUDDY_REPO    GitHub owner/repo to fetch releases from
#   AGENT_BUDDY_VERSION release tag (default: latest)
#   AGENT_BUDDY_BINDIR  install dir (default: ~/.local/bin)
#   AGENT_BUDDY_NO_SETUP=1  install the binary only; skip `setup`
#   AGENT_BUDDY_UNINSTALL=1 remove everything (hooks, daemon, service, state)
set -eu
# Enable pipefail where the shell running us supports it (not POSIX, but bash/
# zsh/most dash builds do) so a failed stage in a pipeline aborts rather than
# being masked by the last command's success.
# shellcheck disable=SC3040
( set -o pipefail 2>/dev/null ) && set -o pipefail || true

REPO="${AGENT_BUDDY_REPO:-nateschnell/agent-buddy}"
VERSION="${AGENT_BUDDY_VERSION:-latest}"
BINDIR="${AGENT_BUDDY_BINDIR:-$HOME/.local/bin}"

say()  { printf '\033[1m%s\033[0m\n' "$*"; }
err()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# sha256 of a file, using whichever tool this OS ships (Linux: sha256sum,
# macOS: shasum). Used to verify the download against the release manifest.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    err "need sha256sum or shasum to verify the download"
  fi
}

# --- uninstall mode --------------------------------------------------------
# Reverse everything: prefer the installed binary's own `uninstall` (it knows
# every artifact); remove the binary + bundled firmware last.
if [ "${AGENT_BUDDY_UNINSTALL:-0}" = "1" ]; then
  say "Uninstalling agent-buddy…"
  if [ -x "$BINDIR/agent-buddy" ]; then
    "$BINDIR/agent-buddy" uninstall || true
    rm -f "$BINDIR/agent-buddy" "$BINDIR"/firmware*.bin "$BINDIR"/firmware*.version
    say "✓ removed $BINDIR/agent-buddy"
  else
    say "  no agent-buddy binary at $BINDIR — nothing to remove"
  fi
  say "Done."
  exit 0
fi

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

# --- preflight -------------------------------------------------------------
command -v curl >/dev/null 2>&1 || err "curl is required but was not found"
command -v tar  >/dev/null 2>&1 || err "tar is required but was not found"

# --- resolve download URLs -------------------------------------------------
asset="agent-buddy-${target}.tar.gz"
if [ "$VERSION" = "latest" ]; then
  base="https://github.com/${REPO}/releases/latest/download"
else
  base="https://github.com/${REPO}/releases/download/${VERSION}"
fi
url="${base}/${asset}"
sums_url="${base}/SHA256SUMS"

say "Installing agent-buddy for ${target}…"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

if ! curl -fsSL "$url" -o "$tmp/$asset"; then
  err "download failed: $url
(If you haven't published a release yet, build locally:
   cd bridge && cargo build --release && cp target/release/agent-buddy \"$BINDIR/\")"
fi

# Verify the archive against the release's published SHA256SUMS before trusting
# it — catches a corrupt/partial download or a tampered asset. Fail closed: an
# unverifiable download is not installed.
if ! curl -fsSL "$sums_url" -o "$tmp/SHA256SUMS"; then
  err "could not fetch SHA256SUMS from the release — refusing to install an unverified binary"
fi
expected="$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || err "SHA256SUMS has no entry for $asset"
actual="$(sha256_of "$tmp/$asset")"
if [ "$expected" != "$actual" ]; then
  err "checksum mismatch for $asset
  expected: $expected
  actual:   $actual
Refusing to install. Re-download or report this."
fi
say "✓ verified $asset (sha256)"

if ! tar -xzf "$tmp/$asset" -C "$tmp"; then
  err "failed to extract $asset (corrupt download?)"
fi
[ -f "$tmp/agent-buddy" ] || err "archive did not contain the agent-buddy binary"
mkdir -p "$BINDIR"
install -m 0755 "$tmp/agent-buddy" "$BINDIR/agent-buddy"
say "✓ installed $BINDIR/agent-buddy"

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
if [ "${AGENT_BUDDY_NO_SETUP:-0}" != "1" ]; then
  say "Wiring into Claude Code…"
  "$BINDIR/agent-buddy" setup
fi

say "Done. Power on your buddy, then run: agent-buddy pair"
