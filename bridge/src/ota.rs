//! Over-the-air firmware flashing — a self-contained Rust implementation of the
//! ArduinoOTA `espota` protocol, so the desktop app (and CLI) can push firmware
//! to the buddy over Wi-Fi with no Python/PlatformIO dependency at runtime.
//!
//! Protocol (matches tools/espota.py, no-auth path):
//!   1. Bind a local TCP listener; MD5 the image.
//!   2. UDP-invite the device on :3232 with `"<cmd> <localPort> <size> <md5>\n"`
//!      (cmd 0 = FLASH); retry until it replies `OK`.
//!   3. The device connects back to our TCP port; stream the image in 1024-byte
//!      chunks, each acked by the device (the final ack — or a trailing result —
//!      contains `OK` once the on-device MD5 verifies).
//!
//! The device must already be in OTA mode (BLE + UI torn down to free heap — see
//! the firmware's `ota` command); otherwise `Update.begin()` fails for lack of a
//! contiguous flash buffer. Callers send that command first, then call [`flash`].

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::time::{Duration, Instant};

const OTA_PORT: u16 = 3232;
const CMD_FLASH: u32 = 0;
const CMD_AUTH: u32 = 200;

/// Flash `image` to the buddy at `ip` (already in OTA mode). `on_progress` is
/// called with 0..=100 as the transfer advances. Blocking — run on a worker
/// thread (the app) or `spawn_blocking` (async callers).
pub fn flash(ip: Ipv4Addr, image: &[u8], mut on_progress: impl FnMut(u8)) -> Result<()> {
    if image.is_empty() {
        bail!("firmware image is empty");
    }
    let md5hex = format!("{:x}", md5::compute(image));
    eprintln!(
        "[ota] flash start: ip={ip} size={} md5={md5hex}",
        image.len()
    );

    // The device connects *back* to this TCP port to pull the image.
    let listener =
        TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).context("binding the OTA listener")?;
    let local_port = listener.local_addr()?.port();
    eprintln!("[ota] listening on local port {local_port}");

    // --- invitation: UDP, retry until the device answers ---
    let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).context("binding the OTA UDP socket")?;
    udp.set_read_timeout(Some(Duration::from_secs(1)))?;
    let invite = format!("{CMD_FLASH} {local_port} {} {md5hex}\n", image.len());
    // Keep inviting for up to ~25s rather than a fixed 10 tries: the first
    // local-network packet from a GUI app can trigger a macOS permission prompt,
    // and the user may take several seconds to click Allow — we want a later
    // invite to land once the grant goes through, instead of giving up.
    let mut answered = false;
    let invite_deadline = Instant::now() + Duration::from_secs(25);
    let mut attempt = 0u32;
    while Instant::now() < invite_deadline {
        attempt += 1;
        if let Err(e) = udp.send_to(invite.as_bytes(), (ip, OTA_PORT)) {
            eprintln!("[ota] invite send error (attempt {attempt}): {e}");
            // A send error here is often the local-network permission denial;
            // keep trying — the user may still be granting it.
            std::thread::sleep(Duration::from_millis(800));
            continue;
        }
        eprintln!("[ota] invite sent (attempt {attempt}), waiting for OK…");
        let mut buf = [0u8; 64];
        match udp.recv(&mut buf) {
            Ok(n) => {
                let resp = String::from_utf8_lossy(&buf[..n]);
                let resp = resp.trim();
                if resp == "OK" {
                    answered = true;
                    break;
                }
                if let Some(nonce) = resp.strip_prefix("AUTH ").map(str::trim) {
                    let password = std::env::var("BUDDY_OTA_PASSWORD").map_err(|_| {
                        anyhow::anyhow!(
                            "the buddy requires an OTA password; set BUDDY_OTA_PASSWORD and retry"
                        )
                    })?;
                    let cnonce_seed = format!(
                        "{}:{}:{}:{}:{:?}",
                        ip,
                        local_port,
                        image.len(),
                        md5hex,
                        std::time::SystemTime::now()
                    );
                    let cnonce = format!("{:x}", md5::compute(cnonce_seed.as_bytes()));
                    let passmd5 = format!("{:x}", md5::compute(password.as_bytes()));
                    let result_text = format!("{passmd5}:{nonce}:{cnonce}");
                    let result = format!("{:x}", md5::compute(result_text.as_bytes()));
                    let auth = format!("{CMD_AUTH} {cnonce} {result}\n");
                    udp.send_to(auth.as_bytes(), (ip, OTA_PORT))
                        .context("sending OTA authentication")?;
                    udp.set_read_timeout(Some(Duration::from_secs(10)))?;
                    let mut auth_buf = [0u8; 64];
                    let n = udp
                        .recv(&mut auth_buf)
                        .context("waiting for OTA authentication response")?;
                    let auth_resp = String::from_utf8_lossy(&auth_buf[..n]);
                    if auth_resp.trim() == "OK" {
                        answered = true;
                        break;
                    }
                    bail!("the buddy rejected the OTA password: {}", auth_resp.trim());
                }
                if resp.starts_with("AUTH") {
                    bail!("the buddy sent an invalid OTA auth challenge: {resp}");
                }
                bail!("the buddy refused the update: {resp}");
            }
            Err(_) => continue, // timed out — re-invite
        }
    }
    if !answered {
        eprintln!(
            "[ota] no OK after 25s of invites — invites likely dropped (Local Network permission?)"
        );
        bail!("no response from the buddy — make sure it's in update mode and on Wi-Fi");
    }
    eprintln!("[ota] got OK; waiting for the device to connect back…");

    // --- transfer: accept the device's TCP connection, stream chunks ---
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut conn = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("the buddy didn't connect back to start the transfer");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e).context("accepting the OTA connection"),
        }
    };
    conn.set_nonblocking(false)?;
    let _ = conn.set_nodelay(true);
    eprintln!("[ota] device connected back — streaming image…");

    let total = image.len();
    let mut offset = 0usize;
    let mut last_ok = false;
    on_progress(0);
    for chunk in image.chunks(1024) {
        conn.set_write_timeout(Some(Duration::from_secs(10)))?;
        conn.write_all(chunk).context("sending a firmware chunk")?;
        offset += chunk.len();
        on_progress((offset * 100 / total) as u8);
        // The device acks every chunk (cumulative byte count; the last one — or a
        // trailing message — carries OK once its MD5 check passes).
        conn.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut ack = [0u8; 32];
        match conn.read(&mut ack) {
            Ok(0) => bail!("the buddy closed the connection mid-transfer"),
            Ok(n) => last_ok = String::from_utf8_lossy(&ack[..n]).contains("OK"),
            Err(e) => return Err(e).context("reading the transfer ack"),
        }
    }
    if last_ok {
        return Ok(());
    }

    // Some firmwares send the OK only after the full image (MD5 verify).
    conn.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut res = [0u8; 64];
    let n = conn
        .read(&mut res)
        .context("reading the final OTA result")?;
    let s = String::from_utf8_lossy(&res[..n]);
    if s.contains("OK") {
        Ok(())
    } else {
        bail!("the buddy rejected the image: {}", s.trim());
    }
}

