//! The Hardware Buddy BLE wire protocol.
//!
//! This is the *exact* protocol the Claude desktop apps speak (see
//! `REFERENCE.md`), re-implemented here so our daemon can play the same
//! central role. Everything on the wire is UTF-8 JSON, one object per line,
//! terminated with `\n`.
//!
//! Directions are from the daemon's point of view:
//!   - `Outbound*`  = daemon -> device   (we write to the NUS RX characteristic)
//!   - `Inbound*`   = device -> daemon   (notifications on the NUS TX char)

use serde::{Deserialize, Serialize};

/// Nordic UART Service UUIDs. The firmware advertises this service and the
/// device picker filters on a name starting with `Claude`.
pub mod nus {
    use uuid::{uuid, Uuid};
    pub const SERVICE: Uuid = uuid!("6e400001-b5a3-f393-e0a9-e50e24dcca9e");
    /// Central -> device (we WRITE here).
    pub const RX: Uuid = uuid!("6e400002-b5a3-f393-e0a9-e50e24dcca9e");
    /// Device -> central (we SUBSCRIBE to notifications here).
    pub const TX: Uuid = uuid!("6e400003-b5a3-f393-e0a9-e50e24dcca9e");
}

/// Name prefix the firmware advertises with, used to filter the scan.
pub const DEVICE_NAME_PREFIX: &str = "Claude";

/// Hard ceiling, in **bytes**, on any single wire line we send the device.
///
/// The firmware reassembles each `\n`-delimited line into a fixed buffer
/// (`_LineBuf<2048>` in `data.h`) and silently drops a line that overruns it —
/// so a heartbeat carrying multibyte transcript text could vanish whole if we
/// only bounded it by character count. Everything that builds an outbound line
/// (the daemon's heartbeat assembly, the hook's summary cap) measures against
/// this in bytes. Kept comfortably under the firmware buffer so MTU-chunked
/// writes and the trailing `\n` still fit.
///
/// WIRE COUPLING: the firmware `_LineBuf` MUST stay strictly larger than this
/// (see `firmware/src/data.h`). Documented as the single source of truth in
/// `REFERENCE.md`. If the two drift, multibyte heartbeats silently drop.
pub const MAX_LINE_BYTES: usize = 1900;

// ---------------------------------------------------------------------------
// Daemon -> device
// ---------------------------------------------------------------------------

/// The heartbeat snapshot. Sent whenever state changes, plus a keepalive
/// every 10s. The firmware treats >30s of silence as a dead link.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Heartbeat {
    pub total: u32,
    pub running: u32,
    pub waiting: u32,
    /// True briefly after a turn completes — drives the device celebrate state
    /// (the firmware reads `doc["completed"]`).
    pub completed: bool,
    /// One-line summary suitable for a small display.
    pub msg: String,
    /// Recent transcript lines, newest first (firmware caps display to a few).
    pub entries: Vec<String>,
    /// Cumulative output tokens since the daemon started.
    pub tokens: u64,
    /// Output tokens since local midnight (persisted across restarts).
    pub tokens_today: u64,
    /// Per-session detail for the device's session picker / per-session
    /// dashboards. Capped and sorted by the daemon (waiting → running → idle).
    /// Omitted when empty so the line stays small. Additive: consumers that
    /// don't model it ignore the key.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionInfo>,
    /// Present only when a permission decision is pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptPayload>,
}

/// One session's live telemetry, nested in a [`Heartbeat`]. Keys are kept short
/// because the device reassembles the whole line into a fixed buffer.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SessionInfo {
    /// Short session id (a prefix of the full id) — the device echoes this when
    /// selecting a session and matches it against a pending prompt's `sid`.
    pub id: String,
    /// Project label (basename of the session's cwd).
    pub cwd: String,
    /// State: `"run"` (generating), `"wait"` (on a permission prompt), `"idle"`.
    pub st: &'static str,
    /// Cumulative output tokens for this session.
    pub tok: u64,
    /// Short model name, e.g. `"opus"` / `"sonnet"` / `"haiku"` (empty if unknown).
    pub m: String,
    /// Context-window fill, 0..100 (last turn's context tokens / model limit).
    pub ctx: u8,
    /// Last turn's raw context tokens (input+cache+output) — the numerator
    /// behind `ctx`, so the device can show the actual count, not just a percent.
    pub ctok: u64,
    /// Model context-window limit in tokens (200000, or 1000000 for `*-1m`
    /// variants). The denominator behind `ctx`; lets the device print "x / y".
    pub clim: u64,
}

