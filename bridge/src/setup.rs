//! `agent-buddy setup`: wire our hooks into Claude Code's settings and
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
    let exe = std::env::current_exe().context("locating the agent-buddy binary")?;
    let exe_str = exe.to_string_lossy().to_string();

    // When installing the service, copy the daemon to its stable location up
    // front and point BOTH the Claude Code hooks and the service at that copy.
    // Otherwise the hooks reference wherever setup happened to run from — e.g.
    // `target/release/agent-buddy`, which the next `cargo build` overwrites in
    // place (breaking its code identity), or a path that later disappears.
    let hook_target = if install_service {
        install_daemon_binary(&exe_str).unwrap_or_else(|_| exe_str.clone())
    } else {
        exe_str.clone()
    };

    // Persist the chosen matcher so later reconciliation (daemon startup, app
    // update) keeps it instead of reverting to the default. Only the explicit
    // CLI path passes a custom `--tools`; everything else uses the default.
    if let Ok(mut cfg) = crate::state::Config::load() {
        if cfg.hook_matcher.as_deref() != Some(matcher) {
            cfg.hook_matcher = Some(matcher.to_string());
            let _ = cfg.save();
        }
    }

    // Wire the *active* agent's hooks — not always Claude's. A CLI user who
    // switched to a non-Claude harness (so its hooks live in e.g.
    // ~/.codex/hooks.json and active_agent != claude-code) and re-runs the
    // installer to update must keep that harness as the only wired one, instead
    // of getting stale Claude hooks re-added alongside it. Mirrors
    // `ensure_active_agent_hooks` and `switch_agent`.
    let cfg = crate::state::Config::load().unwrap_or_default();
    if cfg.active_agent == crate::agent::DEFAULT_AGENT {
        let settings_path = claude_settings_path()?;
        wire_hooks(&settings_path, &hook_target, matcher)?;
        println!("✓ wired hooks into {}", settings_path.display());
        println!("  gate + telemetry matcher: {matcher}");
    } else if let Some(profile) = crate::agent::load_profiles().get(&cfg.active_agent) {
        install_profile(profile, &hook_target)?;
        let target = resolve_target(&profile.install.target_path)?;
        println!("✓ wired {} hooks into {}", profile.id, target.display());
    } else {
        // Active agent has no known profile; fall back to wiring Claude so the
        // install still produces a working default.
        let settings_path = claude_settings_path()?;
        wire_hooks(&settings_path, &hook_target, matcher)?;
        println!("✓ wired hooks into {}", settings_path.display());
        println!("  gate + telemetry matcher: {matcher}");
    }

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

    println!("\nNext: power on your buddy, then run `agent-buddy pair` to confirm the link.");
    Ok(())
}

/// The canonical hook set we own, as `(event, optional-matcher)` pairs. Single
/// source of truth for both wiring and reconciliation, so the two can't drift.
/// `PermissionRequest` + `PreToolUse` are tool-gated (carry the matcher); the
/// rest are bare state events.
///
/// - PermissionRequest: the matcher-scoped approve/deny gate. Claude raises it
///   ONLY when it would actually prompt the user, so the device mirrors the
///   real session's prompts — never auto-approved, allow-listed, bypass-mode,
///   or autonomous subagent tool calls (all of which still fire PreToolUse).
/// - PreToolUse: matcher-scoped telemetry heartbeat. Fires for every matched
///   tool call so the device's token / context readout tracks the turn
///   mid-flight instead of only jumping at Stop.
fn hook_spec(matcher: &str) -> Vec<(&'static str, Option<String>)> {
    let mut spec = vec![
        ("PermissionRequest", Some(matcher.to_string())),
        ("PreToolUse", Some(matcher.to_string())),
    ];
    spec.extend(STATE_EVENTS.iter().map(|ev| (*ev, None)));
    spec
}

/// Reconcile our hook entries in `path` to *exactly* the canonical set for
/// `exe` + `matcher`: strip every prior agent-buddy entry from every event
/// (so events we no longer use, a renamed event, or a stale binary path are
/// cleaned up — not just overwritten), then re-add the current set. The user's
/// own hooks are always left untouched. Idempotent; returns whether the file
/// actually changed (so callers can stay quiet on a no-op).
fn wire_hooks(path: &PathBuf, exe: &str, matcher: &str) -> Result<bool> {
    let mut root: Value = match std::fs::read(path) {
        Ok(b) if !b.is_empty() => serde_json::from_slice(&b)
            .with_context(|| format!("parsing existing {}", path.display()))?,
        _ => json!({}),
    };
    if !root.is_object() {
        return Err(anyhow!("{} is not a JSON object", path.display()));
    }
    let before = root.clone();

    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("`hooks` in settings.json is not an object"))?;

    // 1) Strip ALL our prior entries from every event. This is what makes the
    //    function a reconciler rather than an overwriter: an event we dropped in
    //    a new version, or one whose command points at an old binary path, is
    //    removed here instead of lingering.
    for arr in hooks.values_mut() {
        if let Some(a) = arr.as_array_mut() {
            a.retain(|e| !is_ours(e, exe));
        }
    }

    // 2) Add the canonical set. Quote the path: hook commands run through a
    //    shell, and our install locations contain spaces (e.g. `…/Agent
    //    Buddy.app/…`, `…/Application Support/…`) — without quotes the shell
    //    would split the path mid-word.
    let q = shell_quote(exe);
    for (event, m) in hook_spec(matcher) {
        let cmd = format!("{q} hook {event}");
        let entry = match m {
            Some(m) => json!({ "matcher": m, "hooks": [ { "type": "command", "command": cmd } ] }),
            None => json!({ "hooks": [ { "type": "command", "command": cmd } ] }),
        };
        // Don't `.expect()` an array here: a user's pre-existing settings.json
        // may have one of our event keys set to a non-array value (an older
        // schema, a hand-edit, another tool's format). Step 1 leaves such a
        // value untouched, so overwrite it with a fresh array rather than
        // panicking — this runs on every daemon startup, so a panic here would
        // be a launchd crash-loop. Mirrors `set_event`'s fallback.
        let slot = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        match slot.as_array_mut() {
            Some(a) => a.push(entry),
            None => *slot = json!([entry]),
        }
    }

    // 3) Drop any event array we emptied out in step 1 (keeps the file tidy and
    //    means a removed event leaves no orphan key behind).
    hooks.retain(|_, v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true));

    // Nothing to write if our reconciliation was a no-op — avoids churning the
    // file (and its mtime) on every daemon restart.
    if root == before {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    write_atomic(path, &serde_json::to_vec_pretty(&root)?)?;
    Ok(true)
}

