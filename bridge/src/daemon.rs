//! The long-running daemon: owns the BLE link, serves hook IPC, aggregates
//! session state, and drives the heartbeat + permission loop.
//!
//! Design: a single owner task holds all mutable state. Everything else
//! (IPC accept loop, BLE notification pump, timers) feeds it [`Event`]s over
//! an mpsc channel, so there are no locks around the state.

use crate::agent::{self, AgentProfile};
use crate::ble::BleLink;
use crate::ipc::{
    AdminRequest, AdminResponse, DeviceCommand, Endpoint, FirmwareLatest, HookEvent, HookRequest,
    HookResponse, PushProgress, Query, QueryRequest, QueryResponse, StatusReport, UpdateStatus,
    ENDPOINT_FILE,
};
use crate::protocol::{
    self, Decision, Heartbeat, Inbound, OutboundCmd, PromptPayload, TimeSync, MAX_LINE_BYTES,
};
use crate::state::{config_dir, Config, SessionState};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

/// The thing the owner loop sends heartbeats/commands to. Normally a real BLE
/// buddy; in `--mock-device` mode a virtual device that auto-answers prompts,
/// so the whole hook→daemon→decision loop runs with no hardware.
#[derive(Clone)]
enum Link {
    Ble(BleLink),
    Mock(MockLink),
}

impl Link {
    async fn send<T: Serialize>(&self, v: &T) -> Result<()> {
        let line = protocol::to_line(v)?;
        match self {
            Link::Ble(l) => l.write_line(&line).await,
            Link::Mock(m) => m.send_line(&line).await,
        }
    }
    async fn disconnect(&self) {
        match self {
            Link::Ble(l) => l.disconnect().await,
            Link::Mock(_) => {}
        }
    }
    /// Is the underlying transport still up? Polled each keepalive so a
    /// vanished buddy is noticed promptly instead of only when a write fails.
    async fn is_connected(&self) -> bool {
        match self {
            Link::Ble(l) => l.is_connected().await,
            Link::Mock(_) => true,
        }
    }
}

/// What the mock device decides on each prompt.
#[derive(Clone, Copy)]
pub enum MockPolicy {
    Approve,
    Deny,
}

impl MockPolicy {
    pub fn from_str(s: &str) -> MockPolicy {
        match s.to_ascii_lowercase().as_str() {
            "deny" => MockPolicy::Deny,
            _ => MockPolicy::Approve,
        }
    }
    fn decision(&self) -> &'static str {
        match self {
            MockPolicy::Approve => "once",
            MockPolicy::Deny => "deny",
        }
    }
}

/// A virtual buddy. It "displays" each heartbeat (logged) and, when a prompt
/// arrives, injects a permission decision back into the event loop after a
/// short delay — simulating a user tapping the screen.
#[derive(Clone)]
struct MockLink {
    ev_tx: mpsc::Sender<Event>,
    policy: MockPolicy,
    /// Last prompt id answered, so repeated heartbeats don't re-answer.
    answered: Arc<Mutex<Option<String>>>,
}

impl MockLink {
    async fn send_line(&self, bytes: &[u8]) -> Result<()> {
        let line = String::from_utf8_lossy(bytes);
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            return Ok(());
        };
        if let Some(msg) = v.get("msg").and_then(|m| m.as_str()) {
            debug!("[mock-device] screen: {msg}");
        }
        if let Some(prompt) = v.get("prompt") {
            if let Some(id) = prompt.get("id").and_then(|i| i.as_str()) {
                let mut answered = self.answered.lock().unwrap();
                if answered.as_deref() == Some(id) {
                    return Ok(()); // already answered this prompt
                }
                *answered = Some(id.to_string());
                drop(answered);
                let decision = self.policy.decision();
                let tool = prompt.get("tool").and_then(|t| t.as_str()).unwrap_or("");
                let hint = prompt.get("hint").and_then(|h| h.as_str()).unwrap_or("");
                info!("[mock-device] prompt: {tool} ({hint}) → auto-{decision}");
                let ev_tx = self.ev_tx.clone();
                let id = id.to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    let line = format!(
                        "{{\"cmd\":\"permission\",\"id\":\"{id}\",\"decision\":\"{decision}\"}}"
                    );
                    let _ = ev_tx.send(Event::DeviceLine(line)).await;
                });
            }
        }
        Ok(())
    }
}

/// How long the device may sit on a prompt before we give up. Kept *under* the
/// hook's own deadline (hook::PRETOOL_TIMEOUT, 25s) so the daemon resolves the
/// waiting hook with a Defer through the live channel — and clears the device
/// screen — *before* the hook times out on its own. (If this were larger than
/// the hook timeout, the hook would defer while the device still showed a live
/// prompt whose tap would resolve a dead channel.)
const PROMPT_TTL: Duration = Duration::from_secs(20);
/// Keepalive cadence. The firmware treats >30s of silence as a dead link.
const KEEPALIVE: Duration = Duration::from_secs(3);
/// Max gap between inbound device lines before we treat the link as stalled and
/// force a reconnect. The firmware sends a liveness pong every ~3s, so silence
/// this long means the link wedged (e.g. a macOS CoreBluetooth disconnect that
/// never closes the notify stream) — the only reliable signal, since `is_connected`
/// reports a stale `true` and a fire-and-forget write still returns `Ok`.
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(6);
/// Scan window per reconnect attempt.
const SCAN_SECS: u64 = 12;
/// How long to wait for the device's `{"ack":"wifi"}` (verified NVS persistence)
/// before telling the user the buddy didn't confirm. Generous: the firmware
/// writes NVS and may briefly attempt to join before acking.
const WIFI_ACK_TIMEOUT: Duration = Duration::from_secs(8);
/// After a wifi ack with ok:true, how long to keep waiting for a
/// `{"net":connected}` so we can report "joined <ssid>" instead of just
/// "stored". Short — if the join is slow we report the (already successful)
/// store rather than make the user wait.
const WIFI_JOIN_GRACE: Duration = Duration::from_secs(6);
/// How often to re-check GitHub for a newer app release. Long: a desktop-app
/// update is never urgent, and unauthenticated GitHub API calls are rate-limited
/// (60/hr/IP) — six hours keeps us far clear while still noticing a release
/// within a day. The first check runs shortly after startup (see the checker).
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);

/// Messages into the owner task.
enum Event {
    /// A hook process connected and sent an event; reply via the oneshot.
    Hook(HookEvent, oneshot::Sender<HookResponse>),
    /// A CLI client asked to push a command at the device; reply via the oneshot.
    DeviceCommand(DeviceCommand, oneshot::Sender<AdminResponse>),
    /// A UI client asked for a read-only snapshot; reply via the oneshot.
    Query(Query, oneshot::Sender<QueryResponse>),
    /// A reassembled line arrived from the device.
    DeviceLine(String),
    /// Link came up (BLE or mock).
    Connected(Link),
    /// A connect attempt failed; carries a user-facing classified reason so the
    /// status UI can guide the user (Bluetooth off, buddy asleep, etc.).
    ConnectError(String),
    /// Link went down.
    Disconnected,
    /// Periodic keepalive heartbeat.
    Keepalive,
    /// A pending prompt outlived its TTL.
    PromptTimeout(String),
    /// The background update checker refreshed release info: the app-update
    /// status (desktop track) and the newest firmware per board (both tracks).
    UpdateInfo {
        app: UpdateStatus,
        firmware: HashMap<String, FirmwareLatest>,
    },
    /// A CLI client asked to stream an animation pack to the device. Unlike
    /// [`Event::DeviceCommand`] the reply is a stream, so it carries an mpsc
    /// sender of [`PushUpdate`]s instead of a oneshot. The owner loop spawns a
    /// task to drive the BLE sequence (so awaiting acks doesn't block the loop)
    /// and forwards the device's chunk acks into the task via `pending_push`.
    PushPack {
        id: String,
        dir: std::path::PathBuf,
        set_active: bool,
        updates: mpsc::Sender<PushUpdate>,
    },
    /// The spawned push task finished; the owner loop clears `pending_push` and,
    /// when `set_active` names a pack, points the active agent's theme at it.
    PushFinished { set_active: Option<String> },
}

/// What a push task streams back to the waiting CLI connection: zero or more
/// progress ticks, then exactly one terminal response.
pub enum PushUpdate {
    Progress { done: u64, total: u64, file: String },
    Done(AdminResponse),
}

/// A Wi-Fi provisioning request whose IPC responder is parked until the device
/// confirms (or fails) storing the credentials. We do NOT reply to the CLI/app
/// at write time — only `{"ack":"wifi","ok":...}` (verified NVS persistence in
/// the firmware) resolves it, so "Ok" genuinely means *persisted*.
struct PendingWifi {
    responder: oneshot::Sender<AdminResponse>,
    /// Phase of the correlation: waiting for the store ack, or (after a good
    /// ack) briefly waiting for a join announcement to enrich the reply.
    phase: WifiPhase,
    /// When the current phase must resolve by, even if nothing more arrives.
    deadline: std::time::Instant,
}

enum WifiPhase {
    /// Awaiting `{"ack":"wifi",...}`. Timeout → "did not confirm".
    AwaitingAck,
    /// Got a good ack; awaiting `{"net":connected}` to say "joined <ssid>".
    /// Timeout → resolve Ok with no join info ("stored, not joined yet").
    AwaitingJoin,
}

/// A permission prompt awaiting a device decision.
struct Pending {
    id: String,
    tool: String,
    hint: String,
    /// Claude's permission mode for this call; relabels the device buttons.
    mode: String,
    session: String,
    responder: oneshot::Sender<HookResponse>,
}

