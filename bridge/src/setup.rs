//! `claude-buddy setup`: wire our hooks into Claude Code's settings and
//! install the daemon as a per-user background service. Idempotent.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Default set of tools whose permission prompts route to the buddy. Other
/// tools are untouched (normal Claude Code flow). Override with `--tools`.
pub const DEFAULT_MATCHER: &str = "Bash|Write|Edit|MultiEdit|NotebookEdit|WebFetch";

/// The hook events we register and the Claude Code event name each maps to.
const STATE_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "Stop",
    "SubagentStop",
    "Notification",
];

pub fn run(matcher: &str, install_service: bool) -> Result<()> {
    let exe = std::env::current_exe().context("locating the claude-buddy binary")?;
    let exe_str = exe.to_string_lossy().to_string();

    // When installing the service, copy the daemon to its stable location up
    // front and point BOTH the Claude Code hooks and the service at that copy.
    // Otherwise the hooks reference wherever setup happened to run from — e.g.
    // `target/release/claude-buddy`, which the next `cargo build` overwrites in
    // place (breaking its code identity), or a path that later disappears.
    let hook_target = if install_service {
        install_daemon_binary(&exe_str).unwrap_or_else(|_| exe_str.clone())
    } else {
        exe_str.clone()
    };

    let settings_path = claude_settings_path()?;
    wire_hooks(&settings_path, &hook_target, matcher)?;
    println!("✓ wired hooks into {}", settings_path.display());
    println!("  gate + telemetry matcher: {matcher}");

    if install_service {
        match service_install_and_start(&hook_target) {
            Ok(note) => println!("✓ {note}"),
            Err(e) => {
                println!("! could not install/start the background service automatically: {e}");
                print_manual_service_hint(&exe_str);
            }
        }
        // If the desktop app was installed alongside, make it a first-class app
        // on this machine. Best-effort: a daemon-only install simply skips this.
        if app_exe_path().ok().flatten().is_some() {
            match register_desktop_app() {
                Ok(note) => println!("✓ {note}"),
                Err(e) => println!("! could not register the desktop app: {e}"),
            }
        }
    } else {
        print_manual_service_hint(&exe_str);
    }

    println!("\nNext: power on your buddy, then run `claude-buddy pair` to confirm the link.");
    Ok(())
}

