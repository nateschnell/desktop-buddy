//! The `agent-buddy hook <event>` subcommand.
//!
//! Claude Code spawns this on each hook event, passing the event payload as
//! JSON on stdin. We normalize it into a [`HookEvent`], relay it to the
//! running daemon over loopback, and — for `PermissionRequest` — translate the
//! device's decision into a Claude Code permission verdict on stdout.
//!
//! We gate on `PermissionRequest`, not `PreToolUse`, on purpose: Claude raises
//! `PermissionRequest` ONLY when it would actually prompt the user, so the
//! device shows an accept/reject screen exactly when the real session does —
//! never for auto-approved, allow-listed, bypass-mode, or autonomous subagent
//! tool calls (all of which still fire `PreToolUse`). `PreToolUse` is kept
//! purely as a telemetry heartbeat so the dashboard tracks the turn mid-flight.
//!
//! Guiding rule: a hook must NEVER wedge Claude Code. If the daemon is down,
//! no device is connected, or anything times out, we cleanly defer to Claude
//! Code's normal flow (`"ask"`) and exit 0.

use crate::ipc::{self, Endpoint, HookEvent, HookRequest, HookResponse};
use crate::protocol::MAX_LINE_BYTES;
use anyhow::Result;
use serde_json::Value;
use std::io::Read;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// Round-trip budget for a permission decision. Stays well under typical hook
/// timeouts so Claude Code never kills us mid-wait.
const PRETOOL_TIMEOUT: Duration = Duration::from_secs(25);
/// State events are fire-and-confirm; they shouldn't add latency.
const STATE_TIMEOUT: Duration = Duration::from_secs(2);
/// Telemetry pings ride the tool-call hot path, so the budget is tight: a
/// slow/absent daemon must never stall the tool. Loopback ack is sub-ms; this
/// only bounds a hung daemon, and the result is discarded regardless.
const TELEMETRY_TIMEOUT: Duration = Duration::from_millis(400);

pub async fn run(event_name: &str, agent: Option<&str>) -> Result<()> {
    // Read the hook payload from stdin (may be empty for some events).
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).ok();
    let payload: Value = serde_json::from_str(raw.trim()).unwrap_or(Value::Null);

    // Claude Code (the default, no `--agent`) keeps the byte-identical legacy
    // path below, including transcript-derived token/context telemetry. Any
    // other harness is profile-driven: its event name maps through the loaded
    // profile's `event_map` to one of the same IPC events.
    let is_claude = matches!(agent, None | Some("claude-code"));
    let event = if is_claude {
        let canonical = canonical_event_name(event_name, &payload);
        match build_event(&canonical, &payload) {
            Some(e) => e,
            None => return Ok(()), // event we don't model — no-op, exit 0
        }
    } else {
        match build_event_for_agent(agent.unwrap(), event_name, &payload) {
            Some(e) => e,
            None => return Ok(()),
        }
    };

    // The gate. Only `PermissionRequest` decides a tool's fate on the device;
    // everything else is recorded fire-and-confirm with no verdict.
    if matches!(event, HookEvent::PermissionRequest { .. }) {
        // One exception that survives the move to PermissionRequest: buddy's own
        // repo work can't be gated through the device — you can't tap-approve a
        // flash that's reflashing the very device showing the prompt. Emit no
        // verdict so it falls through to Claude's normal terminal prompt.
        if targets_buddy_repo(&payload) {
            return Ok(());
        }
        let response = relay(event, PRETOOL_TIMEOUT).await.unwrap_or(HookResponse::Defer {
            reason: "daemon unreachable".into(),
        });
        emit_permission_verdict(&response);
        return Ok(());
    }

    // Non-gating events: state changes and the PreToolUse telemetry heartbeat.
    // Telemetry rides the tool-call hot path, so it gets the tightest budget; a
    // slow/absent daemon must never stall the call. The result is discarded
    // either way — these never produce a Claude Code verdict.
    let timeout = if matches!(event, HookEvent::Telemetry { .. }) {
        TELEMETRY_TIMEOUT
    } else {
        STATE_TIMEOUT
    };
    let _ = relay(event, timeout).await;
    Ok(())
}