pub async fn run(mock: Option<MockPolicy>) -> Result<()> {
    // --- Single-instance guard. ---
    // A BLE peripheral accepts ONE central at a time and stops advertising while
    // connected, so two daemons fight over the buddy's single link: the loser
    // silently sees nothing and the device looks dead until it's power-cycled.
    // Take an exclusive advisory lock for the daemon's whole lifetime; a second
    // instance can't acquire it and exits cleanly here. `_lock` must stay in
    // scope — dropping it (or the process exiting) releases the lock.
    let _lock = acquire_singleton_lock()?;

    let mut config = Config::load()?;
    let mut state = SessionState::from_config(&config);

    // Agent profiles, loaded once (embedded defaults overlaid by release/user
    // files). These drive which harness we ingest, the theme pushed to the
    // device, and the app selector. (The NormState→device-telemetry collapse is
    // structural in `ingest::normalized_to_hook_event`; `state_map.json` is the
    // documented spec of that table, validated in agent.rs tests.) If the
    // persisted active agent no longer resolves (e.g. a profile was removed),
    // fall back to the default so we never run agent-less.
    let profiles = agent::load_profiles();
    if !profiles
        .get(&config.active_agent)
        .map(|p| p.supported())
        .unwrap_or(false)
    {
        warn!(
            "active agent {:?} has no supported profile; falling back to {}",
            config.active_agent,
            agent::DEFAULT_AGENT
        );
        config.active_agent = agent::DEFAULT_AGENT.to_string();
    }
    info!("active agent: {}", config.active_agent);

    // --- Reconcile the active agent's hooks. ---
    // Bring our hook entries to exactly the canonical set for this installed
    // daemon every time it starts. This is the self-healing safety net: it
    // wires hooks an install never set up, restores one deleted by hand, repairs
    // a stale binary path, and adds/removes events when a new daemon version
    // changes the set — so a daemon restart (e.g. after an app update) is enough
    // to converge. A no-op writes nothing. Skipped under a mock device so
    // test/dev runs don't touch the user's real settings.json. Reconciles the
    // *active* harness (Claude Code for the default agent); switching agents
    // re-wires hooks via the SetAgent path.
    if mock.is_none() {
        match crate::setup::ensure_active_agent_hooks(&config, &profiles) {
            Ok(true) => info!("reconciled hooks for active agent {}", config.active_agent),
            Ok(false) => {}
            Err(e) => warn!("could not reconcile hooks: {e}"),
        }
    }

    // --- IPC endpoint: bind loopback, publish {port, token} for hooks. ---
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding IPC socket")?;
    let port = listener.local_addr()?.port();
    let token = uuid::Uuid::new_v4().to_string();
    // Loopback HTTP listener for plugin/extension harnesses (opencode, pi,
    // openclaw, hermes) that POST events instead of spawning our hook command.
    let http_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding HTTP ingest socket")?;
    let http_port = http_listener.local_addr()?.port();
    write_endpoint(&Endpoint {
        port,
        token: token.clone(),
        http_port: Some(http_port),
    })?;
    info!("daemon IPC on 127.0.0.1:{port}, HTTP ingest on 127.0.0.1:{http_port}");

    let (ev_tx, mut ev_rx) = mpsc::channel::<Event>(256);

    // Profiles shared with the always-on HTTP listener; the active agent is
    // mirrored behind a lock so the listener only ingests the live harness.
    let profiles_shared = Arc::new(profiles.clone());
    let active_agent = Arc::new(Mutex::new(config.active_agent.clone()));

    spawn_ipc_accept(listener, token.clone(), ev_tx.clone());
    spawn_http_ingest(
        http_listener,
        token,
        profiles_shared,
        active_agent.clone(),
        ev_tx.clone(),
    );
    spawn_keepalive(ev_tx.clone());
    // Lets the owner loop nudge the update checker to poll immediately (on the
    // app being opened) instead of only on its 6h cadence — the fix for a
    // "latest available" banner that could lag reality by hours.
    let update_recheck = std::sync::Arc::new(tokio::sync::Notify::new());
    spawn_update_checker(ev_tx.clone(), update_recheck.clone());
    // Recover the BLE link across system sleep/wake (lid close → reopen): on
    // resume we restart so launchd hands us a fresh CoreBluetooth central. See
    // the function doc for why an in-process central can't be recovered.
    if mock.is_none() {
        spawn_wake_watchdog();
    }

    // JSONL log-poll task for the active agent (Codex). Restarted on switch.
    let mut log_task: Option<tokio::task::JoinHandle<()>> =
        start_log_poll(profiles.get(&config.active_agent), ev_tx.clone());

    if let Some(policy) = mock {
        // Virtual device: "connect" immediately, auto-answer prompts.
        warn!(
            "running with a MOCK DEVICE (auto-{}) — no BLE",
            policy.decision()
        );
        let link = Link::Mock(MockLink {
            ev_tx: ev_tx.clone(),
            policy,
            answered: Arc::new(Mutex::new(None)),
        });
        let _ = ev_tx.send(Event::Connected(link)).await;
    } else {
        spawn_ble_manager(config.preferred_device.clone(), ev_tx.clone());
    }

    let mut link: Option<Link> = None;
    // Prompts not yet shown/answered. Front of the queue is the one on screen.
    let mut prompts: VecDeque<Pending> = VecDeque::new();
    // Last (ssid, ip, online) the device announced — surfaced to the status UI
    // for OTA and the connectivity indicator. `online` is the device's internet
    // reachability probe result (None until first reported). Cleared on drop.
    let mut last_net: Option<(String, String, Option<bool>)> = None;
    // An in-flight Wi-Fi provisioning request whose CLI/app responder is parked
    // until the device acks storing the creds (and, briefly, joining). At most
    // one at a time — the latest request supersedes any earlier unresolved one.
    let mut pending_wifi: Option<PendingWifi> = None;
    // An in-flight animation-pack push. Holds the ack sink the push task drains;
    // the owner loop forwards the device's char/file/chunk acks into it. At most
    // one push at a time (a second is rejected while this is_some()). Cleared by
    // the task's `PushFinished`, or on disconnect (which drops the sink → the
    // task's recv() ends → it aborts and reports the failure to its CLI).
    let mut pending_push: Option<mpsc::Sender<protocol::Ack>> = None;
    // The last classified reason a connect attempt failed, surfaced in status so
    // the UI can guide the user. Cleared on a successful connect.
    let mut last_connect_error: Option<String> = None;
    // Firmware version the connected buddy announced (`{"fw":...}`). Tied to the
    // live link — cleared on disconnect so we never show a stale version for a
    // device that's gone (or, worse, for a different buddy that connects next).
    let mut last_fw: Option<String> = None;
    // Board id the connected buddy announced (`{"board":...}`), tied to the live
    // link like last_fw — cleared on disconnect. Picks the OTA image per board.
    let mut last_board: Option<String> = None;
    // Latest release the background checker found, if any. Surfaced in status so
    // the app can offer an update; refreshed every UPDATE_CHECK_INTERVAL.
    let mut latest_update: Option<UpdateStatus> = None;
    // Newest firmware available per board (keyed by board id), independent of the
    // app version. build_status picks the entry for the connected board so the
    // app can offer an OTA without an app update. Refreshed on the same cadence.
    let mut firmware_latest: HashMap<String, FirmwareLatest> = HashMap::new();

    info!("owner: {}", config.owner);

    loop {
        // If a Wi-Fi request is parked, race its deadline against the next
        // event so we can resolve it on time even when the device says nothing
        // more. `sleep_until` on a far-future instant when nothing is pending.
        let wifi_deadline = pending_wifi
            .as_ref()
            .map(|w| w.deadline)
            .unwrap_or_else(|| std::time::Instant::now() + Duration::from_secs(3600));
        let ev = tokio::select! {
            ev = ev_rx.recv() => match ev {
                Some(ev) => ev,
                None => break, // all senders dropped — shutting down
            },
            _ = tokio::time::sleep_until(wifi_deadline.into()), if pending_wifi.is_some() => {
                resolve_wifi_deadline(&mut pending_wifi, last_net.as_ref());
                continue;
            }
        };
        match ev {
            Event::Connected(l) => {
                link = Some(l.clone());
                last_connect_error = None;
                on_connect(&l, &config, profiles.get(&config.active_agent)).await;
                push_heartbeat(&link, &state, prompts.front()).await;
            }
            Event::ConnectError(reason) => {
                debug!("connect attempt failed: {reason}");
                last_connect_error = Some(reason);
            }
            Event::Disconnected => {
                warn!("device disconnected");
                // Resolve any parked Wi-Fi request before clearing last_net, and
                // mirror resolve_wifi_deadline's phase logic: in AwaitingJoin the
                // creds are already confirmed-stored (the firmware acked NVS
                // persistence), so a disconnect in the join grace window is a
                // success, not a failure — only AwaitingAck is a real failure.
                if let Some(w) = pending_wifi.take() {
                    match w.phase {
                        WifiPhase::AwaitingJoin => {
                            let joined = last_net.as_ref().map(|(s, _, _)| s.clone());
                            let _ = w.responder.send(AdminResponse::Ok { joined });
                        }
                        WifiPhase::AwaitingAck => {
                            let _ = w.responder.send(AdminResponse::Error {
                                message: "the buddy disconnected before confirming Wi-Fi — try again"
                                    .into(),
                            });
                        }
                    }
                }
                last_net = None;
                last_fw = None;
                last_board = None;
                // Drop any in-flight push sink: the task's recv() ends and it
                // reports the disconnect to its CLI.
                pending_push = None;
                drop_link(&mut link, &mut prompts, &mut state);
            }
            Event::Keepalive => {
                if config_token_day_changed(&mut config, &mut state) {
                    let _ = config.save();
                }
                // Evict idle sessions from harnesses that never emit a
                // terminating event (e.g. Codex), so the maps + counts can't
                // grow unbounded over a long uptime. ~10min idle window.
                state.prune_idle(local_now().0, 600);
                // Proactively notice a vanished buddy: poll the live connection
                // state rather than waiting for the notify stream to close or a
                // heartbeat write to fail (both bounded by the BLE supervision
                // timeout). The instant the link is gone we stop gating, so a
                // powered-off or cased buddy never wedges tool calls on a screen
                // nobody can reach.
                // Bound the liveness poll: a wedged macOS CoreBluetooth handle
                // can leave `is_connected()` hanging, and this runs *in* the
                // single owner loop — an unbounded await here would freeze hook
                // IPC and device decisions too. Treat timeout-or-false as gone,
                // matching the 2s/3s wrappers on the send paths. Authoritative
                // liveness still comes from the LIVENESS_TIMEOUT pong path in
                // spawn_ble_manager; this is the proactive, bounded backstop.
                let gone = match &link {
                    Some(l) => !matches!(
                        tokio::time::timeout(Duration::from_secs(2), l.is_connected()).await,
                        Ok(true)
                    ),
                    None => false,
                };
                if gone {
                    warn!("keepalive: buddy no longer connected — dropping link");
                    last_net = None;
                    last_fw = None;
                    last_board = None;
                    pending_push = None;
                    drop_link(&mut link, &mut prompts, &mut state);
                }
                push_heartbeat(&link, &state, prompts.front()).await;
            }
            Event::DeviceLine(line) => {
                handle_device_line(
                    &line,
                    &mut prompts,
                    &mut state,
                    &mut last_net,
                    &mut last_fw,
                    &mut last_board,
                    &mut pending_wifi,
                    &mut pending_push,
                );
                // A decision may have cleared the front prompt — refresh. But
                // skip during a pack push: each chunk ack arrives as a DeviceLine,
                // and a heartbeat write per ack would both spam the link and
                // contend with the push task's writes (liveness is still kept by
                // the periodic Keepalive heartbeat).
                if pending_push.is_none() {
                    push_heartbeat(&link, &state, prompts.front()).await;
                }
            }
            Event::PromptTimeout(id) => {
                if let Some(pos) = prompts.iter().position(|p| p.id == id) {
                    let p = prompts.remove(pos).unwrap();
                    state.set_waiting(&p.session, false);
                    let _ = p.responder.send(HookResponse::Defer {
                        reason: "device did not respond in time".into(),
                    });
                    push_heartbeat(&link, &state, prompts.front()).await;
                }
            }
            Event::Hook(event, responder) => {
                handle_hook(
                    event,
                    responder,
                    &link,
                    &mut state,
                    &mut config,
                    &mut prompts,
                    &ev_tx,
                )
                .await;
                push_heartbeat(&link, &state, prompts.front()).await;
            }
            Event::DeviceCommand(cmd, responder) => {
                // Wi-Fi is correlated: we write the creds (bounded), then PARK
                // the responder until the device acks storing them — we must
                // NOT await the ack here, because this same owner loop processes
                // the DeviceLine that carries it (awaiting would deadlock). A
                // new request supersedes any earlier unresolved one.
                match cmd {
                    DeviceCommand::Wifi { ssid, pass } => {
                        let Some(l) = &link else {
                            let _ = responder.send(AdminResponse::NoDevice);
                            continue;
                        };
                        let out = OutboundCmd::Wifi { ssid, pass };
                        match tokio::time::timeout(Duration::from_secs(3), l.send(&out)).await {
                            Ok(Ok(())) => {
                                // Supersede any earlier unresolved request.
                                if let Some(prev) = pending_wifi.take() {
                                    let _ = prev.responder.send(AdminResponse::Error {
                                        message: "superseded by a newer Wi-Fi request".into(),
                                    });
                                }
                                pending_wifi = Some(PendingWifi {
                                    responder,
                                    phase: WifiPhase::AwaitingAck,
                                    deadline: std::time::Instant::now() + WIFI_ACK_TIMEOUT,
                                });
                            }
                            Ok(Err(e)) => {
                                let _ = responder.send(AdminResponse::Error {
                                    message: e.to_string(),
                                });
                            }
                            Err(_) => {
                                let _ = responder.send(AdminResponse::Error {
                                    message: "device write timed out".into(),
                                });
                            }
                        }
                    }
                    DeviceCommand::Ota => {
                        // Fire-and-forget: tell the device to enter OTA mode.
                        // It frees BLE+sprite and the link drops as a result, so
                        // we don't wait for an ack on the link we're killing — a
                        // successful write is the confirmation. The daemon then
                        // sees the disconnect and scans until the device reboots
                        // into the new image and re-advertises.
                        let Some(l) = &link else {
                            let _ = responder.send(AdminResponse::NoDevice);
                            continue;
                        };
                        match tokio::time::timeout(
                            Duration::from_secs(3),
                            l.send(&OutboundCmd::Ota),
                        )
                        .await
                        {
                            Ok(Ok(())) => {
                                info!("sent OTA-mode command; device will free heap for the flash");
                                let _ = responder.send(AdminResponse::Ok { joined: None });
                            }
                            Ok(Err(e)) => {
                                let _ = responder.send(AdminResponse::Error {
                                    message: e.to_string(),
                                });
                            }
                            Err(_) => {
                                let _ = responder.send(AdminResponse::Error {
                                    message: "device write timed out".into(),
                                });
                            }
                        }
                    }
                    DeviceCommand::SetAgent { id } => {
                        // Switch the active harness. Validate against loaded
                        // profiles, (re)install hooks for the new agent, persist
                        // the choice, and push its theme to the device. A
                        // disconnected device still updates config + hooks; the
                        // theme applies on next connect.
                        match switch_agent(&mut config, &profiles, &id) {
                            Ok(()) => {
                                // Mirror the active agent for the HTTP listener,
                                // and swap the log-poll task to the new agent.
                                if let Ok(mut a) = active_agent.lock() {
                                    *a = id.clone();
                                }
                                if let Some(h) = log_task.take() {
                                    h.abort();
                                }
                                log_task = start_log_poll(profiles.get(&id), ev_tx.clone());
                                if let Some(l) = &link {
                                    push_theme(l, profiles.get(&id)).await;
                                }
                                let _ = responder.send(AdminResponse::Ok { joined: None });
                            }
                            Err(e) => {
                                let _ = responder.send(AdminResponse::Error {
                                    message: e.to_string(),
                                });
                            }
                        }
                    }
                    // A pack push streams its reply, so serve_hook_conn routes it
                    // to Event::PushPack before it ever becomes a DeviceCommand —
                    // this arm is just exhaustiveness insurance.
                    DeviceCommand::PushPack { .. } => {
                        let _ = responder.send(AdminResponse::Error {
                            message: "internal: pack push reached the oneshot path".into(),
                        });
                    }
                }
            }
            Event::UpdateInfo { app, firmware } => {
                if app.available {
                    info!("app update available: {} (current {})", app.latest, app.current);
                }
                latest_update = Some(app);
                firmware_latest = firmware;
            }
            Event::PushPack {
                id,
                dir,
                set_active,
                updates,
            } => {
                // Spawn the push as its own task so awaiting per-chunk acks never
                // blocks this loop (which dispatches those very acks). The task
                // drains `pending_push`, fed by handle_device_line.
                #[cfg(feature = "pack")]
                {
                    let Some(l) = link.clone() else {
                        let _ = updates.send(PushUpdate::Done(AdminResponse::NoDevice)).await;
                        continue;
                    };
                    if pending_push.is_some() {
                        let _ = updates
                            .send(PushUpdate::Done(AdminResponse::Error {
                                message: "a pack push is already in progress".into(),
                            }))
                            .await;
                        continue;
                    }
                    let (ack_tx, ack_rx) = mpsc::channel::<protocol::Ack>(8);
                    pending_push = Some(ack_tx);
                    tokio::spawn(push_pack_task(
                        l,
                        id,
                        dir,
                        set_active,
                        ack_rx,
                        updates,
                        ev_tx.clone(),
                    ));
                }
                #[cfg(not(feature = "pack"))]
                {
                    let _ = (id, dir, set_active);
                    let _ = updates
                        .send(PushUpdate::Done(AdminResponse::Error {
                            message: "this build has no animation-pack support".into(),
                        }))
                        .await;
                }
            }
            Event::PushFinished { set_active } => {
                pending_push = None;
                // On `--set-active`, point the active agent's theme at the pushed
                // pack so it displays even when its id differs from the active
                // harness. (When the ids already match, the firmware auto-loaded
                // it on char_end — this re-push is then harmlessly idempotent.)
                if let (Some(pack_id), Some(l)) = (set_active, link.clone()) {
                    if let Some(p) = profiles.get(&config.active_agent) {
                        let cmd = theme_cmd_with_pack(p, &pack_id);
                        match tokio::time::timeout(Duration::from_secs(2), l.send(&cmd)).await {
                            Ok(Err(e)) => warn!("theme pack-override failed: {e}"),
                            Err(_) => warn!("theme pack-override timed out"),
                            Ok(Ok(())) => info!("pointed theme at pushed pack '{pack_id}'"),
                        }
                    }
                }
            }
            Event::Query(query, responder) => {
                // A recheck nudge re-arms the checker before we build the reply;
                // the snapshot we return is still the cached one, but the fresh
                // result lands within a second or two and shows on the next poll.
                if matches!(query, Query::RecheckUpdates) {
                    update_recheck.notify_one();
                }
                let resp = match query {
                    Query::Status | Query::RecheckUpdates => QueryResponse::Status(build_status(
                        &config,
                        &state,
                        link.is_some(),
                        last_net.as_ref(),
                        last_fw.as_deref(),
                        last_board.as_deref(),
                        last_connect_error.as_deref(),
                        latest_update.as_ref(),
                        &firmware_latest,
                        &profiles,
                    )),
                };
                let _ = responder.send(resp);
            }
        }
    }
    Ok(())
}

