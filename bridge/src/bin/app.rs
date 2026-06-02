//! agent-buddy-app — the desktop control panel.
//!
//! A small native window (eframe/egui) that drives the always-on daemon: it
//! installs/starts the background service, shows live daemon + buddy status,
//! and provisions the buddy's Wi-Fi. It is a *thin client* — it never opens the
//! Bluetooth radio itself; every device action is relayed through the daemon
//! over the local IPC socket (see `client.rs`). That's why the daemon can keep
//! the buddy linked even while this window is closed.
//!
//! All IPC and service-control work happens on a background worker thread so a
//! slow round-trip never freezes the UI.

#![cfg_attr(windows, windows_subsystem = "windows")] // no console window on Windows

use agent_buddy::ipc::StatusReport;
use agent_buddy::{client, ota, setup, state, update};
use eframe::egui;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

// --- palette --------------------------------------------------------------
// Claude's own identity — warm paper, terracotta clay, ink — rather than the
// generic indigo-on-white look. Everything keys off these eight colors.
const BG: egui::Color32 = egui::Color32::from_rgb(0xF4, 0xF2, 0xEA); // warm paper
const CARD: egui::Color32 = egui::Color32::from_rgb(0xFD, 0xFC, 0xF8); // soft white
const INK: egui::Color32 = egui::Color32::from_rgb(0x23, 0x21, 0x1B); // warm near-black
const MUTED: egui::Color32 = egui::Color32::from_rgb(0x8C, 0x86, 0x79); // warm gray
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0xC1, 0x5F, 0x3C); // clay/terracotta
const ACCENT_HOVER: egui::Color32 = egui::Color32::from_rgb(0xAB, 0x52, 0x33); // pressed clay
const GOOD: egui::Color32 = egui::Color32::from_rgb(0x4F, 0x7A, 0x52); // sage
const BAD: egui::Color32 = egui::Color32::from_rgb(0xB5, 0x40, 0x34); // brick
const HAIR: egui::Color32 = egui::Color32::from_rgb(0xE7, 0xE3, 0xD8); // warm hairline

/// A request from the UI to the worker thread.
enum Cmd {
    Refresh,
    Provision {
        ssid: String,
        pass: String,
    },
    InstallStart,
    Start,
    /// Restart the running daemon in place (launchctl kickstart), preserving the
    /// always-on KeepAlive contract — never an unload/load that would tear the
    /// link down and race the respawn.
    Restart,
    Stop,
    /// Flash the bundled firmware to the buddy over the air (one-click update).
    /// Carries the connected device's board id so the worker flashes the image
    /// built for that board.
    UpdateFirmware { board: String },
}

/// A result from the worker thread back to the UI.
enum Msg {
    Status(Result<StatusReport, String>),
    /// Outcome of a user-triggered action (ok?, message). Clears `busy`.
    Action(bool, String),
    /// OTA transfer progress, 0..=100. Drives the update progress bar.
    OtaProgress(u8),
}

fn main() -> eframe::Result<()> {
    // Single-instance guard. Two windows would each paint a menu-bar/tray icon
    // (the "two purple dots" bug) and both poll the daemon. Hold an advisory
    // flock for the process lifetime; if another instance already holds it, exit
    // cleanly so the login item's KeepAlive won't respawn us. `_lock` must stay
    // in scope for the whole run — the lock releases when it (or the process)
    // goes away, leaving nothing stale to clean up after a crash.
    let _lock = match acquire_app_lock() {
        Ok(lock) => lock,
        Err(_) => {
            eprintln!("Agent Buddy is already running.");
            return Ok(());
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 700.0])
            .with_min_inner_size([360.0, 420.0])
            .with_title("Agent Buddy"),
        // Open at the coded size every launch, centered. eframe otherwise
        // persists the last window geometry to storage — which is why a changed
        // default looks like it "didn't take": the remembered size wins. A small
        // control panel is better off with one consistent, known-good size (and
        // it can't get stranded off-screen by a stale saved position).
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        "Agent Buddy",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

/// Take an exclusive advisory lock so only one GUI runs at a time. Returns the
/// held file (keep it alive); errors if another instance holds the lock.
#[cfg(unix)]
fn acquire_app_lock() -> Result<std::fs::File, Box<dyn std::error::Error>> {
    use std::os::unix::io::AsRawFd;
    let path = state::config_dir()?.join("app.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(Box::new(std::io::Error::last_os_error()));
    }
    Ok(file)
}

/// Non-unix: no advisory lock; rely on the OS launcher to keep one instance.
#[cfg(not(unix))]
fn acquire_app_lock() -> Result<std::fs::File, Box<dyn std::error::Error>> {
    let path = state::config_dir()?.join("app.lock");
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&path)?)
}

