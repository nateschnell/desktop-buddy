//! Bake the real build version into the binary.
//!
//! The crate's `Cargo.toml` version is a coarse fallback that nobody bumps per
//! release, so relying on `CARGO_PKG_VERSION` at runtime made the daemon think
//! it was permanently `0.1.0` — the "update available" banner never cleared and
//! the app misreported itself. Instead we resolve a real version here and expose
//! it as `AGENT_BUDDY_VERSION`, mirroring how the firmware bakes `git describe`
//! (`firmware/scripts/fw_version.py`).
//!
//! Precedence:
//!   1. `AGENT_BUDDY_VERSION` env var — CI sets this from the release tag so the
//!      shipped binary is deterministic even if git metadata is unavailable.
//!   2. `git describe --tags --match 'v*' --always --dirty` — `--match 'v*'`
//!      keeps us on the desktop-app tag track, so a nearby firmware-only `fw-v*`
//!      tag can't hijack the app's version.
//!   3. `CARGO_PKG_VERSION` — last-ditch fallback (no env, no git).

use std::process::Command;

fn main() {
    // Re-run when the override changes, or when HEAD moves (new commit/checkout
    // or tag) so a dev rebuild refreshes the baked `git describe`.
    println!("cargo:rerun-if-env-changed=AGENT_BUDDY_VERSION");
    for p in ["../.git/HEAD", "../.git/packed-refs"] {
        if std::path::Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }

    let version = resolve_version();
    println!("cargo:rustc-env=AGENT_BUDDY_VERSION={version}");
}

fn resolve_version() -> String {
    if let Ok(v) = std::env::var("AGENT_BUDDY_VERSION") {
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Some(v) = git_describe() {
        return v;
    }
    // Cargo always sets this for the build script.
    std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "dev".to_string())
}

fn git_describe() -> Option<String> {
    let out = Command::new("git")
        .args([
            "describe",
            "--tags",
            "--match",
            "v*",
            "--always",
            "--dirty",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}
