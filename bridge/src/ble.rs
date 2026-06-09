//! BLE central: scan for a buddy, connect, subscribe to its TX notifications,
//! and write lines to its RX characteristic.
//!
//! The firmware is a GATT *peripheral* exposing the Nordic UART Service; we
//! are the *central*. This mirrors the role the desktop app plays, which is
//! why one firmware works with either.

use crate::protocol::{self, nus};
use anyhow::{anyhow, Context, Result};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Max bytes per write to RX. The firmware reassembles by `\n`, so splitting
/// a long line across writes is fine. 180 stays under a 185-byte negotiated
/// ATT MTU (the common macOS value) with header headroom.
const WRITE_CHUNK: usize = 180;

/// Hard ceiling on the connect→discover→subscribe phase, on top of the scan
/// window. Must exceed `scan_secs` (the daemon passes 12) plus first-pairing
/// headroom: the OS passkey flow runs *inside* `connect()`, so a brand-new
/// device legitimately spends several seconds there. Anything past this is a
/// wedged CoreBluetooth handle against a rebooting CYD, which we abandon so the
/// reconnect loop can re-scan for the fresh advertisement.
const CONNECT_PHASE_GRACE_SECS: u64 = 15;

/// Coarse classification of why a connect/scan attempt failed, stable enough
/// for the daemon to match on and surface actionable status. btleplug does not
/// expose typed permission errors on macOS, so [`AdapterMissing`]/[`NotPermitted`]
/// are derived best-effort from the error chain.
///
/// [`AdapterMissing`]: ConnectErrorKind::AdapterMissing
/// [`NotPermitted`]: ConnectErrorKind::NotPermitted
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectErrorKind {
    /// No Bluetooth adapter is present (powered off, or none on the machine).
    AdapterMissing,
    /// The OS denied BLE access (TCC / Bluetooth permission not granted).
    NotPermitted,
    /// Adapter is fine but no buddy advertised within the scan window.
    NotFound,
    /// Anything else (wedged connect, mid-handshake drop, discovery failure…).
    Other,
}

impl ConnectErrorKind {
    /// Stable lowercase string the daemon can copy into a `StatusReport`.
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectErrorKind::AdapterMissing => "adapter-missing",
            ConnectErrorKind::NotPermitted => "not-permitted",
            ConnectErrorKind::NotFound => "not-found",
            ConnectErrorKind::Other => "other",
        }
    }
}

/// A classified connect/scan failure: the coarse [`ConnectErrorKind`] plus the
/// underlying error chain for logging. Carries through `anyhow` so existing
/// `?`/`context` call sites keep working; the daemon recovers the kind with
/// [`classify_error`].
#[derive(Debug)]
pub struct ConnectError {
    pub kind: ConnectErrorKind,
    pub source: anyhow::Error,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} [{}]", self.source, self.kind.as_str())
    }
}

impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Recover the [`ConnectErrorKind`] from an error returned by [`BleLink::connect`].
/// Prefers an explicit [`ConnectError`] in the chain; otherwise derives a coarse
/// kind from the error text (btleplug gives us no typed permission errors on
/// macOS, so this is the best signal available).
pub fn classify_error(err: &anyhow::Error) -> ConnectErrorKind {
    for cause in err.chain() {
        if let Some(ce) = cause.downcast_ref::<ConnectError>() {
            return ce.kind;
        }
    }
    let text = err
        .chain()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let t = text.to_ascii_lowercase();
    if t.contains("no bluetooth adapter") || t.contains("listing adapters") {
        ConnectErrorKind::AdapterMissing
    } else if t.contains("not authorized")
        || t.contains("not permitted")
        || t.contains("unauthorized")
        || t.contains("permission")
        || t.contains("denied")
    {
        ConnectErrorKind::NotPermitted
    } else if t.contains("no claude buddy found") {
        ConnectErrorKind::NotFound
    } else {
        ConnectErrorKind::Other
    }
}

