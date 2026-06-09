//! Local IPC between the short-lived `hook` invocations and the long-running
//! `daemon`.
//!
//! Claude Code spawns our `agent-buddy hook <event>` process on every hook
//! event. That process is too short-lived to own the BLE connection, so it
//! connects to the already-running daemon over a loopback TCP socket and
//! relays the event. For `PermissionRequest` it then blocks waiting for the
//! device's approve/deny decision.
//!
//! Transport: TCP on `127.0.0.1:<port>`. The port is ephemeral and written to
//! `endpoint.json` in the config dir when the daemon starts; hooks read it
//! back. A random `token` in the same file gates connections so other local
//! users can't drive the device. Framing is newline-delimited JSON, matching
//! the spirit of the device protocol.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Request sent hook -> daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookRequest {
    /// Shared secret from `endpoint.json`.
    pub token: String,
    pub event: HookEvent,
}

/// The hook events we care about, normalized from Claude Code's hook JSON.
/// See <https://docs.claude.com/en/docs/claude-code/hooks>.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookEvent {
    /// A new Claude Code session began. `cwd` is the session's working dir, used
    /// as its project label.
    SessionStart {
        session_id: String,
        #[serde(default)]
        cwd: String,
    },
    /// A session ended.
    SessionEnd { session_id: String },
    /// User submitted a prompt — a turn is starting (session is "running").
    UserPromptSubmit {
        session_id: String,
        #[serde(default)]
        cwd: String,
    },
    /// A turn (or subagent) finished. `session_total_tokens` is the session's
    /// *cumulative* output tokens from the transcript (the daemon folds in only
    /// the delta, so repeats don't double-count). `final_turn` is true for a
    /// real Stop and false for SubagentStop — only a real Stop ends the turn
    /// (sets idle + celebrate); a subagent finishing leaves the parent running.
    /// `model` and `ctx_tokens` (last turn's context size) come from the
    /// transcript and feed the per-session dashboard.
    Stop {
        session_id: String,
        #[serde(default)]
        session_total_tokens: u64,
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        final_turn: bool,
        #[serde(default)]
        model: String,
        #[serde(default)]
        ctx_tokens: u64,
        #[serde(default)]
        cwd: String,
    },
    /// Claude is waiting on the user (idle notification).
    Notification {
        session_id: String,
        #[serde(default)]
        message: Option<String>,
    },
    /// Claude Code is about to show the user a permission prompt for a tool —
    /// the daemon mirrors it on the device and blocks for the tap. Fired from
    /// the `PermissionRequest` hook, which (unlike `PreToolUse`) Claude raises
    /// ONLY when it would genuinely prompt: auto-approved calls, allow-listed
    /// tools, bypassing permission modes, and autonomous subagent calls never
    /// reach here. So the device gates exactly when, and only when, the real
    /// session gates. `mode` is Claude's permission mode (drives the device's
    /// button labels in plan mode); `cwd` labels the session. Telemetry is NOT
    /// carried here — it rides the `Telemetry` event from the `PreToolUse` that
    /// precedes every prompt, so the dashboard is already current.
    PermissionRequest {
        session_id: String,
        tool: String,
        /// Short human hint (command, path, etc.).
        hint: String,
        #[serde(default)]
        mode: String,
        #[serde(default)]
        cwd: String,
    },
    /// A pure telemetry refresh — emitted by EVERY `PreToolUse` (which no longer
    /// gates; gating moved to `PermissionRequest`). Keeps the device's
    /// token/context readout tracking the turn mid-flight instead of only
    /// jumping at Stop. Updates the live state only: no prompt, no
    /// running/waiting change, no durable write (Stop owns persistence). Fields
    /// mirror the telemetry triple on [`HookEvent::Stop`].
    Telemetry {
        session_id: String,
        #[serde(default)]
        session_total_tokens: u64,
        #[serde(default)]
        model: String,
        #[serde(default)]
        ctx_tokens: u64,
        #[serde(default)]
        cwd: String,
    },
}

