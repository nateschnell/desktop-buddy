//! Generic, profile-driven event ingestion shared by all harnesses.
//!
//! Three mechanisms feed the daemon — stdin hooks (`hook.rs`), JSONL log-poll
//! (Codex), and a loopback HTTP listener (plugin/extension harnesses) — but they
//! all converge here: a harness event name → the profile's normalized state →
//! one of the existing [`HookEvent`] variants the daemon already understands.
//! Keeping the translation in one place means `daemon.rs::handle_hook` is
//! untouched and adding a harness stays a data change (its profile), not new code.
//!
//! The functions here are pure (no I/O), so the per-harness mapping is unit
//! tested without spawning anything. The daemon owns the actual tasks (file
//! tailing, the TCP listener) and the channel they post into.

use crate::agent::{AgentProfile, NormState};
use crate::ipc::HookEvent;
use serde_json::Value;

/// A pending permission prompt extracted from a harness payload.
#[derive(Debug, Clone, Default)]
pub struct PromptInfo {
    pub tool: String,
    pub hint: String,
    pub mode: String,
}

/// Collapse a normalized state into one of the daemon's existing [`HookEvent`]
/// variants. This is the single seam between the rich normalized vocabulary and
/// the device's `(running, waiting, completed, prompt?)` model. `permits` is the
/// agent's permission-approval capability — only then can `attention` become a
/// device gate; otherwise it's a finished-turn nudge.
pub fn normalized_to_hook_event(
    norm: NormState,
    session_id: String,
    cwd: String,
    prompt: Option<PromptInfo>,
    permits: bool,
) -> Option<HookEvent> {
    use NormState::*;
    let stop = |final_turn: bool, summary: Option<String>| HookEvent::Stop {
        session_id: session_id.clone(),
        session_total_tokens: 0,
        summary,
        final_turn,
        model: String::new(),
        ctx_tokens: 0,
        cwd: cwd.clone(),
    };
    Some(match norm {
        // Turn starting / in flight → the session is "running".
        Thinking | Working | Juggling | Sweeping | Carrying => HookEvent::UserPromptSubmit {
            session_id,
            cwd,
        },
        // Idle/error end the turn without a celebrate pulse.
        Idle => stop(false, None),
        Error => stop(false, Some("error".into())),
        // Real turn completion → celebrate.
        CodexTurnEnd => stop(true, None),
        // Session over.
        Sleeping => HookEvent::SessionEnd { session_id },
        // Needs the user. With a real permission prompt (and the capability) this
        // is a device gate; otherwise it's a finished-turn nudge (celebrate).
        Attention => match (permits, prompt) {
            (true, Some(p)) if !p.tool.is_empty() => HookEvent::PermissionRequest {
                session_id,
                tool: p.tool,
                hint: p.hint,
                mode: p.mode,
                cwd,
            },
            _ => stop(true, None),
        },
        Notification => HookEvent::Notification {
            session_id,
            message: prompt.map(|p| p.hint).filter(|s| !s.is_empty()),
        },
    })
}

/// The harness's event name for this hook: prefer the payload's own field (some
/// harnesses echo it), else the CLI arg the install wired (`hook <EVENT>`).
pub fn event_name(arg: &str, payload: &Value) -> String {
    for key in ["hook_event_name", "hookEventName", "event", "eventName"] {
        if let Some(s) = payload.get(key).and_then(Value::as_str) {
            return s.to_string();
        }
    }
    arg.to_string()
}

/// Best-effort session id across harness payload shapes.
pub fn session_id(payload: &Value) -> String {
    for key in ["session_id", "sessionId", "conversationId", "conversation_id", "id"] {
        if let Some(s) = payload.get(key).and_then(Value::as_str) {
            return s.to_string();
        }
    }
    "default".to_string()
}