/// Switch the active agent harness: validate the id, swap the installed hooks
/// (uninstall the old harness's, install the new one's), and persist the choice.
/// The theme push + ingestion-task swap are handled by the caller (it owns the
/// link + task handles). Returns an error for an unknown id.
fn switch_agent(
    config: &mut Config,
    profiles: &HashMap<String, AgentProfile>,
    id: &str,
) -> Result<()> {
    let new = profiles
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("unknown agent {id:?}"))?;
    if !new.supported() {
        return Err(anyhow::anyhow!(
            "agent {id:?} uses an unsupported config format and can't be activated"
        ));
    }
    if config.active_agent == id {
        return Ok(()); // already active — nothing to do
    }
    let exe = crate::setup::daemon_exe_path().unwrap_or_else(|_| "agent-buddy".to_string());
    // Install the NEW agent's hooks first; if this fails the OLD agent's hooks
    // are still intact and the buddy keeps working.
    crate::setup::install_profile(new, &exe)
        .with_context(|| format!("installing {id} hooks"))?;
    // Only after the new install succeeds, remove the old agent's hooks
    // (best-effort; any stale/duplicate entries are reconciled on next startup).
    if config.active_agent != id {
        if let Some(old) = profiles.get(&config.active_agent) {
            if let Err(e) = crate::setup::uninstall_profile(old, &exe) {
                warn!("uninstalling {} hooks failed: {e}", old.id);
            }
        }
    }
    config.active_agent = id.to_string();
    config.save().context("saving active agent")?;
    info!("switched active agent to {id}");
    Ok(())
}

/// Assemble the snapshot the desktop UI renders from the owner loop's state.
#[allow(clippy::too_many_arguments)]
fn build_status(
    config: &Config,
    state: &SessionState,
    device_connected: bool,
    last_net: Option<&(String, String, Option<bool>)>,
    last_fw: Option<&str>,
    last_board: Option<&str>,
    last_connect_error: Option<&str>,
    latest_update: Option<&UpdateStatus>,
    firmware_latest: &HashMap<String, FirmwareLatest>,
    profiles: &HashMap<String, AgentProfile>,
) -> StatusReport {
    let now = local_now().0;
    StatusReport {
        device_connected,
        owner: config.owner.clone(),
        // Persona one-shot signals for the desktop widget (mirrors the device's
        // celebrate/dizzy pulses). Windowed in `SessionState`; recomputed here.
        recently_completed: state.recently_completed(now),
        recent_error: state.recent_error(now),
        active_agent: config.active_agent.clone(),
        available_agents: agent::list_agents(profiles),
        tokens_today: state.tokens_today,
        tokens_total: state.tokens,
        sessions_total: state.total(),
        sessions_running: state.running(),
        sessions_waiting: state.waiting(),
        entries: state.entries().to_vec(),
        device_ssid: last_net.map(|(s, _, _)| s.clone()),
        device_ip: last_net.map(|(_, ip, _)| ip.clone()),
        device_online: last_net.and_then(|(_, _, o)| *o),
        device_fw: last_fw.map(str::to_string),
        device_board: last_board.map(str::to_string),
        update: latest_update.cloned(),
        // Firmware available for the connected board (older firmware that omits
        // its board id is treated as the default). Only meaningful while linked.
        firmware_latest: if device_connected {
            firmware_latest
                .get(last_board.unwrap_or(crate::ota::DEFAULT_BOARD))
                .cloned()
        } else {
            None
        },
        // Bluetooth availability/permission aren't directly observable from the
        // owner loop (the adapter handle lives in the BLE manager). We infer
        // from the latest connect error's classification instead: a clear
        // "Bluetooth off / not permitted" reason sets these, otherwise unknown.
        bluetooth_available: device_connected || !connect_error_is_bluetooth(last_connect_error),
        bluetooth_permitted: connect_error_permission_hint(last_connect_error),
        // Only meaningful while disconnected; a live link cleared it already.
        last_connect_error: if device_connected {
            None
        } else {
            last_connect_error.map(str::to_string)
        },
    }
}

/// Does the connect-error reason point at Bluetooth being off / unavailable
/// (as opposed to "buddy not found / asleep")? Drives `bluetooth_available`.
fn connect_error_is_bluetooth(reason: Option<&str>) -> bool {
    let Some(r) = reason else { return false };
    let r = r.to_ascii_lowercase();
    r.contains("bluetooth") || r.contains("adapter") || r.contains("powered")
}

/// Best-effort permission hint from the connect-error text. `Some(false)` when
/// the OS signalled an authorization problem; `None` when we can't tell.
fn connect_error_permission_hint(reason: Option<&str>) -> Option<bool> {
    let r = reason?.to_ascii_lowercase();
    if r.contains("permit") || r.contains("unauthorized") || r.contains("not authorized") {
        Some(false)
    } else {
        None
    }
}

/// Build the device theme command for an agent profile (colors → RGB565,
/// panels/caps → bitmasks, pack id).
fn theme_cmd(p: &AgentProfile) -> OutboundCmd {
    OutboundCmd::Theme {
        id: p.id.clone(),
        label: p.name.clone(),
        pal: p.palette.base_rgb565(),
        hot: p.palette.hot.rgb565(),
        panel: p.palette.panel.rgb565(),
        sel: p.palette.sel.rgb565(),
        ok: p.palette.ok.rgb565(),
        panels: p.panel_bits(),
        caps: p.capabilities.bits(),
        pack: Some(p.pack().to_string()),
    }
}

/// The active agent's theme, but with its `pack` field overridden — used after a
/// `pack push --set-active` so a pack whose id differs from the active harness
/// still displays (the firmware loads `theme().pack`).
fn theme_cmd_with_pack(p: &AgentProfile, pack: &str) -> OutboundCmd {
    match theme_cmd(p) {
        OutboundCmd::Theme {
            id,
            label,
            pal,
            hot,
            panel,
            sel,
            ok,
            panels,
            caps,
            ..
        } => OutboundCmd::Theme {
            id,
            label,
            pal,
            hot,
            panel,
            sel,
            ok,
            panels,
            caps,
            pack: Some(pack.to_string()),
        },
        other => other,
    }
}

/// Push the active agent's theme to the device (bounded like the other syncs).
/// No-op when the profile is missing — the device keeps its last/default theme.
async fn push_theme(link: &Link, profile: Option<&AgentProfile>) {
    let Some(p) = profile else { return };
    let cmd = theme_cmd(p);
    if let Ok(Err(e)) = tokio::time::timeout(Duration::from_secs(2), link.send(&cmd)).await {
        warn!("theme sync failed: {e}");
    }
}

