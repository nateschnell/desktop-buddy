//! agent-buddy — the bridge between a CYD hardware buddy and Claude Code.
//!
//! One binary, several roles:
//!   daemon  — long-running: owns the BLE link + serves hook IPC
//!   hook    — short-lived: invoked by Claude Code on each hook event
//!   setup   — wire hooks into Claude Code settings + install the service
//!   pair    — one-shot connectivity test
//!   status  — show config + whether the daemon is up

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use agent_buddy::{ble, client, daemon, hook, ipc, ota, protocol, setup, state};
#[cfg(feature = "pack")]
use agent_buddy::packs;
#[cfg(feature = "pack")]
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "agent-buddy", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the bridge daemon (holds the BLE connection, serves hooks).
    Daemon {
        /// Skip BLE and use a virtual device that auto-answers prompts. Lets
        /// you exercise the full hook→daemon→decision loop with no hardware.
        #[arg(long)]
        mock_device: bool,
        /// What the mock device decides: `approve` or `deny`.
        #[arg(long, default_value = "approve")]
        mock_decision: String,
    },
    /// Internal: invoked by an agent harness for a hook event.
    Hook {
        /// Hook event name (e.g. PreToolUse, Stop) as the harness names it.
        event: String,
        /// Which agent harness this hook belongs to. Selects the profile used to
        /// map the event. Absent ⇒ Claude Code (the legacy, default path).
        #[arg(long)]
        agent: Option<String>,
    },
    /// Wire hooks into Claude Code and install the background service.
    Setup {
        /// Regex of tool names whose permission prompts go to the buddy.
        #[arg(long, default_value = setup::DEFAULT_MATCHER)]
        tools: String,
        /// Skip installing the background service (just wire hooks).
        #[arg(long)]
        no_service: bool,
    },
    /// Reverse `setup`: remove our hooks, the background daemon + service, the
    /// desktop login item/launcher, and the per-user state. Best-effort.
    Uninstall,
    /// Scan for a buddy, connect once, and report — for verifying setup.
    Pair,
    /// Show config and whether a daemon endpoint is published.
    Status,
    /// Send Wi-Fi credentials to the buddy so it can be updated over the air.
    /// Routed through the running daemon's BLE link; falls back to a direct
    /// connection if no daemon is running.
    Wifi {
        /// Network name. Omit to auto-detect this computer's current Wi-Fi.
        #[arg(long)]
        ssid: Option<String>,
        /// Password. Omit to read it from $BUDDY_WIFI_PASS or an stdin prompt.
        #[arg(long)]
        pass: Option<String>,
    },
    /// Put the buddy into OTA update mode (frees the memory a flash needs), then
    /// — with `--image` — push that firmware over the air via the built-in
    /// flasher (no PlatformIO needed). Without `--image`, just enters update mode
    /// and prints how to flash manually.
    Ota {
        /// Path to a firmware .bin to flash over the air after entering OTA mode.
        #[arg(long)]
        image: Option<String>,
    },
    /// Build animation packs from sprite-forge output (and, later, push them to
    /// the buddy). See `pack build --help`.
    #[cfg(feature = "pack")]
    Pack {
        #[command(subcommand)]
        cmd: PackCmd,
    },
}