/// True if a hook entry's command belongs to us (so we can replace it). Matches
/// any invocation of the `agent-buddy` binary with a `hook` subcommand —
/// quoted or unquoted, at any install path — so re-running setup cleans up
/// entries written by an earlier version (e.g. the old unquoted form) instead
/// of leaving duplicates. A user hook that doesn't run `agent-buddy hook` is
/// left untouched.
fn is_ours(entry: &Value, _exe: &str) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|cmds| {
            cmds.iter().any(|c| {
                c.get("command")
                    .and_then(|v| v.as_str())
                    .map(|s| s.contains("agent-buddy") && s.contains(" hook "))
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

/// Replace any existing agent-buddy entry for `event` with `entry`, leaving the
/// user's other hooks for that event intact. Used by the per-profile writers
/// (the Claude path uses `wire_hooks`' bulk reconcile instead).
fn set_event(hooks: &mut serde_json::Map<String, Value>, event: &str, entry: Value, exe: &str) {
    let arr = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
    let Some(arr) = arr.as_array_mut() else {
        *arr = json!([entry]);
        return;
    };
    arr.retain(|e| !is_ours(e, exe));
    arr.push(entry);
}

/// Write `bytes` to `path` atomically: stage a sibling temp file then rename it
/// over the target. A crash mid-write can't truncate the user's config, a
/// concurrent reader never sees a half-written file, and renaming onto the path
/// replaces a symlink rather than writing *through* it — important because these
/// are the very configs (`settings.json`, `config.toml`, …) the tool promises to
/// edit safely. Callers create the parent dir; the rename is atomic on the same
/// filesystem (the temp file is a sibling, so it always is).
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("cannot write to {} (no file name)", path.display()))?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(".agent-buddy.tmp");
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-harness hook install/uninstall (multi-agent).
//
// Each agent profile carries an `InstallRecipe` (config format + target path +
// the events to register). We install ONLY the active agent's hooks; switching
// agents uninstalls the old harness's and installs the new one's. Every writer
// is idempotent and reversible via the same `is_ours` filter used for Claude.
// ---------------------------------------------------------------------------

use crate::agent::{AgentProfile, ConfigFormat};

/// Resolve a profile's hook-config target to an absolute path. Relative to the
/// user's home, or to our own config dir for plugin manifests we own.
fn resolve_target(t: &crate::agent::TargetPath) -> Result<PathBuf> {
    if t.from_config_dir {
        return Ok(crate::state::config_dir()?.join(&t.home_rel));
    }
    // An opt-in env override (e.g. COPILOT_HOME) replaces $HOME + the leading
    // config segment of home_rel: `.copilot/hooks/hooks.json` under
    // `<COPILOT_HOME>` becomes `<COPILOT_HOME>/hooks/hooks.json`.
    if let Some(var) = t.home_env.as_deref() {
        if let Ok(val) = std::env::var(var) {
            let val = val.trim();
            if !val.is_empty() {
                let tail = t
                    .home_rel
                    .split_once('/')
                    .map(|(_, rest)| rest)
                    .unwrap_or(&t.home_rel);
                return Ok(PathBuf::from(val).join(tail));
            }
        }
    }
    let base = directories::BaseDirs::new().context("could not find home directory")?;
    Ok(base.home_dir().join(&t.home_rel))
}

/// The shell command a hook entry runs: `"<exe>" hook <EVENT> --agent <id>`.
/// The `--agent` flag tells the daemon which profile maps this harness's event.
fn hook_command(exe: &str, event: &str, agent_id: &str) -> String {
    format!("{} hook {event} --agent {agent_id}", shell_quote(exe))
}

/// True if a harness event should carry the per-tool matcher (tool-gated events
/// and the explicit permission gate). Others register matcher-less.
fn event_is_gated(event: &str) -> bool {
    let e = event.to_ascii_lowercase();
    e.contains("tooluse") || e.contains("tool") && (e.contains("pre") || e.contains("post"))
        || e == "permissionrequest"
}

/// Install the hooks for `profile` so its harness talks to our daemon. Dispatch
/// on the config format; the JSON-settings family share one writer.
pub fn install_profile(profile: &AgentProfile, exe: &str) -> Result<()> {
    use ConfigFormat::*;
    match profile.install.config_format {
        ClaudeCodeCompatible | GeminiSettingsJson
        | QwenSettingsJson | UserGlobalHooksJson => {
            write_hooks_json(profile, exe)
        }
        // Antigravity reads a nested `clawd` group, not a top-level `hooks` map.
        AntigravityHooksJson => write_antigravity_hooks_json(profile, exe),
        // Kiro agent files need a top-level `name` and flat `{command}` entries.
        KiroAgentJson => write_kiro_agent_json(profile, exe),
        // Codex ignores ~/.codex/hooks.json entirely unless `[features].hooks =
        // true` is set in ~/.codex/config.toml. Write the hooks the usual way,
        // then enable the flag (best-effort: Codex still animates via the JSONL
        // log-poll fallback if this can't be written).
        CodexHooksJson => {
            write_hooks_json(profile, exe)?;
            if let Err(e) = enable_codex_hooks_feature() {
                tracing::warn!("could not enable Codex [features].hooks: {e}");
            }
            Ok(())
        }
        // Cursor uses a different on-disk schema (flat `{ "command": … }` entries
        // + a numeric top-level `version`) that it reads directly; the generic
        // nested Claude shape is silently ignored, so it needs its own writer.
        CursorHooksJson => write_cursor_hooks_json(profile, exe),
        KimiToml => write_kimi_toml(profile, exe),
        // Plugin/extension harnesses (opencode, pi, openclaw, hermes) have no
        // "run a command on each event" hook; they load a plugin INTO their own
        // process that POSTs events to our loopback HTTP listener. We bundle a
        // minimal plugin per harness (embedded below), drop it where the harness
        // loads plugins from, and register/enable it. The plugin discovers the
        // daemon's live { http_port, token } from a `bridge.json` we write beside
        // it that names our endpoint.json.
        OpencodePlugin => install_opencode_plugin(),
        OpenclawPlugin => install_openclaw_plugin(),
        PiExtension => install_pi_plugin(),
        HermesPlugin => install_hermes_plugin(),
        Unknown => Err(anyhow!(
            "agent {} uses an unrecognized hook-config format",
            profile.id
        )),
    }
}

/// Remove our hooks for `profile` (best-effort, idempotent). Leaves the user's
/// own hooks intact.
pub fn uninstall_profile(profile: &AgentProfile, exe: &str) -> Result<()> {
    use ConfigFormat::*;
    let path = resolve_target(&profile.install.target_path)?;
    match profile.install.config_format {
        ClaudeCodeCompatible | CodexHooksJson | GeminiSettingsJson
        | QwenSettingsJson | UserGlobalHooksJson => {
            strip_our_hooks_json(&path, exe)
        }
        AntigravityHooksJson => strip_antigravity_hooks_json(&path, exe),
        KiroAgentJson => strip_kiro_agent_json(&path),
        // Cursor's flat schema keys our entries off the element's own `command`
        // field, not the nested `hooks[].command` shape `is_ours` looks for.
        CursorHooksJson => strip_cursor_hooks_json(&path),
        KimiToml => strip_kimi_toml(&path),
        OpencodePlugin => uninstall_opencode_plugin(),
        OpenclawPlugin => uninstall_openclaw_plugin(),
        PiExtension => uninstall_pi_plugin(),
        HermesPlugin => uninstall_hermes_plugin(),
        Unknown => Ok(()),
    }
}

/// Write a `{ "hooks": { "<Event>": [ { type:command, command } ] } }` config
/// for the JSON-settings family. Reuses the Claude `set_event`/`is_ours`
/// idempotency, so re-running replaces our entries and leaves the user's alone.
fn write_hooks_json(profile: &AgentProfile, exe: &str) -> Result<()> {
    let path = resolve_target(&profile.install.target_path)?;
    write_hooks_json_at(&path, profile, exe)
}

fn write_hooks_json_at(path: &PathBuf, profile: &AgentProfile, exe: &str) -> Result<()> {
    let mut root: Value = match std::fs::read(path) {
        Ok(b) if !b.is_empty() => serde_json::from_slice(&b)
            .with_context(|| format!("parsing existing {}", path.display()))?,
        _ => json!({}),
    };
    if !root.is_object() {
        return Err(anyhow!("{} is not a JSON object", path.display()));
    }
    let is_qwen = matches!(profile.install.config_format, ConfigFormat::QwenSettingsJson);
    let is_copilot = matches!(profile.install.config_format, ConfigFormat::UserGlobalHooksJson);
    {
        let obj = root.as_object_mut().unwrap();
        // Copilot CLI requires a numeric top-level `version`.
        if is_copilot && !obj.get("version").map(|v| v.is_number()).unwrap_or(false) {
            obj.insert("version".to_string(), json!(1));
        }
    }
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("`hooks` in {} is not an object", path.display()))?;

    let matcher = profile.install.matcher.as_deref();
    let mut events: Vec<String> = profile.install.events.clone();
    // Claude-compatible harnesses also get the explicit permission gate.
    if matches!(profile.install.config_format, ConfigFormat::ClaudeCodeCompatible)
        && profile.capabilities.permission_approval
        && !events.iter().any(|e| e == "PermissionRequest")
    {
        events.push("PermissionRequest".to_string());
    }

    for ev in &events {
        let cmd = hook_command(exe, ev, &profile.id);
        // Qwen registers matcher:"*" on every event except UserPromptSubmit/Stop
        // (its MATCHERLESS set) — not just tool/permission events.
        let want_matcher = if is_qwen {
            !matches!(ev.as_str(), "UserPromptSubmit" | "Stop")
        } else {
            event_is_gated(ev)
        };
        let eff_matcher = if is_qwen {
            Some("*")
        } else {
            matcher
        };
        // Qwen needs a per-event timeout so its blocking permission hook isn't
        // killed before the user decides (600s) and other events get a sane bound.
        let mut cmd_obj = json!({ "type": "command", "command": cmd });
        if is_qwen {
            let timeout = if ev == "PermissionRequest" { 600000 } else { 30000 };
            cmd_obj
                .as_object_mut()
                .unwrap()
                .insert("timeout".to_string(), json!(timeout));
        }
        let entry = if let (true, Some(m)) = (want_matcher, eff_matcher) {
            json!({ "matcher": m, "hooks": [ cmd_obj ] })
        } else {
            json!({ "hooks": [ cmd_obj ] })
        };
        set_event(hooks, ev, entry, exe);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    write_atomic(path, &serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

/// Strip our hook entries from a JSON-settings file, leaving the user's. A
/// missing/empty/garbage file is a no-op (nothing of ours to remove).
fn strip_our_hooks_json(path: &PathBuf, exe: &str) -> Result<()> {
    let bytes = match std::fs::read(path) {
        Ok(b) if !b.is_empty() => b,
        _ => return Ok(()),
    };
    let mut root: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let Some(hooks) = root
        .as_object_mut()
        .and_then(|o| o.get_mut("hooks"))
        .and_then(|h| h.as_object_mut())
    else {
        return Ok(());
    };
    let before = serde_json::to_string(&hooks).unwrap_or_default();
    for (_event, arr) in hooks.iter_mut() {
        if let Some(arr) = arr.as_array_mut() {
            arr.retain(|e| !is_ours(e, exe));
        }
    }
    // Drop now-empty event arrays so a removed event leaves no orphan key.
    hooks.retain(|_, v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true));
    // No-op when nothing of ours was present, so a blanket uninstall doesn't
    // reformat / churn the mtime of an unrelated harness's settings file.
    if serde_json::to_string(&hooks).unwrap_or_default() == before {
        return Ok(());
    }
    write_atomic(path, &serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

/// Write Cursor's hooks.json. Cursor's schema differs from Claude's: it requires
/// a numeric top-level `version` and reads each event entry's `command` field
/// DIRECTLY off the array element — `{ "version": 1, "hooks": { "<event>":
/// [ { "command": "<cmd>" } ] } }`. The generic nested `{ hooks: [ { type,
/// command } ] }` shape is silently ignored by Cursor, so route Cursor here.
/// Idempotent: strips any prior entry whose flat `command` is ours before
/// re-adding, leaving the user's own hooks intact.
fn write_cursor_hooks_json(profile: &AgentProfile, exe: &str) -> Result<()> {
    let path = resolve_target(&profile.install.target_path)?;
    let mut root: Value = match std::fs::read(&path) {
        Ok(b) if !b.is_empty() => serde_json::from_slice(&b)
            .with_context(|| format!("parsing existing {}", path.display()))?,
        _ => json!({}),
    };
    if !root.is_object() {
        return Err(anyhow!("{} is not a JSON object", path.display()));
    }
    {
        let obj = root.as_object_mut().unwrap();
        // Cursor requires a numeric version; set it if missing/non-numeric.
        if !obj.get("version").map(|v| v.is_number()).unwrap_or(false) {
            obj.insert("version".to_string(), json!(1));
        }
        let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
        let hooks = hooks
            .as_object_mut()
            .ok_or_else(|| anyhow!("`hooks` in {} is not an object", path.display()))?;
        for ev in &profile.install.events {
            let cmd = hook_command(exe, ev, &profile.id);
            let arr = hooks.entry(ev.to_string()).or_insert_with(|| json!([]));
            let Some(arr) = arr.as_array_mut() else {
                *arr = json!([{ "command": cmd }]);
                continue;
            };
            arr.retain(|e| !is_ours_cursor(e));
            arr.push(json!({ "command": cmd }));
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    write_atomic(&path, &serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

/// True if a Cursor hooks.json array element is one of ours: a flat
/// `{ "command": "…agent-buddy … hook …" }` entry.
fn is_ours_cursor(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(|v| v.as_str())
        .map(|s| s.contains("agent-buddy") && s.contains(" hook "))
        .unwrap_or(false)
}

/// Strip our flat Cursor entries, leaving the user's. Missing/empty/garbage is a
/// no-op.
fn strip_cursor_hooks_json(path: &PathBuf) -> Result<()> {
    let bytes = match std::fs::read(path) {
        Ok(b) if !b.is_empty() => b,
        _ => return Ok(()),
    };
    let mut root: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let Some(hooks) = root
        .as_object_mut()
        .and_then(|o| o.get_mut("hooks"))
        .and_then(|h| h.as_object_mut())
    else {
        return Ok(());
    };
    let before = serde_json::to_string(&hooks).unwrap_or_default();
    for (_event, arr) in hooks.iter_mut() {
        if let Some(arr) = arr.as_array_mut() {
            arr.retain(|e| !is_ours_cursor(e));
        }
    }
    if serde_json::to_string(&hooks).unwrap_or_default() == before {
        return Ok(());
    }
    write_atomic(path, &serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Antigravity — a nested `clawd` group, NOT a top-level `hooks` map. Only
// PostToolUse uses the `{matcher,hooks}` wrapper; the rest are flat handler
// arrays. `timeout` is in seconds.
// ---------------------------------------------------------------------------

/// The Antigravity hook group key. Antigravity reads handlers from this group,
/// not from a top-level `hooks` object.
const ANTIGRAVITY_GROUP: &str = "clawd";

fn write_antigravity_hooks_json(profile: &AgentProfile, exe: &str) -> Result<()> {
    let path = resolve_target(&profile.install.target_path)?;
    write_antigravity_hooks_at(&path, profile, exe)
}

fn write_antigravity_hooks_at(path: &PathBuf, profile: &AgentProfile, exe: &str) -> Result<()> {
    let mut root: Value = match std::fs::read(path) {
        Ok(b) if !b.is_empty() => serde_json::from_slice(&b)
            .with_context(|| format!("parsing existing {}", path.display()))?,
        _ => json!({}),
    };
    if !root.is_object() {
        return Err(anyhow!("{} is not a JSON object", path.display()));
    }
    {
        let obj = root.as_object_mut().unwrap();
        let group = obj
            .entry(ANTIGRAVITY_GROUP.to_string())
            .or_insert_with(|| json!({}));
        let group = group
            .as_object_mut()
            .ok_or_else(|| anyhow!("`{ANTIGRAVITY_GROUP}` in {} is not an object", path.display()))?;
        for ev in &profile.install.events {
            let cmd = hook_command(exe, ev, &profile.id);
            // PostToolUse wraps in {matcher,hooks}; the rest are flat handlers.
            let entry = if ev == "PostToolUse" {
                json!({ "matcher": "*", "hooks": [ { "type": "command", "command": cmd, "timeout": 10 } ] })
            } else {
                json!({ "type": "command", "command": cmd, "timeout": 10 })
            };
            let arr = group.entry(ev.to_string()).or_insert_with(|| json!([]));
            let Some(arr) = arr.as_array_mut() else {
                *arr = json!([entry]);
                continue;
            };
            arr.retain(|e| !is_ours_antigravity(e));
            arr.push(entry);
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    write_atomic(path, &serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

/// True if an Antigravity group entry is one of ours — either a flat
/// `{type:command,command}` handler or a `{matcher,hooks:[…]}` wrapper whose
/// inner command runs `agent-buddy … hook …`.
fn is_ours_antigravity(entry: &Value) -> bool {
    let cmd_is_ours = |c: &Value| {
        c.get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("agent-buddy") && s.contains(" hook "))
            .unwrap_or(false)
    };
    if cmd_is_ours(entry) {
        return true;
    }
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|cmds| cmds.iter().any(cmd_is_ours))
        .unwrap_or(false)
}

/// Strip only buddy-owned entries from the `clawd` group, deleting the group if
/// it becomes empty. Leaves a coexisting clawd-on-desk install intact.
fn strip_antigravity_hooks_json(path: &PathBuf, _exe: &str) -> Result<()> {
    let bytes = match std::fs::read(path) {
        Ok(b) if !b.is_empty() => b,
        _ => return Ok(()),
    };
    let mut root: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let Some(group) = root
        .as_object_mut()
        .and_then(|o| o.get_mut(ANTIGRAVITY_GROUP))
        .and_then(|g| g.as_object_mut())
    else {
        return Ok(());
    };
    let before = serde_json::to_string(&group).unwrap_or_default();
    for (_event, arr) in group.iter_mut() {
        if let Some(arr) = arr.as_array_mut() {
            arr.retain(|e| !is_ours_antigravity(e));
        }
    }
    group.retain(|_, v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true));
    let after = serde_json::to_string(&group).unwrap_or_default();
    if after == before {
        return Ok(());
    }
    // Drop the whole group if it's now empty.
    if group.is_empty() {
        root.as_object_mut().unwrap().remove(ANTIGRAVITY_GROUP);
    }
    write_atomic(path, &serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Kiro — an agent-descriptor file requiring a top-level `name` and flat
// `{ "command": … }` entries under `hooks[<event>]` (not the nested Claude shape).
// ---------------------------------------------------------------------------

fn write_kiro_agent_json(profile: &AgentProfile, exe: &str) -> Result<()> {
    let path = resolve_target(&profile.install.target_path)?;
    write_kiro_agent_at(&path, profile, exe)
}

fn write_kiro_agent_at(path: &PathBuf, profile: &AgentProfile, exe: &str) -> Result<()> {
    let mut root: Value = match std::fs::read(path) {
        Ok(b) if !b.is_empty() => serde_json::from_slice(&b)
            .with_context(|| format!("parsing existing {}", path.display()))?,
        _ => json!({}),
    };
    if !root.is_object() {
        return Err(anyhow!("{} is not a JSON object", path.display()));
    }
    {
        let obj = root.as_object_mut().unwrap();
        // Kiro requires a top-level `name`; default to the file stem ("buddy"),
        // preserving any existing name. Seed a description only on creation.
        let created = !obj.contains_key("name");
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&profile.id)
            .to_string();
        obj.entry("name".to_string()).or_insert_with(|| json!(stem));
        if created {
            obj.entry("description".to_string())
                .or_insert_with(|| json!("buddy agent with desktop pet hooks"));
        }
        let hooks = obj.entry("hooks".to_string()).or_insert_with(|| json!({}));
        let hooks = hooks
            .as_object_mut()
            .ok_or_else(|| anyhow!("`hooks` in {} is not an object", path.display()))?;
        for ev in &profile.install.events {
            let cmd = hook_command(exe, ev, &profile.id);
            let arr = hooks.entry(ev.to_string()).or_insert_with(|| json!([]));
            let Some(arr) = arr.as_array_mut() else {
                *arr = json!([{ "command": cmd }]);
                continue;
            };
            arr.retain(|e| !is_ours_cursor(e)); // flat `command` dedupe
            arr.push(json!({ "command": cmd }));
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    write_atomic(path, &serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

/// Strip our flat Kiro hook entries, preserving `name`/`description` and the
/// user's own hooks. A missing/empty/garbage file is a no-op.
fn strip_kiro_agent_json(path: &PathBuf) -> Result<()> {
    let bytes = match std::fs::read(path) {
        Ok(b) if !b.is_empty() => b,
        _ => return Ok(()),
    };
    let mut root: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let Some(hooks) = root
        .as_object_mut()
        .and_then(|o| o.get_mut("hooks"))
        .and_then(|h| h.as_object_mut())
    else {
        return Ok(());
    };
    let before = serde_json::to_string(&hooks).unwrap_or_default();
    for (_event, arr) in hooks.iter_mut() {
        if let Some(arr) = arr.as_array_mut() {
            arr.retain(|e| !is_ours_cursor(e));
        }
    }
    hooks.retain(|_, v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true));
    if serde_json::to_string(&hooks).unwrap_or_default() == before {
        return Ok(());
    }
    write_atomic(path, &serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

/// Codex only honors its `hooks.json` when `[features].hooks = true` is present
/// in `~/.codex/config.toml`. Set that flag without disturbing the rest of the
/// user's Codex config. Idempotent: a no-op when already enabled. Best-effort —
/// the caller logs (doesn't fail) on error, since Codex still animates via the
/// JSONL log-poll fallback.
fn enable_codex_hooks_feature() -> Result<()> {
    let base = directories::BaseDirs::new().context("could not find home directory")?;
    let path = base.home_dir().join(".codex").join("config.toml");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = set_features_hooks_true(&existing);
    if updated == existing {
        return Ok(()); // already enabled — don't churn the file
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    write_atomic(&path, updated.as_bytes())?;
    Ok(())
}

/// Pure transform: return `toml` with `[features].hooks = true` ensured. Edits an
/// existing `[features]` table in place (replacing its `hooks = …` line, or
/// inserting one right after the header); otherwise appends a fresh `[features]`
/// table. Conservative — only ever touches the `[features]` section, so the rest
/// of the user's Codex config is preserved byte-for-byte.
fn set_features_hooks_true(toml: &str) -> String {
    let lines: Vec<&str> = toml.lines().collect();
    let is_features_header = |l: &str| l.trim() == "[features]";
    if let Some(hdr) = lines.iter().position(|l| is_features_header(l)) {
        // The section runs until the next table header (a line starting with `[`).
        let end = (hdr + 1..lines.len())
            .find(|&i| lines[i].trim_start().starts_with('['))
            .unwrap_or(lines.len());
        // An existing `hooks` key in this section (exact key before `=`).
        let hooks_idx = (hdr + 1..end)
            .find(|&i| lines[i].split('=').next().map(str::trim) == Some("hooks"));
        let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        match hooks_idx {
            Some(i) => out[i] = "hooks = true".to_string(),
            None => out.insert(hdr + 1, "hooks = true".to_string()),
        }
        let mut joined = out.join("\n");
        if toml.ends_with('\n') && !joined.ends_with('\n') {
            joined.push('\n');
        }
        joined
    } else {
        let mut out = toml.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n'); // blank line before the appended table
        }
        out.push_str("[features]\nhooks = true\n");
        out
    }
}

/// Marker our hook lines carry in the Kimi TOML so we can find + strip them.
const TOML_MARK: &str = "# agent-buddy hook (managed — do not edit)";

/// Write Kimi's TOML hook block. We append a clearly-marked managed section and
/// strip any prior one first so re-running is idempotent.
fn write_kimi_toml(profile: &AgentProfile, exe: &str) -> Result<()> {
    let path = resolve_target(&profile.install.target_path)?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out = strip_marked_block(&existing);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(TOML_MARK);
    out.push('\n');
    for ev in &profile.install.events {
        // One [[hooks]] entry per event invoking our relay.
        out.push_str(&format!(
            "[[hooks]]\nevent = \"{ev}\"\ncommand = {}\n",
            toml_quote(&hook_command(exe, ev, &profile.id))
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    write_atomic(&path, out.as_bytes())?;
    Ok(())
}

fn strip_kimi_toml(path: &PathBuf) -> Result<()> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let stripped = strip_marked_block(&existing);
    write_atomic(path, stripped.as_bytes())?;
    Ok(())
}

/// Remove everything from our `TOML_MARK` line to the end of the file (our block
/// is always appended last), leaving the user's content untouched.
fn strip_marked_block(s: &str) -> String {
    match s.find(TOML_MARK) {
        Some(i) => s[..i].trim_end().to_string(),
        None => s.to_string(),
    }
}

/// TOML basic-string quoting (escape backslash + quote). Our exe paths contain
/// neither control chars nor backslashes on Unix, but Windows paths do.
fn toml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

// ---------------------------------------------------------------------------
// Plugin/extension harnesses (opencode, pi, openclaw, hermes).
//
// These don't spawn a hook command; they load a plugin INTO their own process
// that POSTs lifecycle events to our daemon's loopback HTTP listener. We bundle
// a minimal plugin per harness (embedded via include_str! so a CLI install
// needs no external assets), write it where the harness loads plugins from, and
// register/enable it. Beside each plugin we write a `bridge.json` naming our
// `endpoint.json`; the plugin reads it per-event to find the daemon's live
// { http_port, token }, so a daemon restart (new port/token) needs no re-install.
// ---------------------------------------------------------------------------

const OPENCODE_INDEX_MJS: &str = include_str!("../assets/plugins/opencode/index.mjs");
const OPENCODE_PACKAGE_JSON: &str = include_str!("../assets/plugins/opencode/package.json");
const OPENCLAW_INDEX_JS: &str = include_str!("../assets/plugins/openclaw/index.js");
const OPENCLAW_MANIFEST_JSON: &str = include_str!("../assets/plugins/openclaw/openclaw.plugin.json");
const OPENCLAW_PACKAGE_JSON: &str = include_str!("../assets/plugins/openclaw/package.json");
const PI_INDEX_TS: &str = include_str!("../assets/plugins/pi/index.ts");
const HERMES_INIT_PY: &str = include_str!("../assets/plugins/hermes/__init__.py");
const HERMES_PLUGIN_YAML: &str = include_str!("../assets/plugins/hermes/plugin.yaml");

/// The plugin id we register under in each harness's config (and the dir name).
const PLUGIN_ID: &str = "agent-buddy";

/// Write a `bridge.json` beside an installed plugin so it can find the daemon.
/// It names the absolute path of our `endpoint.json` (stable across daemon
/// restarts — only the port/token *inside* change) plus the agent id to report.
fn write_bridge_json(dir: &std::path::Path, agent_id: &str) -> Result<()> {
    let endpoint = crate::ipc::endpoint_path()?;
    let manifest = json!({
        "endpoint": endpoint.to_string_lossy(),
        "agent": agent_id,
    });
    std::fs::write(dir.join("bridge.json"), serde_json::to_vec_pretty(&manifest)?)
        .with_context(|| format!("writing {}", dir.join("bridge.json").display()))?;
    Ok(())
}

/// Write a set of (filename, contents) into `dir`, creating it.
fn write_plugin_files(dir: &std::path::Path, files: &[(&str, &str)]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, contents) in files {
        std::fs::write(dir.join(name), contents)
            .with_context(|| format!("writing {}", dir.join(name).display()))?;
    }
    Ok(())
}

fn home_dir() -> Result<PathBuf> {
    Ok(directories::BaseDirs::new()
        .context("could not find home directory")?
        .home_dir()
        .to_path_buf())
}

// --- opencode: plugin dir registered by absolute path in opencode.json --------

/// Where we keep the opencode/openclaw plugin payloads (our own config dir, a
/// stable absolute path we then register inside the harness's config).
fn our_plugin_dir(name: &str) -> Result<PathBuf> {
    Ok(crate::state::config_dir()?.join("plugins").join(name))
}

fn install_opencode_plugin() -> Result<()> {
    let dir = our_plugin_dir("opencode")?;
    write_plugin_files(
        &dir,
        &[
            ("index.mjs", OPENCODE_INDEX_MJS),
            ("package.json", OPENCODE_PACKAGE_JSON),
        ],
    )?;
    write_bridge_json(&dir, "opencode")?;
    // Register the dir's absolute path in ~/.config/opencode/opencode.json's
    // `plugin` array. opencode reads the dir's package.json `main`.
    let cfg = home_dir()?.join(".config/opencode/opencode.json");
    let dir_str = dir.to_string_lossy().replace('\\', "/");
    edit_json_string_array(&cfg, &["plugin"], &dir_str, true, |o| {
        o.entry("$schema".to_string())
            .or_insert_with(|| json!("https://opencode.ai/config.json"));
    })
}

fn uninstall_opencode_plugin() -> Result<()> {
    let dir = our_plugin_dir("opencode")?;
    let dir_str = dir.to_string_lossy().replace('\\', "/");
    let cfg = home_dir()?.join(".config/opencode/opencode.json");
    let _ = edit_json_string_array(&cfg, &["plugin"], &dir_str, false, |_| {});
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(())
}

// --- openclaw: plugin dir in openclaw.json paths + entries --------------------

fn install_openclaw_plugin() -> Result<()> {
    let dir = our_plugin_dir("openclaw")?;
    write_plugin_files(
        &dir,
        &[
            ("index.js", OPENCLAW_INDEX_JS),
            ("openclaw.plugin.json", OPENCLAW_MANIFEST_JSON),
            ("package.json", OPENCLAW_PACKAGE_JSON),
        ],
    )?;
    write_bridge_json(&dir, "openclaw")?;
    let cfg = openclaw_config_path()?;
    let dir_str = dir.to_string_lossy().replace('\\', "/");
    edit_openclaw_config(&cfg, &dir_str, true)
}

fn uninstall_openclaw_plugin() -> Result<()> {
    let dir = our_plugin_dir("openclaw")?;
    let dir_str = dir.to_string_lossy().replace('\\', "/");
    if let Ok(cfg) = openclaw_config_path() {
        let _ = edit_openclaw_config(&cfg, &dir_str, false);
    }
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(())
}

fn openclaw_config_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("OPENCLAW_CONFIG_PATH") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let base = std::env::var("OPENCLAW_STATE_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or(home_dir()?.join(".openclaw"));
    Ok(base.join("openclaw.json"))
}

/// Add/remove our plugin dir in openclaw.json: `plugins.load.paths[]` plus a
/// `plugins.entries["agent-buddy"]` enable record. Preserves the user's config.
fn edit_openclaw_config(cfg: &std::path::Path, dir_str: &str, add: bool) -> Result<()> {
    if !add && !cfg.exists() {
        return Ok(());
    }
    let mut root: Value = match std::fs::read(cfg) {
        Ok(b) if !b.is_empty() => serde_json::from_slice(&b)
            .with_context(|| format!("parsing {}", cfg.display()))?,
        _ => json!({}),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", cfg.display()))?;
    let plugins = obj
        .entry("plugins".to_string())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("`plugins` in {} is not an object", cfg.display()))?;
    // paths
    let load = plugins
        .entry("load".to_string())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("`plugins.load` is not an object"))?;
    let paths = load
        .entry("paths".to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| anyhow!("`plugins.load.paths` is not an array"))?;
    paths.retain(|v| v.as_str().map(|s| s.replace('\\', "/") != dir_str).unwrap_or(true));
    if add {
        paths.push(json!(dir_str));
    }
    // entries
    let entries = plugins
        .entry("entries".to_string())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("`plugins.entries` is not an object"))?;
    if add {
        entries.insert(
            PLUGIN_ID.to_string(),
            json!({ "enabled": true, "hooks": { "allowConversationAccess": false } }),
        );
    } else {
        entries.remove(PLUGIN_ID);
    }
    if let Some(parent) = cfg.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    write_atomic(cfg, &serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

// --- pi: TS extension copied into ~/.pi/agent/extensions/agent-buddy ----------

fn pi_extension_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".pi/agent/extensions").join(PLUGIN_ID))
}

fn install_pi_plugin() -> Result<()> {
    let dir = pi_extension_dir()?;
    write_plugin_files(&dir, &[("index.ts", PI_INDEX_TS)])?;
    write_bridge_json(&dir, "pi")?;
    std::fs::write(dir.join(".agent-buddy-managed.json"), MANAGED_MARKER)
        .with_context(|| format!("writing {}", dir.join(".agent-buddy-managed.json").display()))?;
    Ok(())
}

fn uninstall_pi_plugin() -> Result<()> {
    remove_managed_dir(&pi_extension_dir()?)
}

// --- hermes: Python package copied into $HERMES_HOME/plugins/agent-buddy ------

fn hermes_home() -> Result<PathBuf> {
    if let Ok(h) = std::env::var("HERMES_HOME") {
        if !h.is_empty() {
            return Ok(PathBuf::from(h));
        }
    }
    #[cfg(target_os = "windows")]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        if !local.is_empty() {
            return Ok(PathBuf::from(local).join("hermes"));
        }
    }
    Ok(home_dir()?.join(".hermes"))
}

fn install_hermes_plugin() -> Result<()> {
    let home = hermes_home()?;
    let dir = home.join("plugins").join(PLUGIN_ID);
    write_plugin_files(
        &dir,
        &[("__init__.py", HERMES_INIT_PY), ("plugin.yaml", HERMES_PLUGIN_YAML)],
    )?;
    write_bridge_json(&dir, "hermes")?;
    std::fs::write(dir.join(".agent-buddy-managed.json"), MANAGED_MARKER).ok();
    // Enable it. Hermes ships inside a venv and is rarely on PATH, so resolve the
    // venv binary first; fall back to bare `hermes`. Enable for the root home AND
    // each discovered profile (mirrors the reference's per-profile sync). Write
    // the enabled list directly only as a last resort so a missing CLI doesn't
    // leave the plugin installed-but-disabled.
    let cli = hermes_cli_path(&home);
    let mut enabled_any = run_hermes_enable(&cli, &home);
    for profile_home in hermes_profile_homes(&home) {
        if run_hermes_enable(&cli, &profile_home) {
            enabled_any = true;
        }
    }
    if !enabled_any {
        tracing::warn!(
            "could not enable the hermes plugin via the CLI; writing plugins.enabled \
             directly (you may need to run `hermes plugins enable {PLUGIN_ID}` manually)"
        );
        ensure_line_in_file(&home.join("plugins.enabled"), PLUGIN_ID)?;
    }
    Ok(())
}

/// Resolve the Hermes CLI: prefer the venv binary under `<home>/hermes-agent/venv`
/// (Unix `bin/hermes`, Windows `Scripts/hermes.exe`), else bare `hermes` on PATH.
fn hermes_cli_path(home: &std::path::Path) -> PathBuf {
    let venv = home.join("hermes-agent").join("venv");
    #[cfg(target_os = "windows")]
    let candidate = venv.join("Scripts").join("hermes.exe");
    #[cfg(not(target_os = "windows"))]
    let candidate = venv.join("bin").join("hermes");
    if candidate.exists() {
        candidate
    } else {
        PathBuf::from("hermes")
    }
}

/// Profile home dirs that carry a `config.yaml` under `<home>/profiles/*/`.
fn hermes_profile_homes(home: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(home.join("profiles")) {
        for e in entries.flatten() {
            let p = e.path();
            if p.join("config.yaml").exists() {
                out.push(p);
            }
        }
    }
    out
}

/// Run `<cli> plugins enable <PLUGIN_ID>` with `HERMES_HOME` set; true on success.
fn run_hermes_enable(cli: &std::path::Path, hermes_home: &std::path::Path) -> bool {
    std::process::Command::new(cli)
        .args(["plugins", "enable", PLUGIN_ID])
        .env("HERMES_HOME", hermes_home)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn uninstall_hermes_plugin() -> Result<()> {
    let home = hermes_home()?;
    let _ = std::process::Command::new("hermes")
        .args(["plugins", "disable", PLUGIN_ID])
        .env("HERMES_HOME", &home)
        .output();
    let enabled = home.join("plugins.enabled");
    if enabled.exists() {
        let _ = remove_line_from_file(&enabled, PLUGIN_ID);
    }
    remove_managed_dir(&home.join("plugins").join(PLUGIN_ID))
}

// --- shared helpers -----------------------------------------------------------

/// Marker file content proving a plugin dir is ours (so uninstall only deletes
/// dirs we created, never a user's own same-named extension).
const MANAGED_MARKER: &str = "{ \"app\": \"agent-buddy\", \"managed\": true }\n";

/// Remove a plugin dir only if it carries our managed marker (best-effort).
fn remove_managed_dir(dir: &std::path::Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    if dir.join(".agent-buddy-managed.json").exists() {
        let _ = std::fs::remove_dir_all(dir);
    }
    Ok(())
}

/// Ensure `line` is present in `path` (one entry per line), creating the file if
/// needed. Idempotent.
fn ensure_line_in_file(path: &std::path::Path, line: &str) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(line);
    out.push('\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    write_atomic(path, out.as_bytes())?;
    Ok(())
}

/// Remove every occurrence of `line` from `path` (one entry per line).
fn remove_line_from_file(path: &std::path::Path, line: &str) -> Result<()> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let kept: Vec<&str> = existing.lines().filter(|l| l.trim() != line).collect();
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    write_atomic(path, out.as_bytes())?;
    Ok(())
}

/// Add or remove `value` in a nested JSON string array at `keys` (e.g.
/// `["plugin"]`), preserving the rest of the file. `seed` runs on the root
/// object before editing (e.g. to add a `$schema`). Idempotent. A remove on a
/// missing file is a no-op.
fn edit_json_string_array(
    path: &std::path::Path,
    keys: &[&str],
    value: &str,
    add: bool,
    seed: impl FnOnce(&mut serde_json::Map<String, Value>),
) -> Result<()> {
    if !add && !path.exists() {
        return Ok(());
    }
    let mut root: Value = match std::fs::read(path) {
        Ok(b) if !b.is_empty() => serde_json::from_slice(&b)
            .with_context(|| format!("parsing {}", path.display()))?,
        _ => json!({}),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;
    if add {
        seed(obj);
    }
    // Walk/create down to the array's parent.
    let (last, parents) = keys.split_last().expect("at least one key");
    let mut cur = obj;
    for k in parents {
        cur = cur
            .entry(k.to_string())
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| anyhow!("`{k}` in {} is not an object", path.display()))?;
    }
    let arr = cur
        .entry(last.to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| anyhow!("`{last}` in {} is not an array", path.display()))?;
    arr.retain(|v| v.as_str().map(|s| s.replace('\\', "/") != value).unwrap_or(true));
    if add {
        arr.push(json!(value));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    write_atomic(path, &serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

fn claude_settings_path() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("could not find home directory")?;
    Ok(base.home_dir().join(".claude").join("settings.json"))
}

/// The tool matcher to wire, from the persisted config (set by `setup
/// --tools`), falling back to the default. Reading it here is what lets
/// reconciliation preserve a user's custom `--tools` choice instead of resetting
/// it to the default on every daemon restart.
fn configured_matcher() -> String {
    crate::state::Config::load()
        .ok()
        .and_then(|c| c.hook_matcher)
        .unwrap_or_else(|| DEFAULT_MATCHER.to_string())
}

/// Wire/reconcile the Claude Code hooks at the daemon's *stable* install path
/// (the same location the service runs from), using the configured matcher. The
/// desktop app's one-click install calls this right after installing the
/// service, so a GUI install wires hooks exactly like `agent-buddy setup` /
/// `install.sh` do — the install paths can't drift and leave the daemon running
/// with nothing feeding it. Targets `~/.claude/settings.json` (user-global), so
/// hooks apply across every Claude Code project. Returns the settings file.
pub fn wire_claude_hooks() -> Result<PathBuf> {
    let target = installed_daemon_path()?.to_string_lossy().into_owned();
    let settings_path = claude_settings_path()?;
    wire_hooks(&settings_path, &target, &configured_matcher())?;
    Ok(settings_path)
}

/// Reconcile the Claude Code hooks to the canonical set for the *installed*
/// daemon and the configured matcher. Run at daemon startup (and after an app
/// update restarts it), this is the self-healing safety net: it repairs a
/// half-finished install that never wired hooks, restores a hook a user deleted
/// by hand, points a stale command at the current binary path, and adds/removes
/// events when a new daemon version changes the set — all while leaving the
/// user's own hooks untouched. Best-effort: the caller logs any error rather
/// than failing on it. Returns whether anything changed.
pub fn ensure_claude_hooks() -> Result<bool> {
    let target = installed_daemon_path()?;
    // Only reconcile when there's a real installed daemon to point hooks at.
    // Without this guard a daemon run straight from a dev build (`cargo run`,
    // no install) would rewrite the user's settings to invoke a binary that
    // doesn't exist at the stable path. A genuine install always satisfies this
    // (the service runs *from* that path).
    if !target.exists() {
        return Ok(false);
    }
    let settings_path = claude_settings_path()?;
    wire_hooks(&settings_path, &target.to_string_lossy(), &configured_matcher())
}

/// Reconcile the *active* agent's hooks against the installed daemon. For the
/// default Claude Code agent this is exactly [`ensure_claude_hooks`] (the
/// matcher-aware, self-healing path the GUI/CLI/install share). For any other
/// harness it re-applies that profile's [`InstallRecipe`] against the installed
/// daemon binary, so a daemon restart repairs a non-Claude harness's hooks too
/// (stale binary path after an app update, a hook the user deleted, a changed
/// event set). Best-effort; only acts when a real installed daemon exists so a
/// dev `cargo run` never rewrites the user's harness config. Returns whether
/// work was applied.
pub fn ensure_active_agent_hooks(
    config: &crate::state::Config,
    profiles: &std::collections::HashMap<String, AgentProfile>,
) -> Result<bool> {
    // The default agent uses the dedicated, matcher-aware Claude reconcile.
    if config.active_agent == crate::agent::DEFAULT_AGENT {
        return ensure_claude_hooks();
    }
    let target = installed_daemon_path()?;
    if !target.exists() {
        return Ok(false);
    }
    let Some(profile) = profiles.get(&config.active_agent) else {
        return Ok(false);
    };
    install_profile(profile, &target.to_string_lossy())?;
    Ok(true)
}

/// Remove every agent-buddy hook entry from Claude Code's settings, leaving the
/// user's own hooks in place. Used by `uninstall`. A missing, empty, or
/// unparseable settings file is a no-op. Returns whether anything changed.
pub fn strip_claude_hooks() -> Result<bool> {
    strip_hooks_at(&claude_settings_path()?)
}

/// Strip our hooks from the settings file at `path` (split out so it's testable
/// without touching the real `~/.claude/settings.json`).
fn strip_hooks_at(path: &PathBuf) -> Result<bool> {
    let mut root: Value = match std::fs::read(path) {
        Ok(b) if !b.is_empty() => match serde_json::from_slice(&b) {
            Ok(v) => v,
            Err(_) => return Ok(false),
        },
        _ => return Ok(false),
    };
    let Some(obj) = root.as_object_mut() else {
        return Ok(false);
    };
    let Some(hooks) = obj.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return Ok(false);
    };
    let before = serde_json::to_string(hooks).unwrap_or_default();
    for arr in hooks.values_mut() {
        if let Some(a) = arr.as_array_mut() {
            a.retain(|e| !is_ours(e, ""));
        }
    }
    hooks.retain(|_, v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true));
    if serde_json::to_string(hooks).unwrap_or_default() == before {
        return Ok(false);
    }
    write_atomic(&path, &serde_json::to_vec_pretty(&root)?)?;
    Ok(true)
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
    let plist = dir.join("com.nateschnell.agent-buddy.plist");
    let log = base
        .home_dir()
        .join("Library/Logs/agent-buddy.log")
        .to_string_lossy()
        .into_owned();
    // KeepAlive stays unconditional (`<true/>`): a *conditional* KeepAlive that
    // leaves a clean exit alone is only safe once the daemon exits 0 on benign
    // single-instance lock contention. While the daemon still bails non-zero on
    // contention, `SuccessfulExit:false` would re-spawn it into a throttled
    // crash loop — so that change must land together with the daemon's exit-code
    // fix, not before it. (ProgramArguments points at the codesigned helper
    // bundle so the daemon's Bluetooth TCC grant keys to a stable identity.)
    // XML-escape the interpolated paths (parity with the Windows task path): a
    // home dir / username containing `&`, `<`, or `"` would otherwise produce a
    // malformed plist that launchd rejects.
    let exe = xml_escape(exe);
    let log = xml_escape(&log);
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.nateschnell.agent-buddy</string>
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
    let unit = dir.join("agent-buddy.service");
    // RestartSec throttles the respawn so a daemon that fails fast at startup
    // (e.g. BlueZ not yet up) doesn't tight-loop into systemd's StartLimit and
    // then stay dead.
    let body = format!(
        "[Unit]\nDescription=Claude buddy bridge daemon\n\n\
         [Service]\nExecStart={exe} daemon\nRestart=always\nRestartSec=2\n\n\
         [Install]\nWantedBy=default.target\n"
    );
    std::fs::write(&unit, body)?;
    Ok(format!(
        "wrote systemd unit {}. Start it now with:\n    systemctl --user enable --now agent-buddy",
        unit.display()
    ))
}

#[cfg(target_os = "windows")]
fn install_daemon_service(exe: &str) -> Result<String> {
    // Per-user logon-triggered Scheduled Task — the lightweight equivalent of a
    // LaunchAgent / systemd user unit. No elevation, survives the GUI closing.
    //
    // We register it from an XML definition rather than the bare `/SC ONLOGON`
    // form because the CLI flags can't express a restart policy: a plain logon
    // task that crashes stays dead until the *next* logon (Windows has no
    // launchd-style KeepAlive). The XML adds `RestartOnFailure` (retry on crash)
    // and removes the default execution time limit (so the long-running daemon
    // isn't killed after 72h), plus `IgnoreNew` so a second logon can't spawn a
    // duplicate daemon fighting over the buddy's single BLE link (the singleton
    // flock isn't enforced on Windows). If the XML create fails for any reason,
    // fall back to the simple logon task so install never regresses.
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Agent Buddy bridge daemon</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <Enabled>true</Enabled>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure><Interval>PT1M</Interval><Count>999</Count></RestartOnFailure>
  </Settings>
  <Actions Context="Author"><Exec><Command>{cmd}</Command><Arguments>daemon</Arguments></Exec></Actions>
</Task>"#,
        cmd = xml_escape(exe),
    );

    let xml_path = std::env::temp_dir().join("agent-buddy-task.xml");
    let xml_path_str = xml_path.to_string_lossy().into_owned();
    let from_xml = std::fs::write(&xml_path, xml.as_bytes())
        .map_err(anyhow::Error::from)
        .and_then(|_| {
            run_cmd(
                "schtasks",
                &["/Create", "/F", "/TN", WIN_TASK, "/XML", &xml_path_str],
            )
        });
    let _ = std::fs::remove_file(&xml_path);

    if from_xml.is_err() {
        // Fallback: the simple logon task. No crash-restart, but better than no
        // service at all.
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
    }
    Ok(format!(
        "registered logon task {WIN_TASK}. Start it now with:\n    schtasks /Run /TN {WIN_TASK}"
    ))
}

/// Minimal XML text escaping for a path embedded in a task/plist definition.
/// Used by both the Windows Scheduled Task XML and the macOS LaunchAgent plist.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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
const MAC_LABEL: &str = "com.nateschnell.agent-buddy";
/// Stable bundle identifier for the daemon helper .app. macOS keys the
/// Bluetooth (TCC) grant to the code identity derived from this id + the
/// ad-hoc signature, so it MUST stay constant across rebuilds or the user gets
/// re-prompted (or silently denied) every time.
#[cfg(target_os = "macos")]
const MAC_DAEMON_BUNDLE_ID: &str = "com.nateschnell.agent-buddy-helper";

/// Self-signed code-signing identity (created once by the user via
/// `codesign`/Keychain) used to sign the helper + app bundles when present.
/// Signing with a STABLE cert instead of ad-hoc `-` keeps each bundle's
/// designated requirement constant across rebuilds (it becomes `identifier +
/// certificate-leaf-hash`), so the macOS Bluetooth / Local-Network (TCC) grants
/// PERSIST instead of resetting every build. Falls back to ad-hoc if absent.
#[cfg(target_os = "macos")]
const MAC_SIGN_IDENTITY: &str = "Agent Buddy Dev";

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
const WIN_TASK: &str = "AgentBuddy";

/// Locate the `agent-buddy` daemon binary to register as the service. Prefers
/// a copy sitting next to the currently-running executable (so the GUI, which
/// ships beside the daemon, points the service at the bundled daemon), then
/// falls back to the bare name on `PATH`.
pub fn daemon_exe_path() -> Result<String> {
    let here = std::env::current_exe().context("locating this executable")?;
    if let Some(dir) = here.parent() {
        let name = if cfg!(windows) {
            "agent-buddy.exe"
        } else {
            "agent-buddy"
        };
        let candidate = dir.join(name);
        if candidate.exists() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    Ok(if cfg!(windows) {
        "agent-buddy.exe"
    } else {
        "agent-buddy"
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
/// (`Agent Buddy Helper.app/Contents/MacOS/agent-buddy`) so its sibling
/// `Info.plist` can declare `NSBluetoothAlwaysUsageDescription` and a stable
/// `CFBundleIdentifier`. The daemon — not the GUI — is the process that opens
/// CoreBluetooth, so the TCC usage string and code identity must hang off *its*
/// bundle, not the GUI's.
#[cfg(not(target_os = "macos"))]
fn installed_daemon_path() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new().context("home dir")?;
    let name = if cfg!(windows) {
        "agent-buddy.exe"
    } else {
        "agent-buddy"
    };
    Ok(dirs
        .data_local_dir()
        .join("Agent Buddy")
        .join("bin")
        .join(name))
}

#[cfg(target_os = "macos")]
fn installed_daemon_path() -> Result<PathBuf> {
    Ok(mac_daemon_bundle()?.join("Contents/MacOS/agent-buddy"))
}

/// The daemon helper `.app` bundle root.
#[cfg(target_os = "macos")]
fn mac_daemon_bundle() -> Result<PathBuf> {
    let dirs = directories::BaseDirs::new().context("home dir")?;
    Ok(dirs
        .data_local_dir()
        .join("Agent Buddy")
        .join("Agent Buddy Helper.app"))
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
    let _ = write_daemon_version();
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

    let dest = macos_dir.join("agent-buddy");
    if !same_file(src, &dest) {
        install_binary(src, &dest)?;
    }

    // Sibling Info.plist carrying the Bluetooth usage string macOS shows in the
    // permission prompt, plus the stable bundle id the TCC grant keys to.
    // Central role only — NSBluetoothPeripheralUsageDescription is not needed.
    //
    // Activation policy is `LSUIElement`, NOT `LSBackgroundOnly`. This is load
    // bearing for first-run Bluetooth: a background-only process is one macOS
    // considers incapable of UI, so `tccd` *auto-denies* the Bluetooth request
    // without ever prompting — the daemon then scans forever as "not permitted"
    // and the device looks dead. `LSUIElement` is a UI-capable agent (no Dock
    // icon, runs in the GUI launchd domain, which it does via `RunAtLoad`), so
    // tccd presents the standard "Agent Buddy Helper would like to use
    // Bluetooth" prompt the first time the daemon opens CoreBluetooth. The grant
    // attaches to THIS bundle's code identity (the process that actually owns
    // the radio), so the GUI app can't grant it by proxy — it has to be the
    // helper. Once granted, the existing retry loop reconnects on the next pass.
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Agent Buddy Helper</string>
  <key>CFBundleIdentifier</key><string>{MAC_DAEMON_BUNDLE_ID}</string>
  <key>CFBundleExecutable</key><string>agent-buddy</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>{ver}</string>
  <key>CFBundleShortVersionString</key><string>{ver}</string>
  <key>LSMinimumSystemVersion</key><string>10.13</string>
  <key>LSUIElement</key><true/>
  <key>NSBluetoothAlwaysUsageDescription</key><string>Agent Buddy connects to your buddy device over Bluetooth.</string>
</dict></plist>"#,
        ver = env!("CARGO_PKG_VERSION")
    );
    std::fs::write(contents.join("Info.plist"), plist).context("writing daemon Info.plist")?;

    // Sign the bundle so the daemon's Bluetooth TCC grant attaches to a durable
    // identity. Prefers the stable self-signed dev cert (grant persists across
    // rebuilds), falls back to ad-hoc.
    mac_codesign(&bundle.to_string_lossy(), MAC_DAEMON_BUNDLE_ID, false);

    // Stamp the version *after* signing — the sidecar lives outside the bundle
    // (see `daemon_version_sidecar`), so writing it can't disturb the signature.
    let _ = write_daemon_version();
    Ok(dest.to_string_lossy().into_owned())
}

/// This build's version, baked at compile time by `build.rs` from the release
/// tag (`git describe` / `AGENT_BUDDY_VERSION`). The GUI and the daemon it ships
/// share it, so it doubles as "the version of the daemon bundled with this app".
pub fn current_version() -> &'static str {
    env!("AGENT_BUDDY_VERSION")
}

/// Path of the sidecar recording the installed daemon's version. Sits *beside*
/// the install, never inside the macOS bundle, so writing it can't break the
/// bundle's code signature. Lets the GUI tell, on launch, whether the daemon it
/// bundles is newer than the one installed — i.e. whether an app update needs to
/// refresh the daemon too.
fn daemon_version_sidecar() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let parent = mac_daemon_bundle()?
            .parent()
            .context("daemon bundle has no parent")?
            .to_path_buf();
        Ok(parent.join("daemon.version"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let bin = installed_daemon_path()?;
        let dir = bin.parent().context("installed daemon has no parent dir")?;
        Ok(dir.join("daemon.version"))
    }
}

/// Record [`current_version`] as the installed daemon's version. Best-effort.
fn write_daemon_version() -> Result<()> {
    let path = daemon_version_sidecar()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, current_version())
        .with_context(|| format!("writing {}", path.display()))
}

/// The version recorded for the currently-installed daemon, if any. `None` when
/// nothing is installed or the sidecar predates version tracking.
pub fn installed_daemon_version() -> Option<String> {
    let v = std::fs::read_to_string(daemon_version_sidecar().ok()?).ok()?;
    let v = v.trim().to_string();
    (!v.is_empty()).then_some(v)
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

/// Keep the installed daemon in lock-step with the app. If the daemon this app
/// ships is newer than the one currently installed, re-stage it and restart the
/// service — so an in-place app update also updates the background daemon (and,
/// via the daemon's startup hook-reconciliation, its hooks). The GUI calls this
/// once at launch.
///
/// No-op when: nothing is installed yet (first-time install goes through the
/// explicit button), there's no daemon shipped beside this binary (a bare dev
/// build), or the installed daemon is already current. Returns a note when it
/// updated, `None` when it left things alone.
pub fn refresh_daemon_if_outdated() -> Result<Option<String>> {
    let installed_path = installed_daemon_path()?;
    // Nothing installed → leave first-time install to the user's action.
    if !installed_path.exists() {
        return Ok(None);
    }
    let bundled = daemon_exe_path()?;
    // No daemon shipped beside us, or we *are* the install (dev run) → nothing
    // to refresh from.
    if !std::path::Path::new(&bundled).exists() || same_file(&bundled, &installed_path) {
        return Ok(None);
    }
    let current = current_version();
    // Skip when the installed daemon is already as new (or newer). When the
    // sidecar is missing/unparseable (a pre-tracking install), fall through and
    // refresh once — which also stamps the sidecar for next time.
    if let Some(installed) = installed_daemon_version() {
        if !crate::update::is_newer(current, &installed) {
            return Ok(None);
        }
    }
    let dest = install_daemon_binary(&bundled)?;
    install_daemon_service(&dest)?;
    service_restart()?;
    Ok(Some(format!("updated background daemon to {current}")))
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
    run_cmd("systemctl", &["--user", "enable", "--now", "agent-buddy"])?;
    // Enable lingering so the user's systemd instance (and thus this daemon)
    // keeps running across logout and starts at boot before anyone logs in.
    // Without it, `--user` units stop the moment the session ends — a powered
    // buddy would then have nothing to connect to until the next login.
    // Best-effort: enabling linger can require polkit authorization, and a
    // failure here shouldn't fail an otherwise-successful install.
    let user = std::env::var("USER").unwrap_or_default();
    let lingered = if user.is_empty() {
        run_cmd("loginctl", &["enable-linger"]).is_ok()
    } else {
        run_cmd("loginctl", &["enable-linger", &user]).is_ok()
    };
    Ok(if lingered {
        "service started (lingering enabled — survives logout)".into()
    } else {
        "service started (note: enable lingering with `loginctl enable-linger` \
         so the buddy stays reachable after you log out)"
            .into()
    })
}

#[cfg(target_os = "linux")]
pub fn service_restart() -> Result<String> {
    let _ = run_cmd("systemctl", &["--user", "daemon-reload"]);
    run_cmd("systemctl", &["--user", "restart", "agent-buddy"])?;
    Ok("service restarted".into())
}

#[cfg(target_os = "linux")]
pub fn service_stop() -> Result<String> {
    run_cmd("systemctl", &["--user", "stop", "agent-buddy"])?;
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
    let _ = run_cmd("taskkill", &["/IM", "agent-buddy.exe", "/F"]);
    run_cmd("schtasks", &["/Run", "/TN", WIN_TASK])?;
    Ok("service restarted".into())
}

#[cfg(target_os = "windows")]
pub fn service_stop() -> Result<String> {
    // End the scheduled run, then make sure the process is actually gone.
    let _ = run_cmd("schtasks", &["/End", "/TN", WIN_TASK]);
    let _ = run_cmd("taskkill", &["/IM", "agent-buddy.exe", "/F"]);
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
const MAC_APP_LABEL: &str = "com.nateschnell.agent-buddy-app";
#[cfg(target_os = "windows")]
const WIN_APP_TASK: &str = "AgentBuddyApp";

/// Locate the desktop app binary (`agent-buddy-app`) sitting next to this
/// executable, if present. `None` means it isn't installed alongside, so the
/// caller should skip wiring its login item.
pub fn app_exe_path() -> Result<Option<String>> {
    let here = std::env::current_exe().context("locating this executable")?;
    let dir = here
        .parent()
        .context("executable has no parent directory")?;
    let name = if cfg!(windows) {
        "agent-buddy-app.exe"
    } else {
        "agent-buddy-app"
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
pub fn install_app_login_item(app_exe: &str) -> Result<String> {
    // The GUI/tray needs a graphical session, so autostart goes through the
    // freedesktop XDG spec (`~/.config/autostart/*.desktop`) honored by GNOME,
    // KDE, XFCE, and the other major DEs — the cross-DE equivalent of the macOS
    // LaunchAgent / Windows logon task. `OnlyShowIn` is omitted so every DE runs
    // it. `Exec` must be an absolute path; the installed app exe is.
    let base = directories::BaseDirs::new().context("home dir")?;
    let dir = base.home_dir().join(".config/autostart");
    std::fs::create_dir_all(&dir)?;
    let entry = dir.join("agent-buddy-app.desktop");
    // Escape `Exec`: a `%` is a field code in the .desktop spec (literal `%` is
    // `%%`); paths with spaces are fine unquoted for a single-arg Exec, but quote
    // for safety and double any embedded quote.
    let exec = app_exe.replace('%', "%%");
    let body = format!(
        "[Desktop Entry]\nType=Application\nName=Agent Buddy\n\
         Comment=Hardware buddy gateway\nExec=\"{exec}\"\n\
         Terminal=false\nX-GNOME-Autostart-enabled=true\n"
    );
    write_atomic(&entry, body.as_bytes())?;
    Ok(format!("desktop app set to open at login ({})", entry.display()))
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
    let probe = PathBuf::from("/Applications/Agent Buddy.app/Contents/MacOS");
    let bundle = if std::fs::create_dir_all(&probe).is_ok() {
        PathBuf::from("/Applications/Agent Buddy.app")
    } else {
        let base = directories::BaseDirs::new().context("home dir")?;
        base.home_dir().join("Applications/Agent Buddy.app")
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
    let dest_exe = macos_dir.join("agent-buddy-app");
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
        let daemon_src = src_dir.join("agent-buddy");
        let daemon_dest = macos_dir.join("agent-buddy");
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
  <key>CFBundleName</key><string>Agent Buddy</string>
  <key>CFBundleDisplayName</key><string>Agent Buddy</string>
  <key>CFBundleIdentifier</key><string>com.nateschnell.agent-buddy-app</string>
  <key>CFBundleExecutable</key><string>agent-buddy-app</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>{ver}</string>
  <key>CFBundleShortVersionString</key><string>{ver}</string>
  <key>LSMinimumSystemVersion</key><string>10.13</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>Agent Buddy flashes firmware updates to your buddy over your local Wi-Fi network.</string>
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
        "com.nateschnell.agent-buddy-app",
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
    let lnk = programs.join("Agent Buddy.lnk");
    let script = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{lnk}');\
         $s.TargetPath='{exe}';$s.Description='Agent Buddy';$s.Save()",
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
    let desktop = dir.join("agent-buddy.desktop");
    let body = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Agent Buddy\n\
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

// ---------------------------------------------------------------------------
// Uninstall — reverse everything install/setup created. Each step is
// independent and best-effort: a failure (or an artifact that's already gone)
// never blocks the rest, so a half-installed machine still ends up clean.
// Drives the CLI `uninstall`, the desktop app's "Uninstall" action, and the
// Windows installer's uninstall step.
// ---------------------------------------------------------------------------

/// Remove EVERYTHING this tool put on the machine: the Claude Code hooks, the
/// background daemon + its service, the desktop app's login item and clickable
/// launcher, and the per-user state dir. Best-effort — records a note per step
/// and never aborts partway. Returns a human-readable multi-line summary.
pub fn uninstall() -> Result<String> {
    let mut notes: Vec<String> = Vec::new();
    let mut note = |label: &str, r: Result<bool>| match r {
        Ok(true) => notes.push(format!("✓ {label}")),
        Ok(false) => notes.push(format!("· {label} (nothing to remove)")),
        Err(e) => notes.push(format!("! {label}: {e}")),
    };

    // Stop the daemon + tear down its service first, so nothing respawns it
    // while we remove its files and hooks.
    note("background service", remove_daemon_service());
    note("desktop app login item", remove_app_login_item());
    note("Claude Code hooks", strip_claude_hooks());
    // Also strip every non-Claude harness's hooks. Switching the active agent
    // writes hook entries into that harness's OWN config (~/.codex/hooks.json,
    // ~/.gemini/settings.json, ~/.kimi/config.toml, …); without this, a user who
    // tried a non-Claude harness then uninstalled is left with a dead
    // `agent-buddy hook … --agent <id>` wired into their other AI tool — and once
    // the daemon binary below is removed, that hook fails to spawn on every event.
    // is_ours matches by the `agent-buddy`+` hook ` substring (exe-independent),
    // so this is idempotent and a no-op for harnesses that were never wired.
    {
        let exe = installed_daemon_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        for profile in crate::agent::load_profiles().values() {
            if profile.id == crate::agent::DEFAULT_AGENT {
                continue; // already handled by strip_claude_hooks
            }
            note(
                &format!("{} hooks", profile.id),
                uninstall_profile(profile, &exe).map(|_| true),
            );
        }
    }
    note("daemon binary", remove_daemon_binary());
    note("desktop launcher", remove_desktop_launcher());
    note("per-user state", remove_state_dir());

    Ok(format!("Uninstalled Agent Buddy:\n  {}", notes.join("\n  ")))
}

/// Remove the per-user config/state dir (config.json, endpoint.json, lock). On
/// unix the running GUI's open lock fd survives the unlink; on Windows it may
/// keep the file (the GUI isn't running during an installer-driven uninstall).
fn remove_state_dir() -> Result<bool> {
    // Compute the path WITHOUT `config_dir()`, which would re-create it.
    let dir = directories::ProjectDirs::from("com", "anthropic", "agent-buddy")
        .context("could not determine the state dir")?
        .config_dir()
        .to_path_buf();
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    Ok(true)
}

#[cfg(target_os = "macos")]
fn remove_daemon_service() -> Result<bool> {
    let plist = mac_plist_path()?;
    let existed = std::path::Path::new(&plist).exists();
    let _ = run_cmd("launchctl", &["unload", "-w", &plist]);
    if existed {
        let _ = std::fs::remove_file(&plist);
    }
    Ok(existed)
}

#[cfg(target_os = "linux")]
fn remove_daemon_service() -> Result<bool> {
    let _ = run_cmd("systemctl", &["--user", "disable", "--now", "agent-buddy"]);
    let base = directories::BaseDirs::new().context("home dir")?;
    let unit = base
        .home_dir()
        .join(".config/systemd/user/agent-buddy.service");
    let existed = unit.exists();
    if existed {
        let _ = std::fs::remove_file(&unit);
    }
    let _ = run_cmd("systemctl", &["--user", "daemon-reload"]);
    Ok(existed)
}

#[cfg(target_os = "windows")]
fn remove_daemon_service() -> Result<bool> {
    let _ = run_cmd("schtasks", &["/End", "/TN", WIN_TASK]);
    let _ = run_cmd("taskkill", &["/IM", "agent-buddy.exe", "/F"]);
    Ok(run_cmd("schtasks", &["/Delete", "/F", "/TN", WIN_TASK]).is_ok())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn remove_daemon_service() -> Result<bool> {
    Ok(false)
}

#[cfg(target_os = "macos")]
fn remove_app_login_item() -> Result<bool> {
    let base = directories::BaseDirs::new().context("home dir")?;
    let plist = base
        .home_dir()
        .join("Library/LaunchAgents")
        .join(format!("{MAC_APP_LABEL}.plist"));
    let p = plist.to_string_lossy().into_owned();
    let existed = plist.exists();
    let _ = run_cmd("launchctl", &["unload", "-w", &p]);
    if existed {
        let _ = std::fs::remove_file(&plist);
    }
    Ok(existed)
}

#[cfg(target_os = "windows")]
fn remove_app_login_item() -> Result<bool> {
    Ok(run_cmd("schtasks", &["/Delete", "/F", "/TN", WIN_APP_TASK]).is_ok())
}

#[cfg(any(
    target_os = "linux",
    not(any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
fn remove_app_login_item() -> Result<bool> {
    // Remove the XDG autostart entry written by install_app_login_item.
    let base = directories::BaseDirs::new().context("home dir")?;
    let entry = base
        .home_dir()
        .join(".config/autostart/agent-buddy-app.desktop");
    if entry.exists() {
        std::fs::remove_file(&entry).with_context(|| format!("removing {}", entry.display()))?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(target_os = "macos")]
fn remove_daemon_binary() -> Result<bool> {
    let mut removed = false;
    let bundle = mac_daemon_bundle()?;
    if bundle.exists() {
        std::fs::remove_dir_all(&bundle)
            .with_context(|| format!("removing {}", bundle.display()))?;
        removed = true;
    }
    if let Ok(sidecar) = daemon_version_sidecar() {
        if sidecar.exists() {
            let _ = std::fs::remove_file(&sidecar);
            removed = true;
        }
    }
    Ok(removed)
}

#[cfg(not(target_os = "macos"))]
fn remove_daemon_binary() -> Result<bool> {
    let mut removed = false;
    let bin = installed_daemon_path()?;
    if bin.exists() {
        std::fs::remove_file(&bin).with_context(|| format!("removing {}", bin.display()))?;
        removed = true;
    }
    if let Ok(sidecar) = daemon_version_sidecar() {
        if sidecar.exists() {
            let _ = std::fs::remove_file(&sidecar);
            removed = true;
        }
    }
    Ok(removed)
}

#[cfg(target_os = "macos")]
fn remove_desktop_launcher() -> Result<bool> {
    let mut removed = false;
    let mut candidates = vec![PathBuf::from("/Applications/Agent Buddy.app")];
    if let Some(base) = directories::BaseDirs::new() {
        candidates.push(base.home_dir().join("Applications/Agent Buddy.app"));
    }
    for app in candidates {
        // Deleting the bundle we're running from is safe on macOS — the open
        // inode keeps the process alive until it exits.
        if app.exists() && std::fs::remove_dir_all(&app).is_ok() {
            removed = true;
        }
    }
    Ok(removed)
}

#[cfg(target_os = "windows")]
fn remove_desktop_launcher() -> Result<bool> {
    let base = directories::BaseDirs::new().context("home dir")?;
    let lnk = base
        .data_dir()
        .join(r"Microsoft\Windows\Start Menu\Programs")
        .join("Agent Buddy.lnk");
    let existed = lnk.exists();
    if existed {
        let _ = std::fs::remove_file(&lnk);
    }
    Ok(existed)
}

#[cfg(target_os = "linux")]
fn remove_desktop_launcher() -> Result<bool> {
    let base = directories::BaseDirs::new().context("home dir")?;
    let dir = base.home_dir().join(".local/share/applications");
    let desktop = dir.join("agent-buddy.desktop");
    let existed = desktop.exists();
    if existed {
        let _ = std::fs::remove_file(&desktop);
    }
    let _ = run_cmd("update-desktop-database", &[&dir.to_string_lossy()]);
    Ok(existed)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn remove_desktop_launcher() -> Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_command_carries_agent_flag() {
        let c = hook_command("/x/agent-buddy", "PreToolUse", "codex");
        assert_eq!(c, "\"/x/agent-buddy\" hook PreToolUse --agent codex");
        assert!(is_ours(
            &json!({ "hooks": [ { "type": "command", "command": c } ] }),
            ""
        ));
    }

    #[test]
    fn gated_events_get_the_matcher() {
        assert!(event_is_gated("PreToolUse"));
        assert!(event_is_gated("postToolUse"));
        assert!(event_is_gated("PermissionRequest"));
        assert!(!event_is_gated("SessionStart"));
        assert!(!event_is_gated("Notification"));
    }

    #[test]
    fn marked_block_strips_only_our_tail() {
        let user = "[settings]\nkey = 1\n";
        let with = format!("{user}{TOML_MARK}\n[[hooks]]\nevent = \"Stop\"\n");
        assert_eq!(strip_marked_block(&with), "[settings]\nkey = 1");
        // No marker → untouched.
        assert_eq!(strip_marked_block(user), user);
    }

    #[test]
    fn toml_quote_escapes_windows_paths() {
        assert_eq!(toml_quote(r"C:\x\agent-buddy.exe"), r#""C:\\x\\agent-buddy.exe""#);
    }

    #[test]
    fn opencode_plugin_array_add_remove_is_idempotent_and_preserves_config() {
        let p = tmp("opencode");
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            r#"{"$schema":"x","plugin":["user/their-plugin"],"theme":"dark"}"#,
        )
        .unwrap();
        edit_json_string_array(&p, &["plugin"], "/abs/agent-buddy", true, |_| {}).unwrap();
        edit_json_string_array(&p, &["plugin"], "/abs/agent-buddy", true, |_| {}).unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let arr = v["plugin"].as_array().unwrap();
        assert_eq!(arr.iter().filter(|e| *e == "/abs/agent-buddy").count(), 1);
        assert!(arr.iter().any(|e| e == "user/their-plugin"));
        assert_eq!(v["theme"], "dark");
        edit_json_string_array(&p, &["plugin"], "/abs/agent-buddy", false, |_| {}).unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let arr = v["plugin"].as_array().unwrap();
        assert!(!arr.iter().any(|e| e == "/abs/agent-buddy"));
        assert!(arr.iter().any(|e| e == "user/their-plugin"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn opencode_register_seeds_schema_on_fresh_file() {
        let p = tmp("opencode-fresh");
        let _ = std::fs::remove_file(&p);
        edit_json_string_array(&p, &["plugin"], "/abs/x", true, |o| {
            o.entry("$schema".to_string())
                .or_insert_with(|| json!("https://opencode.ai/config.json"));
        })
        .unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["$schema"], "https://opencode.ai/config.json");
        assert_eq!(v["plugin"][0], "/abs/x");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn openclaw_config_links_paths_and_entries_then_unlinks() {
        let p = tmp("openclaw");
        let _ = std::fs::remove_file(&p);
        edit_openclaw_config(&p, "/abs/agent-buddy", true).unwrap();
        edit_openclaw_config(&p, "/abs/agent-buddy", true).unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let paths = v["plugins"]["load"]["paths"].as_array().unwrap();
        assert_eq!(paths.iter().filter(|e| *e == "/abs/agent-buddy").count(), 1);
        assert_eq!(v["plugins"]["entries"]["agent-buddy"]["enabled"], true);
        edit_openclaw_config(&p, "/abs/agent-buddy", false).unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert!(!v["plugins"]["load"]["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == "/abs/agent-buddy"));
        assert!(v["plugins"]["entries"].get("agent-buddy").is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn enabled_file_add_remove_is_idempotent() {
        let p = tmp("enabled");
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, "other-plugin\n").unwrap();
        ensure_line_in_file(&p, "agent-buddy").unwrap();
        ensure_line_in_file(&p, "agent-buddy").unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert_eq!(s.lines().filter(|l| *l == "agent-buddy").count(), 1);
        assert!(s.lines().any(|l| l == "other-plugin"));
        remove_line_from_file(&p, "agent-buddy").unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(!s.lines().any(|l| l == "agent-buddy"));
        assert!(s.lines().any(|l| l == "other-plugin"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn bundled_plugin_assets_are_present() {
        assert!(OPENCODE_INDEX_MJS.contains("export default"));
        assert!(OPENCODE_PACKAGE_JSON.contains("\"type\""));
        assert!(OPENCLAW_INDEX_JS.contains("register(api)"));
        assert!(OPENCLAW_MANIFEST_JSON.contains("onStartup"));
        assert!(PI_INDEX_TS.contains("export default function"));
        assert!(HERMES_INIT_PY.contains("def register("));
        assert!(HERMES_PLUGIN_YAML.contains("hooks:"));
    }

    #[test]
    fn codex_features_flag_is_set_and_idempotent() {
        // Empty config → a fresh [features] table.
        assert_eq!(set_features_hooks_true(""), "[features]\nhooks = true\n");
        // Already enabled → byte-identical no-op.
        let on = "[features]\nhooks = true\n";
        assert_eq!(set_features_hooks_true(on), on);
        // Existing [features] with other keys → key inserted, rest preserved.
        let other = "[features]\nweb_search = true\n";
        let out = set_features_hooks_true(other);
        assert!(out.contains("hooks = true"));
        assert!(out.contains("web_search = true"));
        // A `hooks = false` in [features] is flipped to true, not duplicated.
        let off = "[model]\nname = \"o1\"\n\n[features]\nhooks = false\n";
        let out = set_features_hooks_true(off);
        assert!(out.contains("hooks = true"));
        assert!(!out.contains("hooks = false"));
        assert_eq!(out.matches("hooks =").count(), 1);
        // Unrelated config with no [features] → table appended, original kept.
        let cfg = "[model]\nname = \"gpt-5\"\n";
        let out = set_features_hooks_true(cfg);
        assert!(out.contains("name = \"gpt-5\""));
        assert!(out.contains("[features]\nhooks = true"));
        // A `hooks` key OUTSIDE [features] (different table) is left alone.
        let elsewhere = "[other]\nhooks = false\n";
        let out = set_features_hooks_true(elsewhere);
        assert!(out.contains("[other]\nhooks = false"));
        assert!(out.contains("[features]\nhooks = true"));
    }

    #[test]
    fn install_uninstall_json_roundtrips() {
        // Write our hooks into a temp settings file, then strip them; the user's
        // own hook must survive both.
        let dir = std::env::temp_dir().join(format!("ab-setup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": "my-own-thing" } ] } ] }
            }))
            .unwrap(),
        )
        .unwrap();

        let profile: AgentProfile = serde_json::from_str(
            crate::agent::DEFAULT_PROFILES
                .iter()
                .find(|(id, _)| *id == "gemini-cli")
                .unwrap()
                .1,
        )
        .unwrap();

        // Point a temp profile at our temp file.
        let mut p = profile.clone();
        p.install.target_path = crate::agent::TargetPath {
            home_rel: path.to_string_lossy().into_owned(),
            from_config_dir: false,
            home_env: None,
        };
        // Absolute path: resolve_target joins home + abs → abs on Unix.
        write_hooks_json(&p, "/x/agent-buddy").unwrap();
        let after_install = std::fs::read_to_string(&path).unwrap();
        assert!(after_install.contains("--agent gemini-cli"));
        assert!(after_install.contains("my-own-thing")); // user hook preserved

        strip_our_hooks_json(&path, "/x/agent-buddy").unwrap();
        let after_strip = std::fs::read_to_string(&path).unwrap();
        assert!(!after_strip.contains("agent-buddy"));
        assert!(after_strip.contains("my-own-thing")); // user hook still there

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ab-hooktest-{}-{name}.json", std::process::id()))
    }

    /// Wiring writes once, is a no-op the second time, and reconciles a changed
    /// binary path to a single entry per event (no duplicates, new path).
    #[test]
    fn wire_is_idempotent_and_reconciles_path() {
        let p = tmp("idem");
        let _ = std::fs::remove_file(&p);
        assert!(wire_hooks(&p, "/x/agent-buddy", "Bash").unwrap());
        assert!(!wire_hooks(&p, "/x/agent-buddy", "Bash").unwrap());
        assert!(wire_hooks(&p, "/y/agent-buddy", "Bash").unwrap());

        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let hooks = v["hooks"].as_object().unwrap();
        for ev in ["PermissionRequest", "PreToolUse", "SessionStart", "Stop"] {
            let arr = hooks[ev].as_array().unwrap();
            assert_eq!(
                arr.iter().filter(|e| is_ours(e, "")).count(),
                1,
                "{ev} should have exactly one buddy entry"
            );
            let entry = arr.iter().find(|e| is_ours(e, "")).unwrap();
            let cmd = entry["hooks"][0]["command"].as_str().unwrap();
            assert!(cmd.contains("/y/agent-buddy"), "{ev} should use the new path");
        }
        let _ = std::fs::remove_file(&p);
    }

    /// Regression: a pre-existing settings.json whose event key holds a NON-array
    /// value (older schema, hand-edit, another tool's format) must not panic.
    /// Before the fix this `.expect()`'d an array and crash-looped the daemon
    /// (wire_hooks runs on every startup). Now the malformed value is overwritten
    /// with a fresh array carrying our entry.
    #[test]
    fn wire_survives_non_array_event_value() {
        let p = tmp("malformed");
        // PreToolUse is a string, Stop is an object — both shapes we'd otherwise
        // have unwrapped as arrays.
        std::fs::write(
            &p,
            r#"{"hooks":{"PreToolUse":"oops","Stop":{"bad":true}}}"#,
        )
        .unwrap();
        // Must not panic.
        assert!(wire_hooks(&p, "/x/agent-buddy", "Bash").unwrap());
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let hooks = v["hooks"].as_object().unwrap();
        // The clobbered events are now arrays each carrying our entry.
        for ev in ["PreToolUse", "Stop"] {
            let arr = hooks[ev].as_array().expect("event is now an array");
            assert_eq!(arr.iter().filter(|e| is_ours(e, "")).count(), 1);
        }
        let _ = std::fs::remove_file(&p);
    }

    /// The user's own hooks survive both wiring and stripping; only ours move.
    #[test]
    fn wire_and_strip_preserve_user_hooks() {
        let p = tmp("user");
        std::fs::write(
            &p,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo hi"}]}]},"other":1}"#,
        )
        .unwrap();
        wire_hooks(&p, "/x/agent-buddy", "Bash").unwrap();

        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert!(stop.iter().any(|e| e["hooks"][0]["command"] == "echo hi"));
        assert!(stop.iter().any(|e| is_ours(e, "")));
        assert_eq!(v["other"], 1, "unrelated keys untouched");

        assert!(strip_hooks_at(&p).unwrap());
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert!(stop.iter().any(|e| e["hooks"][0]["command"] == "echo hi"));
        assert!(!stop.iter().any(|e| is_ours(e, "")), "ours are gone");
        assert!(v["hooks"].get("PreToolUse").is_none(), "our-only events removed");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn resolve_target_honors_home_env() {
        use crate::agent::TargetPath;
        let var = format!("AB_TEST_HOME_{}", std::process::id());
        let t = TargetPath {
            home_rel: ".copilot/hooks/hooks.json".into(),
            from_config_dir: false,
            home_env: Some(var.clone()),
        };
        // Unset → falls back to $HOME/.copilot/hooks/hooks.json.
        std::env::remove_var(&var);
        assert!(resolve_target(&t)
            .unwrap()
            .ends_with(".copilot/hooks/hooks.json"));
        // Set → the leading `.copilot/` segment is replaced by the env base.
        std::env::set_var(&var, "/tmp/copilot-home");
        assert_eq!(
            resolve_target(&t).unwrap(),
            PathBuf::from("/tmp/copilot-home/hooks/hooks.json")
        );
        std::env::remove_var(&var);
    }

    #[test]
    fn kiro_writer_seeds_name_and_flat_entries() {
        let p = tmp("kiro");
        let _ = std::fs::remove_file(&p);
        let prof = crate::agent::load_profiles()["kiro-cli"].clone();
        write_kiro_agent_at(&p, &prof, "/x/agent-buddy").unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        // Top-level name is seeded from the file stem.
        let stem = p.file_stem().unwrap().to_str().unwrap();
        assert_eq!(v["name"], stem);
        // Entries are flat `{ "command": ... }` (not the nested Claude shape).
        let first_event = prof.install.events.first().unwrap();
        let arr = v["hooks"][first_event].as_array().unwrap();
        assert!(arr.iter().any(|e| e.get("command").is_some()));
        assert!(arr.iter().all(|e| e.get("hooks").is_none()));
        // Idempotent: re-running keeps exactly one of ours.
        write_kiro_agent_at(&p, &prof, "/x/agent-buddy").unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let arr = v["hooks"][first_event].as_array().unwrap();
        assert_eq!(arr.iter().filter(|e| is_ours_cursor(e)).count(), 1);
        // Strip removes ours, leaves name.
        strip_kiro_agent_json(&p).unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["name"], stem);
        assert!(v["hooks"].get(first_event).is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn antigravity_writer_uses_clawd_group_and_wrappers() {
        let p = tmp("antigravity");
        let _ = std::fs::remove_file(&p);
        let prof = crate::agent::load_profiles()["antigravity-cli"].clone();
        write_antigravity_hooks_at(&p, &prof, "/x/agent-buddy").unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let group = v["clawd"].as_object().unwrap();
        // PreInvocation/Stop are flat handler arrays with timeout 10.
        let pre = group["PreInvocation"].as_array().unwrap();
        assert_eq!(pre[0]["type"], "command");
        assert_eq!(pre[0]["timeout"], 10);
        assert!(pre[0].get("matcher").is_none());
        // PostToolUse uses the {matcher,hooks} wrapper.
        let post = group["PostToolUse"].as_array().unwrap();
        assert_eq!(post[0]["matcher"], "*");
        assert_eq!(post[0]["hooks"][0]["timeout"], 10);
        // Idempotent + strip removes the group when empty.
        write_antigravity_hooks_at(&p, &prof, "/x/agent-buddy").unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(
            v["clawd"]["PreInvocation"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| is_ours_antigravity(e))
                .count(),
            1
        );
        strip_antigravity_hooks_json(&p, "/x/agent-buddy").unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert!(v.get("clawd").is_none(), "empty group is dropped");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn antigravity_strip_preserves_coexisting_clawd_install() {
        let p = tmp("antigravity-coexist");
        // A clawd-on-desk install (not ours) in the same group must survive.
        std::fs::write(
            &p,
            r#"{"clawd":{"Stop":[{"type":"command","command":"node clawd-hook.js Stop"}]}}"#,
        )
        .unwrap();
        let prof = crate::agent::load_profiles()["antigravity-cli"].clone();
        write_antigravity_hooks_at(&p, &prof, "/x/agent-buddy").unwrap();
        strip_antigravity_hooks_json(&p, "/x/agent-buddy").unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let stop = v["clawd"]["Stop"].as_array().unwrap();
        assert!(stop.iter().any(|e| e["command"] == "node clawd-hook.js Stop"));
        assert!(!stop.iter().any(is_ours_antigravity));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn qwen_writer_adds_matcher_and_timeouts() {
        let p = tmp("qwen");
        let _ = std::fs::remove_file(&p);
        let prof = crate::agent::load_profiles()["qwen-code"].clone();
        write_hooks_json_at(&p, &prof, "/x/agent-buddy").unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let hooks = v["hooks"].as_object().unwrap();
        // matcher:"*" on tool events; PermissionRequest gets the 600s timeout.
        let pre = &hooks["PreToolUse"].as_array().unwrap()[0];
        assert_eq!(pre["matcher"], "*");
        assert_eq!(pre["hooks"][0]["timeout"], 30000);
        let perm = &hooks["PermissionRequest"].as_array().unwrap()[0];
        assert_eq!(perm["matcher"], "*");
        assert_eq!(perm["hooks"][0]["timeout"], 600000);
        // UserPromptSubmit/Stop are matcherless.
        assert!(hooks["UserPromptSubmit"].as_array().unwrap()[0]
            .get("matcher")
            .is_none());
        assert!(hooks["Stop"].as_array().unwrap()[0].get("matcher").is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn copilot_writer_sets_top_level_version() {
        let p = tmp("copilot");
        let _ = std::fs::remove_file(&p);
        let prof = crate::agent::load_profiles()["copilot-cli"].clone();
        write_hooks_json_at(&p, &prof, "/x/agent-buddy").unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert!(v["hooks"]["sessionStart"].is_array());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn strip_our_hooks_json_is_noop_on_untouched_file() {
        let p = tmp("noop");
        let body = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo hi"}]}]}}"#;
        std::fs::write(&p, body).unwrap();
        let before = std::fs::read_to_string(&p).unwrap();
        strip_our_hooks_json(&p, "/x/agent-buddy").unwrap();
        // No buddy entry present → file is left byte-identical (no reformat).
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
        let _ = std::fs::remove_file(&p);
    }
}