/// On a fresh connection, sync time + owner + theme so the device clock,
/// greeting, and per-agent palette/animation are correct from the first frame.
async fn on_connect(link: &Link, config: &Config, profile: Option<&AgentProfile>) {
    let (epoch, offset, _today) = local_now();
    let time = TimeSync {
        time: (epoch, offset),
    };
    let send_time = tokio::time::timeout(Duration::from_secs(2), link.send(&time));
    if let Ok(Err(e)) = send_time.await {
        warn!("time sync failed: {e}");
    }
    let owner = OutboundCmd::Owner {
        name: config.owner.clone(),
    };
    let send_owner = tokio::time::timeout(Duration::from_secs(2), link.send(&owner));
    if let Ok(Err(e)) = send_owner.await {
        warn!("owner sync failed: {e}");
    }
    push_theme(link, profile).await;
}

/// Translate a hook event into state changes and (for PermissionRequest) a
/// device prompt whose decision is relayed back to the hook.
async fn handle_hook(
    event: HookEvent,
    responder: oneshot::Sender<HookResponse>,
    link: &Option<Link>,
    state: &mut SessionState,
    config: &mut Config,
    prompts: &mut VecDeque<Pending>,
    ev_tx: &mpsc::Sender<Event>,
) {
    // Stamp the session's wall-clock last-seen so idle eviction (Keepalive) can
    // drop sessions from harnesses that never emit a terminating event.
    state.touch(event.session_id(), local_now().0);
    match event {
        HookEvent::SessionStart { session_id, cwd } => {
            state.session_started(&session_id);
            state.set_cwd(&session_id, &cwd);
            let _ = responder.send(HookResponse::Ack);
        }
        HookEvent::SessionEnd { session_id } => {
            state.session_ended(&session_id);
            let _ = responder.send(HookResponse::Ack);
        }
        HookEvent::UserPromptSubmit { session_id, cwd } => {
            state.set_running(&session_id, true);
            state.set_cwd(&session_id, &cwd);
            let _ = responder.send(HookResponse::Ack);
        }
        HookEvent::Stop {
            session_id,
            session_total_tokens,
            summary,
            final_turn,
            model,
            ctx_tokens,
            cwd,
        } => {
            let (epoch, _, today) = local_now();
            // Fold in only the newly-seen tokens for this session (dedupes
            // repeated Stop/SubagentStop for the same transcript) and refresh
            // its per-session telemetry (model, context size, project).
            state.record_turn(
                &session_id,
                session_total_tokens,
                &model,
                ctx_tokens,
                &cwd,
                &today,
            );
            state.sync_to_config(config);
            let _ = config.save();
            // Only a real Stop ends the turn. A subagent finishing leaves the
            // parent session running.
            if final_turn {
                state.set_running(&session_id, false);
                state.mark_completed(epoch + 5); // ~5s celebrate window
            }
            if let Some(s) = summary {
                // The ingest layer collapses a NormState::Error turn into a Stop
                // carrying the literal "error" marker (ingest.rs). Use it to arm
                // the desktop widget's dizzy one-shot; real summaries are just
                // pushed to the ticker.
                if s == "error" {
                    state.mark_error(epoch + 3); // ~3s dizzy window
                }
                state.push_entry(s);
            }
            let _ = responder.send(HookResponse::Ack);
        }
        HookEvent::Telemetry {
            session_id,
            session_total_tokens,
            model,
            ctx_tokens,
            cwd,
        } => {
            // Every PreToolUse fires this (gating moved to PermissionRequest):
            // update the live readout only — no prompt, no running/waiting
            // change, no config.save (Stop persists). Folds tokens via the same
            // deduped path, so it can't double-count against the eventual Stop.
            let (_, _, today) = local_now();
            state.record_turn(
                &session_id,
                session_total_tokens,
                &model,
                ctx_tokens,
                &cwd,
                &today,
            );
            let _ = responder.send(HookResponse::Ack);
        }
        HookEvent::Notification {
            session_id: _,
            message,
        } => {
            // A notification (e.g. "needs your input") can fire mid-turn, so it
            // must NOT flip the session to idle. Just surface the text.
            if let Some(m) = message {
                state.push_entry(m);
            }
            let _ = responder.send(HookResponse::Ack);
        }
        HookEvent::PermissionRequest {
            session_id,
            tool,
            hint,
            mode,
            cwd,
        } => {
            // Claude only raises this when it would genuinely prompt, so reaching
            // here means the real session is gating. Telemetry was already
            // refreshed by the `Telemetry` event from the `PreToolUse` that
            // precedes every prompt, so this arm only drives the device prompt.
            // No device? Defer immediately so the user is never blocked.
            if link.is_none() {
                let _ = responder.send(HookResponse::Defer {
                    reason: "no buddy connected".into(),
                });
                return;
            }
            let id = format!("req_{}", uuid::Uuid::new_v4().simple());
            state.set_waiting(&session_id, true);
            state.set_cwd(&session_id, &cwd);
            state.push_entry(format!("approve: {tool}"));
            prompts.push_back(Pending {
                id: id.clone(),
                tool,
                hint,
                mode,
                session: session_id,
                responder,
            });
            // Arm a TTL so a forgotten prompt can't wedge a session.
            let ev_tx = ev_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(PROMPT_TTL).await;
                let _ = ev_tx.send(Event::PromptTimeout(id)).await;
            });
        }
    }
}

/// Force the link down and release any waiting hooks with a Defer. Called both
/// on an observed BLE disconnect and when a keepalive poll finds the link gone,
/// so a powered-off / out-of-range buddy stops gating within a heartbeat
/// instead of after the full BLE supervision timeout. Deferred (not denied) so
/// the freed calls fall through to Claude Code's normal permission flow.
fn drop_link(link: &mut Option<Link>, prompts: &mut VecDeque<Pending>, state: &mut SessionState) {
    *link = None;
    for p in prompts.drain(..) {
        let _ = p.responder.send(HookResponse::Defer {
            reason: "device disconnected".into(),
        });
        state.set_waiting(&p.session, false);
    }
}

/// Parse one device line; resolve a matching pending prompt on a decision,
/// correlate a Wi-Fi store ack/join with a parked provisioning request, route a
/// pack push's transfer acks, and record any Wi-Fi the device announces joining.
#[allow(clippy::too_many_arguments)] // cohesive owner-loop state, not a config bag
fn handle_device_line(
    line: &str,
    prompts: &mut VecDeque<Pending>,
    state: &mut SessionState,
    last_net: &mut Option<(String, String, Option<bool>)>,
    last_fw: &mut Option<String>,
    last_board: &mut Option<String>,
    pending_wifi: &mut Option<PendingWifi>,
    pending_push: &mut Option<mpsc::Sender<protocol::Ack>>,
) {
    match protocol::parse_inbound(line) {
        Ok(Inbound::Permission(d)) => {
            if let Some(pos) = prompts.iter().position(|p| p.id == d.id) {
                let p = prompts.remove(pos).unwrap();
                state.set_waiting(&p.session, false);
                let allow = d.decision == Decision::Once;
                info!(
                    "device {} {}",
                    if allow { "approved" } else { "denied" },
                    p.tool
                );
                let _ = p.responder.send(HookResponse::Decision { allow });
            } else {
                debug!("decision for unknown/expired prompt {}", d.id);
            }
        }
        Ok(Inbound::Ack(a)) => {
            // An in-flight pack push owns the char/file transfer acks — forward
            // them to its task (which is awaiting them for flow control) instead
            // of the generic logging below. try_send can't block this loop; a
            // closed channel means the task ended, so drop the stale sink.
            let is_push_ack = matches!(
                a.ack.as_str(),
                "char_begin" | "file" | "chunk" | "file_end" | "char_end"
            );
            if is_push_ack && pending_push.is_some() {
                let failed = pending_push
                    .as_ref()
                    .map(|tx| tx.try_send(a.clone()).is_err())
                    .unwrap_or(true);
                if failed {
                    *pending_push = None;
                }
            } else if a.ack == "wifi" {
                // Correlate with the parked provisioning request. The firmware's
                // ok reflects *verified NVS persistence* (see firmware xfer.h /
                // net.cpp), so only ok:true means the creds will survive a reboot.
                if let Some(w) = pending_wifi.take() {
                    if a.ok {
                        info!("buddy confirmed storing Wi-Fi credentials");
                        // Briefly wait for a join announcement to enrich the
                        // reply, but the store is already confirmed-good.
                        *pending_wifi = Some(PendingWifi {
                            responder: w.responder,
                            phase: WifiPhase::AwaitingJoin,
                            deadline: std::time::Instant::now() + WIFI_JOIN_GRACE,
                        });
                    } else {
                        let msg = a.error.clone().unwrap_or_else(|| {
                            "the buddy could not store the Wi-Fi credentials".into()
                        });
                        warn!("buddy nacked wifi: {msg}");
                        let _ = w.responder.send(AdminResponse::Error { message: msg });
                    }
                } else if !a.ok {
                    warn!("device nacked wifi (no pending request): {:?}", a.error);
                }
            } else if !a.ok {
                warn!("device nacked {}: {:?}", a.ack, a.error);
            } else {
                debug!("ack {}", a.ack);
            }
        }
        Ok(Inbound::Other(v)) => {
            // The device announces its WiFi address on connect (net.cpp). Surface
            // it at info so the user can find the buddy for an OTA upload.
            if let Some(net) = v.get("net") {
                if net.get("connected") == Some(&true.into()) {
                    let ip = net.get("ip").and_then(|s| s.as_str()).unwrap_or("?");
                    let ssid = net.get("ssid").and_then(|s| s.as_str()).unwrap_or("?");
                    // Internet reachability (vs merely associated). Absent on older
                    // firmware → None (unknown), shown by the app as just "joined".
                    let online = net.get("online").and_then(|b| b.as_bool());
                    info!("buddy joined WiFi \"{ssid}\" at {ip} (internet: {}) — OTA: --upload-port {ip} (or buddy.local)",
                          match online { Some(true) => "yes", Some(false) => "no", None => "?" });
                    *last_net = Some((ssid.to_string(), ip.to_string(), online));
                    // If a provisioning request is in its join-grace phase, resolve
                    // it now with the richer "joined <ssid>" outcome.
                    if matches!(
                        pending_wifi.as_ref().map(|w| &w.phase),
                        Some(WifiPhase::AwaitingJoin)
                    ) {
                        if let Some(w) = pending_wifi.take() {
                            let _ = w.responder.send(AdminResponse::Ok {
                                joined: Some(ssid.to_string()),
                            });
                        }
                    }
                } else if net.get("connected") == Some(&false.into()) {
                    info!("buddy left WiFi; clearing OTA address");
                    *last_net = None;
                }
            } else if let Some(fw) = v.get("fw").and_then(|f| f.as_str()) {
                // The device announces its firmware version + board id on BLE
                // connect (`{"fw":"v0.1.0","board":"cyd"}`). Record both so the
                // app can compare the version against the image it bundles for
                // that board and offer an OTA update. Log only on a change — the
                // device re-announces a couple times per connect.
                if last_fw.as_deref() != Some(fw) {
                    info!("buddy firmware version: {fw}");
                }
                *last_fw = Some(fw.to_string());
                // `board` rides the same announce; older firmware omits it.
                if let Some(board) = v.get("board").and_then(|b| b.as_str()) {
                    if last_board.as_deref() != Some(board) {
                        info!("buddy board: {board}");
                    }
                    *last_board = Some(board.to_string());
                }
            } else {
                debug!("device: {v}");
            }
        }
        Err(e) => debug!("unparseable device line {line:?}: {e}"),
    }
}