/// Subcommands of `agent-buddy pack`.
#[cfg(feature = "pack")]
#[derive(Subcommand)]
enum PackCmd {
    /// Encode a sprite-forge pack dir into the `.spr` files the device renders.
    /// Pure host-side: writes `<out>/<id>/<state>.spr`, no device needed.
    Build {
        /// sprite-forge pack dir, e.g. `tools/sprite-forge/packs/claude-code`.
        #[arg(long)]
        src: PathBuf,
        /// Pack id (the `/agents/<id>` folder name). Defaults to the src dir name.
        #[arg(long)]
        id: Option<String>,
        /// Output dir holding `<id>/<state>.spr`. Defaults to the config dir's
        /// `packs/` (where `pack push` looks for it).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Target sprite height in px (aspect-preserved, clamped to 154).
        #[arg(long, default_value_t = 140)]
        height: u16,
        /// Per-frame duration override (ms). Default: per-state from metadata.
        #[arg(long)]
        frame_ms: Option<u16>,
        /// Don't set the loop flag in the `.spr` files.
        #[arg(long)]
        no_loop: bool,
        /// Alpha below this (0–255) maps to the transparent key.
        #[arg(long, default_value_t = 128)]
        alpha_threshold: u8,
    },
    /// Stream a built pack's `.spr` files to the connected buddy over BLE
    /// (through the running daemon). Lands them at `/agents/<id>/`.
    Push {
        /// On-disk pack dir of `<state>.spr` files. Defaults to the config dir's
        /// `packs/<id>` (where `pack build` writes).
        #[arg(long)]
        src: Option<PathBuf>,
        /// Pack id (the `/agents/<id>` folder name). Defaults to the src dir name.
        #[arg(long)]
        id: Option<String>,
        /// Also point the active agent's theme at this pack so it displays even
        /// when its id differs from the active harness.
        #[arg(long)]
        set_active: bool,
    },
    /// Build a sprite-forge pack and push it to the buddy in one step.
    Install {
        /// sprite-forge pack dir, e.g. `tools/sprite-forge/packs/claude-code`.
        #[arg(long)]
        src: PathBuf,
        /// Pack id (the `/agents/<id>` folder name). Defaults to the src dir name.
        #[arg(long)]
        id: Option<String>,
        /// Target sprite height in px (aspect-preserved, clamped to 154).
        #[arg(long, default_value_t = 140)]
        height: u16,
        /// Per-frame duration override (ms). Default: per-state from metadata.
        #[arg(long)]
        frame_ms: Option<u16>,
        /// Don't set the loop flag in the `.spr` files.
        #[arg(long)]
        no_loop: bool,
        /// Alpha below this (0–255) maps to the transparent key.
        #[arg(long, default_value_t = 128)]
        alpha_threshold: u8,
        /// Also point the active agent's theme at this pack so it displays.
        #[arg(long)]
        set_active: bool,
    },
}

fn main() -> Result<()> {
    // Capture the local UTC offset NOW, while the process is still
    // single-threaded — the `time` crate refuses `current_local_offset()` once
    // tokio spawns worker threads. Falls back to UTC if even this fails.
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let _ = daemon::LOCAL_OFFSET.set(offset);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main())
}

async fn async_main() -> Result<()> {
    // IMPORTANT: logs go to stderr. For the `hook` role, stdout carries the
    // permission verdict JSON that Claude Code parses.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("AGENT_BUDDY_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Daemon {
            mock_device,
            mock_decision,
        } => {
            let mock = if mock_device {
                Some(daemon::MockPolicy::from_str(&mock_decision))
            } else {
                None
            };
            daemon::run(mock).await
        }
        Command::Hook { event, agent } => hook::run(&event, agent.as_deref()).await,
        Command::Setup { tools, no_service } => setup::run(&tools, !no_service),
        Command::Uninstall => {
            println!("{}", setup::uninstall()?);
            Ok(())
        }
        Command::Pair => pair().await,
        Command::Status => status(),
        Command::Wifi { ssid, pass } => wifi(ssid, pass).await,
        Command::Ota { image } => ota(image).await,
        #[cfg(feature = "pack")]
        Command::Pack { cmd } => pack(cmd).await,
    }
}

/// `agent-buddy pack <sub>`.
#[cfg(feature = "pack")]
async fn pack(cmd: PackCmd) -> Result<()> {
    match cmd {
        PackCmd::Build {
            src,
            id,
            out,
            height,
            frame_ms,
            no_loop,
            alpha_threshold,
        } => pack_build(src, id, out, height, frame_ms, !no_loop, alpha_threshold).map(|_| ()),
        PackCmd::Push {
            src,
            id,
            set_active,
        } => pack_push(src, id, set_active).await,
        PackCmd::Install {
            src,
            id,
            height,
            frame_ms,
            no_loop,
            alpha_threshold,
            set_active,
        } => {
            // Build into the default cache dir, then push that.
            let (out_pack, id) =
                pack_build(src, id, None, height, frame_ms, !no_loop, alpha_threshold)?;
            pack_push(Some(out_pack), Some(id), set_active).await
        }
    }
}