impl HookEvent {
    /// The session id this event belongs to (every variant carries one).
    pub fn session_id(&self) -> &str {
        match self {
            HookEvent::SessionStart { session_id, .. }
            | HookEvent::SessionEnd { session_id }
            | HookEvent::UserPromptSubmit { session_id, .. }
            | HookEvent::Stop { session_id, .. }
            | HookEvent::Notification { session_id, .. }
            | HookEvent::PermissionRequest { session_id, .. }
            | HookEvent::Telemetry { session_id, .. } => session_id,
        }
    }
}

/// Response daemon -> hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookResponse {
    /// State event recorded; nothing for the hook to do.
    Ack,
    /// A permission decision came back from the device.
    Decision { allow: bool },
    /// No device connected, or it didn't answer in time. The hook should fall
    /// back to Claude Code's normal (terminal) permission flow.
    Defer { reason: String },
    /// The daemon rejected the request (bad token, etc.).
    Error { message: String },
}

/// Filename (under the config dir) holding the live `{port, token}`.
pub const ENDPOINT_FILE: &str = "endpoint.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
    /// Loopback HTTP port plugin/extension harnesses POST events to. `None` on
    /// older daemons (additive). Plugins read this to find the listener.
    #[serde(default)]
    pub http_port: Option<u16>,
}

/// Path to the daemon's published endpoint file.
pub fn endpoint_path() -> anyhow::Result<PathBuf> {
    Ok(crate::state::config_dir()?.join(ENDPOINT_FILE))
}

/// Read the live daemon endpoint, or an error if no daemon is running.
///
/// Parsing `endpoint.json` only proves a daemon *once* published it — the file
/// outlives a crash. [`endpoint_if_live`] is the honest check: it additionally
/// probes the socket so callers don't report a dead daemon as running.
pub fn read_endpoint() -> anyhow::Result<Endpoint> {
    let bytes = std::fs::read(endpoint_path()?)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Like [`read_endpoint`], but additionally proves the daemon is actually
/// listening by opening (and immediately dropping) a short-timeout TCP
/// connection to the published port. A parseable-but-stale `endpoint.json`
/// (left behind by a crash) yields the friendly "isn't running" error and the
/// confirmed-dead file is removed opportunistically so the next caller is fast.
pub fn endpoint_if_live() -> anyhow::Result<Endpoint> {
    let ep = read_endpoint()?;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], ep.port));
    match std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500)) {
        Ok(_stream) => Ok(ep),
        Err(_) => {
            // Connect refused / timed out: the file is stale. Best-effort clean
            // it up (ignore failures — another daemon may be mid-write).
            if let Ok(path) = endpoint_path() {
                let _ = std::fs::remove_file(path);
            }
            Err(anyhow::anyhow!("the buddy daemon isn’t running"))
        }
    }
}

// ---------------------------------------------------------------------------
// Admin channel — out-of-band commands (not hook events) sent to the daemon so
// it can relay them over the BLE link it owns. Same socket + token as hooks;
// the daemon distinguishes an [`AdminRequest`] from a [`HookRequest`] by shape.
// ---------------------------------------------------------------------------

/// Request sent (CLI) -> daemon to push a command at the device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminRequest {
    /// Shared secret from `endpoint.json`.
    pub token: String,
    pub command: DeviceCommand,
}

/// A command for the daemon to forward to the connected device.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeviceCommand {
    /// Provision Wi-Fi credentials for OTA.
    Wifi { ssid: String, pass: String },
    /// Ask the device to enter OTA mode (free heap for the flash).
    Ota,
    /// Switch the active agent harness: the daemon uninstalls the old harness's
    /// hooks, installs the new one's, persists the choice, and pushes the new
    /// theme to the device. `id` must match a loaded [`crate::agent::AgentProfile`].
    SetAgent { id: String },
    /// Stream an on-disk animation pack (`<dir>/<state>.spr` files) to the
    /// connected device's asset store at `/agents/<id>/`. The daemon reads the
    /// files itself (same machine) and drives the `char_begin`/`file`/`chunk`/
    /// `file_end`/`char_end` BLE sequence with per-chunk flow control. Unlike the
    /// other commands this replies with a *stream* of [`PushProgress`] lines
    /// followed by a terminal [`AdminResponse`]. `set_active` also points the
    /// active agent's theme at this pack so it displays even when its id differs
    /// from the active harness.
    PushPack {
        id: String,
        dir: PathBuf,
        #[serde(default)]
        set_active: bool,
    },
}

