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
    /// Internal: invoked by Claude Code for a hook event.
    Hook {
        /// Claude Code hook event name (e.g. PreToolUse, Stop).
        event: String,
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
        Command::Hook { event } => hook::run(&event).await,
        Command::Setup { tools, no_service } => setup::run(&tools, !no_service),
        Command::Pair => pair().await,
        Command::Status => status(),
        Command::Wifi { ssid, pass } => wifi(ssid, pass).await,
        Command::Ota { image } => ota(image).await,
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
        println!("  Now push it over the air:");
        println!("      cd firmware && pio run -e cyd-ota -t upload --upload-port buddy.local");
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
    println!("  Update with:");
    println!("    pio run -e cyd-ota -t upload --upload-port buddy.local");
    println!("  (or use the IP shown in the daemon log / your router if .local fails)");
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