/// Encode a sprite-forge pack dir into `<out>/<id>/<state>.spr`.
#[cfg(feature = "pack")]
fn pack_build(
    src: PathBuf,
    id: Option<String>,
    out: Option<PathBuf>,
    height: u16,
    frame_ms: Option<u16>,
    loop_anim: bool,
    alpha_threshold: u8,
) -> Result<(PathBuf, String)> {
    if !src.is_dir() {
        anyhow::bail!("source pack dir not found: {}", src.display());
    }
    let id = id
        .or_else(|| {
            src.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
        .context("could not derive a pack id from --src; pass --id")?;
    let out_root = match out {
        Some(o) => o,
        None => state::config_dir()?.join("packs"),
    };
    let out_pack = out_root.join(&id);

    let opts = packs::BuildOpts {
        target_h: height,
        frame_ms,
        loop_anim,
        alpha_threshold,
        transparent: packs::SPR_TRANSPARENT,
    };
    let reports = packs::build_pack(&src, &out_pack, &opts)?;

    println!("Built pack '{id}' → {}", out_pack.display());
    let mut total = 0usize;
    for r in &reports {
        total += r.bytes;
        println!(
            "  {:<10} {:>2} frames  {:>3}×{:<3}  {:>4} ms  {:>6.1} KB",
            r.state,
            r.frames,
            r.w,
            r.h,
            r.frame_ms,
            r.bytes as f64 / 1024.0,
        );
    }
    println!("  {} states, {:.1} KB total", reports.len(), total as f64 / 1024.0);
    // The FNK0104's LittleFS budget is ~3.375 MB; warn well before that.
    const BUDGET_WARN: usize = 2 * 1024 * 1024;
    if total > BUDGET_WARN {
        println!(
            "  ⚠ {:.1} MB is large for the device filesystem (~3.4 MB) — consider \
             a smaller --height or fewer frames.",
            total as f64 / (1024.0 * 1024.0)
        );
    }
    println!("Push it with:  agent-buddy pack push --src {}", out_pack.display());
    Ok((out_pack, id))
}

/// Stream a built pack's `.spr` files to the connected buddy via the daemon.
#[cfg(feature = "pack")]
async fn pack_push(src: Option<PathBuf>, id: Option<String>, set_active: bool) -> Result<()> {
    use ipc::{AdminRequest, AdminResponse, DeviceCommand};

    // Resolve (dir, id): an explicit --src wins (id defaults to its name); else
    // --id points at the built pack under the config dir.
    let (dir, id) = match (src, id) {
        (Some(src), id) => {
            let id = id
                .or_else(|| src.file_name().and_then(|n| n.to_str()).map(String::from))
                .context("could not derive a pack id from --src; pass --id")?;
            (src, id)
        }
        (None, Some(id)) => (state::config_dir()?.join("packs").join(&id), id),
        (None, None) => {
            anyhow::bail!("pass --src <pack dir> or --id <pack-id> (a previously built pack)")
        }
    };
    if !dir.is_dir() {
        anyhow::bail!("pack dir not found: {} (run `agent-buddy pack build` first)", dir.display());
    }
    // Canonicalize: the daemon runs from a different cwd and reads these files.
    let dir = std::fs::canonicalize(&dir).with_context(|| format!("resolving {}", dir.display()))?;
    let has_spr = std::fs::read_dir(&dir)?.flatten().any(|e| {
        e.path().extension().and_then(|x| x.to_str()) == Some("spr")
    });
    if !has_spr {
        anyhow::bail!("no .spr files in {} — run `agent-buddy pack build` first", dir.display());
    }

    let ep = ipc::read_endpoint().map_err(|_| {
        anyhow::anyhow!("no daemon is running — pushing a pack needs the daemon's BLE link")
    })?;
    println!("Pushing pack '{id}' to the buddy…");
    let last = std::sync::atomic::AtomicU8::new(255);
    let resp = send_admin_streaming(
        &ep,
        &AdminRequest {
            token: ep.token.clone(),
            command: DeviceCommand::PushPack {
                id: id.clone(),
                dir,
                set_active,
            },
        },
        |done, total, _file| {
            let pct = (done.saturating_mul(100) / total.max(1)) as u8;
            let prev = last.swap(pct, std::sync::atomic::Ordering::Relaxed);
            if pct == 100 || prev == 255 || pct / 5 != prev / 5 {
                eprint!("\r  {pct:3}%");
                let _ = std::io::Write::flush(&mut std::io::stderr());
            }
        },
    )
    .await?;
    eprintln!();
    match resp {
        AdminResponse::Ok { .. } => {
            println!("✓ pushed pack '{id}' to /agents/{id} on the buddy.");
            if !set_active {
                println!(
                    "  If '{id}' isn't the active agent's pack, it won't display yet — \
                     re-run with --set-active or switch agents."
                );
            }
            Ok(())
        }
        AdminResponse::NoDevice => {
            anyhow::bail!("the daemon is running but no buddy is connected — wake it and retry")
        }
        AdminResponse::Error { message } => anyhow::bail!("push failed: {message}"),
    }
}

/// Like [`send_admin`], but for the streaming `PushPack` reply: relays each
/// `{"kind":"progress",...}` line to `on_progress` and returns the terminal
/// [`AdminResponse`]. A long per-line timeout backstops the daemon-side per-step
/// timeouts (which surface a terminal error quickly on a real stall).
#[cfg(feature = "pack")]
async fn send_admin_streaming(
    ep: &ipc::Endpoint,
    req: &ipc::AdminRequest,
    mut on_progress: impl FnMut(u64, u64, &str),
) -> Result<ipc::AdminResponse> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", ep.port)).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut bytes = serde_json::to_vec(req)?;
    bytes.push(b'\n');
    write_half.write_all(&bytes).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            reader.read_line(&mut line),
        )
        .await
        .map_err(|_| anyhow::anyhow!("the daemon went silent during the push"))??;
        if n == 0 {
            anyhow::bail!("the daemon closed the connection mid-push");
        }
        let trimmed = line.trim();
        // A progress line has done/total/file; a terminal AdminResponse doesn't,
        // so it fails this parse and falls through.
        if let Ok(p) = serde_json::from_str::<ipc::PushProgress>(trimmed) {
            if p.kind == "progress" {
                on_progress(p.done, p.total, &p.file);
                continue;
            }
        }
        return Ok(serde_json::from_str(trimmed)?);
    }
}