/// Interim progress line streamed during a [`DeviceCommand::PushPack`], before
/// the terminal [`AdminResponse`]. The `kind` field is always `"progress"`, so a
/// reader can tell it apart from an `AdminResponse` (whose `kind` is
/// `ok`/`no_device`/`error`) on the same connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushProgress {
    /// Always `"progress"`.
    pub kind: String,
    /// Bytes pushed + acked so far, across all files.
    pub done: u64,
    /// Total bytes in the pack.
    pub total: u64,
    /// The state file currently being written (e.g. `"idle.spr"`).
    pub file: String,
}

/// Response daemon -> (CLI) for an [`AdminRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdminResponse {
    /// Command written to the device.
    ///
    /// For Wi-Fi this is only sent after the device confirms it stored the
    /// credentials (`{"ack":"wifi","ok":true}`), so "Ok" means *persisted*, not
    /// merely *transmitted*. `joined` carries the network the device announced
    /// joining, when it reported one before the response was resolved — letting
    /// the UI say "joined <ssid>" vs. "stored, not joined yet".
    Ok {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        joined: Option<String>,
    },
    /// Daemon is running but no buddy is currently connected.
    NoDevice,
    /// The daemon rejected or failed the request.
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Query channel — read-only snapshots for the desktop UI. Same socket + token
// as hooks/admin; the daemon tells the three apart by shape (a HookRequest has
// `event`, an AdminRequest has `command`, a QueryRequest has `query`).
// ---------------------------------------------------------------------------

/// Request sent (GUI) -> daemon for a read-only snapshot of its state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    /// Shared secret from `endpoint.json`.
    pub token: String,
    pub query: Query,
}

/// A read-only question for the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Query {
    /// A full snapshot of daemon + device state.
    Status,
    /// Nudge the daemon to re-check GitHub for updates *now* instead of waiting
    /// for its periodic (6h) poll. The response carries the current snapshot;
    /// the freshly-fetched result lands a moment later and surfaces on the next
    /// `Status` poll. Lets the app show the newest release within seconds of
    /// being opened rather than up to 6h stale.
    RecheckUpdates,
}

/// Response daemon -> (GUI) for a [`QueryRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueryResponse {
    Status(StatusReport),
    /// The daemon rejected the request (bad token, etc.).
    Error {
        message: String,
    },
}