/// Board id assumed when the connected device doesn't report one (firmware that
/// predates the `{"board":...}` announce). The original single image was the
/// CYD, so unknown == cyd keeps those installs working.
pub const DEFAULT_BOARD: &str = "cyd";

/// Candidate firmware filenames for a board, most specific first. Multi-board
/// releases ship `firmware-<board>.bin`; the plain `firmware.bin` (no suffix) is
/// the legacy single image, which was always the CYD — so it's a fallback only
/// for the CYD, never for another board (flashing a CYD image onto an ESP32-S3
/// board would brick the flash).
fn firmware_filenames(board: &str) -> Vec<String> {
    let mut names = vec![format!("firmware-{board}.bin")];
    if board == DEFAULT_BOARD {
        names.push("firmware.bin".to_string());
    }
    names
}

/// Path to the firmware image bundled with the desktop app for `board`, resolved
/// relative to the running executable: `…/Claude Buddy.app/Contents/Resources/`
/// on macOS (the GUI lives in `Contents/MacOS`), or alongside the binary for dev
/// runs. Returns `None` if no matching image exists.
pub fn bundled_firmware_path(board: &str) -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let macos_dir = exe.parent()?;
    // Contents/MacOS/claude-buddy-app -> Contents/Resources/, else beside the binary.
    let dirs = [macos_dir.parent().map(|p| p.join("Resources")), Some(macos_dir.to_path_buf())];
    for dir in dirs.into_iter().flatten() {
        for name in firmware_filenames(board) {
            let p = dir.join(&name);
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

/// Version of the firmware image bundled for `board`. Read from a sibling
/// `*.version` file matching the resolved image (`firmware-<board>.version` or
/// the legacy `firmware.version`), falling back to the app's own version
/// (`CARGO_PKG_VERSION`) — valid because one release tag builds both the app and
/// the firmware it ships. Returns `None` only when no image is bundled for the
/// board, so the app can decide whether to offer an update. The string matches
/// what the device reports (`git describe`, e.g. `"v0.1.0"`); compare with
/// [`crate::update::is_newer`].
pub fn bundled_firmware_version(board: &str) -> Option<String> {
    let bin = bundled_firmware_path(board)?;
    let vfile = bin.with_extension("version");
    if let Ok(s) = std::fs::read_to_string(&vfile) {
        let s = s.trim();
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    Some(env!("CARGO_PKG_VERSION").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_filenames_are_board_specific_with_legacy_cyd_fallback() {
        // The CYD also accepts the legacy un-suffixed image (old bundles).
        assert_eq!(
            firmware_filenames("cyd"),
            vec!["firmware-cyd.bin".to_string(), "firmware.bin".to_string()]
        );
        // Every other board takes ONLY its own image — never the legacy CYD
        // image, which would be the wrong silicon.
        assert_eq!(
            firmware_filenames("fnk0104"),
            vec!["firmware-fnk0104.bin".to_string()]
        );
        assert_eq!(DEFAULT_BOARD, "cyd");
    }
}