/// The pending-permission payload nested inside a [`Heartbeat`].
#[derive(Debug, Clone, Serialize, Default)]
pub struct PromptPayload {
    /// Opaque id the device echoes back in its decision. We mint a uuid.
    pub id: String,
    pub tool: String,
    /// Short human hint (e.g. the command being run). Firmware truncates.
    pub hint: String,
    /// Claude's permission mode for this call (`"default"` / `"plan"` /
    /// `"acceptEdits"`). The device relabels its buttons for plan mode. Omitted
    /// when empty/default.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub mode: String,
    /// Short id of the session this prompt belongs to (matches a
    /// [`SessionInfo::id`]), so the device can name the project being approved.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sid: String,
}

/// One-shot turn event — the raw SDK content array for a completed turn.
/// Dropped by the desktop transport if it serializes larger than 4KB; we
/// keep ours small. Part of the protocol surface; not emitted by the scaffold.
#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
pub struct TurnEvent {
    pub evt: &'static str, // "turn"
    pub role: String,
    pub content: serde_json::Value,
}

/// Commands the daemon sends with a `cmd` field; each expects an ack.
/// Serialized with an internally-tagged `cmd` discriminator.
///
/// Several variants (folder push, name/unpair) are part of the protocol
/// surface but not yet driven by the daemon scaffold — kept here so the wire
/// format lives in one place.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum OutboundCmd {
    Status,
    Name {
        name: String,
    },
    Owner {
        name: String,
    },
    /// Wi-Fi credentials for OTA: `{"cmd":"wifi","ssid":"...","pass":"..."}`.
    /// The device saves them to NVS and connects (see firmware net.cpp).
    Wifi {
        ssid: String,
        pass: String,
    },
    /// Ask the device to enter OTA mode: `{"cmd":"ota"}`. It tears down BLE +
    /// the UI sprite to free the heap a flash needs, then waits for the espota
    /// push (the BLE link drops as a result — that's expected, daemon reconnects
    /// after the device reboots into the new image).
    Ota,
    Unpair,
    // --- folder push (GIF character packs) ---
    CharBegin {
        name: String,
        total: u64,
    },
    File {
        path: String,
        size: u64,
    },
    Chunk {
        d: String,
    }, // base64
    FileEnd,
    CharEnd,
}

/// Time sync, sent once on connect: `{"time":[epoch_sec, tz_offset_sec]}`.
#[derive(Debug, Clone, Serialize)]
pub struct TimeSync {
    pub time: (i64, i32),
}

// ---------------------------------------------------------------------------
// device -> daemon
// ---------------------------------------------------------------------------

/// Anything the device sends us. We only act on permission decisions; acks
/// and status responses are logged. Untagged so we can parse whichever shape
/// arrived on a given line.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Inbound {
    /// `{"cmd":"permission","id":"...","decision":"once"|"deny"}`
    Permission(PermissionDecision),
    /// `{"ack":"<cmd>","ok":true,"n":0,...}`
    Ack(Ack),
    /// Anything else (status payloads we don't model, future fields).
    Other(serde_json::Value),
}

#[derive(Debug, Clone, Deserialize)]
pub struct PermissionDecision {
    /// Discriminator; always "permission". Kept so untagged parsing is
    /// unambiguous against [`Ack`].
    #[allow(dead_code)]
    pub cmd: PermissionTag,
    pub id: String,
    pub decision: Decision,
}

