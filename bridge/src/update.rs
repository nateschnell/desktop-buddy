//! Checking GitHub Releases for a newer claude-buddy, plus version-string
//! comparison shared with the firmware-update path.
//!
//! Two consumers:
//!   * the daemon's periodic update check ([`latest_release`]) — surfaced to the
//!     desktop app so it can offer to download a newer app build;
//!   * the desktop app's firmware comparison ([`is_newer`]) — does the image
//!     this app bundles supersede the version the connected buddy reported?
//!
//! HTTP is delegated to `curl` rather than a Rust TLS stack: it's already a hard
//! dependency of `install.sh`, keeps the daemon's dependency tree small, and
//! matches the codebase's existing habit of shelling out to system tools.

use anyhow::{anyhow, Context, Result};
use std::process::Command;

/// `owner/repo` to check for releases. Single source of truth, kept in sync with
/// `install.sh`'s `CLAUDE_BUDDY_REPO` default and Cargo.toml's `repository`.
pub const REPO: &str = "nateschnell/desktop-buddy";

/// A published release the app could update to.
#[derive(Debug, Clone)]
pub struct Release {
    /// Tag as published, e.g. `"v0.1.2"`.
    pub tag: String,
    /// The GitHub release page, for a guided download.
    pub url: String,
}

/// Fetch the latest published (non-draft, non-prerelease) release via the GitHub
/// API. Blocking — call from `spawn_blocking`. Errors on a network failure, a
/// non-2xx response (e.g. rate limit), or unparseable JSON; callers treat any
/// error as "no info this round" rather than surfacing it.
pub fn latest_release() -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "15",
            "-H",
            "Accept: application/vnd.github+json",
            // GitHub rejects API requests without a User-Agent.
            "-H",
            "User-Agent: claude-buddy",
            &url,
        ])
        .output()
        .context("running curl for the release check")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("release check failed: {}", err.trim()));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing the GitHub release JSON")?;
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("release JSON had no tag_name"))?
        .to_string();
    let page = v
        .get("html_url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    Ok(Release { tag, url: page })
}

/// Parse a loose version string into a `(major, minor, patch)` triple. Tolerates
/// a leading `v` and ignores any pre-release / build / git-describe suffix after
/// the patch number — so `"v0.1.2"`, `"0.1.2"`, and `"v0.1.2-3-gdba2033-dirty"`
/// all parse to `(0, 1, 2)`. Returns `None` when there's no `x.y.z` core (e.g.
/// the firmware's `"dev"` / `"unknown"` fallbacks), which callers treat as
/// "don't reason about it".
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    // Drop the git-describe "-N-gHASH[-dirty]" tail or any "+build" metadata.
    let core = s.split(['-', '+']).next().unwrap_or(s);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// True if `candidate` is a strictly newer version than `current`. Conservative:
/// if *either* side lacks a parseable `x.y.z` core, returns `false` — we never
/// nag about an update we can't actually reason about (e.g. a `"dev"` firmware
/// build, or an app built from an untagged tree).
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_loose_versions() {
        assert_eq!(parse_version("v0.1.2"), Some((0, 1, 2)));
        assert_eq!(parse_version("0.1.2"), Some((0, 1, 2)));
        assert_eq!(parse_version("v0.1.2-3-gdba2033-dirty"), Some((0, 1, 2)));
        assert_eq!(parse_version("1.0"), Some((1, 0, 0)));
        assert_eq!(parse_version("2"), Some((2, 0, 0)));
        assert_eq!(parse_version("v2.10.0"), Some((2, 10, 0)));
        // No x.y.z core → None (firmware fallbacks, garbage).
        assert_eq!(parse_version("dev"), None);
        assert_eq!(parse_version("unknown"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("gdba2033"), None);
    }

    #[test]
    fn newer_is_strict_and_conservative() {
        assert!(is_newer("v0.2.0", "v0.1.9"));
        assert!(is_newer("0.1.10", "0.1.9")); // numeric, not lexical
        assert!(is_newer("v1.0.0", "0.9.9"));
        // Equal or older → not newer.
        assert!(!is_newer("v0.1.2", "v0.1.2"));
        assert!(!is_newer("v0.1.1", "v0.1.2"));
        // A clean tag is not "newer" than the same tag with a dirty dev suffix
        // (both parse to the same core) — avoids nagging a dev who's ahead.
        assert!(!is_newer("v0.1.2", "v0.1.2-4-gabc123"));
        // Unparseable on either side → false.
        assert!(!is_newer("v0.2.0", "dev"));
        assert!(!is_newer("dev", "v0.1.0"));
    }
}