/// How long to wait for a control ack (`char_begin`/`file`/`file_end`/
/// `char_end`). Generous: a `file` open or `char_begin` mkdir touches LittleFS.
#[cfg(feature = "pack")]
const PUSH_CTRL_ACK_TIMEOUT: Duration = Duration::from_secs(8);
/// How long to wait for a single `chunk` ack. Usually milliseconds, but a
/// LittleFS flash-erase between blocks can stall a write briefly.
#[cfg(feature = "pack")]
const PUSH_CHUNK_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Drive the full `char_begin → (file → chunk* → file_end)* → char_end` push,
/// then report completion to the CLI stream and clear the owner loop's
/// `pending_push`. Runs as its own task: it `await`s acks (fed by the owner loop
/// via `ack_rx`) without ever blocking that loop.
#[cfg(feature = "pack")]
async fn push_pack_task(
    link: Link,
    id: String,
    dir: std::path::PathBuf,
    set_active: bool,
    mut ack_rx: mpsc::Receiver<protocol::Ack>,
    updates: mpsc::Sender<PushUpdate>,
    ev_tx: mpsc::Sender<Event>,
) {
    let (resp, set_active_id) = match run_push(&link, &id, &dir, &mut ack_rx, &updates).await {
        Ok(()) => (
            AdminResponse::Ok { joined: None },
            if set_active { Some(id) } else { None },
        ),
        Err(e) => (AdminResponse::Error { message: e.to_string() }, None),
    };
    // Tell the owner loop first (clears pending_push + applies set_active), then
    // resolve the CLI's stream.
    let _ = ev_tx
        .send(Event::PushFinished {
            set_active: set_active_id,
        })
        .await;
    let _ = updates.send(PushUpdate::Done(resp)).await;
}

/// The push protocol itself. Errors carry a user-facing "retry the push" hint.
#[cfg(feature = "pack")]
async fn run_push(
    link: &Link,
    id: &str,
    dir: &std::path::Path,
    ack_rx: &mut mpsc::Receiver<protocol::Ack>,
    updates: &mpsc::Sender<PushUpdate>,
) -> Result<()> {
    use base64::Engine;
    // Collect the state files present, in PersonaState order.
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for state in crate::packs::STATE_NAMES {
        let p = dir.join(format!("{state}.spr"));
        if let Ok(bytes) = tokio::fs::read(&p).await {
            files.push((format!("{state}.spr"), bytes));
        }
    }
    if files.is_empty() {
        anyhow::bail!("no <state>.spr files in {} — build the pack first", dir.display());
    }
    let total: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();

    link.send(&OutboundCmd::CharBegin {
        name: id.to_string(),
        total,
    })
    .await
    .context("sending char_begin")?;
    await_push_ack(ack_rx, "char_begin", PUSH_CTRL_ACK_TIMEOUT).await?;

    let mut done: u64 = 0;
    for (path, bytes) in &files {
        link.send(&OutboundCmd::File {
            path: path.clone(),
            size: bytes.len() as u64,
        })
        .await
        .with_context(|| format!("sending file header for {path}"))?;
        await_push_ack(ack_rx, "file", PUSH_CTRL_ACK_TIMEOUT)
            .await
            .with_context(|| format!("opening {path} on the buddy"))?;

        // Chunk at the device's decode-buffer cap (mirrors packs::spr_to_chunks,
        // tracking each slice's length for progress).
        for slice in bytes.chunks(crate::packs::CHUNK_BYTES) {
            let d = base64::engine::general_purpose::STANDARD.encode(slice);
            link.send(&OutboundCmd::Chunk { d })
                .await
                .with_context(|| format!("sending a chunk of {path}"))?;
            await_push_ack(ack_rx, "chunk", PUSH_CHUNK_ACK_TIMEOUT)
                .await
                .with_context(|| format!("writing {path}"))?;
            done += slice.len() as u64;
            let _ = updates
                .send(PushUpdate::Progress {
                    done,
                    total,
                    file: path.clone(),
                })
                .await;
        }

        link.send(&OutboundCmd::FileEnd)
            .await
            .with_context(|| format!("finishing {path}"))?;
        await_push_ack(ack_rx, "file_end", PUSH_CTRL_ACK_TIMEOUT)
            .await
            .with_context(|| format!("closing {path} on the buddy"))?;
    }

    link.send(&OutboundCmd::CharEnd).await.context("sending char_end")?;
    await_push_ack(ack_rx, "char_end", PUSH_CTRL_ACK_TIMEOUT).await?;
    Ok(())
}

/// Await the next push ack, asserting it's the one expected and `ok`.
#[cfg(feature = "pack")]
async fn await_push_ack(
    ack_rx: &mut mpsc::Receiver<protocol::Ack>,
    expect: &str,
    timeout: Duration,
) -> Result<()> {
    let ack = tokio::time::timeout(timeout, ack_rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("the buddy stopped acking ({expect}) — retry the push"))?
        .ok_or_else(|| anyhow::anyhow!("the buddy disconnected mid-push — retry"))?;
    if ack.ack != expect {
        anyhow::bail!("protocol desync: got ack '{}' while awaiting '{expect}'", ack.ack);
    }
    if !ack.ok {
        anyhow::bail!(
            "the buddy rejected {expect}: {}",
            ack.error.unwrap_or_else(|| "no reason given".into())
        );
    }
    Ok(())
}

/// A parked Wi-Fi request hit its phase deadline with no further device word.
/// In `AwaitingAck` the device never confirmed storing the creds → error. In
/// `AwaitingJoin` the store already succeeded; we just didn't see a join in the
/// grace window → report success (stored, join not yet observed).
fn resolve_wifi_deadline(
    pending_wifi: &mut Option<PendingWifi>,
    last_net: Option<&(String, String, Option<bool>)>,
) {
    let Some(w) = pending_wifi.take() else { return };
    match w.phase {
        WifiPhase::AwaitingAck => {
            warn!("buddy did not confirm storing Wi-Fi within the timeout");
            let _ = w.responder.send(AdminResponse::Error {
                message: "the buddy did not confirm storing Wi-Fi — try again".into(),
            });
        }
        WifiPhase::AwaitingJoin => {
            // Stored OK; surface a join only if we happen to already know one.
            let joined = last_net.map(|(s, _, _)| s.clone());
            let _ = w.responder.send(AdminResponse::Ok { joined });
        }
    }
}

/// Build and send a heartbeat reflecting current state and the on-screen
/// prompt (front of the queue), if any. Returns false if the write failed so
/// the caller can tear down a dead link.
///
/// Strings are truncated to the device's fixed buffer sizes (TamaState in the
/// firmware: msg[24], lines[8][92], promptTool[20], promptHint[44]).
async fn push_heartbeat(link: &Option<Link>, state: &SessionState, front: Option<&Pending>) {
    let Some(link) = link else { return };

    let prompt = front.map(|p| PromptPayload {
        id: p.id.clone(),
        tool: truncate(&p.tool, 19),
        hint: truncate(&p.hint, 43),
        // "default" carries no extra UI meaning; only send a non-default mode.
        mode: if p.mode == "default" {
            String::new()
        } else {
            p.mode.clone()
        },
        sid: SessionState::short_id_of(&p.session),
    });

    let msg = if let Some(p) = front {
        format!("approve: {}", p.tool)
    } else if state.running() > 0 {
        state
            .entries()
            .first()
            .cloned()
            .unwrap_or_else(|| "working…".into())
    } else if state.total() > 0 {
        "idle".into()
    } else {
        "zzz".into()
    };

    let entries: Vec<String> = state
        .entries()
        .iter()
        .take(8)
        .map(|e| truncate(e, 91))
        .collect();

    // Per-session detail for the device's session picker. cwd/model are already
    // short, but truncate to the firmware's fixed buffers to be safe.
    let sessions = state
        .sessions_snapshot()
        .into_iter()
        .map(|mut s| {
            s.cwd = truncate(&s.cwd, 15);
            s.m = truncate(&s.m, 7);
            s
        })
        .collect();

    let now = local_now().0;
    let mut hb = Heartbeat {
        total: state.total(),
        running: state.running(),
        waiting: state.waiting(),
        completed: state.recently_completed(now),
        msg: truncate(&msg, 23),
        entries,
        tokens: state.tokens,
        tokens_today: state.tokens_today,
        sessions,
        prompt,
    };

    // Hard byte budget. Per-field truncation bounds each string, but the
    // *assembled* line can still overrun the firmware's fixed line buffer once
    // many entries/sessions (especially multibyte) stack up — and the firmware
    // drops an over-length line whole, so the entire heartbeat would silently
    // vanish. Shed the largest variable contributors until we're under budget:
    // entries first (the ticker degrades gracefully — fewer recent lines), then
    // sessions. The prompt and counters are load-bearing and never shed.
    enforce_line_budget(&mut hb);

    // Bound the write so a hung BLE link can't stall the owner loop (which
    // also services device decisions and hook IPC). On failure, force the link
    // down so the BLE manager reconnects — otherwise a half-open link would
    // leave hooks waiting and the device frozen.
    match tokio::time::timeout(Duration::from_secs(2), link.send(&hb)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            warn!("heartbeat write failed: {e} — dropping link");
            let link = link.clone();
            tokio::spawn(async move { link.disconnect().await });
        }
        Err(_) => {
            warn!("heartbeat write timed out — dropping link");
            let link = link.clone();
            tokio::spawn(async move { link.disconnect().await });
        }
    }
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

/// Expand a leading `~/` to the user's home directory.
fn expand_tilde(p: &str) -> std::path::PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(b) = directories::BaseDirs::new() {
            return b.home_dir().join(rest);
        }
    }
    std::path::PathBuf::from(p)
}

/// A trivial single-`*` glob match (`rollout-*.jsonl`), enough for log patterns.
fn glob_match(name: &str, pat: &str) -> bool {
    match pat.split_once('*') {
        Some((pre, suf)) => {
            name.len() >= pre.len() + suf.len() && name.starts_with(pre) && name.ends_with(suf)
        }
        None => name == pat,
    }
}

/// Spawn the JSONL log-poll task for `profile` if it declares a `log_config`
/// (Codex). Returns the handle so the owner loop can abort it on agent switch.
/// `None` when the agent doesn't log-poll.
fn start_log_poll(
    profile: Option<&AgentProfile>,
    ev_tx: mpsc::Sender<Event>,
) -> Option<tokio::task::JoinHandle<()>> {
    let profile = profile?;
    profile.log_config.as_ref()?;
    let profile = profile.clone();
    Some(tokio::spawn(log_poll_loop(profile, ev_tx)))
}

/// Tail a harness's rolling JSONL session logs, classify each new line via the
/// profile's `log_event_map`, and feed the resulting state into the owner loop.
/// Only ingests lines written *after* the daemon started (or after a file first
/// appears) so launching doesn't replay the whole session history.
async fn log_poll_loop(profile: AgentProfile, ev_tx: mpsc::Sender<Event>) {
    let cfg = profile.log_config.clone().expect("log_config present");
    let dir = expand_tilde(&cfg.session_dir);
    let interval = Duration::from_millis(cfg.poll_interval_ms.max(250));
    // Per file: byte offset already consumed + the session cwd (recovered from
    // the file's session_meta line, which only appears once).
    let mut offsets: HashMap<std::path::PathBuf, (usize, String)> = HashMap::new();
    let mut first_cycle = true;
    loop {
        tokio::time::sleep(interval).await;
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue, // dir not there yet — keep waiting
        };
        let mut seen: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !glob_match(&name, &cfg.file_pattern) {
                continue;
            }
            let path = entry.path();
            seen.insert(path.clone());
            let data = match std::fs::read(&path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            // A stable session id per rollout file: its trailing UUID (the last
            // 5 hyphen-delimited segments of the stem), else the stem itself.
            let session_id = format!("{}:{}", profile.id, codex_session_id(&path));
            // Seed offset: pre-existing files at startup start at end (no replay);
            // files first seen later are new sessions → start at 0.
            let entry_state = offsets
                .entry(path.clone())
                .or_insert_with(|| (if first_cycle { data.len() } else { 0 }, String::new()));
            if data.len() < entry_state.0 {
                entry_state.0 = 0; // truncated/rotated
            }
            // Process only complete lines; advance offset past the last newline.
            let chunk = &data[entry_state.0..];
            let Some(last_nl) = chunk.iter().rposition(|&b| b == b'\n') else {
                continue;
            };
            let text = String::from_utf8_lossy(&chunk[..=last_nl]);
            for line in text.lines() {
                let (norm, line_cwd) = crate::ingest::classify_log_line(&profile, line);
                if let Some(cwd) = line_cwd {
                    entry_state.1 = cwd; // cache the session cwd for later lines
                }
                if let Some(norm) = norm {
                    if let Some(ev) = crate::ingest::normalized_to_hook_event(
                        norm,
                        session_id.clone(),
                        entry_state.1.clone(),
                        None,
                        profile.capabilities.permission_approval,
                    ) {
                        let (tx, _rx) = oneshot::channel();
                        if ev_tx.send(Event::Hook(ev, tx)).await.is_err() {
                            return; // owner loop gone
                        }
                    }
                }
            }
            entry_state.0 += last_nl + 1;
        }
        // Drop offsets for files that no longer exist so they don't accumulate
        // over a long uptime (rotated/deleted rollouts).
        offsets.retain(|p, _| seen.contains(p));
        first_cycle = false;
    }
}

