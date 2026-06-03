//! In-place self-update for the desktop app.
//!
//! The app self-replaces from a **signed + notarized** GitHub release asset and
//! relaunches — no browser, no manual drag. This is the path the guided download
//! used to stand in for; it became safe to do once the macOS bundles were
//! Developer ID-signed + notarized (an unsigned in-place swap can't clear
//! Gatekeeper on relaunch).
//!
//! macOS is implemented. The flow, all shelling out to system tools:
//!   1. download the `.dmg` to a temp work dir;
//!   2. `hdiutil attach` it and locate the `.app` inside;
//!   3. **verify** it: `codesign --verify` + `spctl --assess` (Gatekeeper), and
//!      that its signing team matches the app we're replacing — a look-alike
//!      download can't take over the install path;
//!   4. `ditto` the new app out to a staging dir, then detach the image;
//!   5. hand off to a small **detached** helper (`setsid`) that waits for this
//!      process to exit, swaps the bundle in place (with a backup/rollback), and
//!      `open`s the new app.
//!
//! The caller runs the swap by calling [`install_and_relaunch`] and, on `Ok`,
//! exiting the process with status 0 — the helper is already waiting. Exiting 0
//! matters: the app's launchd agent uses `KeepAlive { SuccessfulExit = false }`,
//! so a clean exit won't make launchd respawn the *old* bundle out from under
//! the helper; the helper's `open` is what brings the new one back.
//!
//! Other platforms return an error so the UI falls back to the guided download.

#[cfg(target_os = "macos")]
use anyhow::{anyhow, bail, Context};
use anyhow::Result;

/// Whether in-place self-update is implemented for this platform. The UI gates
/// the "Update & restart" button on this (falling back to the guided download
/// otherwise), so we never offer a one-click update we can't actually perform.
pub fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// Download, verify, and stage the update, then spawn the detached helper that
/// swaps the bundle and relaunches. On `Ok(())` the caller MUST exit the process
/// (status 0) promptly — the helper is already waiting on this PID. `stage` is
/// called with short human-readable progress labels ("Downloading update…",
/// "Verifying signature…", …) for the UI overlay.
#[cfg(target_os = "macos")]
pub fn install_and_relaunch(pkg_url: &str, stage: impl Fn(&str)) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("locating the running app")?;
    let target_app = app_bundle_of(&exe)
        .ok_or_else(|| anyhow!("not running from a .app bundle — can't self-replace"))?;

    let work = unique_tmp_dir()?;
    let dmg = work.join("update.dmg");
    let mnt = work.join("mnt");
    let staging = work.join("staging");
    std::fs::create_dir_all(&mnt).ok();
    std::fs::create_dir_all(&staging).ok();

    stage("Downloading update…");
    crate::update::download_to_file(pkg_url, &dmg)?;

    stage("Mounting…");
    run(
        "hdiutil",
        &[
            "attach",
            "-nobrowse",
            "-noverify",
            "-noautoopen",
            "-mountpoint",
            path_str(&mnt)?,
            path_str(&dmg)?,
        ],
    )
    .context("mounting the update disk image")?;

    // Everything between attach and detach must detach on the way out, success
    // or failure — a stranded mount is a real leak. Do the verify+stage inside a
    // closure, then detach unconditionally.
    let staged: Result<PathBuf> = (|| {
        let new_app = find_app_in(&mnt)?;
        stage("Verifying signature…");
        verify_signed(&new_app)?;
        ensure_same_identity(&target_app, &new_app)?;
        stage("Staging…");
        let name = new_app
            .file_name()
            .ok_or_else(|| anyhow!("update app has no name"))?;
        let staged = staging.join(name);
        run("ditto", &[path_str(&new_app)?, path_str(&staged)?])
            .context("copying the new app out of the image")?;
        Ok(staged)
    })();
    let _ = run("hdiutil", &["detach", "-quiet", path_str(&mnt)?]);
    let staged = staged?;

    stage("Installing…");
    let script = write_swap_script(&work, &staged, &target_app)?;

    // Detached helper: own session (setsid) so our exit can't HUP it; logs to the
    // work dir for postmortem. It gets our PID so it can wait for us to quit
    // before touching the bundle.
    let mut cmd = Command::new("/bin/bash");
    cmd.arg(path_str(&script)?)
        .arg(std::process::id().to_string())
        .stdin(Stdio::null());
    if let Ok(log) = std::fs::File::create(work.join("swap.log")) {
        if let Ok(errlog) = log.try_clone() {
            cmd.stdout(Stdio::from(log)).stderr(Stdio::from(errlog));
        }
    }
    // SAFETY: setsid() is async-signal-safe and the only call in the child
    // between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("launching the update helper")?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn install_and_relaunch(_pkg_url: &str, _stage: impl Fn(&str)) -> Result<()> {
    anyhow::bail!("in-place update isn't supported on this platform yet")
}

// --- macOS helpers --------------------------------------------------------
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};

/// The nearest ancestor of `p` that is a `.app` bundle (the thing we replace).
#[cfg(target_os = "macos")]
fn app_bundle_of(p: &Path) -> Option<PathBuf> {
    p.ancestors()
        .find(|a| a.extension().map(|e| e == "app").unwrap_or(false))
        .map(|a| a.to_path_buf())
}