/// Build an IPC event for a non-Claude harness, profile-driven. Loads the
/// agent's profile and maps the harness event name through its `event_map`
/// (see [`crate::ingest`]). Returns `None` for an unknown agent or unmapped
/// event (a clean no-op, exit 0 — a hook must never wedge the harness).
fn build_event_for_agent(agent: &str, event_name: &str, payload: &Value) -> Option<HookEvent> {
    let profiles = crate::agent::load_profiles();
    let profile = profiles.get(agent)?;
    let name = crate::ingest::event_name(event_name, payload);
    // Reclassify failure/status events by payload inspection (Cursor stop=error,
    // Antigravity PostToolUse/Stop failure) before the static event_map lookup.
    let name = crate::ingest::reclassify_event(profile, &name, payload);
    crate::ingest::map_hook_event(profile, &name, payload)
}

/// Claude Code passes the event name both as our CLI arg and (usually) in the
/// payload's `hook_event_name`. Prefer the payload when present.
fn canonical_event_name(arg: &str, payload: &Value) -> String {
    payload
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .unwrap_or(arg)
        .to_string()
}

fn session_id(payload: &Value) -> String {
    payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string()
}

fn build_event(name: &str, payload: &Value) -> Option<HookEvent> {
    let session_id = session_id(payload);
    let cwd = cwd(payload);
    match name {
        "SessionStart" => Some(HookEvent::SessionStart { session_id, cwd }),
        "SessionEnd" => Some(HookEvent::SessionEnd { session_id }),
        "UserPromptSubmit" => Some(HookEvent::UserPromptSubmit { session_id, cwd }),
        "Notification" => Some(HookEvent::Notification {
            session_id,
            message: payload
                .get("message")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }),
        "Stop" | "SubagentStop" => {
            let (session_total_tokens, model, ctx_tokens) = telemetry_fields(payload);
            // A stop-hook continuation (`stop_hook_active`) is not a real turn
            // completion — suppress the celebrate so we don't fire it twice.
            let continuation = payload
                .get("stop_hook_active")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(HookEvent::Stop {
                session_id,
                session_total_tokens,
                summary: last_assistant_summary(payload),
                final_turn: name == "Stop" && !continuation,
                model,
                ctx_tokens,
                cwd,
            })
        }
        // PreToolUse fires for every matched tool call — including ones Claude
        // auto-approves and autonomous subagent calls — so it no longer gates.
        // It's purely a telemetry heartbeat that keeps the device's token /
        // context readout tracking the turn mid-flight. The actual approve/deny
        // gate is `PermissionRequest`.
        "PreToolUse" => {
            let (session_total_tokens, model, ctx_tokens) = telemetry_fields(payload);
            Some(HookEvent::Telemetry {
                session_id,
                session_total_tokens,
                model,
                ctx_tokens,
                cwd,
            })
        }
        // Claude raises PermissionRequest ONLY when it would actually prompt the
        // user — never for auto-approved, allow-listed, bypass-mode, or
        // autonomous subagent calls — so gating on it makes the device mirror
        // the real session's prompts exactly.
        "PermissionRequest" => {
            let tool = payload
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("tool")
                .to_string();
            Some(HookEvent::PermissionRequest {
                session_id,
                hint: tool_hint(&tool, payload.get("tool_input")),
                tool,
                mode: permission_mode(payload).to_string(),
                cwd,
            })
        }
        _ => None,
    }
}

/// The session's working directory, if Claude provided it.
fn cwd(payload: &Value) -> String {
    payload
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Claude's permission mode for this call (`default` when absent).
fn permission_mode(payload: &Value) -> &str {
    payload
        .get("permission_mode")
        .and_then(Value::as_str)
        .unwrap_or("default")
}

/// Build a short human hint from the tool input for the device screen.
fn tool_hint(tool: &str, input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    let pick = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| input.get(*k).and_then(|v| v.as_str()))
            .map(str::to_string)
    };
    let hint = match tool {
        "Bash" => pick(&["command"]),
        "Write" | "Edit" | "Read" | "NotebookEdit" => pick(&["file_path", "notebook_path"]),
        "WebFetch" | "WebSearch" => pick(&["url", "query"]),
        _ => None,
    }
    .or_else(|| pick(&["command", "file_path", "url", "query", "path"]))
    .unwrap_or_else(|| input.to_string());
    // Firmware truncates, but keep the wire small.
    hint.chars().take(80).collect()
}

/// The buddy project this binary was built into. The hook runs as
/// `<root>/bridge/target/release/agent-buddy`, so the repo root is 4 ancestors
/// up. `None` if the binary was moved somewhere that doesn't match — in which
/// case we simply don't bypass (gate as normal), the safe way to fail.
fn buddy_project_root() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()?
        .ancestors()
        .nth(4)
        .map(Path::to_path_buf)
}

