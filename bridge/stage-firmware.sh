#!/usr/bin/env sh
# Stage the built per-board firmware next to the desktop app / daemon so the app
# bundles them (into Claude Buddy.app/Contents/Resources/) and can offer over-the-
# air updates to whichever board is connected. For each board it writes
# firmware-<board>.bin + firmware-<board>.version (and, for the CYD, a legacy
# firmware.bin / firmware.version alias so older bundles keep working). The
# version is the SAME `git describe` the firmware build bakes into the image, so
# the app's "up to date" check and the version the buddy reports line up.
#
# Usage (after building the firmware envs and `cargo build [--release] ...`):
#   ./stage-firmware.sh                 # -> target/release/, stages every built board
#   ./stage-firmware.sh target/debug    # -> a specific dir
#   ./stage-firmware.sh target/release cyd   # -> only the named board(s)
set -eu

here="$(cd "$(dirname "$0")" && pwd)"     # bridge/
root="$(cd "$here/.." && pwd)"            # repo root
dest="${1:-$here/target/release}"
shift 2>/dev/null || true
boards="${*:-cyd fnk0104}"                # default: every board we ship

ver="$( cd "$root" && git describe --tags --always --dirty 2>/dev/null || echo dev )"
mkdir -p "$dest"

staged=""
for board in $boards; do
  fw="$root/firmware/.pio/build/$board/firmware.bin"
  if [ ! -f "$fw" ]; then
    echo "skip: $fw not found — build it first: (cd firmware && pio run -e $board)" >&2
    continue
  fi
  cp "$fw" "$dest/firmware-$board.bin"
  printf '%s\n' "$ver" > "$dest/firmware-$board.version"
  # Keep the un-suffixed names as the CYD alias for backward compatibility.
  if [ "$board" = "cyd" ]; then
    cp "$fw" "$dest/firmware.bin"
    printf '%s\n' "$ver" > "$dest/firmware.version"
  fi
  staged="$staged $board"
done

if [ -z "$staged" ]; then
  echo "error: no firmware images staged — build at least one env first" >&2
  exit 1
fi
echo "staged firmware for$staged ($ver) into $dest"
