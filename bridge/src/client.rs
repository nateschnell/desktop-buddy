//! A small, blocking IPC client for talking to the running daemon.
//!
//! The CLI's hot path uses tokio; the desktop GUI does not want an async
//! runtime just to do a couple of short request/response round-trips, so this
//! module speaks the same newline-delimited JSON protocol over a plain
//! `std::net::TcpStream`. Each call is one connect → write → read-one-line →
//! close, with read/write timeouts so a wedged daemon can't freeze the UI.
//!
//! Every helper first reads the daemon's published `endpoint.json`; if it's
//! missing, the daemon isn't running and the call fails fast with a friendly
//! message.

use crate::ipc::{
    self, AdminRequest, AdminResponse, DeviceCommand, Endpoint, Query, QueryRequest, QueryResponse,
    StatusReport,
};
use anyhow::{anyhow, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

/// OTA app-slot size per board — must match the `ota_0` size in that board's
/// partition CSV. An image larger than this can't be flashed. Unknown boards
/// fall back to the smallest (CYD) slot, the safe conservative bound.
pub fn ota_slot_size(board: &str) -> usize {
    match board {
        // firmware/partitions_ota_16mb.csv → ota_0 = 6.25MB
        "fnk0104" => 0x640000,
        // firmware/partitions_ota.csv → ota_0 = 1.8125MB (CYD / default)
        _ => 0x1D0000,
    }
}

/// How long a single IPC round-trip may take before we give up. Fine for the
/// snapshot/status poll, which the daemon answers immediately.
const TIMEOUT: Duration = Duration::from_secs(6);

/// Wi-Fi provisioning is correlated daemon-side: the daemon parks the reply
/// until the device confirms storing the creds (and briefly until it joins),
/// up to ~14s. Use a longer budget so a slow-but-working confirmation isn't
/// misread as a timeout.
const WIFI_TIMEOUT: Duration = Duration::from_secs(20);

/// Read the live endpoint, mapping the common "no daemon" case to a message
/// that makes sense in the UI. Honest: probes the socket, so a stale
/// `endpoint.json` left by a crash reports "isn't running" instead of a
/// connection error later.
fn endpoint() -> Result<Endpoint> {
    ipc::endpoint_if_live().map_err(|_| anyhow!("the buddy daemon isn’t running"))
}

/// One connect → send → receive-one-line → close round-trip, with the default
/// [`TIMEOUT`].
fn round_trip<Req: Serialize, Resp: DeserializeOwned>(ep: &Endpoint, req: &Req) -> Result<Resp> {
    round_trip_within(ep, req, TIMEOUT)
}

/// [`round_trip`] with an explicit read/write timeout, for calls the daemon may
/// take longer to answer (e.g. correlated Wi-Fi provisioning).
fn round_trip_within<Req: Serialize, Resp: DeserializeOwned>(
    ep: &Endpoint,
    req: &Req,
    timeout: Duration,
) -> Result<Resp> {
    let stream = TcpStream::connect(("127.0.0.1", ep.port)).context("connecting to the daemon")?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut bytes = serde_json::to_vec(req)?;
    bytes.push(b'\n');
    (&stream).write_all(&bytes).context("sending request")?;
    (&stream).flush().ok();

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).context("reading response")?;
    if n == 0 {
        return Err(anyhow!("the daemon closed the connection without replying"));
    }
    serde_json::from_str(line.trim()).context("parsing daemon response")
}

/// True if a daemon is actually listening (not just an endpoint file left over
/// from a crash). Does one short-timeout socket probe.
pub fn daemon_running() -> bool {
    ipc::endpoint_if_live().is_ok()
}

/// Fetch a full snapshot of daemon + device state for the status UI.
pub fn status() -> Result<StatusReport> {
    let ep = endpoint()?;
    let req = QueryRequest {
        token: ep.token.clone(),
        query: Query::Status,
    };
    match round_trip::<_, QueryResponse>(&ep, &req)? {
        QueryResponse::Status(s) => Ok(s),
        QueryResponse::Error { message } => Err(anyhow!(message)),
    }
}

/// Nudge the daemon to re-check GitHub for updates immediately instead of on its
/// 6h cadence. Fire-and-forget from the UI's view: the fresh result arrives on a
/// later `status()` poll, so we discard the (still-cached) snapshot it returns.
pub fn recheck_updates() -> Result<()> {
    let ep = endpoint()?;
    let req = QueryRequest {
        token: ep.token.clone(),
        query: Query::RecheckUpdates,
    };
    match round_trip::<_, QueryResponse>(&ep, &req)? {
        QueryResponse::Status(_) => Ok(()),
        QueryResponse::Error { message } => Err(anyhow!(message)),
    }
}