/// Does this tool call operate on the buddy project — via its cwd, a target
/// file, or a shell command that references the repo path? Such calls are
/// firmware / bridge dev work on the buddy itself and must not be gated
/// through it (you can't tap-approve a flash that's reflashing the device).
fn targets_buddy_repo(payload: &Value) -> bool {
    match buddy_project_root().as_deref().and_then(Path::to_str) {
        Some(root) => payload_targets(payload, root),
        None => false,
    }
}

/// Pure matcher behind [`targets_buddy_repo`], split out so it can be tested
/// with a fixed `root` (the real root is derived from the running binary).
fn payload_targets(payload: &Value, root: &str) -> bool {
    // "In the repo" = the root itself or a path beneath it. (Plain starts_with
    // would also match a sibling like `<root>-old`; require the `/` boundary.)
    let under = |p: &str| p == root || p.starts_with(&format!("{root}/"));

    if payload
        .get("cwd")
        .and_then(Value::as_str)
        .is_some_and(under)
    {
        return true;
    }
    let Some(input) = payload.get("tool_input") else {
        return false;
    };
    for key in ["file_path", "notebook_path", "path"] {
        if input.get(key).and_then(Value::as_str).is_some_and(under) {
            return true;
        }
    }
    // Shell commands carry no structured path; match the repo root anywhere in
    // the command text (e.g. `cd <root>/firmware && pio run -t upload`).
    input
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains(root))
}

/// The transcript-derived telemetry triple — cumulative output tokens, current
/// model id, and last-turn context size — shared by Stop and the PreToolUse
/// piggyback so both report identical numbers. Each reads the most recent
/// finalized assistant message, so mid-turn (after the message that requested
/// this tool is written) it reflects the turn so far, not just the prior turn.
fn telemetry_fields(payload: &Value) -> (u64, String, u64) {
    (
        session_total_output_tokens(payload),
        last_assistant_model(payload).unwrap_or_default(),
        last_turn_context_tokens(payload),
    )
}

/// Best-effort: sum `usage.output_tokens` across every assistant message in
/// the transcript — i.e. the session's cumulative output tokens so far. The
/// daemon diffs this against what it last saw for the session, so it's safe to
/// recompute on every Stop/SubagentStop without double-counting.
fn session_total_output_tokens(payload: &Value) -> u64 {
    let Some(transcript) = read_transcript(payload) else {
        return 0;
    };
    transcript
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("assistant"))
        .filter_map(|v| {
            v.get("message")
                .and_then(|m| m.get("usage"))
                .and_then(|u| u.get("output_tokens"))
                .and_then(|n| n.as_u64())
        })
        .sum()
}

/// The model id of the most recent assistant message in the transcript
/// (`message.model`). Used as the session's current model.
fn last_assistant_model(payload: &Value) -> Option<String> {
    let transcript = read_transcript(payload)?;
    transcript
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("assistant"))
        .and_then(|v| {
            v.get("message")
                .and_then(|m| m.get("model"))
                .and_then(|s| s.as_str())
                .map(str::to_string)
        })
}

/// The context size of the most recent turn: the most recent assistant
/// message's `usage` summed across input + cache + output tokens — i.e. how
/// full the context window is after that turn. 0 if unavailable.
fn last_turn_context_tokens(payload: &Value) -> u64 {
    let Some(transcript) = read_transcript(payload) else {
        return 0;
    };
    let usage = transcript
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("assistant"))
        .and_then(|v| v.get("message")?.get("usage").cloned());
    let Some(usage) = usage else { return 0 };
    let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    n("input_tokens")
        + n("cache_read_input_tokens")
        + n("cache_creation_input_tokens")
        + n("output_tokens")
}

/// First line of the most recent assistant text block, for the ticker.
fn last_assistant_summary(payload: &Value) -> Option<String> {
    let transcript = read_transcript(payload)?;
    let msg = transcript
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("assistant"))?;
    let content = msg.get("message")?.get("content")?.as_array()?;
    let text = content
        .iter()
        .find_map(|b| b.get("text").and_then(|t| t.as_str()))?;
    // Byte-bounded, not char-bounded: the daemon folds this into a heartbeat
    // capped by MAX_LINE_BYTES, and a single multibyte line that overruns the
    // firmware buffer drops the *whole* heartbeat. 60 bytes is plenty for the
    // ticker and leaves the line budget to the daemon's final byte-shedding.
    Some(truncate_bytes(text.lines().next().unwrap_or(""), 60))
}