/// Best-effort working directory across harness payload shapes.
pub fn cwd(payload: &Value) -> String {
    for key in ["cwd", "workspaceRoot", "workspace_root", "workspace", "projectRoot", "project_root"]
    {
        if let Some(s) = payload.get(key).and_then(Value::as_str) {
            return s.to_string();
        }
    }
    // Antigravity nested PascalCase (preferred when present).
    if let Some(s) = payload
        .get("toolCall")
        .and_then(|t| t.get("args"))
        .and_then(|a| a.get("Cwd"))
        .and_then(Value::as_str)
    {
        return s.to_string();
    }
    // Array fallbacks: Antigravity workspacePaths[], Cursor workspace_roots[].
    for key in ["workspacePaths", "workspace_roots", "workspaceRoots"] {
        if let Some(s) = payload
            .get(key)
            .and_then(Value::as_array)
            .and_then(|a| a.iter().find_map(Value::as_str))
        {
            return s.to_string();
        }
    }
    String::new()
}

/// Extract a tool name + short hint from a harness payload, for a permission
/// prompt. Tries the common key spellings; the hint is byte-bounded small.
pub fn prompt_info(payload: &Value, mode_default: &str) -> PromptInfo {
    let tool = ["tool_name", "toolName", "tool"]
        .iter()
        .find_map(|k| payload.get(*k).and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let input = payload
        .get("tool_input")
        .or_else(|| payload.get("toolInput"))
        .or_else(|| payload.get("input"));
    let hint = input
        .and_then(|i| {
            ["command", "file_path", "path", "url", "query", "description"]
                .iter()
                .find_map(|k| i.get(*k).and_then(Value::as_str))
                .map(str::to_string)
        })
        // No tool-input hint: fall back to a notification/message field so a
        // Notification surfaces its human text instead of an empty hint.
        .or_else(|| {
            ["message", "notification", "body", "text"]
                .iter()
                .find_map(|k| payload.get(*k).and_then(Value::as_str))
                .map(str::to_string)
        })
        .unwrap_or_default();
    let mode = payload
        .get("permission_mode")
        .or_else(|| payload.get("permissionMode"))
        .and_then(Value::as_str)
        .unwrap_or(mode_default)
        .to_string();
    PromptInfo {
        tool,
        hint: hint.chars().take(80).collect(),
        mode,
    }
}

/// Synthesize a failure/status-specific event name by inspecting the payload,
/// gated on the profile's `stdin_format`. The event_map already declares the
/// synthesized names (`PostToolUseFailure`/`StopFailure`/`stopFailure`), so the
/// existing name→state pipeline consumes them. Returns the original name when no
/// reclassification applies. Mirrors the reference harness shims, which the
/// static event_map can't express (the lookup never sees these synthetic names).
pub fn reclassify_event(profile: &AgentProfile, name: &str, payload: &Value) -> String {
    use crate::agent::StdinFormat::*;
    // Truthy = present and not null/false/"" (mirrors the reference hasError).
    let truthy = |k: &str| match payload.get(k) {
        Some(Value::Null) | None => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    };
    match profile.install.stdin_format {
        AntigravityHookJson => match name {
            "PostToolUse" if truthy("error") => "PostToolUseFailure".to_string(),
            "Stop" => {
                let term_failed = payload
                    .get("terminationReason")
                    .and_then(Value::as_str)
                    .map(|s| {
                        let s = s.to_ascii_lowercase();
                        s.contains("error") || s.contains("failed") || s.contains("failure")
                    })
                    .unwrap_or(false);
                if truthy("error") || term_failed {
                    "StopFailure".to_string()
                } else if payload.get("fullyIdle").and_then(Value::as_bool) == Some(false) {
                    // A non-idle Stop is not a real end-of-turn → keep working.
                    "PostToolUse".to_string()
                } else {
                    name.to_string()
                }
            }
            _ => name.to_string(),
        },
        CursorHookJson => match name {
            "stop" if payload.get("status").and_then(Value::as_str) == Some("error") => {
                "stopFailure".to_string()
            }
            _ => name.to_string(),
        },
        _ => name.to_string(),
    }
}

/// Map a single harness hook event (by name) to a [`HookEvent`], using the
/// profile's `event_map`. Returns `None` for an unmapped event (a no-op).
pub fn map_hook_event(profile: &AgentProfile, name: &str, payload: &Value) -> Option<HookEvent> {
    let norm = *profile.event_map.get(name)?;
    let prompt = matches!(norm, NormState::Attention | NormState::Notification)
        .then(|| prompt_info(payload, "default"));
    normalized_to_hook_event(
        norm,
        session_id(payload),
        cwd(payload),
        prompt,
        profile.capabilities.permission_approval,
    )
}

/// Classify one JSONL log line (Codex rollout) into a normalized state via the
/// profile's `log_event_map`. The map keys are `"<top>:<inner>"` (e.g.
/// `"event_msg:task_started"`, `"response_item:function_call"`) or a bare top
/// type (`"session_meta"`). Returns `None` for an unmapped or unparseable line.
/// Returns `(normalized state, cwd)`. The cwd is recovered from a `session_meta`
/// line's `payload.cwd` (only present there) so the log-poll caller can label
/// the session; `None` on any other line.
pub fn classify_log_line(profile: &AgentProfile, line: &str) -> (Option<NormState>, Option<String>) {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return (None, None);
    };
    let Some(top) = v.get("type").and_then(Value::as_str) else {
        return (None, None);
    };
    // The session_meta line carries the session cwd under `payload`.
    let line_cwd = if top == "session_meta" {
        v.get("payload")
            .map(cwd)
            .filter(|s| !s.is_empty())
    } else {
        None
    };
    // Bare top-type key (e.g. session_meta).
    if let Some(n) = profile.log_event_map.get(top) {
        return (Some(*n), line_cwd);
    }
    // Nested type under payload/msg → "top:inner".
    let inner = v
        .get("payload")
        .or_else(|| v.get("msg"))
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str);
    let norm = inner.and_then(|inner| {
        let key = format!("{top}:{inner}");
        profile.log_event_map.get(&key).copied()
    });
    (norm, line_cwd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::load_profiles;
    use serde_json::json;

    #[test]
    fn working_state_runs_the_session() {
        let ev = normalized_to_hook_event(
            NormState::Working,
            "s1".into(),
            "/p".into(),
            None,
            false,
        );
        assert!(matches!(ev, Some(HookEvent::UserPromptSubmit { .. })));
    }

    #[test]
    fn attention_gates_only_with_permission_and_a_tool() {
        // No capability → finished-turn Stop, not a gate.
        let ev = normalized_to_hook_event(
            NormState::Attention,
            "s1".into(),
            "/p".into(),
            Some(PromptInfo { tool: "Bash".into(), hint: "ls".into(), mode: "default".into() }),
            false,
        );
        assert!(matches!(ev, Some(HookEvent::Stop { final_turn: true, .. })));
        // Capability + a real tool → a device gate.
        let ev = normalized_to_hook_event(
            NormState::Attention,
            "s1".into(),
            "/p".into(),
            Some(PromptInfo { tool: "Bash".into(), hint: "ls".into(), mode: "default".into() }),
            true,
        );
        assert!(matches!(ev, Some(HookEvent::PermissionRequest { .. })));
    }

    #[test]
    fn maps_codex_log_lines() {
        let codex = &load_profiles()["codex"];
        // Nested payload type → "event_msg:task_started" → thinking.
        let line = json!({"type":"event_msg","payload":{"type":"task_started"}}).to_string();
        assert_eq!(classify_log_line(codex, &line).0, Some(NormState::Thinking));
        // Bare top type.
        let meta = json!({"type":"session_meta"}).to_string();
        assert_eq!(classify_log_line(codex, &meta).0, Some(NormState::Idle));
        // task_complete → codex-turn-end.
        let done = json!({"type":"event_msg","payload":{"type":"task_complete"}}).to_string();
        assert_eq!(classify_log_line(codex, &done).0, Some(NormState::CodexTurnEnd));
        // Unmapped → None.
        let other = json!({"type":"event_msg","payload":{"type":"nope"}}).to_string();
        assert_eq!(classify_log_line(codex, &other).0, None);
    }

    #[test]
    fn classify_log_line_recovers_cwd_from_session_meta() {
        let codex = &load_profiles()["codex"];
        let meta = json!({"type":"session_meta","payload":{"cwd":"/home/me/proj"}}).to_string();
        let (norm, cwd) = classify_log_line(codex, &meta);
        assert_eq!(norm, Some(NormState::Idle));
        assert_eq!(cwd.as_deref(), Some("/home/me/proj"));
        // A non-session_meta line surfaces no cwd.
        let line = json!({"type":"event_msg","payload":{"type":"task_started"}}).to_string();
        assert_eq!(classify_log_line(codex, &line).1, None);
    }

    #[test]
    fn cwd_extracts_arrays_and_nested() {
        assert_eq!(cwd(&json!({"workspace_roots":["/w"]})), "/w");
        assert_eq!(cwd(&json!({"workspacePaths":["/w/proj"]})), "/w/proj");
        assert_eq!(cwd(&json!({"toolCall":{"args":{"Cwd":"/w/proj"}}})), "/w/proj");
        // toolCall.args.Cwd wins over workspacePaths.
        assert_eq!(
            cwd(&json!({"toolCall":{"args":{"Cwd":"/a"}},"workspacePaths":["/b"]})),
            "/a"
        );
    }

    #[test]
    fn reclassify_handles_failure_and_status() {
        let p = load_profiles();
        let cursor = &p["cursor-agent"];
        // Cursor stop with status:error → stopFailure → Error.
        let payload = json!({"status":"error"});
        let name = reclassify_event(cursor, "stop", &payload);
        assert_eq!(name, "stopFailure");
        assert!(matches!(
            map_hook_event(cursor, &name, &payload),
            Some(HookEvent::Stop { final_turn: false, summary: Some(_), .. })
        ));
        let anti = &p["antigravity-cli"];
        // PostToolUse with error → PostToolUseFailure → Error.
        assert_eq!(
            reclassify_event(anti, "PostToolUse", &json!({"error":"boom"})),
            "PostToolUseFailure"
        );
        // Stop with terminationReason failed → StopFailure.
        assert_eq!(
            reclassify_event(anti, "Stop", &json!({"terminationReason":"failed"})),
            "StopFailure"
        );
        // Stop with fullyIdle:false → reclassified to PostToolUse (working).
        assert_eq!(
            reclassify_event(anti, "Stop", &json!({"fullyIdle":false})),
            "PostToolUse"
        );
        // A clean Stop is unchanged.
        assert_eq!(reclassify_event(anti, "Stop", &json!({})), "Stop");
    }

    #[test]
    fn notification_surfaces_message_not_tool_hint() {
        // qwen-code maps Notification (no tool_input) — the message field shows.
        let p = load_profiles();
        let qwen = &p["qwen-code"];
        // Notification: a plain message field, no tool_input.
        let pi = prompt_info(&json!({"message":"needs input"}), "default");
        assert_eq!(pi.hint, "needs input");
        let _ = qwen;
    }

    #[test]
    fn maps_a_gemini_hook_event_by_name() {
        let gem = &load_profiles()["gemini-cli"];
        let p = json!({"sessionId":"abc","workspaceRoot":"/w"});
        // BeforeTool → working → running.
        assert!(matches!(
            map_hook_event(gem, "BeforeTool", &p),
            Some(HookEvent::UserPromptSubmit { .. })
        ));
        // SessionEnd → sleeping → SessionEnd.
        assert!(matches!(
            map_hook_event(gem, "SessionEnd", &p),
            Some(HookEvent::SessionEnd { .. })
        ));
        // Unknown event name → no-op.
        assert!(map_hook_event(gem, "Bogus", &p).is_none());
    }

    #[test]
    fn extracts_fields_across_shapes() {
        assert_eq!(session_id(&json!({"sessionId":"x"})), "x");
        assert_eq!(session_id(&json!({"session_id":"y"})), "y");
        assert_eq!(cwd(&json!({"workspaceRoot":"/w"})), "/w");
        let pi = prompt_info(&json!({"toolName":"Bash","toolInput":{"command":"ls"}}), "default");
        assert_eq!(pi.tool, "Bash");
        assert_eq!(pi.hint, "ls");
    }
}