struct App {
    tx: Sender<Cmd>,
    rx: Receiver<Msg>,
    status: Option<StatusReport>,
    status_err: Option<String>,
    last_action: Option<(bool, String)>,
    busy: bool,
    /// Live OTA transfer percentage while an update is in flight (`None` = idle).
    ota_progress: Option<u8>,
    ssid: String,
    pass: String,
    show_pass: bool,
    /// Whether we managed to read the current Wi-Fi automatically at launch.
    /// `false` on macOS without Location access (the SSID comes back redacted),
    /// which is the cue to show a "type your network name" hint.
    ssid_autofilled: bool,
    /// The live system-tray icon. Kept alive for the app's lifetime (dropping
    /// it removes the icon). `None` where a tray couldn't be created.
    tray: Option<tray_icon::TrayIcon>,
    /// Tray menu clicks, forwarded from the global event channel.
    tray_rx: Option<Receiver<TrayAction>>,
}

/// What a tray menu item does when clicked.
enum TrayAction {
    Open,
    Start,
    Stop,
    Quit,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_theme(&cc.egui_ctx);

        let (tx, rx_cmd) = std::sync::mpsc::channel::<Cmd>();
        let (tx_msg, rx) = std::sync::mpsc::channel::<Msg>();
        spawn_worker(cc.egui_ctx.clone(), rx_cmd, tx_msg);
        let _ = tx.send(Cmd::Refresh); // fetch immediately, don't wait for the tick

        let (tray, tray_rx) = init_tray(&cc.egui_ctx);

        let detected_ssid = client::current_ssid();
        App {
            tx,
            rx,
            status: None,
            status_err: None,
            last_action: None,
            busy: false,
            ota_progress: None,
            ssid_autofilled: detected_ssid.is_some(),
            ssid: detected_ssid.unwrap_or_default(),
            pass: String::new(),
            show_pass: false,
            tray,
            tray_rx,
        }
    }

    fn send(&mut self, cmd: Cmd) {
        self.busy = true;
        self.last_action = None;
        let _ = self.tx.send(cmd);
    }

    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Status(Ok(s)) => {
                    self.status = Some(s);
                    self.status_err = None;
                }
                Msg::Status(Err(e)) => {
                    self.status = None;
                    self.status_err = Some(e);
                }
                Msg::Action(ok, text) => {
                    self.busy = false;
                    self.ota_progress = None;
                    self.last_action = Some((ok, text));
                }
                Msg::OtaProgress(pct) => {
                    self.ota_progress = Some(pct);
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        self.handle_tray(ctx);
        // Keep the status ticking even if the user is idle.
        ctx.request_repaint_after(Duration::from_secs(2));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(BG)
                    .inner_margin(egui::Margin::same(18.0)),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                // Everything lives in a vertical scroll area so the panel stays
                // usable at any height — shrink the window and the content
                // scrolls instead of clipping off the bottom. `auto_shrink` off
                // keeps the cards full-width and the area full-height.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.header(ui);
                        ui.add_space(4.0);

                        let running = self.status.is_some();
                        self.status_card(ui);
                        self.update_card(ui);

                        if running {
                            // While the buddy isn't linked, lead with first-run
                            // pairing guidance rather than gating everything
                            // behind a connection that may be stuck.
                            let connected = self
                                .status
                                .as_ref()
                                .map(|s| s.device_connected)
                                .unwrap_or(false);
                            if !connected {
                                self.pairing_card(ui);
                            }
                            self.wifi_card(ui);
                            self.service_card(ui);
                        }

                        self.footer(ui);
                    });
            });
    }
}