/// Merge our hook entries into `~/.claude/settings.json`, replacing any prior
/// `claude-buddy` entries so re-running setup is safe.
fn wire_hooks(path: &PathBuf, exe: &str, matcher: &str) -> Result<()> {
    let mut root: Value = match std::fs::read(path) {
        Ok(b) if !b.is_empty() => serde_json::from_slice(&b)
            .with_context(|| format!("parsing existing {}", path.display()))?,
        _ => json!({}),
    };
    if !root.is_object() {
        return Err(anyhow!("{} is not a JSON object", path.display()));
    }

    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("`hooks` in settings.json is not an object"))?;

    // Quote the path: hook commands run through `/bin/sh`, and our install
    // locations contain spaces (e.g. `…/Claude Buddy.app/…`, `…/Application
    // Support/…`). Without quotes the shell splits the path mid-word.
    let q = shell_quote(exe);

    // PermissionRequest: the matcher-scoped approve/deny gate. Claude raises it
    // ONLY when it would actually prompt the user, so the device mirrors the
    // real session's prompts — never auto-approved, allow-listed, bypass-mode,
    // or autonomous subagent tool calls (all of which still fire PreToolUse).
    let permreq_entry = json!({
        "matcher": matcher,
        "hooks": [ { "type": "command", "command": format!("{q} hook PermissionRequest") } ]
    });
    set_event(hooks, "PermissionRequest", permreq_entry, exe);

    // PreToolUse: matcher-scoped telemetry heartbeat. Fires for every matched
    // tool call (it no longer gates) so the device's token / context readout
    // tracks the turn mid-flight instead of only jumping at Stop.
    let pretool_entry = json!({
        "matcher": matcher,
        "hooks": [ { "type": "command", "command": format!("{q} hook PreToolUse") } ]
    });
    set_event(hooks, "PreToolUse", pretool_entry, exe);

    // State events: matcher-less command hooks.
    for ev in STATE_EVENTS {
        let entry = json!({
            "hooks": [ { "type": "command", "command": format!("{q} hook {ev}") } ]
        });
        set_event(hooks, ev, entry, exe);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, serde_json::to_vec_pretty(&root)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Replace any existing claude-buddy entry for `event` with `entry`, leaving
/// the user's other hooks for that event intact.
fn set_event(hooks: &mut serde_json::Map<String, Value>, event: &str, entry: Value, exe: &str) {
    let arr = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
    let Some(arr) = arr.as_array_mut() else {
        *arr = json!([entry]);
        return;
    };
    arr.retain(|e| !is_ours(e, exe));
    arr.push(entry);
}

/// True if a hook entry's command belongs to us (so we can replace it). Matches
/// any invocation of the `claude-buddy` binary with a `hook` subcommand —
/// quoted or unquoted, at any install path — so re-running setup cleans up
/// entries written by an earlier version (e.g. the old unquoted form) instead
/// of leaving duplicates. A user hook that doesn't run `claude-buddy hook` is
/// left untouched.
fn is_ours(entry: &Value, _exe: &str) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|cmds| {
            cmds.iter().any(|c| {
                c.get("command")
                    .and_then(|v| v.as_str())
                    .map(|s| s.contains("claude-buddy") && s.contains(" hook "))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Quote a path for safe interpolation into a hook command string. Hook
/// commands run through a shell, and our install paths contain spaces. Double
/// quotes work for both `/bin/sh` and Windows `cmd.exe`; our paths never
/// contain characters those shells treat specially inside double quotes.
fn shell_quote(s: &str) -> String {
    format!("\"{s}\"")
}

fn claude_settings_path() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("could not find home directory")?;
    Ok(base.home_dir().join(".claude").join("settings.json"))
}

// ---------------------------------------------------------------------------
// Background service (best-effort, per-platform). Scaffold: writes the unit
// file; activation is printed for the user to confirm.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn install_daemon_service(exe: &str) -> Result<String> {
    let base = directories::BaseDirs::new().context("home dir")?;
    let dir = base.home_dir().join("Library/LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    let plist = dir.join("com.anthropic.claude-buddy.plist");
    let log = base
        .home_dir()
        .join("Library/Logs/claude-buddy.log")
        .to_string_lossy()
        .into_owned();
    // KeepAlive stays unconditional (`<true/>`): a *conditional* KeepAlive that
    // leaves a clean exit alone is only safe once the daemon exits 0 on benign
    // single-instance lock contention. While the daemon still bails non-zero on
    // contention, `SuccessfulExit:false` would re-spawn it into a throttled
    // crash loop — so that change must land together with the daemon's exit-code
    // fix, not before it. (ProgramArguments points at the codesigned helper
    // bundle so the daemon's Bluetooth TCC grant keys to a stable identity.)
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.anthropic.claude-buddy</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>"#
    );

    // Only touch the loaded agent when the definition actually changed — a
    // routine re-run with identical bytes must leave the healthy, running daemon
    // alone (no unload/load churn). On a genuine change (e.g. the binary moved
    // into the helper bundle), rewrite the plist and re-bootstrap it so launchd
    // picks up the new definition.
    let unchanged = std::fs::read(&plist)
        .map(|b| b == body.as_bytes())
        .unwrap_or(false);
    if unchanged {
        return Ok(format!("LaunchAgent {} already current", plist.display()));
    }
    std::fs::write(&plist, &body)?;
    if mac_service_loaded() {
        let p = plist.to_string_lossy().into_owned();
        // Reload so the new definition takes effect. `unload`+`load` is the
        // portable form; this is the one place it's warranted (the definition
        // genuinely changed), not the steady-state Start path.
        let _ = run_cmd("launchctl", &["unload", &p]);
        let _ = run_cmd("launchctl", &["load", "-w", &p]);
    }
    Ok(format!(
        "wrote LaunchAgent {}. Start it now with:\n    launchctl load {}",
        plist.display(),
        plist.display()
    ))
}

#[cfg(target_os = "linux")]
fn install_daemon_service(exe: &str) -> Result<String> {
    let base = directories::BaseDirs::new().context("home dir")?;
    let dir = base.home_dir().join(".config/systemd/user");
    std::fs::create_dir_all(&dir)?;
    let unit = dir.join("claude-buddy.service");
    let body = format!(
        "[Unit]\nDescription=Claude buddy bridge daemon\n\n\
         [Service]\nExecStart={exe} daemon\nRestart=always\n\n\
         [Install]\nWantedBy=default.target\n"
    );
    std::fs::write(&unit, body)?;
    Ok(format!(
        "wrote systemd unit {}. Start it now with:\n    systemctl --user enable --now claude-buddy",
        unit.display()
    ))
}

#[cfg(target_os = "windows")]
fn install_daemon_service(exe: &str) -> Result<String> {
    // Per-user logon-triggered Scheduled Task — the lightweight equivalent of a
    // LaunchAgent / systemd user unit. No elevation, survives the GUI closing,
    // restarts at next logon.
    run_cmd(
        "schtasks",
        &[
            "/Create",
            "/F",
            "/SC",
            "ONLOGON",
            "/TN",
            WIN_TASK,
            "/TR",
            &format!("\"{exe}\" daemon"),
        ],
    )?;
    Ok(format!(
        "registered logon task {WIN_TASK}. Start it now with:\n    schtasks /Run /TN {WIN_TASK}"
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn install_daemon_service(_exe: &str) -> Result<String> {
    Err(anyhow!("unsupported platform"))
}

fn print_manual_service_hint(exe: &str) {
    println!("\nTo run the daemon manually (for testing):\n    {exe} daemon");
}

// ---------------------------------------------------------------------------
// Service control — used by the desktop app to install/start/stop the daemon
// without the user touching a terminal. Each shells out to the platform's
// service manager and surfaces any failure as an error with the tool's stderr.
// ---------------------------------------------------------------------------

/// The macOS LaunchAgent label / plist basename and the Windows task name.
#[cfg(target_os = "macos")]
const MAC_LABEL: &str = "com.anthropic.claude-buddy";
/// Stable bundle identifier for the daemon helper .app. macOS keys the
/// Bluetooth (TCC) grant to the code identity derived from this id + the
/// ad-hoc signature, so it MUST stay constant across rebuilds or the user gets
/// re-prompted (or silently denied) every time.
#[cfg(target_os = "macos")]
const MAC_DAEMON_BUNDLE_ID: &str = "com.anthropic.claude-buddy-helper";

/// Self-signed code-signing identity (created once by the user via
/// `codesign`/Keychain) used to sign the helper + app bundles when present.
/// Signing with a STABLE cert instead of ad-hoc `-` keeps each bundle's
/// designated requirement constant across rebuilds (it becomes `identifier +
/// certificate-leaf-hash`), so the macOS Bluetooth / Local-Network (TCC) grants
/// PERSIST instead of resetting every build. Falls back to ad-hoc if absent.
#[cfg(target_os = "macos")]
const MAC_SIGN_IDENTITY: &str = "Claude Buddy Dev";

/// Codesign a bundle, preferring [`MAC_SIGN_IDENTITY`] and falling back to
/// ad-hoc. Best-effort — an unsigned bundle still runs, it just re-prompts for
/// permissions. `identifier` pins the designated requirement's identifier.
#[cfg(target_os = "macos")]
fn mac_codesign(bundle: &str, identifier: &str, deep: bool) {
    let try_sign = |identity: &str| {
        let mut args: Vec<&str> = vec!["--force"];
        if deep {
            args.push("--deep");
        }
        args.extend_from_slice(&["--sign", identity, "-i", identifier, bundle]);
        run_cmd("codesign", &args)
    };
    if try_sign(MAC_SIGN_IDENTITY).is_err() {
        let _ = try_sign("-"); // ad-hoc fallback when the dev cert isn't installed
    }
}

#[cfg(target_os = "windows")]
const WIN_TASK: &str = "ClaudeBuddy";

/// Locate the `claude-buddy` daemon binary to register as the service. Prefers
/// a copy sitting next to the currently-running executable (so the GUI, which
/// ships beside the daemon, points the service at the bundled daemon), then
/// falls back to the bare name on `PATH`.
pub fn daemon_exe_path() -> Result<String> {
    let here = std::env::current_exe().context("locating this executable")?;
    if let Some(dir) = here.parent() {
        let name = if cfg!(windows) {
            "claude-buddy.exe"
        } else {
            "claude-buddy"
        };
        let candidate = dir.join(name);
        if candidate.exists() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    Ok(if cfg!(windows) {
        "claude-buddy.exe"
    } else {
        "claude-buddy"
    }
    .to_string())
}

/// Run a command, returning a friendly error (with stderr) on non-zero exit.
fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("running `{cmd}`"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    Err(anyhow!(
        "`{cmd}` failed{}",
        if err.is_empty() {
            String::new()
        } else {
            format!(": {err}")
        }
    ))
}

/// Stable install path for the daemon binary: a per-user app-data dir that
/// `cargo build` never writes to. The service points *here* rather than at the
/// dev build output, so a routine rebuild can't swap the binary out from under
/// the running service — which on macOS would change its (ad-hoc) code identity
/// and silently revoke its granted Bluetooth permission.
///
/// On macOS the daemon lives inside a minimal `.app` bundle
/// (`Claude Buddy Helper.app/Contents/MacOS/claude-buddy`) so its sibling
/// `Info.plist` can declare `NSBluetoothAlwaysUsageDescription` and a stable
/// `CFBundleIdentifier`. The daemon — not the GUI — is the process that opens
/// CoreBluetooth, so the TCC usage string and code identity must hang off *its*
/// bundle, not the GUI's.
#[cfg(not(target_os = "macos"))]
fn installed_daemon_path() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new().context("home dir")?;
    let name = if cfg!(windows) {
        "claude-buddy.exe"
    } else {
        "claude-buddy"
    };
    Ok(dirs
        .data_local_dir()
        .join("Claude Buddy")
        .join("bin")
        .join(name))
}

#[cfg(target_os = "macos")]
fn installed_daemon_path() -> Result<PathBuf> {
    Ok(mac_daemon_bundle()?.join("Contents/MacOS/claude-buddy"))
}

/// The daemon helper `.app` bundle root.
#[cfg(target_os = "macos")]
fn mac_daemon_bundle() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new().context("home dir")?;
    Ok(dirs
        .data_local_dir()
        .join("Claude Buddy")
        .join("Claude Buddy Helper.app"))
}

/// Install an executable from `src` to `dest` via a same-directory temp file and
/// an atomic rename, giving `dest` a fresh inode. This is essential on macOS:
/// overwriting an executable in place (`std::fs::copy` truncates the existing
/// inode) leaves the kernel's cached code identity pointing at the old bytes, so
/// the next launch is killed by AMFI ("Killed: 9"). A rename swaps in a new
/// inode the kernel evaluates fresh — and leaves any process still running the
/// old inode untouched, so it's safe even when `dest` is the running daemon.
fn install_binary(src: &str, dest: &std::path::Path) -> Result<()> {
    let parent = dest.parent().context("install destination has no parent")?;
    std::fs::create_dir_all(parent)?;
    let stem = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("binary");
    let tmp = parent.join(format!(".{stem}.new"));
    std::fs::copy(src, &tmp).with_context(|| format!("staging install to {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, dest).with_context(|| format!("installing {}", dest.display()))?;
    Ok(())
}

/// True if two paths resolve to the same file (so we can skip a self-copy).
fn same_file(a: &str, b: &std::path::Path) -> bool {
    std::fs::canonicalize(a)
        .ok()
        .zip(std::fs::canonicalize(b).ok())
        .map(|(a, b)| a == b)
        .unwrap_or(false)
}

/// Install the daemon binary to its stable location, returning that path. No-op
/// when `src` already *is* the install.
#[cfg(not(target_os = "macos"))]
fn install_daemon_binary(src: &str) -> Result<String> {
    let dest = installed_daemon_path()?;
    if !same_file(src, &dest) {
        install_binary(src, &dest)?;
    }
    Ok(dest.to_string_lossy().into_owned())
}

/// macOS: install the daemon *inside* a helper `.app` bundle, refresh its
/// `Info.plist` (NSBluetoothAlwaysUsageDescription + stable CFBundleIdentifier),
/// and ad-hoc codesign the bundle so the Bluetooth TCC grant keys to a durable
/// code identity. The atomic-rename install (`install_binary`) gives the
/// executable a fresh inode — required to dodge AMFI "Killed: 9" — but the
/// signature must be (re)applied afterward, since signing the old inode doesn't
/// carry over. We always rewrite the plist + re-sign so a bundle left in a
/// half-installed state by an older version self-heals.
#[cfg(target_os = "macos")]
fn install_daemon_binary(src: &str) -> Result<String> {
    let bundle = mac_daemon_bundle()?;
    let contents = bundle.join("Contents");
    let macos_dir = contents.join("MacOS");
    std::fs::create_dir_all(&macos_dir)?;

    let dest = macos_dir.join("claude-buddy");
    if !same_file(src, &dest) {
        install_binary(src, &dest)?;
    }

    // Sibling Info.plist carrying the Bluetooth usage string macOS shows in the
    // permission prompt, plus the stable bundle id the TCC grant keys to.
    // Central role only — NSBluetoothPeripheralUsageDescription is not needed.
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Claude Buddy Helper</string>
  <key>CFBundleIdentifier</key><string>{MAC_DAEMON_BUNDLE_ID}</string>
  <key>CFBundleExecutable</key><string>claude-buddy</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>{ver}</string>
  <key>CFBundleShortVersionString</key><string>{ver}</string>
  <key>LSMinimumSystemVersion</key><string>10.13</string>
  <key>LSBackgroundOnly</key><true/>
  <key>NSBluetoothAlwaysUsageDescription</key><string>Claude Buddy connects to your buddy device over Bluetooth.</string>
</dict></plist>"#,
        ver = env!("CARGO_PKG_VERSION")
    );
    std::fs::write(contents.join("Info.plist"), plist).context("writing daemon Info.plist")?;

    // Sign the bundle so the daemon's Bluetooth TCC grant attaches to a durable
    // identity. Prefers the stable self-signed dev cert (grant persists across
    // rebuilds), falls back to ad-hoc.
    mac_codesign(&bundle.to_string_lossy(), MAC_DAEMON_BUNDLE_ID, false);

    Ok(dest.to_string_lossy().into_owned())
}

/// Install the daemon as a background service AND start it now. Idempotent —
/// safe to call when it's already installed/running. The binary is first copied
/// to a stable install location so the service is insulated from dev rebuilds.
pub fn service_install_and_start(exe: &str) -> Result<String> {
    let installed = install_daemon_binary(exe)?;
    install_daemon_service(&installed)?;
    service_start()?;
    Ok(format!(
        "background service installed from {installed} and started"
    ))
}

/// Start the (already-installed) background service. Idempotent and gentle: if
/// the agent is already loaded we leave it alone — KeepAlive guarantees launchd
/// is keeping it alive, so churning it with unload/load would only tear down a
/// healthy BLE link and risk a flock race against the respawn. We only
/// bootstrap (load) when it isn't loaded yet.
#[cfg(target_os = "macos")]
pub fn service_start() -> Result<String> {
    if mac_service_loaded() {
        return Ok("service already running".into());
    }
    let plist = mac_plist_path()?;
    run_cmd("launchctl", &["load", "-w", &plist])?;
    Ok("service started".into())
}

/// Restart the running service the right way: `launchctl kickstart -k` asks
/// launchd to kill and re-exec the job in place. This preserves the KeepAlive
/// contract (the job stays loaded the whole time) and avoids the unload/load
/// flock race — far safer than tearing the agent down and bringing it back up.
/// Falls back to a fresh `service_start` if the job isn't loaded at all.
#[cfg(target_os = "macos")]
pub fn service_restart() -> Result<String> {
    if !mac_service_loaded() {
        return service_start();
    }
    let target = format!("gui/{}/{MAC_LABEL}", unsafe { libc::getuid() });
    run_cmd("launchctl", &["kickstart", "-k", &target])?;
    Ok("service restarted".into())
}

/// True if the LaunchAgent is currently loaded into the user's GUI domain.
/// Probes `launchctl print`, falling back to `launchctl list` on older systems
/// that don't speak the modern domain syntax.
#[cfg(target_os = "macos")]
fn mac_service_loaded() -> bool {
    let target = format!("gui/{}/{MAC_LABEL}", unsafe { libc::getuid() });
    let printed = std::process::Command::new("launchctl")
        .args(["print", &target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if printed {
        return true;
    }
    std::process::Command::new("launchctl")
        .args(["list", MAC_LABEL])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
pub fn service_stop() -> Result<String> {
    let plist = mac_plist_path()?;
    run_cmd("launchctl", &["unload", "-w", &plist])?;
    Ok("service stopped".into())
}

#[cfg(target_os = "macos")]
fn mac_plist_path() -> Result<String> {
    let base = directories::BaseDirs::new().context("home dir")?;
    Ok(base
        .home_dir()
        .join("Library/LaunchAgents")
        .join(format!("{MAC_LABEL}.plist"))
        .to_string_lossy()
        .into_owned())
}

#[cfg(target_os = "linux")]
pub fn service_start() -> Result<String> {
    let _ = run_cmd("systemctl", &["--user", "daemon-reload"]);
    run_cmd("systemctl", &["--user", "enable", "--now", "claude-buddy"])?;
    Ok("service started".into())
}

#[cfg(target_os = "linux")]
pub fn service_restart() -> Result<String> {
    let _ = run_cmd("systemctl", &["--user", "daemon-reload"]);
    run_cmd("systemctl", &["--user", "restart", "claude-buddy"])?;
    Ok("service restarted".into())
}

#[cfg(target_os = "linux")]
pub fn service_stop() -> Result<String> {
    run_cmd("systemctl", &["--user", "stop", "claude-buddy"])?;
    Ok("service stopped".into())
}

#[cfg(target_os = "windows")]
pub fn service_start() -> Result<String> {
    run_cmd("schtasks", &["/Run", "/TN", WIN_TASK])?;
    Ok("service started".into())
}

#[cfg(target_os = "windows")]
pub fn service_restart() -> Result<String> {
    // End the current run (the logon task's own restart settings bring it back),
    // make sure the process is gone, then kick a fresh run immediately.
    let _ = run_cmd("schtasks", &["/End", "/TN", WIN_TASK]);
    let _ = run_cmd("taskkill", &["/IM", "claude-buddy.exe", "/F"]);
    run_cmd("schtasks", &["/Run", "/TN", WIN_TASK])?;
    Ok("service restarted".into())
}

#[cfg(target_os = "windows")]
pub fn service_stop() -> Result<String> {
    // End the scheduled run, then make sure the process is actually gone.
    let _ = run_cmd("schtasks", &["/End", "/TN", WIN_TASK]);
    let _ = run_cmd("taskkill", &["/IM", "claude-buddy.exe", "/F"]);
    Ok("service stopped".into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn service_start() -> Result<String> {
    Err(anyhow!("service control is unsupported on this platform"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn service_restart() -> Result<String> {
    Err(anyhow!("service control is unsupported on this platform"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn service_stop() -> Result<String> {
    Err(anyhow!("service control is unsupported on this platform"))
}

// ---------------------------------------------------------------------------
// Desktop app "open at login". Like the daemon service, but for the GUI: it
// auto-starts at login and after a reboot. Unlike the daemon it is NOT kept
// alive on a clean exit — quitting from the tray stays quit until next login;
// only a crash respawns it.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
const MAC_APP_LABEL: &str = "com.anthropic.claude-buddy-app";
#[cfg(target_os = "windows")]
const WIN_APP_TASK: &str = "ClaudeBuddyApp";

/// Locate the desktop app binary (`claude-buddy-app`) sitting next to this
/// executable, if present. `None` means it isn't installed alongside, so the
/// caller should skip wiring its login item.
pub fn app_exe_path() -> Result<Option<String>> {
    let here = std::env::current_exe().context("locating this executable")?;
    let dir = here
        .parent()
        .context("executable has no parent directory")?;
    let name = if cfg!(windows) {
        "claude-buddy-app.exe"
    } else {
        "claude-buddy-app"
    };
    let cand = dir.join(name);
    Ok(cand.exists().then(|| cand.to_string_lossy().into_owned()))
}

/// Register the desktop app to open at login (and start it now). Idempotent.
#[cfg(target_os = "macos")]
pub fn install_app_login_item(app_exe: &str) -> Result<String> {
    let base = directories::BaseDirs::new().context("home dir")?;
    let dir = base.home_dir().join("Library/LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    let plist = dir.join(format!("{MAC_APP_LABEL}.plist"));
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{MAC_APP_LABEL}</string>
  <key>ProgramArguments</key><array><string>{app_exe}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
</dict></plist>"#
    );
    std::fs::write(&plist, body)?;
    let p = plist.to_string_lossy().into_owned();
    let _ = run_cmd("launchctl", &["unload", &p]); // drop any stale definition
    run_cmd("launchctl", &["load", "-w", &p])?;
    Ok("desktop app set to open at login".into())
}

#[cfg(target_os = "windows")]
pub fn install_app_login_item(app_exe: &str) -> Result<String> {
    run_cmd(
        "schtasks",
        &[
            "/Create",
            "/F",
            "/SC",
            "ONLOGON",
            "/TN",
            WIN_APP_TASK,
            "/TR",
            &format!("\"{app_exe}\""),
        ],
    )?;
    let _ = run_cmd("schtasks", &["/Run", "/TN", WIN_APP_TASK]);
    Ok("desktop app set to open at login".into())
}

#[cfg(target_os = "linux")]
pub fn install_app_login_item(_app_exe: &str) -> Result<String> {
    // The GUI/tray needs a graphical session; autostart belongs to the desktop
    // environment (an XDG `~/.config/autostart/*.desktop` entry), not a systemd
    // user unit, and varies by DE — left to the user for now.
    Err(anyhow!(
        "open-at-login isn’t wired for Linux yet — add an XDG autostart .desktop entry"
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn install_app_login_item(_app_exe: &str) -> Result<String> {
    Err(anyhow!("open-at-login is unsupported on this platform"))
}

// ---------------------------------------------------------------------------
// Desktop launcher — register the GUI as a *clickable* app, distinct from the
// login item above. The login item is about auto-starting at login; this is
// about being reopenable like any other app once it's been quit: it shows up in
// Finder/Spotlight (macOS .app bundle in /Applications), the Start Menu
// (Windows .lnk shortcut), and the application menu (Linux XDG .desktop entry).
// Without this the GUI is just a bare binary with no way to relaunch by hand.
// ---------------------------------------------------------------------------

/// Make the desktop GUI a first-class app on this machine: register a clickable
/// launcher AND set it to open at login, pointed at the same artifact. Returns
/// a one-line summary. Idempotent — safe to re-run.
pub fn register_desktop_app() -> Result<String> {
    let app = app_exe_path()?
        .ok_or_else(|| anyhow!("the desktop app binary isn't installed alongside this one"))?;
    let (launch_exe, launcher_note) = install_desktop_launcher(&app)?;
    let login_note = match install_app_login_item(&launch_exe) {
        Ok(n) => n,
        // Registering the launcher is the headline; a login-item failure (e.g.
        // Linux, where autostart is left to the DE) shouldn't fail the whole op.
        Err(e) => format!("login item skipped ({e})"),
    };
    Ok(format!("{launcher_note}; {login_note}"))
}

/// Install the clickable launcher for the GUI. Returns the executable the launcher
/// points at (which the login item should also use) plus a human-readable note.
#[cfg(target_os = "macos")]
pub fn install_desktop_launcher(app_exe: &str) -> Result<(String, String)> {
    // Prefer /Applications; fall back to ~/Applications if that isn't writable
    // (managed Macs) so the install still succeeds without sudo.
    let probe = PathBuf::from("/Applications/Claude Buddy.app/Contents/MacOS");
    let bundle = if std::fs::create_dir_all(&probe).is_ok() {
        PathBuf::from("/Applications/Claude Buddy.app")
    } else {
        let base = directories::BaseDirs::new().context("home dir")?;
        base.home_dir().join("Applications/Claude Buddy.app")
    };
    let contents = bundle.join("Contents");
    let macos_dir = contents.join("MacOS");
    std::fs::create_dir_all(&macos_dir)?;
    std::fs::create_dir_all(contents.join("Resources"))?;

    // Make the bundle self-contained: the GUI *and* the daemon must both live in
    // Contents/MacOS, because the GUI's Install button locates the daemon as its
    // sibling (`daemon_exe_path`). Skip any copy we'd be making onto ourselves
    // (re-running setup from inside the installed bundle). Atomic-rename installs
    // avoid the AMFI "Killed: 9" that in-place overwrites of a registered bundle
    // binary trigger on Apple Silicon.
    let dest_exe = macos_dir.join("claude-buddy-app");
    if !same_file(app_exe, &dest_exe) {
        install_binary(app_exe, &dest_exe)?;
    }
    // The daemon sits beside the GUI in its source dir; bring it along. Any
    // firmware images sitting there too (release artifacts / dev build) are
    // bundled into Resources/ so the app's one-click "Update firmware" button
    // has something to flash for whichever board is connected. Each board ships
    // its own image (firmware-<board>.bin + .version) plus a legacy firmware.bin
    // (= CYD); copy them all verbatim — see ota::bundled_firmware_path.
    if let Some(src_dir) = std::path::Path::new(app_exe).parent() {
        let daemon_src = src_dir.join("claude-buddy");
        let daemon_dest = macos_dir.join("claude-buddy");
        if daemon_src.exists() && !same_file(&daemon_src.to_string_lossy(), &daemon_dest) {
            install_binary(&daemon_src.to_string_lossy(), &daemon_dest)?;
        }
        let resources = contents.join("Resources");
        if let Ok(entries) = std::fs::read_dir(src_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // firmware.bin / firmware.version / firmware-<board>.{bin,version}
                if name.starts_with("firmware") && (name.ends_with(".bin") || name.ends_with(".version")) {
                    // Plain copy (not install_binary — it's data, not an executable).
                    let _ = std::fs::copy(entry.path(), resources.join(&*name));
                }
            }
        }
    }

    let plist = contents.join("Info.plist");
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Claude Buddy</string>
  <key>CFBundleDisplayName</key><string>Claude Buddy</string>
  <key>CFBundleIdentifier</key><string>com.anthropic.claude-buddy-app</string>
  <key>CFBundleExecutable</key><string>claude-buddy-app</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>{ver}</string>
  <key>CFBundleShortVersionString</key><string>{ver}</string>
  <key>LSMinimumSystemVersion</key><string>10.13</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>Claude Buddy flashes firmware updates to your buddy over your local Wi-Fi network.</string>
</dict></plist>"#,
        ver = env!("CARGO_PKG_VERSION")
    );
    std::fs::write(&plist, body)?;

    // Ad-hoc codesign the bundle with its stable identifier so the macOS Local
    // Network (and any future) TCC grant attaches to a durable code identity
    // rather than churning every rebuild — same rationale as the daemon helper.
    // The over-the-air firmware flasher does LAN I/O (espota UDP/TCP), which
    // macOS gates behind Local Network permission keyed to this identity.
    // --deep so the daemon binary nested in Contents/MacOS is signed too.
    // Prefers the stable dev cert (Local-Network grant persists), else ad-hoc.
    mac_codesign(
        &bundle.to_string_lossy(),
        "com.anthropic.claude-buddy-app",
        true,
    );

    // Nudge Launch Services so Spotlight/Finder see it immediately.
    let _ = run_cmd(
        "/System/Library/Frameworks/CoreServices.framework/Frameworks/\
         LaunchServices.framework/Support/lsregister",
        &["-f", &bundle.to_string_lossy()],
    );

    Ok((
        dest_exe.to_string_lossy().into_owned(),
        format!("installed app bundle {}", bundle.display()),
    ))
}

#[cfg(target_os = "windows")]
pub fn install_desktop_launcher(app_exe: &str) -> Result<(String, String)> {
    // A Start Menu shortcut is the Windows equivalent of an /Applications entry.
    // No native .lnk writer in std, so drive the WScript.Shell COM object via
    // PowerShell — present on every supported Windows.
    let base = directories::BaseDirs::new().context("home dir")?;
    let programs = base
        .data_dir()
        .join(r"Microsoft\Windows\Start Menu\Programs");
    std::fs::create_dir_all(&programs)?;
    let lnk = programs.join("Claude Buddy.lnk");
    let script = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{lnk}');\
         $s.TargetPath='{exe}';$s.Description='Claude Buddy';$s.Save()",
        lnk = lnk.display(),
        exe = app_exe,
    );
    run_cmd(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
    )?;
    Ok((
        app_exe.to_string(),
        format!("added Start Menu shortcut {}", lnk.display()),
    ))
}

#[cfg(target_os = "linux")]
pub fn install_desktop_launcher(app_exe: &str) -> Result<(String, String)> {
    // An XDG .desktop entry in the per-user applications dir makes the GUI show
    // up (and be searchable/pinnable) in the desktop environment's app menu.
    let base = directories::BaseDirs::new().context("home dir")?;
    let dir = base.home_dir().join(".local/share/applications");
    std::fs::create_dir_all(&dir)?;
    let desktop = dir.join("claude-buddy.desktop");
    let body = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Claude Buddy\n\
         Comment=Control panel for your Claude hardware buddy\n\
         Exec={app_exe}\n\
         Terminal=false\n\
         Categories=Utility;\n\
         StartupNotify=true\n"
    );
    std::fs::write(&desktop, body)?;
    // Refresh the menu cache so it appears without a re-login (best-effort).
    let _ = run_cmd("update-desktop-database", &[&dir.to_string_lossy()]);
    Ok((
        app_exe.to_string(),
        format!("added application menu entry {}", desktop.display()),
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn install_desktop_launcher(_app_exe: &str) -> Result<(String, String)> {
    Err(anyhow!(
        "desktop launcher registration is unsupported on this platform"
    ))
}
