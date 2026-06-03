//! Checking GitHub Releases for newer agent-buddy / firmware, plus the version
//! comparison shared with the firmware-update path.
//!
//! There are two independent release tracks on the public repo, distinguished
//! by tag:
//!   * `vX.Y.Z`    — desktop-app releases (the app-update banner watches these);
//!   * `fw-vX.Y.Z` — firmware-only releases (no desktop rebuild).
//!
//! Both carry the per-board `firmware-<board>.bin` images, so a device can be
//! OTA-updated to the newest firmware from EITHER track without updating the app.
//!
//! Consumers:
//!   * the daemon's periodic check ([`fetch_releases`]) — surfaced to the app as
//!     the app-update banner ([`latest_app_release`]) and the per-board firmware
//!     offer ([`latest_firmware`]);
//!   * the OTA flow downloads the chosen image with [`download_firmware`];
//!   * the desktop app's version math ([`is_newer`]).
//!
//! HTTP is delegated to `curl` rather than a Rust TLS stack: it's already a hard
//! dependency of `install.sh`, keeps the daemon's dependency tree small, and
//! matches the codebase's existing habit of shelling out to system tools.

use anyhow::{anyhow, Context, Result};
use std::process::Command;

/// `owner/repo` to check for releases. Single source of truth, kept in sync with
/// `install.sh`'s `AGENT_BUDDY_REPO` default and Cargo.toml's `repository`.
pub const REPO: &str = "nateschnell/agent-buddy";

/// A downloadable file attached to a release.
#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    /// File name, e.g. `"firmware-cyd.bin"`.
    pub name: String,
    /// Direct download URL (`browser_download_url`; redirects to GitHub's CDN).
    pub url: String,
}

/// A published release on the public repo.
#[derive(Debug, Clone)]
pub struct Release {
    /// Tag as published, e.g. `"v0.1.2"` (desktop) or `"fw-v0.1.4"` (firmware).
    pub tag: String,
    /// The GitHub release page, for a guided app download.
    pub url: String,
    pub prerelease: bool,
    pub draft: bool,
    pub assets: Vec<ReleaseAsset>,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// Run `curl` with the given args, returning stdout on success. Errors on a
/// non-zero exit (network failure, non-2xx via `-f`, etc.).
fn curl(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("curl")
        .args(args)
        .output()
        .context("running curl")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("curl failed: {}", err.trim()));
    }
    Ok(out.stdout)
}

/// Fetch the most recent published releases (newest-first). Blocking — call from
/// `spawn_blocking`. Callers treat any error as "no info this round".
pub fn fetch_releases() -> Result<Vec<Release>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=30");
    let body = curl(&[
        "-fsSL",
        "--max-time",
        "15",
        "-H",
        "Accept: application/vnd.github+json",
        // GitHub rejects API requests without a User-Agent.
        "-H",
        "User-Agent: agent-buddy",
        &url,
    ])?;
    parse_releases(&body)
}

/// Parse the GitHub `GET /releases` JSON array. Split out from [`fetch_releases`]
/// so it's unit-testable without the network.
fn parse_releases(body: &[u8]) -> Result<Vec<Release>> {
    let v: serde_json::Value =
        serde_json::from_slice(body).context("parsing the GitHub releases JSON")?;
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow!("releases JSON was not an array"))?;
    Ok(arr.iter().filter_map(parse_release).collect())
}

fn parse_release(v: &serde_json::Value) -> Option<Release> {
    let tag = v.get("tag_name")?.as_str()?.to_string();
    let url = v
        .get("html_url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    let prerelease = v.get("prerelease").and_then(|b| b.as_bool()).unwrap_or(false);
    let draft = v.get("draft").and_then(|b| b.as_bool()).unwrap_or(false);
    let assets = v
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let name = a.get("name")?.as_str()?.to_string();
                    let url = a.get("browser_download_url")?.as_str()?.to_string();
                    Some(ReleaseAsset { name, url })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Release {
        tag,
        url,
        prerelease,
        draft,
        assets,
    })
}