/// Truncate `s` to at most `max` UTF-8 bytes, cutting on a char boundary. No
/// ellipsis — this feeds a fixed-width ticker where the cut is invisible noise.
/// `max` is assumed comfortably under [`MAX_LINE_BYTES`]; the assert documents
/// the invariant for future callers.
fn truncate_bytes(s: &str, max: usize) -> String {
    debug_assert!(max < MAX_LINE_BYTES);
    if s.len() <= max {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= max)
        .last()
        .unwrap_or(0);
    s[..end].to_string()
}

fn read_transcript(payload: &Value) -> Option<String> {
    let path = payload.get("transcript_path").and_then(|v| v.as_str())?;
    // Read only the tail: a long session's transcript can be many MB, but every
    // consumer here scans for the *most recent* assistant message (model, token
    // usage, summary) — all of which live in the final lines. Bounding the read
    // caps hot-path memory/latency on every Stop/PreToolUse.
    read_tail(path, 256 * 1024)
}

/// Read at most the last `max` bytes of a file as (lossy) UTF-8. When the file
/// is larger than `max` we drop the partial first line so callers only ever see
/// whole JSONL records.
fn read_tail(path: &str, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        if let Some(nl) = text.find('\n') {
            text.drain(..=nl); // discard the line we started in the middle of
        }
    }
    Some(text)
}

/// Connect to the daemon, send the event, await the response (bounded).
async fn relay(event: HookEvent, timeout: Duration) -> Result<HookResponse> {
    let ep = load_endpoint()?;
    let fut = async {
        let stream = TcpStream::connect(("127.0.0.1", ep.port)).await?;
        let (read_half, mut write_half) = stream.into_split();

        let mut bytes = serde_json::to_vec(&HookRequest {
            token: ep.token,
            event,
        })?;
        bytes.push(b'\n');
        write_half.write_all(&bytes).await?;
        write_half.flush().await?;

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        Ok::<HookResponse, anyhow::Error>(serde_json::from_str(line.trim())?)
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(r) => r,
        Err(_) => Ok(HookResponse::Defer {
            reason: "timed out".into(),
        }),
    }
}

/// The hook reads the published endpoint and then connects straight away, so
/// the connect itself is the liveness check — no extra probe on the hot path.
/// Delegates to the centralized reader so the file shape lives in one place.
fn load_endpoint() -> Result<Endpoint> {
    ipc::read_endpoint()
}

/// Print the device's decision in Claude Code's `PermissionRequest` output
/// shape. Note the nesting differs from `PreToolUse`: the verdict is
/// `decision.behavior` (`"allow"`/`"deny"`), not a flat `permissionDecision`,
/// and there is no `"ask"` — the prompt only reached us because Claude was
/// already going to ask.
///
/// Only an explicit device decision yields a verdict. Every "no decision"
/// outcome — daemon down, no buddy connected, timed out — emits NOTHING and
/// exits 0, so Claude Code's own permission prompt is shown as usual. Emitting
/// a verdict there would either silently auto-answer a prompt the buddy isn't
/// driving, or (with a stale daemon) add friction we want to avoid.
fn emit_permission_verdict(resp: &HookResponse) {
    let behavior = match resp {
        HookResponse::Decision { allow: true } => "allow",
        HookResponse::Decision { allow: false } => "deny",
        HookResponse::Defer { .. } | HookResponse::Ack | HookResponse::Error { .. } => return,
    };
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": { "behavior": behavior },
        }
    });
    println!("{out}");
}

#[cfg(test)]
mod tests {
    use super::{
        cwd, last_assistant_model, last_turn_context_tokens, payload_targets, permission_mode,
        truncate_bytes,
    };
    use serde_json::json;

    const ROOT: &str = "/Users/x/buddy/claude-cyd-buddy";

    /// Write a transcript JSONL to a unique temp path and return a payload that
    /// points at it. `tag` makes the filename unique per test.
    fn payload_with_transcript(tag: &str, lines: &[serde_json::Value]) -> serde_json::Value {
        let body: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let path = std::env::temp_dir().join(format!("buddy-hook-test-{tag}.jsonl"));
        std::fs::write(&path, body).unwrap();
        json!({ "transcript_path": path.to_str().unwrap() })
    }