/// One-shot: scan, connect, say hello, listen briefly, disconnect.
async fn pair() -> Result<()> {
    use protocol::{Heartbeat, OutboundCmd, TimeSync};
    let config = state::Config::load()?;
    println!("Scanning for a Claude buddy (up to 12s)…");
    let (link, mut lines, _id) =
        ble::BleLink::connect(config.preferred_device.as_deref(), 12).await?;

    link.send(&TimeSync { time: (0, 0) }).await?;
    link.send(&OutboundCmd::Owner {
        name: config.owner.clone(),
    })
    .await?;
    link.send(&Heartbeat {
        total: 1,
        msg: "paired!".into(),
        ..Default::default()
    })
    .await?;
    println!("✓ connected and sent a hello heartbeat.");

    // Drain anything the device says for a couple seconds.
    let listen = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(l) = lines.recv().await {
            println!("  device: {l}");
        }
    });
    let _ = listen.await;
    link.disconnect().await;
    println!("Done. If you saw acks above, the link works.");
    Ok(())
}

fn status() -> Result<()> {
    let config = state::Config::load()?;
    println!("owner:            {}", config.owner);
    println!(
        "preferred device: {}",
        config
            .preferred_device
            .as_deref()
            .unwrap_or("(first found)")
    );
    println!(
        "tokens today:     {} ({})",
        config.tokens_today, config.tokens_day
    );

    // Honest: a parseable endpoint.json only proves a daemon once existed (the
    // file outlives a crash). Probe the socket so we don't claim a dead daemon
    // is running. `endpoint_if_live` also cleans up a confirmed-stale file.
    match ipc::endpoint_if_live() {
        Ok(_) => {
            let path = state::config_dir()?.join(ipc::ENDPOINT_FILE);
            println!("daemon endpoint:  {} (daemon is running)", path.display());
        }
        Err(_) => {
            println!(
                "daemon endpoint:  none (daemon not running — start with `agent-buddy daemon`)"
            );
        }
    }
    Ok(())
}