// --- sections -------------------------------------------------------------
impl App {
    fn header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // A small clay tile as the mark, painted in code so there's no asset
            // to ship and it stays crisp at any DPI.
            let (rect, _) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, egui::Rounding::same(9.0), ACCENT);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "✳",
                egui::FontId::proportional(17.0),
                CARD,
            );
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.add_space(1.0);
                ui.label(
                    egui::RichText::new("Agent Buddy")
                        .color(INK)
                        .size(21.0)
                        .strong(),
                );
                ui.label(
                    egui::RichText::new("Hardware bridge")
                        .color(MUTED)
                        .size(11.5),
                );
            });
        });
        ui.add_space(2.0);
    }

    fn status_card(&mut self, ui: &mut egui::Ui) {
        // Set to the device's board id when an update button is clicked; the
        // worker then flashes the image bundled for that board.
        let mut want_update: Option<String> = None;
        card(ui, |ui| {
            // While a firmware update is flashing, the buddy is "disconnected"
            // over BLE by design — so show progress here, above the normal
            // connection state, and nothing else.
            if let Some(pct) = self.ota_progress {
                ui.label(
                    egui::RichText::new("Updating firmware")
                        .color(INK)
                        .size(15.0)
                        .strong(),
                );
                ui.add_space(8.0);
                ui.add(
                    egui::ProgressBar::new(pct as f32 / 100.0)
                        .desired_height(10.0)
                        .text(format!("{pct}%")),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("Keep the buddy powered — it reboots when done.")
                        .color(MUTED)
                        .size(11.0),
                );
                return;
            }
            match (&self.status, &self.status_err) {
                (Some(s), _) => {
                    status_row(ui, "Daemon", true, "running");
                    status_row(
                        ui,
                        "Buddy",
                        s.device_connected,
                        if s.device_connected {
                            "connected"
                        } else {
                            "not connected"
                        },
                    );

                    // When the buddy isn't linked, translate the daemon's
                    // diagnostics into a single actionable next step instead of
                    // leaving the user staring at "not connected".
                    if !s.device_connected {
                        let hint = disconnected_hint(s);
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(hint).color(MUTED).size(11.5));
                    }

                    ui.add_space(6.0);
                    hairline(ui);
                    ui.add_space(6.0);

                    metric(ui, "Owner", &s.owner);
                    metric(ui, "Tokens today", &fmt_count(s.tokens_today));
                    match fmt_sessions(s) {
                        // Only when something's actually live — otherwise a row of
                        // zeros reads as broken, not idle.
                        (text, true) => metric(ui, "Sessions", &text),
                        (text, false) => metric_colored(ui, "Sessions", &text, MUTED),
                    }
                    // Buddy firmware version, once it's reported it (older
                    // firmware never does → the row simply doesn't appear).
                    if let Some(fw) = &s.device_fw {
                        metric(ui, "Firmware", fw);
                    }
                    if let (Some(ssid), Some(ip)) = (&s.device_ssid, &s.device_ip) {
                        metric(ui, "On Wi-Fi", &format!("{ssid} · {ip}"));
                        match s.device_online {
                            Some(true) => metric_colored(ui, "Internet", "Online ✓", GOOD),
                            Some(false) => {
                                metric_colored(ui, "Internet", "joined, but no internet", BAD)
                            }
                            None => metric_colored(ui, "Internet", "checking…", MUTED),
                        }
                        // Over-the-air firmware update — needs Wi-Fi (ip known) and
                        // a firmware image bundled with this app. We only push the
                        // primary "update" button when the bundled image is
                        // actually *newer* than what the buddy runs; otherwise we
                        // either confirm it's current or, when the buddy didn't
                        // report a comparable version, still allow a manual flash.
                        // (Live progress renders at the top of the card, since the
                        // buddy "disconnects" over BLE during the flash.)
                        if self.ota_progress.is_none() {
                            // Which board the buddy reports decides which bundled
                            // image we compare against and flash. Older firmware
                            // omits it → assume the default (CYD).
                            let board = s
                                .device_board
                                .clone()
                                .unwrap_or_else(|| ota::DEFAULT_BOARD.to_string());
                            if let Some(bundled) = ota::bundled_firmware_version(&board) {
                                let newer = s
                                    .device_fw
                                    .as_deref()
                                    .map(|d| update::is_newer(&bundled, d))
                                    .unwrap_or(false);
                                let device_known = s
                                    .device_fw
                                    .as_deref()
                                    .and_then(update::parse_version)
                                    .is_some();
                                ui.add_space(10.0);
                                if newer {
                                    if primary_button(
                                        ui,
                                        &format!("Update firmware → {bundled}"),
                                        !self.busy,
                                    )
                                    .clicked()
                                    {
                                        // Deferred: `s` borrows self here, so we
                                        // can't self.send (mutable) until the card
                                        // closure returns.
                                        want_update = Some(board.clone());
                                    }
                                } else if device_known {
                                    // Buddy already runs this build (or newer) — a
                                    // quiet confirmation, no needless re-flash nudge.
                                    metric_colored(ui, "Firmware update", "up to date ✓", GOOD);
                                } else if primary_button(ui, "Update firmware", !self.busy)
                                    .clicked()
                                {
                                    // Buddy didn't report a comparable version
                                    // (older firmware / dev build) — still let the
                                    // user flash the bundled image by hand.
                                    want_update = Some(board.clone());
                                }
                            }
                        }
                    }

                    if !s.entries.is_empty() {
                        ui.add_space(6.0);
                        hairline(ui);
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new("RECENT ACTIVITY")
                                .color(MUTED)
                                .size(10.0)
                                .strong(),
                        );
                        ui.add_space(2.0);
                        for e in s.entries.iter().take(5) {
                            ui.label(egui::RichText::new(format!("· {e}")).color(INK).size(12.0));
                        }
                    }
                }
                (None, _) => {
                    status_row(ui, "Daemon", false, "not running");
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(
                            "The background bridge isn’t running. Install it once and it stays \
                             on — surviving reboots and this window closing.",
                        )
                        .color(MUTED)
                        .size(12.0),
                    );
                    ui.add_space(10.0);
                    if primary_button(ui, "Install & start background service", !self.busy)
                        .clicked()
                    {
                        self.send(Cmd::InstallStart);
                    }
                    if let Some(err) = &self.status_err {
                        // Only worth showing if it's not the plain "not running".
                        if !err.contains("isn’t running") {
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new(err).color(BAD).size(11.0));
                        }
                    }
                }
            }
        });
        if let Some(board) = want_update {
            self.send(Cmd::UpdateFirmware { board });
        }
    }

    /// "A newer Agent Buddy is out" banner. Renders only when the daemon's
    /// periodic check found a strictly-newer release. The action is a guided
    /// download (opens the GitHub release page) rather than an in-place
    /// self-update: replacing the running app bundle needs Developer ID signing +
    /// notarization to clear Gatekeeper, which isn't set up yet. Once it is, this
    /// becomes the place to wire an automatic updater.
    fn update_card(&mut self, ui: &mut egui::Ui) {
        let Some((latest, url, current)) = self.status.as_ref().and_then(|s| {
            s.update
                .as_ref()
                .filter(|u| u.available && !u.url.is_empty())
                .map(|u| (u.latest.clone(), u.url.clone(), u.current.clone()))
        }) else {
            return;
        };
        card(ui, |ui| {
            ui.label(
                egui::RichText::new("Update available")
                    .color(ACCENT)
                    .size(15.0)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!(
                    "Agent Buddy {latest} is out — you have v{current}."
                ))
                .color(MUTED)
                .size(12.0),
            );
            // macOS can't self-swap an unsigned app bundle past Gatekeeper, so
            // it's a guided download + drag-to-Applications for now.
            #[cfg(target_os = "macos")]
            {
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(
                        "Download it, then drag it into Applications to replace this version.",
                    )
                    .color(MUTED)
                    .size(11.0),
                );
            }
            ui.add_space(10.0);
            if primary_button(ui, &format!("Download {latest}"), true).clicked() {
                open_url(&url);
            }
        });
    }

    /// First-run pairing / onboarding guidance, shown while the buddy isn't
    /// linked. Explains the 6-digit-code confirmation flow and offers a deep
    /// link to the Bluetooth settings pane plus a manual retry.
    fn pairing_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.label(
                egui::RichText::new("Pair your buddy")
                    .color(INK)
                    .size(15.0)
                    .strong(),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Power on your buddy and keep it nearby. The first time, it shows a \
                     6-digit code and macOS pops a Bluetooth window — confirm that the codes \
                     match to pair. After that it reconnects on its own.",
                )
                .color(MUTED)
                .size(12.0),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ghost_button(ui, "Open Bluetooth settings", true).clicked() {
                    open_bluetooth_settings();
                }
                if ghost_button(ui, "Retry", !self.busy).clicked() {
                    // A bare refresh re-reads the daemon's connect state; the
                    // daemon scans continuously, so this just pulls a fresh
                    // snapshot rather than forcing a reconnect.
                    self.send(Cmd::Refresh);
                }
            });
        });
    }

    fn wifi_card(&mut self, ui: &mut egui::Ui) {
        let s = self.status.as_ref();
        let connected = s.map(|s| s.device_connected).unwrap_or(false);
        // The buddy is already on a working network when it has reported a join
        // *and* its own internet probe came back online. In that case the full
        // credentials form is just clutter — tuck it behind a quiet disclosure
        // so re-provisioning is one click away without dominating the panel.
        let already_online = s
            .map(|s| s.device_ssid.is_some() && s.device_online == Some(true))
            .unwrap_or(false);

        if already_online {
            card(ui, |ui| {
                egui::CollapsingHeader::new(
                    egui::RichText::new("Change Wi-Fi network")
                        .color(INK)
                        .size(14.0)
                        .strong(),
                )
                .id_source("wifi_change")
                .show_unindented(ui, |ui| {
                    ui.add_space(6.0);
                    self.wifi_form(ui, connected);
                });
            });
            return;
        }

        card(ui, |ui| {
            ui.label(
                egui::RichText::new("Provision Wi-Fi")
                    .color(INK)
                    .size(15.0)
                    .strong(),
            );
            ui.label(
                egui::RichText::new(
                    "Send your network to the buddy so it can update over the air.",
                )
                .color(MUTED)
                .size(12.0),
            );
            ui.add_space(8.0);
            self.wifi_form(ui, connected);
        });
    }

    /// The network/password inputs + send button. Shared by the open form and
    /// the collapsed "change network" disclosure.
    fn wifi_form(&mut self, ui: &mut egui::Ui, connected: bool) {
        field_label(ui, "Network");
        ui.add(
            egui::TextEdit::singleline(&mut self.ssid)
                .hint_text("Wi-Fi name")
                .desired_width(f32::INFINITY),
        );
        if !self.ssid_autofilled {
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(
                    "Couldn’t read your current network automatically — type your Wi-Fi name.",
                )
                .color(MUTED)
                .size(11.0),
            );
        }
        ui.add_space(6.0);

        field_label(ui, "Password");
        ui.add(
            egui::TextEdit::singleline(&mut self.pass)
                .password(!self.show_pass)
                .hint_text("Wi-Fi password")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(2.0);
        ui.checkbox(
            &mut self.show_pass,
            egui::RichText::new("Show password").size(11.0).color(MUTED),
        );

        ui.add_space(10.0);
        let can_send = connected && !self.busy && !self.ssid.trim().is_empty();
        if primary_button(ui, "Send to buddy", can_send).clicked() {
            let (ssid, pass) = (self.ssid.trim().to_string(), self.pass.clone());
            self.send(Cmd::Provision { ssid, pass });
        }
        if !connected {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Wake the buddy and wait for “connected” above first.")
                    .color(MUTED)
                    .size(11.0),
            );
        }
    }

    fn service_card(&mut self, ui: &mut egui::Ui) {
        // If we're rendering this card the daemon is already running (the caller
        // gates on `running`). So Start would be a no-op — relabel it and disable
        // it so a confused user can't churn a healthy always-on link. Restart is
        // the in-place re-exec (kickstart) and is the right knob if it's wedged.
        let running = self.status.is_some();
        card(ui, |ui| {
            ui.label(
                egui::RichText::new("Service")
                    .color(INK)
                    .size(15.0)
                    .strong(),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ghost_button(ui, "Restart", !self.busy).clicked() {
                    self.send(Cmd::Restart);
                }
                if ghost_button(ui, "Stop", !self.busy).clicked() {
                    self.send(Cmd::Stop);
                }
                let start_label = if running { "Running" } else { "Start" };
                if ghost_button(ui, start_label, !self.busy && !running).clicked() {
                    self.send(Cmd::Start);
                }
            });
            if running {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Already on and kept alive automatically.")
                        .color(MUTED)
                        .size(11.0),
                );
            }
        });
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        // Transient action feedback lives just above the baseline so the eye
        // lands on it after pressing a button.
        if self.busy {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(ACCENT));
                ui.label(egui::RichText::new("Working…").color(MUTED).size(12.0));
            });
        } else if let Some((ok, text)) = &self.last_action {
            let (mark, color) = if *ok { ("✓", GOOD) } else { ("✕", BAD) };
            ui.label(
                egui::RichText::new(format!("{mark}  {text}"))
                    .color(color)
                    .size(12.0),
            );
        }
        ui.add_space(8.0);
        // A single, quiet baseline — version only. The raw config path was dev
        // clutter the user never acts on, so it's gone.
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                        .color(MUTED)
                        .size(10.5),
                );
            });
        });
    }
}