/// Everything the control-panel UI renders. A point-in-time snapshot; the GUI
/// polls it on a timer rather than subscribing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusReport {
    /// Is a buddy currently linked over BLE?
    pub device_connected: bool,
    /// Owner name the daemon greets the device with.
    pub owner: String,
    /// Output tokens counted today (local) and cumulatively since daemon start.
    pub tokens_today: u64,
    pub tokens_total: u64,
    /// Active Claude Code sessions, and how many are running / waiting.
    pub sessions_total: u32,
    pub sessions_running: u32,
    pub sessions_waiting: u32,
    /// Recent activity ticker, newest first.
    pub entries: Vec<String>,
    /// The Wi-Fi the device announced joining this session, if any (for OTA).
    pub device_ssid: Option<String>,
    pub device_ip: Option<String>,
    /// Internet reachability from the device's own probe: Some(true)=online,
    /// Some(false)=joined Wi-Fi but no internet, None=unknown/not reported.
    #[serde(default)]
    pub device_online: Option<bool>,
    /// Whether a Bluetooth adapter is present and usable. `false` is the cue to
    /// tell the user to turn Bluetooth on rather than "no buddy found".
    #[serde(default)]
    pub bluetooth_available: bool,
    /// Whether the OS has granted this process Bluetooth permission, when that
    /// is observable. `None` = unknown (we couldn't tell apart "denied" from
    /// "off"); `Some(false)` is a strong hint to send the user to System
    /// Settings → Privacy → Bluetooth.
    #[serde(default)]
    pub bluetooth_permitted: Option<bool>,
    /// Human-readable reason the most recent connect attempt failed, if the
    /// daemon isn't currently linked. Surfaced so the UI can guide the user
    /// (Bluetooth off, buddy asleep, pairing needed) instead of a bare spinner.
    #[serde(default)]
    pub last_connect_error: Option<String>,
    /// Firmware version the connected buddy announced over BLE (`{"fw":"..."}`),
    /// e.g. `"v0.1.0"`. `None` until the device reports it (older firmware never
    /// will). The app compares it to the image it bundles to offer an OTA update.
    #[serde(default)]
    pub device_fw: Option<String>,
    /// Board id the connected buddy announced over BLE (`{"board":"..."}`),
    /// e.g. `"cyd"` or `"fnk0104"`. `None` until the device reports it (older
    /// firmware never will → treated as the default `"cyd"`). Selects which
    /// bundled firmware image + OTA slot size the app uses.
    #[serde(default)]
    pub device_board: Option<String>,
    /// Result of the daemon's periodic check for a newer desktop-app release.
    /// `None` until the first check completes (and stays `None` if checks keep
    /// failing, e.g. offline). Additive — old clients ignore it.
    #[serde(default)]
    pub update: Option<UpdateStatus>,
    /// Newest firmware available from GitHub Releases for the *connected board*,
    /// independent of the app's own version — so a device can be OTA-updated
    /// without updating the desktop app. `None` until the first check completes,
    /// no buddy is connected, or no release offers an image for the board. The
    /// app compares its `version` to what the device reports (and to the image it
    /// bundles) to decide whether to offer the update. Additive.
    #[serde(default)]
    pub firmware_latest: Option<FirmwareLatest>,
    /// The active agent harness id (e.g. `"claude-code"`). Drives the app's agent
    /// selector. Defaults to `"claude-code"` for old clients. Additive.
    #[serde(default)]
    pub active_agent: String,
    /// All agents the daemon can switch to (id + display name), for the selector.
    /// Additive — old clients ignore it.
    #[serde(default)]
    pub available_agents: Vec<crate::agent::AgentSummary>,
}

/// A newer agent-buddy release the daemon found on GitHub, surfaced to the
/// desktop app's "update available" banner.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateStatus {
    /// The version this build is (baked `AGENT_BUDDY_VERSION`).
    pub current: String,
    /// The latest published release tag (e.g. `"v0.1.2"`).
    pub latest: String,
    /// True when `latest` is strictly newer than `current`.
    pub available: bool,
    /// GitHub release page, for a guided download.
    pub url: String,
    /// Direct download URL for this platform's in-place installer asset — macOS
    /// `.dmg`, Windows `Setup.exe`, Linux `.AppImage`. `None` when the release
    /// carries no package for this OS, in which case the app falls back to the
    /// guided download at [`url`](Self::url). Selected by the OS the daemon (and
    /// thus the app beside it) runs on.
    #[serde(default)]
    pub pkg_url: Option<String>,
    /// File name of that installer asset (e.g. `"Agent-Buddy-v0.1.6.dmg"`).
    #[serde(default)]
    pub pkg_name: Option<String>,
}

/// The newest firmware image available from a GitHub release for a given board,
/// surfaced so the app can offer (and download) an OTA update without shipping a
/// new app build.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FirmwareLatest {
    /// Clean firmware version, e.g. `"v0.1.4"` (any `fw-` routing prefix stripped).
    pub version: String,
    /// Direct download URL for the board's `firmware-<board>.bin` asset.
    pub url: String,
    /// Download URL of the sibling `<bin>.sha256` checksum, when the release
    /// published one. Used to verify the image before flashing; `None` for
    /// releases that predate checksum publishing (best-effort path).
    #[serde(default)]
    pub sha256_url: Option<String>,
}