/// Derive a stable session id from a Codex rollout filename: the trailing UUID
/// (last 5 hyphen-delimited segments of the file stem, e.g.
/// `rollout-2026-06-05T...-<8>-<4>-<4>-<4>-<12>`), falling back to the whole stem.
fn codex_session_id(path: &std::path::Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    let segs: Vec<&str> = stem.split('-').collect();
    if segs.len() >= 5 {
        segs[segs.len() - 5..].join("-")
    } else {
        stem.to_string()
    }
}

/// Accept loopback HTTP POSTs from plugin/extension harnesses and feed mapped
/// events into the owner loop. Stateless per-request: the POST body names its
/// own `agent` + `event`; we only ingest when that agent is the *active* one
/// (mirrored in `active`) so a background harness can't drive the one device.
fn spawn_http_ingest(
    listener: TcpListener,
    token: String,
    profiles: Arc<HashMap<String, AgentProfile>>,
    active: Arc<Mutex<String>>,
    ev_tx: mpsc::Sender<Event>,
) {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let token = token.clone();
                    let profiles = profiles.clone();
                    let active = active.clone();
                    let ev_tx = ev_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_http_conn(stream, &token, profiles, active, &ev_tx).await
                        {
                            debug!("http ingest conn error: {e}");
                        }
                    });
                }
                Err(e) => {
                    warn!("http ingest accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });
}

/// Handle one HTTP request: read headers + body, check the token, map the event,
/// and post it to the owner loop. Always replies `200 ok` so a plugin never
/// hangs; gating/ignoring happens silently.
/// Upper bound on bytes read from a single local IPC/HTTP connection: comfortably
/// above the largest legitimate request (a ≤64KB body plus small headers/JSON),
/// but a hard ceiling so a misbehaving local client can't grow the daemon's heap.
const IPC_MAX_CONN_BYTES: u64 = 256 * 1024;

async fn serve_http_conn(
    stream: TcpStream,
    token: &str,
    profiles: Arc<HashMap<String, AgentProfile>>,
    active: Arc<Mutex<String>>,
    ev_tx: &mpsc::Sender<Event>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    // Bound the whole connection's bytes so a local client can't stream unbounded
    // data into the daemon's heap (the body is separately capped at 64KB; this
    // caps the header lines too). Loopback + token-gated, so this is just defense.
    let mut reader = BufReader::new(tokio::io::AsyncReadExt::take(read_half, IPC_MAX_CONN_BYTES));

    // Request line + headers.
    let mut content_length = 0usize;
    let mut req_token = String::new();
    let mut line = String::new();
    // First line (method/path) — read and ignore.
    reader.read_line(&mut line).await?;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break; // end of headers
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = lower.strip_prefix("x-buddy-token:") {
            req_token = v.trim().to_string();
        }
    }

    let mut body = vec![0u8; content_length.min(64 * 1024)];
    if !body.is_empty() {
        tokio::io::AsyncReadExt::read_exact(&mut reader, &mut body).await?;
    }

    let reply = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
    let _ = write_half.write_all(reply).await;
    let _ = write_half.flush().await;

    // Token may ride a header or the JSON body; require a match either way.
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let body_token = payload.get("token").and_then(|v| v.as_str()).unwrap_or("");
    if req_token != token && body_token != token {
        debug!("http ingest: bad token, ignoring");
        return Ok(());
    }

    let agent_id = payload
        .get("agent")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // Only the active agent drives the device.
    if active.lock().map(|a| *a != agent_id).unwrap_or(true) {
        return Ok(());
    }
    let Some(profile) = profiles.get(&agent_id) else {
        return Ok(());
    };
    let name = crate::ingest::event_name(
        payload.get("event").and_then(|v| v.as_str()).unwrap_or(""),
        &payload,
    );
    // Namespace the session id per agent so distinct harnesses (and the
    // synthetic "default") can't alias the same SessionState entry.
    let mut payload = payload;
    let raw_sid = crate::ingest::session_id(&payload);
    let namespaced = namespace_session(&agent_id, &raw_sid);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("session_id".to_string(), Value::String(namespaced));
    }
    if let Some(ev) = crate::ingest::map_hook_event(profile, &name, &payload) {
        let (tx, _rx) = oneshot::channel();
        let _ = ev_tx.send(Event::Hook(ev, tx)).await;
    }
    Ok(())
}

/// Prefix a raw session id with its agent so cross-harness ids never alias the
/// shared SessionState map. Idempotent: an already-prefixed id is left as-is.
fn namespace_session(agent_id: &str, raw: &str) -> String {
    let prefix = format!("{agent_id}:");
    if raw.starts_with(&prefix) {
        raw.to_string()
    } else {
        format!("{agent_id}:{raw}")
    }
}

use serde_json::Value;

/// Accept hook connections; each sends one [`HookRequest`] line and reads one
/// [`HookResponse`] line.
fn spawn_ipc_accept(listener: TcpListener, token: String, ev_tx: mpsc::Sender<Event>) {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let token = token.clone();
                    let ev_tx = ev_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_hook_conn(stream, &token, &ev_tx).await {
                            debug!("hook connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    error!("IPC accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });
}

async fn serve_hook_conn(
    stream: TcpStream,
    token: &str,
    ev_tx: &mpsc::Sender<Event>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    // Cap total bytes so a local client can't stream gigabytes into one line
    // within the read timeout (loopback + token-gated; defense in depth).
    let mut reader = BufReader::new(tokio::io::AsyncReadExt::take(read_half, IPC_MAX_CONN_BYTES));
    let mut line = String::new();
    // Bound the read so a connected-but-silent client can't park a task.
    tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
        .await
        .map_err(|_| anyhow::anyhow!("hook client read timed out"))??;

    let trimmed = line.trim();

    // Two client shapes share this socket. HookRequest is the hot path, so try
    // it first; an AdminRequest (which has `command`, not `event`) fails that
    // parse and falls through. The two are structurally disjoint, so there's no
    // ambiguity. Each gets its own response type serialized back.
    if let Ok(req) = serde_json::from_str::<HookRequest>(trimmed) {
        let resp = if req.token == token {
            let (tx, rx) = oneshot::channel();
            if ev_tx.send(Event::Hook(req.event, tx)).await.is_err() {
                HookResponse::Error {
                    message: "daemon shutting down".into(),
                }
            } else {
                rx.await.unwrap_or(HookResponse::Defer {
                    reason: "daemon dropped the request".into(),
                })
            }
        } else {
            HookResponse::Error {
                message: "bad token".into(),
            }
        };
        return write_response(&mut write_half, &resp).await;
    }

    if let Ok(req) = serde_json::from_str::<AdminRequest>(trimmed) {
        if req.token != token {
            return write_response(
                &mut write_half,
                &AdminResponse::Error {
                    message: "bad token".into(),
                },
            )
            .await;
        }
        // A pack push streams progress, so it gets its own multi-line path; every
        // other admin command is a single parked-oneshot reply.
        if let DeviceCommand::PushPack {
            id,
            dir,
            set_active,
        } = req.command
        {
            return stream_push(&mut write_half, ev_tx, id, dir, set_active).await;
        }
        let resp = {
            let (tx, rx) = oneshot::channel();
            if ev_tx
                .send(Event::DeviceCommand(req.command, tx))
                .await
                .is_err()
            {
                AdminResponse::Error {
                    message: "daemon shutting down".into(),
                }
            } else {
                rx.await.unwrap_or(AdminResponse::Error {
                    message: "daemon dropped the request".into(),
                })
            }
        };
        return write_response(&mut write_half, &resp).await;
    }

    if let Ok(req) = serde_json::from_str::<QueryRequest>(trimmed) {
        let resp = if req.token == token {
            let (tx, rx) = oneshot::channel();
            if ev_tx.send(Event::Query(req.query, tx)).await.is_err() {
                QueryResponse::Error {
                    message: "daemon shutting down".into(),
                }
            } else {
                rx.await.unwrap_or(QueryResponse::Error {
                    message: "daemon dropped the request".into(),
                })
            }
        } else {
            QueryResponse::Error {
                message: "bad token".into(),
            }
        };
        return write_response(&mut write_half, &resp).await;
    }

    write_response(
        &mut write_half,
        &HookResponse::Error {
            message: "unrecognized request".into(),
        },
    )
    .await
}

/// Drive a `PushPack` over one IPC connection: hand the owner loop an mpsc
/// sender, then relay each [`PushUpdate`] as a JSON line — `PushProgress` ticks
/// followed by the terminal [`AdminResponse`].
async fn stream_push(
    write_half: &mut (impl AsyncWriteExt + Unpin),
    ev_tx: &mpsc::Sender<Event>,
    id: String,
    dir: std::path::PathBuf,
    set_active: bool,
) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<PushUpdate>(16);
    if ev_tx
        .send(Event::PushPack {
            id,
            dir,
            set_active,
            updates: tx,
        })
        .await
        .is_err()
    {
        return write_response(
            write_half,
            &AdminResponse::Error {
                message: "daemon shutting down".into(),
            },
        )
        .await;
    }
    while let Some(u) = rx.recv().await {
        match u {
            PushUpdate::Progress { done, total, file } => {
                write_response(
                    write_half,
                    &PushProgress {
                        kind: "progress".into(),
                        done,
                        total,
                        file,
                    },
                )
                .await?;
            }
            PushUpdate::Done(resp) => {
                return write_response(write_half, &resp).await;
            }
        }
    }
    // The task dropped its sender without a terminal Done (shouldn't happen).
    write_response(
        write_half,
        &AdminResponse::Error {
            message: "the push ended unexpectedly".into(),
        },
    )
    .await
}

/// Write a JSON response line to an IPC client.
async fn write_response<T: Serialize>(
    write_half: &mut (impl AsyncWriteExt + Unpin),
    resp: &T,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(resp)?;
    bytes.push(b'\n');
    write_half.write_all(&bytes).await?;
    write_half.flush().await?;
    Ok(())
}

/// Reconnect backoff bounds: start tight so a returning buddy is picked up
/// promptly, then grow to save battery/CPU while it's away. Reset to the floor
/// on a successful connect.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_secs(2);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How often the wake watchdog ticks (monotonic). Each tick compares wall-clock
/// advancement against this interval; a jump far past it means the machine slept
/// and just resumed. See [`spawn_wake_watchdog`].
const WAKE_TICK: Duration = Duration::from_secs(10);
/// Wall-clock jump (over one [`WAKE_TICK`]) that we treat as a system resume.
/// Comfortably above any scheduler jitter or small NTP step, well below the
/// shortest realistic lid-close, so resumes are caught without false positives.
const WAKE_GAP_THRESHOLD: Duration = Duration::from_secs(40);
/// How long a *previously connected* link may stay un-regained before we assume
/// the in-process CoreBluetooth central is wedged and restart for a fresh one.
/// At ~72s per failed attempt (12s scan + 60s capped backoff) this is a few
/// attempts. See the stall guard in [`ble_manager_loop`].
const RECONNECT_STALL_LIMIT: Duration = Duration::from_secs(180);