// --- tray -----------------------------------------------------------------
impl App {
    /// Apply any pending tray-menu clicks, and turn a window-close into a
    /// hide-to-tray (so the panel tucks away while the daemon keeps running).
    fn handle_tray(&mut self, ctx: &egui::Context) {
        // Drain first so the immutable borrow ends before we call self.send().
        let mut actions = Vec::new();
        if let Some(rx) = &self.tray_rx {
            while let Ok(a) = rx.try_recv() {
                actions.push(a);
            }
        }
        for a in actions {
            match a {
                TrayAction::Open => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayAction::Start => self.send(Cmd::Start),
                TrayAction::Stop => self.send(Cmd::Stop),
                TrayAction::Quit => std::process::exit(0),
            }
        }

        // With a tray, the [x] hides to it instead of quitting; the daemon is
        // the always-on part, and the tray's Quit is how you actually exit.
        if self.tray.is_some() && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }
}

/// Build the tray icon and start forwarding its menu clicks. On Linux the
/// system tray needs a running GTK loop, which fights the winit loop eframe
/// owns — so we skip it there and the window simply behaves normally (closing
/// quits); every control is still reachable in-window.
#[cfg(not(target_os = "linux"))]
fn init_tray(ctx: &egui::Context) -> (Option<tray_icon::TrayIcon>, Option<Receiver<TrayAction>>) {
    let rx = spawn_menu_pump(ctx.clone());
    spawn_event_drain();
    (build_tray().ok(), Some(rx))
}