/// The newest desktop-app release: a published (non-draft, non-prerelease) tag on
/// the app track (`v*`, NOT `fw-*`), highest version. Drives the app
/// "update available" banner. `None` if the list has no comparable app release.
pub fn latest_app_release(releases: &[Release]) -> Option<&Release> {
    releases
        .iter()
        .filter(|r| !r.draft && !r.prerelease)
        // App track only: `v0.1.3` yes, `fw-v0.1.4` no.
        .filter(|r| r.tag.starts_with('v'))
        .filter_map(|r| parse_version(&r.tag).map(|v| (v, r)))
        .max_by_key(|(v, _)| *v)
        .map(|(_, r)| r)
}

/// The newest firmware available for `board`, across ALL published releases that
/// carry a matching `firmware-<board>.bin` asset — both the desktop (`v*`) and
/// firmware-only (`fw-v*`) tracks. Returns the clean firmware version string
/// (the tag with any `fw-` prefix stripped, e.g. `"v0.1.4"`) and the `.bin`
/// asset's download URL. `None` if no release offers an image for the board.
pub fn latest_firmware(releases: &[Release], board: &str) -> Option<(String, String)> {
    let names = crate::ota::firmware_filenames(board);
    releases
        .iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter_map(|r| {
            let clean = firmware_version_string(&r.tag);
            let ver = parse_version(&clean)?;
            let asset = names.iter().find_map(|n| r.asset(n))?;
            Some((ver, clean, asset.url.clone()))
        })
        .max_by_key(|(ver, _, _)| *ver)
        .map(|(_, vstr, url)| (vstr, url))
}

/// Every board id any release offers a `firmware-<board>.bin` image for, plus
/// the default board (so the legacy un-suffixed `firmware.bin` is always
/// considered). The daemon iterates these to build the per-board firmware offer.
pub fn firmware_boards(releases: &[Release]) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    set.insert(crate::ota::DEFAULT_BOARD.to_string());
    for r in releases {
        for a in &r.assets {
            if let Some(board) = a
                .name
                .strip_prefix("firmware-")
                .and_then(|rest| rest.strip_suffix(".bin"))
            {
                set.insert(board.to_string());
            }
        }
    }
    set.into_iter().collect()
}

/// A release tag as a clean firmware version string: the firmware track tags
/// releases `fw-vX.Y.Z`, but the firmware itself reports a clean `vX.Y.Z` (see
/// `firmware/scripts/fw_version.py`), so strip the routing-only `fw-` prefix.
fn firmware_version_string(tag: &str) -> String {
    tag.strip_prefix("fw-").unwrap_or(tag).to_string()
}