/// Classify a connect error into a short, user-facing reason for the status UI.
/// String-based for now; when ble.rs grows typed error kinds (cross-bucket),
/// this is the single place to switch on them instead.
fn classify_connect_error(e: &anyhow::Error) -> String {
    let s = e.to_string();
    let low = s.to_ascii_lowercase();
    if low.contains("no bluetooth adapter") {
        "no Bluetooth adapter found — is Bluetooth hardware present?".into()
    } else if low.contains("bluetooth") || low.contains("permit") || low.contains("scan") {
        "couldn't start a Bluetooth scan — is Bluetooth on and permitted?".into()
    } else if low.contains("no claude buddy found") {
        "no buddy found nearby — make sure it's powered on and awake".into()
    } else {
        s
    }
}

/// The BLE reconnect/forward loop: connect, forward lines, reconnect on drop.
///
/// Runs forever; it only returns when the owner channel is closed (the daemon
/// is shutting down). It must never be spawned bare: a panic anywhere in here
/// would end reconnection while the daemon keeps running and answering IPC,
/// silently abandoning the device. [`spawn_ble_manager`] supervises it and
/// respawns it on panic — keep them paired.
async fn ble_manager_loop(mut preferred: Option<String>, ev_tx: mpsc::Sender<Event>) {
    // Same-lifetime reconnect should prefer the device we actually linked to.
    // Seeded from config, then narrowed to the connected peripheral id after
    // the first successful link (below).
    let mut backoff = RECONNECT_BACKOFF_MIN;
    // Set when a *previously good* link drops; the elapsed time since then is how
    // long we've been unable to regain a link we know was reachable. If that
    // outruns RECONNECT_STALL_LIMIT the in-process central is presumed wedged
    // (the classic sleep/wake case the wake watchdog usually catches first, but
    // also any cause it can't see — a brief sub-threshold sleep, a BT driver
    // glitch) and we restart once for a fresh central. Re-stamped on each drop
    // and never armed until we've connected at least once in THIS process — so a
    // respawn that simply can't find an absent buddy keeps retrying forever
    // instead of churning the process.
    let mut stall_since: Option<std::time::Instant> = None;
    loop {
        match BleLink::connect(preferred.as_deref(), SCAN_SECS).await {
            Ok((link, mut lines, connected_id)) => {
                backoff = RECONNECT_BACKOFF_MIN; // reset on success
                                                 // Pin same-lifetime reconnects to the device we actually
                                                 // linked to, so a returning buddy is matched directly
                                                 // instead of re-running advertise discovery.
                preferred = Some(connected_id);
                // Keep macOS from idle-sleeping (and powering down the BT
                // radio) for the life of this connection, so the link
                // survives the screen going dark. Released automatically
                // when `_sleep_guard` drops as we leave this match arm on
                // disconnect. The display still sleeps normally.
                let _sleep_guard =
                    crate::power::PowerAssertion::prevent_idle_sleep("Claude buddy connected");
                if ev_tx
                    .send(Event::Connected(Link::Ble(link.clone())))
                    .await
                    .is_err()
                {
                    return;
                }
                // Forward lines until the link drops. Detect a drop FAST:
                // race the line pump against a 2s liveness poll. On macOS a
                // dropped BLE link often leaves the notify channel open (no
                // clean `None`), so without the poll we'd wait the whole
                // LIVENESS_TIMEOUT — that was the ~15s recovery lag. is_connected()
                // reports the drop within ~2s; the silence deadline stays as the
                // backstop for a wedged-but-"connected" link.
                let mut last_line = std::time::Instant::now();
                loop {
                    tokio::select! {
                        r = lines.recv() => match r {
                            Some(line) => {
                                last_line = std::time::Instant::now();
                                if ev_tx.send(Event::DeviceLine(line)).await.is_err() {
                                    return;
                                }
                            }
                            None => break, // pump closed: clean disconnect
                        },
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {
                            let gone = !matches!(
                                tokio::time::timeout(Duration::from_secs(2), link.is_connected()).await,
                                Ok(true)
                            );
                            if gone {
                                warn!("link dropped — reconnecting");
                                break;
                            }
                            if last_line.elapsed() >= LIVENESS_TIMEOUT {
                                warn!(
                                    "no device traffic in {}s — forcing reconnect",
                                    LIVENESS_TIMEOUT.as_secs()
                                );
                                break;
                            }
                        }
                    }
                }
                // Best-effort cleanup, bounded: on a wedged macOS link the
                // disconnect ack rides the same dead event loop and never
                // returns, so we must not await it unbounded.
                let _ = tokio::time::timeout(Duration::from_secs(2), link.disconnect()).await;
                // Start the stall clock: from here, failing to reconnect for
                // RECONNECT_STALL_LIMIT means the central is likely wedged.
                stall_since = Some(std::time::Instant::now());
                if ev_tx.send(Event::Disconnected).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                let reason = classify_connect_error(&e);
                debug!("connect attempt failed: {e}");
                // A link we know was reachable has stayed unreachable too long:
                // the in-process CoreBluetooth central is presumed wedged (see
                // spawn_wake_watchdog). Restart once for a fresh stack — launchd
                // respawns us; the fresh instance won't re-arm this until it has
                // connected, so an absent buddy can't make us churn.
                if let Some(since) = stall_since {
                    if since.elapsed() >= RECONNECT_STALL_LIMIT {
                        warn!(
                            "reconnect stalled {}s after a drop — restarting for a fresh \
                             Bluetooth stack",
                            since.elapsed().as_secs()
                        );
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        std::process::exit(0);
                    }
                }
                // Surface the classified reason to the status UI. Ignore a
                // send failure (owner loop gone → we'll exit next iteration).
                let _ = ev_tx.send(Event::ConnectError(reason)).await;
            }
        }
        // Keep retrying forever, but pace it: tight floor so a returning
        // buddy is picked up within a scan window, growing to a cap so an
        // absent buddy doesn't keep the radio hot. Reset to the floor on a
        // successful connect (above).
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
}

/// Maintain the BLE connection, supervised so it can never silently die.
///
/// The reconnect/forward logic ([`ble_manager_loop`]) runs in its own task. A
/// panic in that path used to be unrecoverable: tokio kills only the panicking
/// task, so the daemon stayed alive and answering IPC — looking healthy — while
/// never reconnecting again until a full daemon restart. A powered device with
/// nothing driving the link is exactly the "permanently abandoned" failure this
/// project must avoid. Here we own the loop's [`JoinHandle`]: if it panics we
/// log loudly and respawn after a short delay; a *clean* return means the owner
/// channel is gone (daemon shutting down), so we stop.
fn spawn_ble_manager(preferred: Option<String>, ev_tx: mpsc::Sender<Event>) {
    tokio::spawn(async move {
        loop {
            // Re-seed `preferred` from the original each respawn: we lose the
            // narrowed peripheral id, which just means one extra discovery pass
            // — cheap, and far better than not reconnecting at all.
            let task = tokio::spawn(ble_manager_loop(preferred.clone(), ev_tx.clone()));
            match task.await {
                Ok(()) => {
                    // The loop only returns when an ev_tx send failed, i.e. the
                    // owner loop is gone — the daemon is shutting down.
                    debug!("BLE manager exited (owner gone) — not respawning");
                    return;
                }
                Err(e) if e.is_panic() => {
                    error!("BLE manager panicked ({e}) — respawning so reconnection continues");
                }
                Err(e) => {
                    // Task cancelled (runtime shutting down). Nothing to recover.
                    debug!("BLE manager task ended: {e}");
                    return;
                }
            }
            // Bounded delay so a panic-on-every-startup can't hot-loop the CPU.
            tokio::time::sleep(Duration::from_secs(1)).await;
            // If the owner is already gone, don't bother respawning.
            if ev_tx.is_closed() {
                return;
            }
        }
    });
}

fn spawn_keepalive(ev_tx: mpsc::Sender<Event>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(KEEPALIVE);
        loop {
            tick.tick().await;
            if ev_tx.send(Event::Keepalive).await.is_err() {
                return;
            }
        }
    });
}

/// Detect a system sleep/wake transition and restart the process so the next
/// instance comes up with a FRESH CoreBluetooth central.
///
/// Why restart rather than reconnect in place: when the lid closes, macOS
/// sleeps, the Bluetooth radio powers down, and our BLE link drops. On wake the
/// OS frequently *auto-restores* the ACL connection to the buddy at the system
/// level — so the buddy sees itself "connected" and STOPS advertising — while
/// our long-lived process's central is left wedged. It can neither see the
/// (non-advertising) buddy in a scan nor recover by building a new
/// `Manager`/`Adapter` in-process; verified in the field, it scans fruitlessly
/// forever (the buddy meanwhile shows under `system_profiler`'s "Connected:").
/// A brand-new *process* gets a clean central that immediately surfaces the
/// already-connected peripheral and links in well under a second. The daemon is
/// a launchd `KeepAlive` job, so exiting hands us straight back. This is the
/// targeted fix for "closed the laptop, reopened, and it never reconnects".
///
/// Detection needs no OS power API, run loop, or entitlement: we compare
/// wall-clock advancement against a monotonic tick. macOS suspends the monotonic
/// clock (and our timers) while asleep, so the first tick after wake shows the
/// wall clock having jumped far past the tick interval — an unmistakable resume
/// signal. The same signature holds for suspend/resume on other platforms, so
/// this stays unconditional. Display-only sleep keeps the process running (wall
/// ≈ monotonic) and never trips it, and a live link is already gone by the time
/// the system has slept, so a resume-restart never interrupts an active link.
fn spawn_wake_watchdog() {
    tokio::spawn(async move {
        let mut last = std::time::SystemTime::now();
        loop {
            tokio::time::sleep(WAKE_TICK).await;
            let now = std::time::SystemTime::now();
            // `duration_since` errors only if the clock went backwards (e.g. an
            // NTP step); treat that as "no jump" rather than a false resume.
            let wall = now.duration_since(last).unwrap_or(Duration::ZERO);
            last = now;
            if wall >= WAKE_GAP_THRESHOLD {
                warn!(
                    "system resume detected (wall clock advanced {}s over a {}s tick) — \
                     restarting for a fresh Bluetooth stack so the buddy reconnects",
                    wall.as_secs(),
                    WAKE_TICK.as_secs()
                );
                // Let the radio finish coming back so the respawned process's
                // first scan lands on a ready adapter. launchd respawns us
                // (KeepAlive); the OS releases our singleton lock + IPC socket
                // on exit, and the fresh instance re-publishes its endpoint.
                tokio::time::sleep(Duration::from_secs(2)).await;
                std::process::exit(0);
            }
        }
    });
}