/// Unit-like tag enum so `cmd: "permission"` is required for this variant.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionTag {
    Permission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Approve this tool call once.
    Once,
    /// Reject it.
    Deny,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // `n` is part of the wire format; not consumed yet.
pub struct Ack {
    pub ack: String,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub n: i64,
    #[serde(default)]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Line (de)serialization helpers — every message is one JSON object + '\n'.
// ---------------------------------------------------------------------------

/// Serialize any value to a single newline-terminated wire line.
pub fn to_line<T: Serialize>(v: &T) -> anyhow::Result<Vec<u8>> {
    let mut s = serde_json::to_vec(v)?;
    s.push(b'\n');
    Ok(s)
}

/// Parse one inbound line (without the trailing newline) into [`Inbound`].
pub fn parse_inbound(line: &str) -> anyhow::Result<Inbound> {
    Ok(serde_json::from_str(line)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_omits_absent_prompt() {
        let hb = Heartbeat {
            total: 1,
            msg: "idle".into(),
            ..Default::default()
        };
        let s = String::from_utf8(to_line(&hb).unwrap()).unwrap();
        assert!(!s.contains("prompt"));
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn heartbeat_omits_empty_sessions_but_includes_filled() {
        let empty = Heartbeat::default();
        let s = String::from_utf8(to_line(&empty).unwrap()).unwrap();
        assert!(!s.contains("sessions"));

        let hb = Heartbeat {
            sessions: vec![SessionInfo {
                id: "abc123".into(),
                cwd: "buddy".into(),
                st: "run",
                tok: 4200,
                m: "opus".into(),
                ctx: 37,
                ctok: 74_000,
                clim: 200_000,
            }],
            ..Default::default()
        };
        let s = String::from_utf8(to_line(&hb).unwrap()).unwrap();
        assert!(s.contains(r#""sessions""#));
        assert!(s.contains(r#""st":"run""#));
        assert!(s.contains(r#""ctx":37"#));
    }

    #[test]
    fn prompt_payload_omits_empty_mode() {
        let plain = PromptPayload {
            id: "req_1".into(),
            tool: "Bash".into(),
            ..Default::default()
        };
        let s = serde_json::to_string(&plain).unwrap();
        assert!(!s.contains("mode"));
        assert!(!s.contains("sid"));

        let planning = PromptPayload {
            id: "req_2".into(),
            tool: "ExitPlanMode".into(),
            mode: "plan".into(),
            sid: "abc123".into(),
            ..Default::default()
        };
        let s = serde_json::to_string(&planning).unwrap();
        assert!(s.contains(r#""mode":"plan""#));
        assert!(s.contains(r#""sid":"abc123""#));
    }

    #[test]
    fn parses_permission_decision() {
        let line = r#"{"cmd":"permission","id":"req_abc","decision":"once"}"#;
        match parse_inbound(line).unwrap() {
            Inbound::Permission(p) => {
                assert_eq!(p.id, "req_abc");
                assert_eq!(p.decision, Decision::Once);
            }
            other => panic!("expected permission, got {other:?}"),
        }
    }

    #[test]
    fn parses_ack() {
        let line = r#"{"ack":"status","ok":true,"n":0}"#;
        match parse_inbound(line).unwrap() {
            Inbound::Ack(a) => assert_eq!(a.ack, "status"),
            other => panic!("expected ack, got {other:?}"),
        }
    }

    #[test]
    fn max_line_bytes_stays_under_firmware_buffer() {
        // The firmware _LineBuf<2048> (data.h) must be strictly larger than the
        // budget we cap outbound lines to, with headroom for the trailing '\n'.
        // Compile-time invariant: a const guard so a future bump that crosses
        // the firmware buffer fails to build, not just at test time.
        const _: () = assert!(MAX_LINE_BYTES < 2048);
    }

    #[test]
    fn outbound_cmd_tagging() {
        let s = String::from_utf8(
            to_line(&OutboundCmd::Owner {
                name: "Felix".into(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(s.contains(r#""cmd":"owner""#));
        assert!(s.contains(r#""name":"Felix""#));
    }
}
