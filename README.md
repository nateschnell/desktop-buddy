# Agent Buddy — desktop app & bridge

Agent Buddy is a little hardware companion for Claude Code: an ESP32
touchscreen device that shows live session telemetry (tokens, context, activity)
and surfaces tool approvals, driven over Bluetooth by a small background daemon
on your machine.

This repository holds the **desktop side** — the cross-platform app and the
`agent-buddy` daemon that bridges Claude Code to the device — and publishes the
downloadable releases. (The device firmware lives in a separate repository.)

## Install

**macOS / Linux**

```sh
curl -fsSL https://raw.githubusercontent.com/nateschnell/agent-buddy/main/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/nateschnell/agent-buddy/main/install.ps1 | iex
```

Or grab a one-file desktop package from the
[latest release](https://github.com/nateschnell/agent-buddy/releases/latest):
a universal macOS `.dmg`, a Windows `Setup.exe`, or a Linux `.AppImage`. Each
bundles the app, the background daemon, and the device firmware images (for
one-click over-the-air updates), and self-installs the background service +
Claude Code hooks on first run.

The macOS `.dmg` is signed with a Developer ID and notarized by Apple, so it
opens without a Gatekeeper prompt.

## What's in here

- `bridge/` — the Rust crate that builds two binaries: `agent-buddy` (the
  background daemon / CLI) and `agent-buddy-app` (the desktop GUI, `--features
  gui`).
- `packaging/` — per-OS packagers that assemble the downloadable desktop
  packages (`macos/make-app.sh`, `windows/installer.iss`,
  `linux/make-appimage.sh`). macOS signing + notarization is documented in
  `packaging/macos/NOTARIZE.md`.
- `install.sh` / `install.ps1` — the one-line installers.
- `.github/workflows/release.yml` — builds and publishes every platform's CLI
  archive and desktop package on each `vX.Y.Z` tag.

## Build from source

```sh
cd bridge
cargo build --release --features gui   # daemon + GUI
cargo test
```

## License

See [LICENSE](LICENSE).