#[cfg(target_os = "linux")]
fn init_tray(_ctx: &egui::Context) -> (Option<tray_icon::TrayIcon>, Option<Receiver<TrayAction>>) {
    (None, None)
}

/// Construct the tray icon with its menu. Items carry stable string ids that
/// [`spawn_menu_pump`] matches on.
#[cfg(not(target_os = "linux"))]
fn build_tray() -> Result<tray_icon::TrayIcon, Box<dyn std::error::Error>> {
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
    let menu = Menu::new();
    menu.append(&MenuItem::with_id("open", "Open Agent Buddy", true, None))?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id("start", "Start service", true, None))?;
    menu.append(&MenuItem::with_id("stop", "Stop service", true, None))?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id("quit", "Quit Agent Buddy", true, None))?;

    let tray = tray_icon::TrayIconBuilder::new()
        .with_tooltip("Agent Buddy")
        .with_menu(Box::new(menu))
        .with_icon(tray_icon_image())
        .build()?;
    Ok(tray)
}

/// Forward tray menu clicks from the crate-global channel onto an mpsc the UI
/// drains, waking the UI immediately on each click.
#[cfg(not(target_os = "linux"))]
fn spawn_menu_pump(ctx: egui::Context) -> Receiver<TrayAction> {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let events = tray_icon::menu::MenuEvent::receiver();
        while let Ok(ev) = events.recv() {
            let action = match ev.id.0.as_str() {
                "open" => TrayAction::Open,
                "start" => TrayAction::Start,
                "stop" => TrayAction::Stop,
                "quit" => TrayAction::Quit,
                _ => continue,
            };
            if tx.send(action).is_err() {
                break;
            }
            ctx.request_repaint();
        }
    });
    rx
}