/// Periodically check GitHub Releases and feed the result to the owner loop
/// (surfaced to the desktop app). One call lists recent releases; from it we
/// derive the app-update status (desktop `v*` track) AND the newest firmware per
/// board (across both the `v*` and firmware-only `fw-v*` tracks) so a device can
/// be OTA-updated independently of the app version. The HTTP call shells out to
/// `curl` and is blocking, so it runs on `spawn_blocking`. A failed check
/// (offline, rate-limited) is logged at debug and skipped — the last good result
/// stays in effect until the next success.
fn spawn_update_checker(
    ev_tx: mpsc::Sender<Event>,
    recheck: std::sync::Arc<tokio::sync::Notify>,
) {
    tokio::spawn(async move {
        // Don't compete with startup: let the BLE connect + first heartbeat land
        // before doing network I/O, then check immediately and on a long cadence.
        tokio::time::sleep(Duration::from_secs(15)).await;
        let current = env!("AGENT_BUDDY_VERSION").to_string();
        loop {
            match tokio::task::spawn_blocking(crate::update::fetch_releases).await {
                Ok(Ok(releases)) => {
                    // App-update banner: newest desktop-track release, if any.
                    // Also surface this platform's installer asset (the `.dmg` /
                    // `Setup.exe` / `.AppImage`) so the app can update in place
                    // instead of only opening the release page.
                    let app = match crate::update::latest_app_release(&releases) {
                        Some(rel) => {
                            let pkg = crate::update::platform_package(rel);
                            UpdateStatus {
                                current: current.clone(),
                                available: crate::update::is_newer(&rel.tag, &current),
                                latest: rel.tag.clone(),
                                url: rel.url.clone(),
                                pkg_url: pkg.map(|a| a.url.clone()),
                                pkg_name: pkg.map(|a| a.name.clone()),
                            }
                        }
                        None => UpdateStatus {
                            current: current.clone(),
                            latest: current.clone(),
                            available: false,
                            url: String::new(),
                            pkg_url: None,
                            pkg_name: None,
                        },
                    };
                    // Newest firmware per board, version + download URL.
                    let mut firmware: HashMap<String, FirmwareLatest> = HashMap::new();
                    for board in crate::update::firmware_boards(&releases) {
                        if let Some((version, url, sha256_url)) =
                            crate::update::latest_firmware(&releases, &board)
                        {
                            firmware.insert(
                                board,
                                FirmwareLatest {
                                    version,
                                    url,
                                    sha256_url,
                                },
                            );
                        }
                    }
                    if ev_tx
                        .send(Event::UpdateInfo { app, firmware })
                        .await
                        .is_err()
                    {
                        return; // owner loop gone
                    }
                }
                Ok(Err(e)) => debug!("update check failed: {e}"),
                Err(e) => debug!("update check task panicked: {e}"),
            }
            // Wait for the periodic tick OR an on-demand nudge, whichever first.
            // `Notify` holds one permit, so a nudge that arrives mid-fetch isn't
            // lost — the next `notified()` returns at once (one extra poll).
            tokio::select! {
                _ = tokio::time::sleep(UPDATE_CHECK_INTERVAL) => {}
                _ = recheck.notified() => debug!("on-demand update recheck"),
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Filename (under the config dir) of the daemon's single-instance lock.
const LOCK_FILE: &str = "daemon.lock";

/// Holds the daemon's single-instance lock open for the process lifetime. The
/// flock is released automatically when this drops or the process exits — no
/// stale PID/lock file to clean up after a crash.
struct SingletonLock(#[allow(dead_code)] std::fs::File);

/// Take the exclusive single-instance lock, or fail with a clear message if
/// another daemon already holds it.
#[cfg(unix)]
fn acquire_singleton_lock() -> Result<SingletonLock> {
    use std::os::unix::io::AsRawFd;
    let path = config_dir()?.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening lock file {}", path.display()))?;

    // LOCK_EX | LOCK_NB: grab the exclusive lock or return immediately. Contention
    // here is usually *benign and transient*: a service reload (launchctl
    // unload→load) or a restart races the old process releasing its fd. Retry a
    // few times over ~1.5s to ride out that handoff before concluding a real
    // second instance is running.
    let mut last_err = std::io::Error::last_os_error();
    for attempt in 0..5 {
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(SingletonLock(file));
        }
        last_err = std::io::Error::last_os_error();
        if !matches!(last_err.raw_os_error(), Some(libc::EWOULDBLOCK)) {
            // A non-contention error (e.g. EINTR/ENOLCK) — surface it.
            return Err(
                anyhow::Error::from(last_err).context(format!("flock on {}", path.display()))
            );
        }
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    // Still held after the retries: another daemon really owns the link. Exit
    // *cleanly* (status 0) rather than bailing non-zero. A non-zero exit would
    // make launchd treat the loser as a crash and crash-loop it; a clean exit
    // lets the service manager's KeepAlive/SuccessfulExit policy back off.
    let _ = last_err;
    info!(
        "another agent-buddy daemon already holds the lock on {} — exiting cleanly; \
         the running instance owns the buddy. To restart the service: \
         `launchctl kickstart -k gui/$(id -u)/com.nateschnell.agent-buddy`.",
        path.display()
    );
    std::process::exit(0);
}

/// Non-unix fallback: no advisory lock available, so rely on the OS service
/// manager to keep a single instance. Still opens the file so the guard type is
/// uniform across platforms.
#[cfg(not(unix))]
fn acquire_singleton_lock() -> Result<SingletonLock> {
    warn!("single-instance lock not enforced on this platform");
    let path = config_dir()?.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening lock file {}", path.display()))?;
    Ok(SingletonLock(file))
}

fn write_endpoint(ep: &Endpoint) -> Result<()> {
    let path = config_dir()?.join(ENDPOINT_FILE);
    // Write to a temp file then rename, so a reader never sees a half-written
    // (unparseable) endpoint.json — important now that a stale file is probed
    // and removed elsewhere; the rename is atomic on the same filesystem.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(ep)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    // Best-effort tighten perms on unix (token gates the socket).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("publishing {}", path.display()))?;
    Ok(())
}

/// Process-wide local UTC offset, captured once at startup *before* the tokio
/// runtime spawns threads (see `crate::capture_local_offset`). `time`'s
/// `current_local_offset()` refuses to run once the process is multithreaded,
/// so reading it lazily here would almost always yield UTC.
pub static LOCAL_OFFSET: std::sync::OnceLock<time::UtcOffset> = std::sync::OnceLock::new();

/// `(unix_epoch_secs, utc_offset_secs, local_YYYY-MM-DD)`.
fn local_now() -> (i64, i32, String) {
    use time::{OffsetDateTime, UtcOffset};
    let offset = *LOCAL_OFFSET.get().unwrap_or(&UtcOffset::UTC);
    let now = OffsetDateTime::now_utc();
    let local = now.to_offset(offset);
    let date = format!(
        "{:04}-{:02}-{:02}",
        local.year(),
        local.month() as u8,
        local.day()
    );
    (now.unix_timestamp(), offset.whole_seconds(), date)
}

/// If the local day rolled over, reset the daily counter. Returns true if
/// anything changed (so the caller can persist).
fn config_token_day_changed(config: &mut Config, state: &mut SessionState) -> bool {
    let (_, _, today) = local_now();
    if state.tokens_day != today && !state.tokens_day.is_empty() {
        state.add_tokens(0, &today); // rolls the counter
        state.sync_to_config(config);
        return true;
    }
    false
}

/// Serialized byte length of a heartbeat line, including the trailing `\n` the
/// wire adds (matching [`protocol::to_line`]).
fn heartbeat_line_len(hb: &Heartbeat) -> usize {
    // serde_json::to_vec on a Heartbeat can't fail (no non-string map keys, no
    // serialize errors), but be defensive: an error here just means "treat it
    // as over budget" so we keep shedding rather than panic.
    serde_json::to_vec(hb)
        .map(|v| v.len() + 1)
        .unwrap_or(usize::MAX)
}

/// Shed the largest variable contributors from a heartbeat until its serialized
/// line fits [`protocol::MAX_LINE_BYTES`], so a multibyte-heavy heartbeat can't
/// overflow the firmware line buffer and get dropped whole. Entries go first
/// (the ticker simply shows fewer recent lines), then per-session rows. The
/// prompt and counters are never shed — they're the load-bearing payload.
fn enforce_line_budget(hb: &mut Heartbeat) {
    while heartbeat_line_len(hb) > MAX_LINE_BYTES && !hb.entries.is_empty() {
        hb.entries.pop(); // drop the oldest (entries are newest-first)
    }
    while heartbeat_line_len(hb) > MAX_LINE_BYTES && !hb.sessions.is_empty() {
        hb.sessions.pop();
    }
}

/// Truncate `s` to at most `max` **bytes**, appending a `…` (3 bytes) when it
/// had to cut — the ellipsis is counted *inside* the budget so the result never
/// exceeds `max` bytes. Cuts on a char boundary so multibyte text is never
/// split. Byte-bounded (not char-bounded) because the firmware reassembles each
/// line into a fixed-size buffer measured in bytes; see [`protocol::MAX_LINE_BYTES`].
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Reserve room for the 3-byte ellipsis.
    let budget = max.saturating_sub("…".len());
    let end = s
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= budget)
        .last()
        .unwrap_or(0);
    let mut out = s[..end].to_string();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "pack")]
    fn ack(name: &str, ok: bool) -> protocol::Ack {
        protocol::Ack {
            ack: name.into(),
            ok,
            n: 0,
            error: if ok { None } else { Some("nope".into()) },
        }
    }

    #[cfg(feature = "pack")]
    #[tokio::test]
    async fn await_push_ack_accepts_expected_ok() {
        let (tx, mut rx) = mpsc::channel::<protocol::Ack>(4);
        tx.send(ack("chunk", true)).await.unwrap();
        assert!(await_push_ack(&mut rx, "chunk", Duration::from_secs(1)).await.is_ok());
    }

    #[cfg(feature = "pack")]
    #[tokio::test]
    async fn await_push_ack_rejects_nack_wrong_name_and_disconnect() {
        // A nack (ok:false) for the expected step is an error.
        let (tx, mut rx) = mpsc::channel::<protocol::Ack>(4);
        tx.send(ack("file", false)).await.unwrap();
        assert!(await_push_ack(&mut rx, "file", Duration::from_secs(1)).await.is_err());

        // An ack for a *different* step is a protocol desync error.
        let (tx, mut rx) = mpsc::channel::<protocol::Ack>(4);
        tx.send(ack("char_end", true)).await.unwrap();
        assert!(await_push_ack(&mut rx, "chunk", Duration::from_secs(1)).await.is_err());

        // A closed channel (owner loop dropped the sink on disconnect) is an error.
        let (tx, mut rx) = mpsc::channel::<protocol::Ack>(4);
        drop(tx);
        assert!(await_push_ack(&mut rx, "chunk", Duration::from_secs(1)).await.is_err());
    }

    #[cfg(feature = "pack")]
    #[tokio::test]
    async fn await_push_ack_times_out_when_silent() {
        let (_tx, mut rx) = mpsc::channel::<protocol::Ack>(4);
        // Keep _tx alive (channel open) but send nothing → must hit the timeout.
        let r = await_push_ack(&mut rx, "chunk", Duration::from_millis(50)).await;
        assert!(r.is_err());
    }

    #[test]
    fn truncate_is_byte_bounded_with_ellipsis_inside_budget() {
        // Short string passes through untouched, no ellipsis.
        assert_eq!(truncate("hello", 23), "hello");
        // ASCII over budget: result (incl. the 3-byte '…') never exceeds max.
        let out = truncate("abcdefghij", 6);
        assert!(out.len() <= 6, "len {} > 6", out.len());
        assert!(out.ends_with('…'));
        // Multibyte: never split a codepoint and never exceed the byte budget.
        let s = "héllo wörld with açcénts everywhere";
        let out = truncate(s, 12);
        assert!(out.len() <= 12, "len {} > 12", out.len());
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        assert!(out.ends_with('…'));
    }

    #[test]
    fn enforce_line_budget_sheds_entries_then_sessions() {
        // Build a heartbeat whose serialized line clearly overruns the budget
        // via many long entries, then confirm shedding brings it under.
        let big = "x".repeat(200);
        let hb = Heartbeat {
            total: 3,
            msg: "working".into(),
            entries: (0..20).map(|_| big.clone()).collect(),
            sessions: (0..6)
                .map(|i| protocol::SessionInfo {
                    id: format!("sess{i}"),
                    cwd: "project".into(),
                    st: "run",
                    tok: 1234,
                    m: "opus".into(),
                    ctx: 42,
                    ctok: 84_000,
                    clim: 200_000,
                })
                .collect(),
            ..Default::default()
        };
        assert!(heartbeat_line_len(&hb) > MAX_LINE_BYTES);
        let mut hb = hb;
        enforce_line_budget(&mut hb);
        assert!(
            heartbeat_line_len(&hb) <= MAX_LINE_BYTES,
            "line still {} bytes",
            heartbeat_line_len(&hb)
        );
        // Entries are shed before sessions: with entries alone enough to get
        // under budget, the sessions should be untouched.
        assert_eq!(hb.sessions.len(), 6);
    }

    #[test]
    fn enforce_line_budget_keeps_a_small_heartbeat_intact() {
        let mut hb = Heartbeat {
            total: 1,
            msg: "idle".into(),
            entries: vec!["did a thing".into(), "did another".into()],
            ..Default::default()
        };
        let before = hb.entries.len();
        enforce_line_budget(&mut hb);
        assert_eq!(hb.entries.len(), before); // nothing shed
    }
}