/// Provision the buddy's Wi-Fi for OTA. Resolves the SSID (flag → auto-detect →
/// prompt) and password (flag → $BUDDY_WIFI_PASS → prompt), then pushes them to
/// the device: via the running daemon's BLE link if one is up, else by
/// connecting directly.
async fn wifi(ssid: Option<String>, pass: Option<String>) -> Result<()> {
    use ipc::{AdminRequest, AdminResponse, DeviceCommand};
    use protocol::OutboundCmd;

    let ssid = match ssid {
        Some(s) => s,
        None => match client::current_ssid() {
            Some(s) => {
                println!("Detected current network: {s}");
                prompt(&format!("Network name [{s}]: "))?
                    .filter(|v| !v.is_empty())
                    .unwrap_or(s)
            }
            None => prompt("Network name: ")?
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow::anyhow!("a network name is required"))?,
        },
    };

    let pass = match pass.or_else(|| std::env::var("BUDDY_WIFI_PASS").ok()) {
        Some(p) => p,
        // Note: this prompt echoes. Pass --pass or $BUDDY_WIFI_PASS to avoid it.
        None => prompt(&format!("Password for \"{ssid}\": "))?.unwrap_or_default(),
    };

    let command = DeviceCommand::Wifi {
        ssid: ssid.clone(),
        pass,
    };

    // Preferred path: hand it to the daemon, which owns the single BLE link.
    //
    // A BLE peripheral accepts ONE central. If a live daemon holds the link, we
    // must NEVER open a competing central — that would make both fail and the
    // buddy look dead. So we only fall back to a direct connect when there is
    // *no daemon listening at all*. We distinguish the two cases by whether the
    // IPC socket connects: a refused/absent socket means no daemon (direct
    // fallback is safe); a connected socket means the daemon is live and any
    // later error (timeout, write, parse) must surface to the user with a
    // retry hint, not trigger a fallback.
    if let Ok(ep) = ipc::read_endpoint() {
        match send_admin(
            &ep,
            &AdminRequest {
                token: ep.token.clone(),
                command: command.clone(),
            },
        )
        .await
        {
            Ok(AdminResponse::Ok { joined }) => {
                report_sent(&ssid, joined.as_deref());
                return Ok(());
            }
            Ok(AdminResponse::NoDevice) => {
                anyhow::bail!(
                    "the daemon is running but no buddy is connected — wake the buddy and retry"
                );
            }
            Ok(AdminResponse::Error { message }) => anyhow::bail!("daemon error: {message}"),
            Err(e) => {
                // Reached a live daemon but the exchange failed: do NOT open a
                // second central behind its back. Probe the socket to tell
                // "daemon is up but slow/erroring" from "daemon went away".
                if daemon_socket_alive(ep.port).await {
                    anyhow::bail!(
                        "the daemon is running but didn't answer in time ({e}) — \
                         please try again in a moment"
                    );
                }
                eprintln!("could not reach the daemon ({e}); trying a direct connection…");
            }
        }
    }

    // Fallback: no daemon (or it went away) — connect directly. Safe only
    // because without a daemon nothing else is holding the device's one link.
    let DeviceCommand::Wifi { ssid, pass } = command else {
        unreachable!("wifi() only ever builds DeviceCommand::Wifi");
    };
    println!("Scanning for a Claude buddy (up to 12s)…");
    let config = state::Config::load()?;
    let (link, _lines, _id) = ble::BleLink::connect(config.preferred_device.as_deref(), 12).await?;
    link.send(&OutboundCmd::Wifi {
        ssid: ssid.clone(),
        pass,
    })
    .await?;
    link.disconnect().await;
    // Direct path can't observe the device's ack/join (no notification pump
    // wired here), so report the transmit-only outcome.
    report_sent(&ssid, None);
    Ok(())
}