/// Download a release `.bin` asset into memory. Follows redirects (`-L`): GitHub
/// asset URLs 302 to `objects.githubusercontent.com`. Generous timeout for a
/// ~2MB image on a slow link. Blocking.
pub fn download_firmware(url: &str) -> Result<Vec<u8>> {
    let bytes = curl(&[
        "-fsSL",
        "-L",
        "--max-time",
        "120",
        "-H",
        "User-Agent: agent-buddy",
        url,
    ])?;
    if bytes.is_empty() {
        return Err(anyhow!("the downloaded firmware was empty"));
    }
    Ok(bytes)
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

    #[test]
    fn fw_version_string_strips_routing_prefix() {
        assert_eq!(firmware_version_string("fw-v0.1.4"), "v0.1.4");
        assert_eq!(firmware_version_string("v0.1.3"), "v0.1.3");
        assert_eq!(firmware_version_string("fw-v0.1.4-2-gabc"), "v0.1.4-2-gabc");
    }

    /// A desktop `v0.1.3` release (desktop + firmware assets) and a *later*
    /// firmware-only `fw-v0.1.4` release (firmware assets only). Exercises track
    /// separation, the CYD legacy `firmware.bin` fallback, and per-board lookup.
    const SAMPLE: &str = r#"[
      {
        "tag_name": "fw-v0.1.4", "html_url": "https://example/fw-v0.1.4",
        "draft": false, "prerelease": false,
        "assets": [
          {"name": "firmware-cyd.bin",      "browser_download_url": "https://dl/cyd-0.1.4.bin"},
          {"name": "firmware-cyd.version",  "browser_download_url": "https://dl/cyd-0.1.4.version"},
          {"name": "firmware-fnk0104.bin",  "browser_download_url": "https://dl/fnk-0.1.4.bin"}
        ]
      },
      {
        "tag_name": "v0.1.3", "html_url": "https://example/v0.1.3",
        "draft": false, "prerelease": false,
        "assets": [
          {"name": "Agent-Buddy-v0.1.3.dmg", "browser_download_url": "https://dl/app.dmg"},
          {"name": "firmware.bin",           "browser_download_url": "https://dl/legacy-0.1.3.bin"},
          {"name": "firmware-fnk0104.bin",   "browser_download_url": "https://dl/fnk-0.1.3.bin"}
        ]
      },
      {
        "tag_name": "v0.0.9-rc1", "html_url": "https://example/rc",
        "draft": false, "prerelease": true,
        "assets": [{"name": "firmware-cyd.bin", "browser_download_url": "https://dl/cyd-rc.bin"}]
      },
      {
        "tag_name": "v9.9.9", "html_url": "https://example/draft",
        "draft": true, "prerelease": false,
        "assets": [{"name": "firmware-cyd.bin", "browser_download_url": "https://dl/cyd-draft.bin"}]
      }
    ]"#;

    #[test]
    fn app_track_ignores_firmware_only_and_prerelease_and_draft() {
        let rels = parse_releases(SAMPLE.as_bytes()).unwrap();
        let app = latest_app_release(&rels).expect("an app release");
        // Newest published *app-track* tag — not fw-v0.1.4, not the rc, not the draft.
        assert_eq!(app.tag, "v0.1.3");
    }

    #[test]
    fn firmware_latest_spans_both_tracks_per_board() {
        let rels = parse_releases(SAMPLE.as_bytes()).unwrap();

        // CYD: newest image is the firmware-only fw-v0.1.4 (clean version v0.1.4).
        let (ver, url) = latest_firmware(&rels, "cyd").expect("cyd firmware");
        assert_eq!(ver, "v0.1.4");
        assert_eq!(url, "https://dl/cyd-0.1.4.bin");

        // FNK0104: both releases have it; newest is fw-v0.1.4.
        let (ver, url) = latest_firmware(&rels, "fnk0104").expect("fnk firmware");
        assert_eq!(ver, "v0.1.4");
        assert_eq!(url, "https://dl/fnk-0.1.4.bin");
    }

    #[test]
    fn cyd_legacy_firmware_bin_is_a_fallback_only_for_cyd() {
        // A release that ONLY has the legacy un-suffixed firmware.bin.
        let json = r#"[{
          "tag_name": "v0.1.0", "html_url": "h", "draft": false, "prerelease": false,
          "assets": [{"name": "firmware.bin", "browser_download_url": "https://dl/legacy.bin"}]
        }]"#;
        let rels = parse_releases(json.as_bytes()).unwrap();
        // CYD accepts the legacy name…
        assert_eq!(
            latest_firmware(&rels, "cyd"),
            Some(("v0.1.0".to_string(), "https://dl/legacy.bin".to_string()))
        );
        // …but another board must not flash the CYD image.
        assert_eq!(latest_firmware(&rels, "fnk0104"), None);
    }

    #[test]
    fn baked_version_is_present_and_parseable() {
        let v = env!("AGENT_BUDDY_VERSION");
        assert!(!v.is_empty());
        assert!(parse_version(v).is_some(), "baked version {v:?} should parse");
    }
}