    #[test]
    fn extracts_model_and_context_from_transcript() {
        let p = payload_with_transcript(
            "model-ctx",
            &[
                json!({"type": "user", "message": {"content": "hi"}}),
                json!({"type": "assistant", "message": {
                    "model": "claude-opus-4-8",
                    "content": [{"type": "text", "text": "older"}],
                    "usage": {"input_tokens": 10, "output_tokens": 5}
                }}),
                json!({"type": "assistant", "message": {
                    "model": "claude-sonnet-4-6",
                    "content": [{"type": "text", "text": "newest"}],
                    "usage": {
                        "input_tokens": 1000,
                        "cache_read_input_tokens": 4000,
                        "cache_creation_input_tokens": 500,
                        "output_tokens": 200
                    }
                }}),
            ],
        );
        // Latest assistant message wins.
        assert_eq!(
            last_assistant_model(&p).as_deref(),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(last_turn_context_tokens(&p), 1000 + 4000 + 500 + 200);
    }

    #[test]
    fn missing_transcript_is_zero_and_none() {
        let p = json!({});
        assert_eq!(last_assistant_model(&p), None);
        assert_eq!(last_turn_context_tokens(&p), 0);
    }

    #[test]
    fn cwd_and_mode_accessors() {
        let p = json!({ "cwd": "/home/me/proj", "permission_mode": "plan" });
        assert_eq!(cwd(&p), "/home/me/proj");
        assert_eq!(permission_mode(&p), "plan");
        // Absent mode defaults to "default"; absent cwd is empty.
        assert_eq!(permission_mode(&json!({})), "default");
        assert_eq!(cwd(&json!({})), "");
    }

    #[test]
    fn pretool_is_telemetry_only_and_permission_request_gates() {
        use crate::ipc::HookEvent;
        let p = json!({
            "session_id": "s1",
            "tool_name": "Bash",
            "tool_input": { "command": "ls -la" },
            "permission_mode": "default",
            "cwd": "/proj"
        });
        // PreToolUse fires for every matched call (incl. auto-approved and
        // subagent ones), so it must NOT gate — it maps to a telemetry refresh.
        assert!(matches!(
            super::build_event("PreToolUse", &p),
            Some(HookEvent::Telemetry { .. })
        ));
        // PermissionRequest — Claude's "I'm actually prompting" signal — is the
        // gate, carrying the tool + hint the device displays.
        match super::build_event("PermissionRequest", &p) {
            Some(HookEvent::PermissionRequest { tool, hint, .. }) => {
                assert_eq!(tool, "Bash");
                assert_eq!(hint, "ls -la");
            }
            other => panic!("expected PermissionRequest, got {other:?}"),
        }
    }

    #[test]
    fn matches_target_file_in_repo() {
        let p = json!({ "tool_input": { "file_path": format!("{ROOT}/firmware/src/main.cpp") } });
        assert!(payload_targets(&p, ROOT));
    }

    #[test]
    fn matches_command_referencing_repo() {
        let p = json!({ "tool_input": { "command": format!("cd {ROOT}/firmware && pio run -t upload") } });
        assert!(payload_targets(&p, ROOT));
    }

    #[test]
    fn matches_cwd_in_repo() {
        let p = json!({ "cwd": ROOT, "tool_input": { "command": "ls" } });
        assert!(payload_targets(&p, ROOT));
    }

    #[test]
    fn ignores_paths_outside_repo() {
        let p = json!({ "tool_input": { "file_path": "/etc/hosts" } });
        assert!(!payload_targets(&p, ROOT));
    }

    #[test]
    fn does_not_match_sibling_prefix() {
        // `<root>-old` shares the string prefix but is a different directory.
        let p = json!({ "tool_input": { "file_path": format!("{ROOT}-old/secret") } });
        assert!(!payload_targets(&p, ROOT));
    }

    #[test]
    fn no_signal_does_not_match() {
        let p = json!({ "tool_input": { "command": "echo hello" } });
        assert!(!payload_targets(&p, ROOT));
    }

    #[test]
    fn truncate_bytes_cuts_on_char_boundary_under_budget() {
        // ASCII: trimmed to the byte budget.
        assert_eq!(truncate_bytes("hello world", 5), "hello");
        // Short string passes through untouched.
        assert_eq!(truncate_bytes("hi", 60), "hi");
        // Multibyte: never split a codepoint, and never exceed the budget.
        let s = "é".repeat(40); // 80 bytes
        let out = truncate_bytes(&s, 60);
        assert!(out.len() <= 60);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        // The boundary cut lands on a whole 'é' (2 bytes each) → 30 of them.
        assert_eq!(out.chars().count(), 30);
    }
}