/// Drain (and discard) the tray's own hover/click event channel so it can't
/// grow unbounded — we drive everything off the menu instead.
#[cfg(not(target_os = "linux"))]
fn spawn_event_drain() {
    thread::spawn(|| {
        let events = tray_icon::TrayIconEvent::receiver();
        while events.recv().is_ok() {}
    });
}

/// A 32×32 RGBA app glyph drawn in code (no asset to ship): a filled indigo
/// disc with a soft anti-aliased edge.
#[cfg(not(target_os = "linux"))]
fn tray_icon_image() -> tray_icon::Icon {
    const N: u32 = 32;
    let mut rgba = vec![0u8; (N * N * 4) as usize];
    let c = (N as f32 - 1.0) / 2.0;
    let r = c;
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            let d = (dx * dx + dy * dy).sqrt();
            if d <= r {
                let i = ((y * N + x) * 4) as usize;
                let alpha = ((r - d) * 255.0).clamp(0.0, 255.0) as u8; // feather the rim
                rgba[i] = 0x5A;
                rgba[i + 1] = 0x4F;
                rgba[i + 2] = 0xE6;
                rgba[i + 3] = if d <= r - 1.0 { 255 } else { alpha };
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, N, N).expect("valid tray icon")
}

// --- worker ---------------------------------------------------------------
fn spawn_worker(ctx: egui::Context, rx: Receiver<Cmd>, tx: Sender<Msg>) {
    thread::spawn(move || {
        loop {
            // Block for a command; on timeout, do a routine status refresh.
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(Cmd::UpdateFirmware { board }) => {
                    // Long-running with live progress, so it can't go through the
                    // one-shot `handle()`; stream OtaProgress, then the outcome.
                    let action = match ota::bundled_firmware_path(&board) {
                        Some(path) => match std::fs::read(&path) {
                            Ok(bytes) => {
                                let txp = tx.clone();
                                match client::update_firmware(&bytes, &board, |pct| {
                                    let _ = txp.send(Msg::OtaProgress(pct));
                                    ctx.request_repaint();
                                }) {
                                    Ok(()) => {
                                        (true, "firmware updated — buddy is rebooting".to_string())
                                    }
                                    Err(e) => (
                                        false,
                                        format!(
                                            "{e}\nIf this keeps failing, allow “Agent Buddy” \
                                             under System Settings ▸ Privacy & Security ▸ \
                                             Local Network, then try again."
                                        ),
                                    ),
                                }
                            }
                            Err(e) => (false, format!("couldn’t read bundled firmware: {e}")),
                        },
                        None => (
                            false,
                            "no firmware bundled with this app to install".to_string(),
                        ),
                    };
                    let _ = tx.send(Msg::Action(action.0, action.1));
                    let _ = tx.send(Msg::Status(client::status().map_err(|e| e.to_string())));
                }
                Ok(cmd) => {
                    if let Some(action) = handle(cmd) {
                        let _ = tx.send(Msg::Action(action.0, action.1));
                    }
                    // Always follow an action with a fresh snapshot.
                    let _ = tx.send(Msg::Status(client::status().map_err(|e| e.to_string())));
                }
                Err(RecvTimeoutError::Timeout) => {
                    let _ = tx.send(Msg::Status(client::status().map_err(|e| e.to_string())));
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
            ctx.request_repaint();
        }
    });
}

/// Execute one command. Returns an action outcome to report, or `None` for a
/// bare refresh.
fn handle(cmd: Cmd) -> Option<(bool, String)> {
    match cmd {
        Cmd::Refresh => None,
        // Handled directly in the worker loop (streams progress); never reaches here.
        Cmd::UpdateFirmware { .. } => None,
        Cmd::Provision { ssid, pass } => Some(match client::provision_wifi(&ssid, &pass) {
            // The daemon resolves Ok only after the device confirms it *stored*
            // the credentials, so this is "saved to the buddy", not merely sent.
            // Whether it actually joined shows up on the status card's "On Wi-Fi"
            // line once the device announces a network.
            Ok(()) => (true, format!("saved Wi-Fi “{ssid}” to the buddy")),
            Err(e) => (false, e.to_string()),
        }),
        Cmd::InstallStart => Some(
            match setup::daemon_exe_path().and_then(|exe| setup::service_install_and_start(&exe)) {
                // Daemon is the must-have; also register this GUI as a clickable
                // desktop app so it can be reopened by hand. Best-effort — a failure
                // here doesn't undo the daemon install.
                Ok(note) => match setup::register_desktop_app() {
                    Ok(app_note) => (true, format!("{note}; {app_note}")),
                    Err(_) => (true, note),
                },
                Err(e) => (false, e.to_string()),
            },
        ),
        Cmd::Start => Some(match setup::service_start() {
            Ok(note) => (true, note),
            Err(e) => (false, e.to_string()),
        }),
        Cmd::Restart => Some(match setup::service_restart() {
            Ok(note) => (true, note),
            Err(e) => (false, e.to_string()),
        }),
        Cmd::Stop => Some(match setup::service_stop() {
            Ok(note) => (true, note),
            Err(e) => (false, e.to_string()),
        }),
    }
}

/// Turn the daemon's connect diagnostics into one actionable next step for a
/// not-connected buddy. Order matters: a missing/denied radio trumps everything
/// (nothing else can work), then the daemon's classified last error, then the
/// generic "power it on" fallback.
fn disconnected_hint(s: &StatusReport) -> String {
    if !s.bluetooth_available {
        return "Turn on Bluetooth on this Mac, then wait a moment.".into();
    }
    if s.bluetooth_permitted == Some(false) {
        return "Allow Bluetooth for Agent Buddy in System Settings → Privacy & Security → Bluetooth."
            .into();
    }
    if let Some(err) = &s.last_connect_error {
        let low = err.to_lowercase();
        if low.contains("pair") || low.contains("auth") || low.contains("encrypt") {
            return "Confirm the 6-digit code shown on your buddy to finish pairing.".into();
        }
        if low.contains("permission") || low.contains("denied") || low.contains("unauthorized") {
            return "Allow Bluetooth for Agent Buddy in System Settings → Privacy & Security → Bluetooth."
                .into();
        }
        return format!("Last try: {err}. Power on your buddy and keep it nearby.");
    }
    "Power on your buddy and keep it nearby — it’ll link automatically.".into()
}

/// Open a URL in the user's default browser (the "Download" action on the
/// update banner). Best-effort per OS.
fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    // The empty "" is `start`'s window-title arg, so a URL with spaces isn't
    // mistaken for the title.
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let _ = url;
}