/// Put the buddy into OTA update mode, then guide the flash. Must go through the
/// daemon (it owns the BLE link); there's no direct fallback because the whole
/// point is to have the device tear BLE down. After this, espota's `begin()` has
/// the full heap (BLE + the UI sprite are freed), which is what makes OTA work on
/// this RAM-tight board.
async fn ota(image: Option<String>) -> Result<()> {
    use ipc::{AdminRequest, AdminResponse, DeviceCommand};

    let ep = ipc::read_endpoint().map_err(|_| {
        anyhow::anyhow!(
            "no daemon is running — OTA mode needs the daemon's BLE link to reach the buddy"
        )
    })?;

    // No image: just enter OTA mode and tell the user how to flash manually.
    let Some(image) = image else {
        match send_admin(
            &ep,
            &AdminRequest {
                token: ep.token.clone(),
                command: DeviceCommand::Ota,
            },
        )
        .await
        {
            Ok(AdminResponse::Ok { .. }) => {}
            Ok(AdminResponse::NoDevice) => {
                anyhow::bail!("the daemon is running but no buddy is connected — wake it and retry")
            }
            Ok(AdminResponse::Error { message }) => anyhow::bail!("daemon error: {message}"),
            Err(e) => anyhow::bail!("could not reach the daemon: {e}"),
        }
        println!("✓ buddy is entering update mode — its screen shows \"Updating firmware\".");
        println!("  It's now listening for an image at buddy.local. Flash one with the");
        println!("  desktop app's \"Update firmware\" button, or re-run with an image path:");
        println!("      agent-buddy ota <firmware.bin>");
        return Ok(());
    };

    // Built-in flasher path: shared orchestration (enter OTA mode → flash).
    let bytes = std::fs::read(&image).with_context(|| format!("reading {image}"))?;
    // Board id for the slot-size sanity check — ask the daemon what's connected,
    // defaulting to the CYD when it's unknown (older firmware / no status).
    let board = client::status()
        .ok()
        .and_then(|s| s.device_board)
        .unwrap_or_else(|| ota::DEFAULT_BOARD.to_string());
    println!(
        "Updating the buddy over the air ({} KB)…",
        bytes.len() / 1024
    );
    let last = std::sync::atomic::AtomicU8::new(255);
    tokio::task::spawn_blocking(move || {
        client::update_firmware(&bytes, &board, |pct| {
            let prev = last.swap(pct, std::sync::atomic::Ordering::Relaxed);
            if pct == 100 || prev == 255 || pct / 10 != prev / 10 {
                eprint!("\r  {pct:3}%");
                let _ = std::io::Write::flush(&mut std::io::stderr());
            }
        })
    })
    .await
    .context("OTA task panicked")??;
    eprintln!();
    println!("✓ update complete — the buddy is rebooting into the new firmware.");
    Ok(())
}

/// Is something actually listening on the daemon's IPC port right now? A bare
/// connect with a short timeout: a refused/timed-out socket means no live
/// daemon, so a direct BLE fallback is safe.
async fn daemon_socket_alive(port: u16) -> bool {
    let connect = tokio::net::TcpStream::connect(("127.0.0.1", port));
    matches!(
        tokio::time::timeout(std::time::Duration::from_millis(500), connect).await,
        Ok(Ok(_))
    )
}

/// Print the post-provisioning guidance (how to actually run an OTA upload).
/// `joined` is the network the buddy confirmed joining, when known.
fn report_sent(ssid: &str, joined: Option<&str>) {
    match joined {
        Some(net) => println!("✓ buddy stored and joined \"{net}\"."),
        None => {
            println!("✓ sent Wi-Fi credentials for \"{ssid}\" to the buddy.");
            println!("  It will join the network and be reachable for OTA at buddy.local.");
        }
    }
    println!("  Update its firmware from the desktop app's \"Update firmware\" button,");
    println!("  or run:  agent-buddy ota <firmware.bin>");
    println!("  (reachable at buddy.local once it joins; use the IP from the daemon log if .local fails)");
}

/// One round-trip to the daemon's IPC socket for an admin command.
async fn send_admin(ep: &ipc::Endpoint, req: &ipc::AdminRequest) -> Result<ipc::AdminResponse> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", ep.port)).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut bytes = serde_json::to_vec(req)?;
    bytes.push(b'\n');
    write_half.write_all(&bytes).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    // Generous: a Wi-Fi command is correlated daemon-side — the daemon parks the
    // reply until the device confirms storing the creds (and briefly until it
    // joins), up to ~14s. Stay above that so a slow-but-working confirmation
    // isn't misread as a timeout (which the caller now treats as live-but-slow,
    // not a reason to open a competing BLE central).
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        reader.read_line(&mut line),
    )
    .await??;
    Ok(serde_json::from_str(line.trim())?)
}

/// Print a prompt to stdout and read one trimmed line from stdin. Returns
/// `None` on EOF (e.g. non-interactive with no input).
fn prompt(label: &str) -> Result<Option<String>> {
    use std::io::Write;
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
}