/// Push Wi-Fi credentials to the connected buddy via the daemon's BLE link.
pub fn provision_wifi(ssid: &str, pass: &str) -> Result<()> {
    let ep = endpoint()?;
    let req = AdminRequest {
        token: ep.token.clone(),
        command: DeviceCommand::Wifi {
            ssid: ssid.to_string(),
            pass: pass.to_string(),
        },
    };
    match round_trip_within::<_, AdminResponse>(&ep, &req, WIFI_TIMEOUT)? {
        AdminResponse::Ok { .. } => Ok(()),
        AdminResponse::NoDevice => Err(anyhow!(
            "the daemon is running but no buddy is connected — wake the buddy and retry"
        )),
        AdminResponse::Error { message } => Err(anyhow!(message)),
    }
}

/// Run a full over-the-air firmware update: capture the buddy's IP, tell it to
/// enter OTA mode (which frees the heap a flash needs by tearing down BLE + the
/// UI), then push `image` over Wi-Fi via the built-in flasher. Blocking;
/// `on_progress` is called with 0..=100. Shared by the desktop "Update" button
/// and the CLI so the orchestration lives in one place.
pub fn update_firmware(image: &[u8], board: &str, on_progress: impl FnMut(u8)) -> Result<()> {
    if image.is_empty() {
        return Err(anyhow!("firmware image is empty"));
    }
    let slot = ota_slot_size(board);
    if image.len() > slot {
        return Err(anyhow!(
            "firmware image is too large for OTA: {} bytes > {} byte {board} app slot",
            image.len(),
            slot
        ));
    }
    // Capture the IP BEFORE entering OTA mode: the device drops BLE for the
    // flash, taking the daemon's record of the IP with it.
    let s = status()?;
    if !s.device_connected {
        return Err(anyhow!("no buddy is connected — wake it and retry"));
    }
    let ip_str = s
        .device_ip
        .ok_or_else(|| anyhow!("the buddy isn’t on Wi-Fi yet — set up Wi-Fi first"))?;
    let ip: std::net::Ipv4Addr = ip_str
        .parse()
        .with_context(|| format!("parsing the buddy’s IP ({ip_str})"))?;

    // Ask the device to enter OTA mode.
    let ep = endpoint()?;
    let req = AdminRequest {
        token: ep.token.clone(),
        command: DeviceCommand::Ota,
    };
    match round_trip::<_, AdminResponse>(&ep, &req)? {
        AdminResponse::Ok { .. } => {}
        AdminResponse::NoDevice => {
            return Err(anyhow!("no buddy is connected — wake it and retry"))
        }
        AdminResponse::Error { message } => return Err(anyhow!(message)),
    }

    // Let it tear down BLE + free heap before the flash begins.
    std::thread::sleep(Duration::from_secs(3));
    crate::ota::flash(ip, image, on_progress)
}

/// Best-effort: the SSID this computer is currently joined to. Per-OS and
/// allowed to fail (returns `None` → the caller prompts / leaves the field
/// blank). macOS in particular may redact the SSID without Location access.
pub fn current_ssid() -> Option<String> {
    use std::process::Command;

    let parse_after_colon = |out: &str, key: &str| -> Option<String> {
        out.lines()
            .map(str::trim)
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty() && v != "<redacted>")
    };

    let run = |cmd: &str, args: &[&str]| -> Option<String> {
        let out = Command::new(cmd).args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    };

    #[cfg(target_os = "macos")]
    {
        // `networksetup -getairportnetwork <dev>` → "Current Wi-Fi Network: NAME".
        for dev in ["en0", "en1"] {
            if let Some(out) = run("networksetup", &["-getairportnetwork", dev]) {
                if let Some(s) = parse_after_colon(&out, "Current Wi-Fi Network") {
                    return Some(s);
                }
            }
        }
        None
    }

    #[cfg(target_os = "linux")]
    {
        // nmcli marks the active connection with "yes:".
        let out = run("nmcli", &["-t", "-f", "active,ssid", "dev", "wifi"])?;
        out.lines()
            .find_map(|l| l.strip_prefix("yes:"))
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    }

    #[cfg(target_os = "windows")]
    {
        // `netsh wlan show interfaces` → a "SSID : NAME" line (not "BSSID").
        let out = run("netsh", &["wlan", "show", "interfaces"])?;
        out.lines()
            .map(str::trim)
            .find(|l| l.starts_with("SSID") && !l.starts_with("BSSID"))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (parse_after_colon, run);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_size_is_per_board_with_cyd_fallback() {
        assert_eq!(ota_slot_size("cyd"), 0x1D0000); // partitions_ota.csv ota_0
        assert_eq!(ota_slot_size("fnk0104"), 0x640000); // partitions_ota_16mb.csv ota_0
        // An unknown board falls back to the smallest (CYD) slot — conservative.
        assert_eq!(ota_slot_size("nonesuch"), 0x1D0000);
    }
}