/// Best-effort deep link to the OS Bluetooth privacy pane so the user can grant
/// or check permission without hunting through System Settings.
fn open_bluetooth_settings() {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Bluetooth")
        .spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "ms-settings:bluetooth"])
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open")
        .arg("settings://bluetooth")
        .spawn();
}

// --- widgets --------------------------------------------------------------
fn install_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::light();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = BG;
    style.visuals.override_text_color = Some(INK);

    // Rounded, paper-white controls that warm on hover instead of the default
    // cold gray. Text fields read as inset wells against the card.
    for w in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        w.rounding = egui::Rounding::same(9.0);
    }
    style.visuals.widgets.inactive.bg_fill = CARD;
    style.visuals.widgets.inactive.weak_bg_fill = CARD;
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, HAIR);
    style.visuals.widgets.hovered.bg_fill = CARD;
    style.visuals.widgets.hovered.weak_bg_fill = CARD;
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    style.visuals.extreme_bg_color = egui::Color32::from_rgb(0xF7, 0xF5, 0xEE);

    style.visuals.selection.bg_fill = ACCENT.linear_multiply(0.22);
    style.visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);

    // A touch more line-height than egui's default makes the dense status block
    // breathe; a hair of letter-tracking on the body keeps it crisp.
    style.spacing.button_padding = egui::vec2(14.0, 9.0);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.interact_size.y = 30.0;

    ctx.set_style(style);
}