/// The single `.app` at the top level of a mounted image.
#[cfg(target_os = "macos")]
fn find_app_in(dir: &Path) -> Result<PathBuf> {
    for entry in std::fs::read_dir(dir).context("reading the mounted image")? {
        let p = entry?.path();
        if p.extension().map(|e| e == "app").unwrap_or(false) {
            return Ok(p);
        }
    }
    bail!("no .app found in the update image")
}

/// Hard gate before we'll install anything: the new app must be validly signed
/// AND pass Gatekeeper (i.e. notarized). Either failing aborts the update with
/// the bundle untouched.
#[cfg(target_os = "macos")]
fn verify_signed(app: &Path) -> Result<()> {
    run(
        "codesign",
        &["--verify", "--deep", "--strict", path_str(app)?],
    )
    .context("the downloaded update isn't validly code-signed")?;
    run("spctl", &["--assess", "--type", "execute", path_str(app)?])
        .context("the downloaded update failed Gatekeeper assessment (not notarized?)")?;
    Ok(())
}

/// Refuse to install an update signed by a different team than the running app —
/// a look-alike notarized bundle shouldn't be able to seize the install path.
/// If either side is unsigned (a local dev build), we can't compare, so we don't
/// block.
#[cfg(target_os = "macos")]
fn ensure_same_identity(target: &Path, new: &Path) -> Result<()> {
    match (team_identifier(target), team_identifier(new)) {
        (Some(a), Some(b)) => {
            if a != b {
                bail!("update is signed by a different team ({b}) than the installed app ({a})");
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The `TeamIdentifier` from `codesign -dv`, or `None` if unsigned/unknown.
#[cfg(target_os = "macos")]
fn team_identifier(app: &Path) -> Option<String> {
    let path = app.to_str()?;
    let out = std::process::Command::new("codesign")
        .args(["-dv", "--verbose=4", path])
        .output()
        .ok()?;
    // codesign prints these fields to stderr.
    let text = String::from_utf8_lossy(&out.stderr);
    text.lines().find_map(|l| {
        l.strip_prefix("TeamIdentifier=")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != "not set")
    })
}

/// Write the detached swap+relaunch helper. It waits for `$1` (our PID) to exit,
/// moves the current bundle aside, `ditto`s the staged app into place (rolling
/// back on failure), clears any quarantine flag, relaunches, and cleans up.
#[cfg(target_os = "macos")]
fn write_swap_script(work: &Path, staged: &Path, target: &Path) -> Result<PathBuf> {
    let script = work.join("swap.sh");
    // Paths are embedded double-quoted; app bundle paths don't contain quotes.
    let body = format!(
        r#"#!/bin/bash
set -u
PID="$1"
STAGED="{staged}"
TARGET="{target}"
WORK="{work}"

# Wait (up to ~30s) for the app to fully exit so the bundle isn't in use.
for _ in $(seq 1 300); do
  kill -0 "$PID" 2>/dev/null || break
  sleep 0.1
done

BACKUP="${{TARGET}}.oldbundle"
rm -rf "$BACKUP"
if [ -d "$TARGET" ] && mv "$TARGET" "$BACKUP" 2>/dev/null; then
  # Moved the old bundle aside: install the new one, roll back on failure.
  if ditto "$STAGED" "$TARGET" 2>/dev/null; then
    rm -rf "$BACKUP"
  else
    rm -rf "$TARGET"
    mv "$BACKUP" "$TARGET" 2>/dev/null
  fi
else
  # Couldn't move it aside (gone, or no permission) — best-effort overwrite.
  ditto "$STAGED" "$TARGET" 2>/dev/null || true
fi

# Always relaunch *something* so we never strand the user without an app.
xattr -dr com.apple.quarantine "$TARGET" 2>/dev/null
open "$TARGET" 2>/dev/null || open -a "Agent Buddy" 2>/dev/null || true

cd /
rm -rf "$WORK"
"#,
        staged = staged.display(),
        target = target.display(),
        work = work.display(),
    );
    std::fs::write(&script, body).context("writing the update helper")?;
    Ok(script)
}

/// A fresh, uniquely-named temp dir for one update run.
#[cfg(target_os = "macos")]
fn unique_tmp_dir() -> Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "agent-buddy-update-{}-{}",
        std::process::id(),
        stamp
    ));
    std::fs::create_dir_all(&dir).context("creating the update work dir")?;
    Ok(dir)
}

/// Borrow a path as `&str`, erroring on non-UTF-8 (we control all of these).
#[cfg(target_os = "macos")]
fn path_str(p: &Path) -> Result<&str> {
    p.to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 path: {}", p.display()))
}

/// Run a command to completion, turning a non-zero exit into an error carrying
/// its stderr.
#[cfg(target_os = "macos")]
fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("running {cmd}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        if err.is_empty() {
            bail!("{cmd} exited with {}", out.status);
        }
        bail!("{cmd}: {err}");
    }
    Ok(())
}