/// A live connection to a buddy. Cloneable: the underlying `Peripheral` is an
/// `Arc`, so the daemon can write from one task while another forwards
/// notifications.
#[derive(Clone)]
pub struct BleLink {
    peripheral: Peripheral,
    rx_char: Characteristic,
    /// Keep the adapter alive for the whole connection. btleplug's CoreBluetooth
    /// `Peripheral` holds only a *Weak* reference to the central's
    /// `AdapterManager`; the sole strong reference lives in the `Adapter`. If we
    /// drop the adapter when `connect()` returns, that manager is torn down, the
    /// peripheral's internal event loop loses its sender and logs "Event
    /// receiver died, breaking out of corebluetooth device loop" — notifications
    /// silently stop and the daemon's liveness watchdog forces a needless
    /// reconnect every minute or two. Holding it here keeps the link alive.
    #[allow(dead_code)]
    adapter: Adapter,
    /// Serializes whole-line writes. `write_line` splits a line into MTU-sized
    /// `WithoutResponse` packets; without this lock two concurrent writers (e.g.
    /// the owner loop's heartbeat and a pack-push task, which both hold a clone
    /// of this link) could interleave their packets on the wire and corrupt both
    /// lines. Shared across clones via `Arc`, so the serialization is per-device.
    write_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl BleLink {
    /// Scan for up to `scan_secs`, connect to the first buddy (preferring
    /// `preferred_id` if set), subscribe to TX, and spawn a task that forwards
    /// reassembled lines on the returned channel.
    ///
    /// Returns the live link, the line receiver, and the connected peripheral's
    /// id (so the daemon can prefer the *same* device on the next reconnect
    /// rather than blindly reusing a stale cached handle).
    ///
    /// Errors carry a [`ConnectErrorKind`] (recoverable via [`classify_error`])
    /// so the daemon can surface actionable status instead of a silent log.
    pub async fn connect(
        preferred_id: Option<&str>,
        scan_secs: u64,
    ) -> Result<(BleLink, mpsc::Receiver<String>, String)> {
        let adapter = first_adapter().await?;

        info!("scanning for a Claude buddy…");
        adapter
            .start_scan(ScanFilter {
                services: vec![nus::SERVICE],
            })
            .await
            .map_err(|e| ConnectError {
                kind: ConnectErrorKind::NotPermitted,
                source: anyhow::Error::new(e)
                    .context("start_scan failed (is Bluetooth on / permitted?)"),
            })?;

        let peripheral = discover(&adapter, preferred_id, scan_secs).await?;
        let _ = adapter.stop_scan().await;

        let name = peripheral
            .properties()
            .await
            .ok()
            .flatten()
            .and_then(|p| p.local_name)
            .unwrap_or_else(|| "Claude buddy".into());
        info!("connecting to {name} ({})…", peripheral.id());

        // Bound the whole connect→discover→subscribe phase. A wedged connect
        // against a rebooting CYD never returns on its own; without this the
        // reconnect loop would stall here forever. The budget exceeds the scan
        // window plus first-pairing headroom (the OS passkey flow runs inside
        // connect()).
        let rx_char = match tokio::time::timeout(
            Duration::from_secs(scan_secs + CONNECT_PHASE_GRACE_SECS),
            async {
                peripheral.connect().await.context("connect failed")?;
                // Triggers OS pairing on first contact for an encrypted device;
                // the device shows a passkey and the OS prompts for it.
                peripheral
                    .discover_services()
                    .await
                    .context("service discovery failed")?;
                let (rx_char, tx_char) = find_chars(&peripheral)?;
                peripheral
                    .subscribe(&tx_char)
                    .await
                    .context("subscribe to TX failed")?;
                Ok::<Characteristic, anyhow::Error>(rx_char)
            },
        )
        .await
        {
            Ok(Ok(rx_char)) => rx_char,
            Ok(Err(e)) => {
                return Err(ConnectError {
                    kind: ConnectErrorKind::Other,
                    source: e,
                }
                .into());
            }
            Err(_) => {
                // Timed out mid-handshake. Best-effort drop of the stale
                // CoreBluetooth handle (itself bounded — the ack may ride the
                // same dead event loop), so the next scan picks the fresh
                // advertisement instead of the dead cached handle.
                let _ = tokio::time::timeout(Duration::from_secs(2), peripheral.disconnect()).await;
                return Err(ConnectError {
                    kind: ConnectErrorKind::Other,
                    source: anyhow!("connect/discover timed out"),
                }
                .into());
            }
        };

        let id = peripheral.id().to_string();
        let (line_tx, line_rx) = mpsc::channel::<String>(64);
        spawn_notification_pump(peripheral.clone(), line_tx);

        info!("connected to {name}");
        Ok((
            BleLink {
                peripheral,
                rx_char,
                adapter,
                write_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            },
            line_rx,
            id,
        ))
    }

    /// True while the OS reports the link as up. (Disconnect is normally
    /// detected via the notification stream closing; this is for callers that
    /// want to poll.)
    #[allow(dead_code)]
    pub async fn is_connected(&self) -> bool {
        self.peripheral.is_connected().await.unwrap_or(false)
    }

    /// Write one already-serialized wire line (caller includes the trailing
    /// `\n`), chunked to stay under the MTU.
    pub async fn write_line(&self, bytes: &[u8]) -> Result<()> {
        // Hold the lock for the whole line so its MTU chunks can't interleave
        // with another concurrent writer's packets.
        let _guard = self.write_lock.lock().await;
        for chunk in bytes.chunks(WRITE_CHUNK) {
            self.peripheral
                .write(&self.rx_char, chunk, WriteType::WithoutResponse)
                .await
                .context("write to RX failed")?;
        }
        Ok(())
    }

    /// Convenience: serialize a value and write it.
    pub async fn send<T: serde::Serialize>(&self, v: &T) -> Result<()> {
        self.write_line(&protocol::to_line(v)?).await
    }

    pub async fn disconnect(&self) {
        let _ = self.peripheral.disconnect().await;
    }
}

async fn first_adapter() -> Result<Adapter> {
    let manager = Manager::new().await.context("BLE manager init failed")?;
    let adapters = manager.adapters().await.map_err(|e| ConnectError {
        kind: ConnectErrorKind::AdapterMissing,
        source: anyhow::Error::new(e).context("listing adapters failed"),
    })?;
    adapters.into_iter().next().ok_or_else(|| {
        ConnectError {
            kind: ConnectErrorKind::AdapterMissing,
            source: anyhow!("no Bluetooth adapter found"),
        }
        .into()
    })
}

/// Poll discovered peripherals until one matches, or the scan window expires.
async fn discover(
    adapter: &Adapter,
    preferred_id: Option<&str>,
    scan_secs: u64,
) -> Result<Peripheral> {
    let deadline = Duration::from_secs(scan_secs);
    let poll = Duration::from_millis(500);
    let mut waited = Duration::ZERO;

    loop {
        // First advertising buddy seen this pass, used as the fallback when the
        // preferred device isn't (yet) advertising.
        let mut fallback: Option<Peripheral> = None;

        for p in adapter.peripherals().await.unwrap_or_default() {
            let props = p.properties().await.ok().flatten();
            let name = props.as_ref().and_then(|p| p.local_name.clone());
            let advertises_nus = props
                .as_ref()
                .map(|p| p.services.contains(&nus::SERVICE))
                .unwrap_or(false);
            let name_match = name
                .as_deref()
                .map(|n| n.starts_with(protocol::DEVICE_NAME_PREFIX))
                .unwrap_or(false);
            // A live candidate must be advertising the service (or a matching
            // name) *now* — a just-rebooted CYD that hasn't re-advertised keeps
            // its stale cached id, and connecting to that dead handle would
            // wedge. Require fresh advertisement before considering it.
            if !(advertises_nus || name_match) {
                continue;
            }

            // Prefer the same device across reconnects when it is advertising.
            if let Some(pref) = preferred_id {
                if p.id().to_string() == pref {
                    debug!("matched preferred peripheral ({})", p.id());
                    return Ok(p);
                }
                // Not the preferred one; hold the first such buddy as a fallback
                // (e.g. the preferred device is gone, or this is a fresh buddy).
                if fallback.is_none() {
                    fallback = Some(p);
                }
                continue;
            }

            debug!("matched peripheral {:?} ({})", name, p.id());
            return Ok(p);
        }
        // Preferred device not advertising this pass: only fall back to another
        // advertising buddy once the scan window has elapsed, so we give the
        // preferred device a chance to reappear first.
        if preferred_id.is_some() && waited >= deadline {
            if let Some(p) = fallback {
                debug!("preferred device absent; falling back to {}", p.id());
                return Ok(p);
            }
        }
        if waited >= deadline {
            return Err(ConnectError {
                kind: ConnectErrorKind::NotFound,
                source: anyhow!(
                    "no Claude buddy found within {scan_secs}s (is it awake and is Bluetooth on?)"
                ),
            }
            .into());
        }
        tokio::time::sleep(poll).await;
        waited += poll;
    }
}

/// Locate the RX (write) and TX (notify) characteristics on a connected peer.
fn find_chars(p: &Peripheral) -> Result<(Characteristic, Characteristic)> {
    let chars = p.characteristics();
    let rx = chars
        .iter()
        .find(|c| c.uuid == nus::RX)
        .cloned()
        .ok_or_else(|| anyhow!("device is missing the NUS RX characteristic"))?;
    let tx = chars
        .iter()
        .find(|c| c.uuid == nus::TX)
        .cloned()
        .ok_or_else(|| anyhow!("device is missing the NUS TX characteristic"))?;
    Ok((rx, tx))
}

/// Forward TX notifications to `line_tx`, reassembling `\n`-delimited lines
/// across MTU-fragmented notifications. Exits when notifications end (i.e. on
/// disconnect), which drops the sender and signals the daemon to reconnect.
fn spawn_notification_pump(peripheral: Peripheral, line_tx: mpsc::Sender<String>) {
    tokio::spawn(async move {
        let mut stream = match peripheral.notifications().await {
            Ok(s) => s,
            Err(e) => {
                warn!("could not open notification stream: {e}");
                return;
            }
        };
        let mut buf: Vec<u8> = Vec::with_capacity(256);
        while let Some(n) = stream.next().await {
            if n.uuid != nus::TX {
                continue;
            }
            buf.extend_from_slice(&n.value);
            // Drain complete lines.
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let s = String::from_utf8_lossy(&line[..line.len() - 1])
                    .trim_end_matches('\r')
                    .to_string();
                if !s.is_empty() && line_tx.send(s).await.is_err() {
                    return; // daemon dropped the receiver
                }
            }
            // Guard against an unbounded line with no newline.
            if buf.len() > 8192 {
                warn!("dropping oversized inbound buffer ({} bytes)", buf.len());
                buf.clear();
            }
        }
        debug!("notification stream ended (device disconnected)");
    });
}