/// A white rounded panel with padding — the building block of every section.
fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(CARD)
        .rounding(egui::Rounding::same(14.0))
        .stroke(egui::Stroke::new(1.0, HAIR))
        .inner_margin(egui::Margin::same(16.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn hairline(ui: &mut egui::Ui) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, HAIR);
}

fn status_row(ui: &mut egui::Ui, label: &str, ok: bool, value: &str) {
    let color = if ok { GOOD } else { MUTED };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("●").color(color).size(11.0));
        ui.add_space(1.0);
        ui.label(egui::RichText::new(label).color(INK).size(14.5).strong());
        // The state itself reads as a soft tinted pill, right-aligned — the
        // single most-glanced-at thing in the window.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            pill(ui, value, color);
        });
    });
}

/// A small rounded badge: tinted fill, saturated text. Used for the live
/// daemon/buddy state so it pops without shouting.
fn pill(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::none()
        .fill(mix(color, CARD, 0.86))
        .rounding(egui::Rounding::same(7.0))
        .inner_margin(egui::Margin::symmetric(9.0, 3.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).color(color).size(11.5).strong());
        });
}

/// Linear blend from `a` toward `b` by `t` (0 = all `a`, 1 = all `b`).
fn mix(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let f = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8;
    egui::Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

fn metric(ui: &mut egui::Ui, label: &str, value: &str) {
    metric_colored(ui, label, value, INK);
}

fn metric_colored(ui: &mut egui::Ui, label: &str, value: &str, color: egui::Color32) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).color(MUTED).size(12.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(value).color(color).size(12.5).strong());
        });
    });
}

fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(MUTED).size(11.0).strong());
    ui.add_space(2.0);
}

fn primary_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let resp = ui.add_enabled(
        enabled,
        egui::Button::new(
            egui::RichText::new(text)
                .color(egui::Color32::WHITE)
                .size(14.0),
        )
        .fill(ACCENT)
        .min_size(egui::vec2(ui.available_width(), 38.0)),
    );
    // egui won't recolor an explicit `.fill`, so paint the hover/press state on
    // top for tactile feedback: darker clay when pressed, a hair darker on hover.
    if enabled {
        let over = if resp.is_pointer_button_down_on() {
            Some(ACCENT_HOVER)
        } else if resp.hovered() {
            Some(mix(ACCENT, ACCENT_HOVER, 0.5))
        } else {
            None
        };
        if let Some(fill) = over {
            ui.painter()
                .rect_filled(resp.rect, egui::Rounding::same(9.0), fill);
            ui.painter().text(
                resp.rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(14.0),
                egui::Color32::WHITE,
            );
        }
    }
    resp
}

fn ghost_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(text).color(INK).size(13.0))
            .fill(CARD)
            .stroke(egui::Stroke::new(1.0, HAIR))
            .min_size(egui::vec2(0.0, 32.0)),
    )
}

/// Human sessions line, plus whether anything is actually live. Returns false
/// for the all-zero case so the caller can mute it — a bold "0 active · 0
/// running · 0 waiting" reads as broken rather than calm.
fn fmt_sessions(s: &StatusReport) -> (String, bool) {
    if s.sessions_total == 0 {
        return ("none active".to_string(), false);
    }
    let mut parts = vec![format!("{} active", s.sessions_total)];
    if s.sessions_running > 0 {
        parts.push(format!("{} running", s.sessions_running));
    }
    if s.sessions_waiting > 0 {
        parts.push(format!("{} waiting", s.sessions_waiting));
    }
    (parts.join(" · "), true)
}

/// 12345 → "12,345"; big numbers → "1.2k" style stays readable.
fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}
