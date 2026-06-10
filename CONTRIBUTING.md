# Contributing to Agent Buddy

Thanks for your interest! This repo is the **open-source gateway** — the
`agent-buddy` daemon + desktop app/widget that bridges your AI-tool sessions to a
buddy (on your desktop, or on the optional physical display). Contributions to it
are welcome.

## How the repo is laid out

This repo is published from a private monorepo. A few directories here are
**mirrors** and a couple are **owned by this repo**:

| Path | Status |
| --- | --- |
| `bridge/` | mirror — the Rust daemon + desktop app/widget. **Edit these for code contributions.** |
| `packaging/` | mirror — installer/packaging scripts. |
| `install.sh`, `install.ps1` | mirror — install scripts. |
| `.github/`, `README.md`, `LICENSE`, `.gitignore` | owned by this repo. |

The board **firmware is closed-source** and is **not** in this repo. The physical
display is a paid upsell; the daemon/app is free and open precisely because it
injects hooks into your tools and reads session activity — it should be auditable.

## Opening a PR

1. Fork this repo and branch off `main`.
2. Make your change in `bridge/` (and/or `packaging/` / `install.*`).
3. Make sure it builds and tests pass:
   ```bash
   cd bridge
   cargo check --features gui
   cargo test
   cargo fmt
   ```
   CI runs exactly this on your PR.
4. Open the PR against `main`. Describe the change and why.

## What happens after you open a PR

Because the code of truth lives in the private monorepo, a maintainer **back-ports
accepted PRs** into the monorepo (you keep authorship credit via
`Co-authored-by`), and the change syncs back here on the next release. So:

- Your PR may be **closed with a reference to the merge commit** rather than
  showing the green "merged" badge — that's the back-port flow, not a rejection.
- **Don't be surprised if `bridge/`/`packaging/` get overwritten by a release
  sync** — never hand-edit those on a long-lived branch expecting it to stick;
  always go through a PR.

## Scope

- ✅ Daemon/bridge behavior, desktop app/widget, packaging, install scripts.
- ❌ Firmware (closed-source, not here).
- For anything large or design-changing, **open an issue first** so we can agree on
  the approach before you invest the work.

By contributing you agree your contribution is licensed under this repo's
[LICENSE](LICENSE).
