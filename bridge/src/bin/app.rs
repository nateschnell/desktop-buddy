//! agent-buddy-app — the desktop control panel.
//!
//! A native window (eframe/egui) that drives the always-on gateway: it
//! installs/starts the background service, shows live gateway + buddy status,
//! and provisions the buddy's Wi-Fi. It is a *thin client* — it never opens the
//! Bluetooth radio itself; every device action is relayed through the gateway
//! over the local IPC socket (see `client.rs`). That's why the gateway can keep
//! the buddy linked even while this window is closed.
//!
//! The UI is a two-pane app shell: a fixed left nav rail and a wide content
//! pane (Overview / Wi-Fi / Gateway / Settings). It is harness-agnostic —
//! neutral surfaces + a single teal accent — so it doesn't read as any one
//! vendor's product. Light is the warm default; a follow-the-OS dark theme is
//! one toggle away and is remembered across launches.
//!
//! All IPC and service-control work happens on a background worker thread so a
//! slow round-trip never freezes the UI.

#![cfg_attr(windows, windows_subsystem = "windows")] // no console window on Windows

use agent_buddy::ipc::StatusReport;
use agent_buddy::{client, ota, selfupdate, setup, state, update};
use eframe::egui;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

// --- icons ----------------------------------------------------------------
// Icons come from a bundled Lucide font (ISC license) installed as a dedicated
// font family — egui's stock fonts don't carry these glyphs, so without it
// every icon renders as a missing-glyph box. The codepoints are Lucide's
// private-use-area assignments (see `assets/lucide.ttf`).
mod ic {
    pub const OVERVIEW: &str = "\u{E1C1}"; // layout-dashboard
    pub const WIFI: &str = "\u{E1AE}";
    pub const GATEWAY: &str = "\u{E417}"; // arrow-right-left (relay)
    pub const SETTINGS: &str = "\u{E154}";
    pub const SUN: &str = "\u{E178}";
    pub const MOON: &str = "\u{E11E}";
    pub const CHECK: &str = "\u{E06C}";
    pub const CROSS: &str = "\u{E1B2}"; // x
    pub const STAR: &str = "\u{E412}"; // sparkles — the brand mark
}

/// License notices for the fonts bundled *into* this binary: the Lucide icon
/// font (`assets/lucide.ttf`, ISC + the Feather MIT subset) and IBM Plex Sans
/// (`assets/IBMPlexSans-*.ttf`, SIL OFL 1.1). The font bytes are `include_bytes!`'d
/// below, so the notices ship compiled into every copy of the GUI (and are
/// surfaced in Settings → About). The packaging scripts also drop the same text
/// beside the binary as `THIRD_PARTY_LICENSES` for good measure.
const THIRD_PARTY_LICENSES: &str = concat!(
    "=== Lucide icon font (assets/lucide.ttf) ===\n\n",
    include_str!("../../assets/LICENSE"),
    "\n\n=== IBM Plex Sans (assets/IBMPlexSans-*.ttf) ===\n\n",
    include_str!("../../assets/IBMPlexSans-LICENSE.txt"),
);

/// A `FontId` for the bundled Lucide icon glyphs at the given size.
fn icon_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("icons".into()))
}

/// The family carrying the bundled SemiBold weight. egui has no synthetic bold —
/// `RichText::strong()` only swaps in a color, which we already override — so the
/// only way to get real weight contrast is to route emphasized text through its
/// own bold font family. Headings/labels/values use `.family(bold())`.
fn bold() -> egui::FontFamily {
    egui::FontFamily::Name("bold".into())
}

/// Register the bundled brand + icon fonts. The proportional default becomes IBM
/// Plex Sans so the UI stops reading as stock egui; a dedicated `"bold"` family
/// carries Plex SemiBold for real weight hierarchy; Lucide is its own `"icons"`
/// family. Called once at startup; without it the icon codepoints have no glyph
/// and paint as boxes, and text falls back to egui's default face. Each custom
/// family keeps egui's bundled fonts as a fallback tail so any glyph Plex/Lucide
/// lack (arrows, emoji) still resolves instead of tofu-ing.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "plex".to_owned(),
        egui::FontData::from_static(include_bytes!("../../assets/IBMPlexSans-Regular.ttf")),
    );
    fonts.font_data.insert(
        "plex-sb".to_owned(),
        egui::FontData::from_static(include_bytes!("../../assets/IBMPlexSans-SemiBold.ttf")),
    );
    fonts.font_data.insert(
        "lucide".to_owned(),
        egui::FontData::from_static(include_bytes!("../../assets/lucide.ttf")),
    );
    fonts.families.insert(
        egui::FontFamily::Name("icons".into()),
        vec!["lucide".to_owned()],
    );
    // Proportional: Plex first (the brand face), egui's defaults as a glyph
    // fallback, plus Lucide so a stray inline icon glyph still resolves.
    {
        let prop = fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default();
        prop.insert(0, "plex".to_owned());
        prop.push("lucide".to_owned());
    }
    // Bold: Plex SemiBold first, sharing the same fallback tail (degrades to Plex
    // regular, then egui's faces, for anything SemiBold doesn't cover).
    let mut bold_fam = fonts.families[&egui::FontFamily::Proportional].clone();
    bold_fam.insert(0, "plex-sb".to_owned());
    fonts.families.insert(bold(), bold_fam);
    ctx.set_fonts(fonts);
}

// --- platform-specific copy -----------------------------------------------
// User-facing instructions that name an OS settings pane must match the OS the
// build runs on — otherwise Windows/Linux users are handed Mac directions.
#[cfg(target_os = "macos")]
const THIS_COMPUTER: &str = "Mac";
#[cfg(not(target_os = "macos"))]
const THIS_COMPUTER: &str = "computer";

#[cfg(target_os = "macos")]
const BT_SETTINGS_PATH: &str = "System Settings > Privacy & Security > Bluetooth";
#[cfg(target_os = "windows")]
const BT_SETTINGS_PATH: &str = "Settings > Bluetooth & devices";
#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
const BT_SETTINGS_PATH: &str = "your system’s Bluetooth settings";

// Shown after a failed OTA flash. On macOS the Local Network permission is the
// usual culprit; elsewhere it's almost always a Wi-Fi mismatch.
#[cfg(target_os = "macos")]
const OTA_FAIL_HINT: &str = "If this keeps failing, allow “Agent Buddy” under System Settings > Privacy & Security > Local Network, then try again.";
#[cfg(not(target_os = "macos"))]
const OTA_FAIL_HINT: &str =
    "If this keeps failing, make sure this computer and your buddy are on the same Wi-Fi, then try again.";

// --- palette --------------------------------------------------------------
// Harness-agnostic: neutral surfaces, a single confident teal accent, and a
// hardware-dashboard cool cast. Two full palettes — light (default) and dark —
// resolve at runtime so the same draw code paints either theme. Everything keys
// off a `Pal`; nothing reaches for a global color.
#[derive(Clone, Copy)]
struct Pal {
    bg: egui::Color32,           // content-pane background
    surface: egui::Color32,      // sidebar background
    card: egui::Color32,         // card / control fill
    ink: egui::Color32,          // primary text
    muted: egui::Color32,        // secondary text
    faint: egui::Color32,        // tertiary / disabled text
    accent: egui::Color32,       // brand teal: primary actions, active nav, dots
    accent_hover: egui::Color32, // pressed/hover accent
    on_accent: egui::Color32,    // text/glyph on an accent fill
    good: egui::Color32,         // healthy / online
    bad: egui::Color32,          // error / offline
    hair: egui::Color32,         // hairlines, borders, separators
    field: egui::Color32,        // inset text-field well
    nav_active: egui::Color32,   // active nav-row tint
    shadow: egui::Color32,       // card drop-shadow tint (pre-blurred alpha)
}

impl Pal {
    fn light() -> Self {
        Pal {
            bg: egui::Color32::from_rgb(0xF4, 0xF6, 0xF7),
            surface: egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
            card: egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
            ink: egui::Color32::from_rgb(0x14, 0x18, 0x1A),
            muted: egui::Color32::from_rgb(0x5B, 0x67, 0x70),
            faint: egui::Color32::from_rgb(0x9A, 0xA5, 0xAB),
            accent: egui::Color32::from_rgb(0x0D, 0x94, 0x88), // teal-600
            accent_hover: egui::Color32::from_rgb(0x0F, 0x76, 0x6E), // teal-700
            on_accent: egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
            good: egui::Color32::from_rgb(0x16, 0xA3, 0x4A),
            bad: egui::Color32::from_rgb(0xDC, 0x26, 0x26),
            hair: egui::Color32::from_rgb(0xE3, 0xE8, 0xEA),
            field: egui::Color32::from_rgb(0xF1, 0xF4, 0xF5),
            nav_active: egui::Color32::from_rgb(0xE0, 0xF2, 0xF1),
            // A soft, cool drop shadow lifts cards off the page. Kept low-alpha so
            // it reads as depth, not a border.
            shadow: egui::Color32::from_black_alpha(20),
        }
    }

    fn dark() -> Self {
        Pal {
            bg: egui::Color32::from_rgb(0x0D, 0x12, 0x13),
            surface: egui::Color32::from_rgb(0x11, 0x18, 0x1A),
            card: egui::Color32::from_rgb(0x16, 0x1D, 0x1E),
            ink: egui::Color32::from_rgb(0xE6, 0xED, 0xED),
            muted: egui::Color32::from_rgb(0x93, 0xA1, 0xA1),
            faint: egui::Color32::from_rgb(0x5E, 0x6B, 0x6B),
            accent: egui::Color32::from_rgb(0x2D, 0xD4, 0xBF), // bright teal for dark
            accent_hover: egui::Color32::from_rgb(0x5E, 0xEA, 0xD4),
            on_accent: egui::Color32::from_rgb(0x05, 0x24, 0x21), // dark ink on bright teal
            good: egui::Color32::from_rgb(0x34, 0xD3, 0x99),
            bad: egui::Color32::from_rgb(0xF8, 0x71, 0x71),
            hair: egui::Color32::from_rgb(0x23, 0x2C, 0x2E),
            field: egui::Color32::from_rgb(0x10, 0x16, 0x17),
            nav_active: egui::Color32::from_rgb(0x10, 0x2A, 0x28),
            // Darker, slightly stronger so the lift still registers against the
            // near-black background.
            shadow: egui::Color32::from_black_alpha(90),
        }
    }
}

/// Which theme the user picked. `System` follows the OS; the two explicit modes
/// are the manual override, remembered across launches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemePref {
    System,
    Light,
    Dark,
}

impl ThemePref {
    fn as_str(self) -> &'static str {
        match self {
            ThemePref::System => "system",
            ThemePref::Light => "light",
            ThemePref::Dark => "dark",
        }
    }
    fn parse(s: &str) -> ThemePref {
        match s.trim() {
            "light" => ThemePref::Light,
            "dark" => ThemePref::Dark,
            _ => ThemePref::System,
        }
    }
}

/// Which content page is showing in the right pane.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Wifi,
    Gateway,
    Settings,
}

/// A request from the UI to the worker thread.
enum Cmd {
    Refresh,
    /// Nudge the daemon to re-check GitHub for the newest release right now,
    /// rather than waiting out its 6h poll. Fired at launch and whenever the
    /// window regains focus, so the "update available" / firmware banners always
    /// reflect the latest within seconds of the user looking — produces no
    /// `Action`, the result lands on the next routine status poll.
    RecheckUpdates,
    Provision {
        ssid: String,
        pass: String,
    },
    InstallStart,
    Start,
    /// Restart the running gateway in place (launchctl kickstart), preserving the
    /// always-on KeepAlive contract — never an unload/load that would tear the
    /// link down and race the respawn.
    Restart,
    Stop,
    /// Flash firmware to the buddy over the air (one-click update). Carries the
    /// connected device's board id (selects the image + OTA slot) and the source:
    /// `url: Some` downloads that newer image from the GitHub release first;
    /// `url: None` flashes the image bundled with this app.
    UpdateFirmware {
        board: String,
        url: Option<String>,
        sha256_url: Option<String>,
    },
    /// Switch the active agent harness. The daemon re-wires hooks + re-themes the
    /// device.
    SetAgent { id: String },
    /// Update *this app* in place: download the newer signed installer from the
    /// release (`url` = the platform package's direct download), verify it, swap
    /// the bundle, and relaunch. The worker exits the process on success — the
    /// detached helper does the swap once we're gone. macOS only for now.
    SelfUpdate { url: String },
    /// Run once at launch: if this app ships a newer gateway/daemon than the one
    /// installed, re-stage it and restart the service (which then reconciles its
    /// own hooks). Keeps the background daemon in lock-step with an in-place app
    /// update. Silent when nothing needs doing.
    Maintain,
    /// Remove everything Agent Buddy installed (hooks, daemon, service, login
    /// item, launcher, state) — the user-triggered counterpart to install.
    Uninstall,
}

/// A result from the worker thread back to the UI.
enum Msg {
    Status(Result<StatusReport, String>),
    /// Outcome of a user-triggered action (ok?, message). Clears `busy`.
    Action(bool, String),
    /// OTA transfer progress, 0..=100. Drives the update progress bar.
    OtaProgress(u8),
    /// A human label for the current firmware-update phase ("Downloading…",
    /// "Sending to your buddy…", "Verifying & rebooting…"). Shown above the bar so
    /// phases the percentage can't measure (download, verify) still reassure.
    OtaPhase(String),
    /// A short progress label for an in-place app self-update ("Downloading
    /// update…", "Verifying signature…", …). Drives the install overlay.
    UpdateStage(String),
}

fn main() -> eframe::Result<()> {
    // Single-instance guard. Two windows would each paint a menu-bar/tray icon
    // (the "two dots" bug) and both poll the gateway. Hold an advisory flock for
    // the process lifetime; if another instance already holds it, exit cleanly so
    // the login item's KeepAlive won't respawn us. `_lock` must stay in scope for
    // the whole run — the lock releases when it (or the process) goes away,
    // leaving nothing stale to clean up after a crash.
    let _lock = match acquire_app_lock() {
        Ok(lock) => lock,
        Err(_) => {
            eprintln!("Agent Buddy is already running.");
            return Ok(());
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 620.0])
            .with_min_inner_size([780.0, 500.0])
            .with_title("Agent Buddy"),
        // Open at the coded size every launch, centered. eframe otherwise
        // persists the last window geometry to storage — which is why a changed
        // default looks like it "didn't take": the remembered size wins. A
        // control panel is better off with one consistent, known-good size (and
        // it can't get stranded off-screen by a stale saved position).
        persist_window: false,
        // Report the OS light/dark choice each frame (so "System" theme can
        // follow it); we still paint our own palette, so this only feeds the
        // resolver — eframe's own visuals are overridden in `apply_style`.
        follow_system_theme: true,
        default_theme: eframe::Theme::Light,
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

/// Read the remembered theme preference (defaults to follow-the-OS).
fn load_theme_pref() -> ThemePref {
    state::config_dir()
        .ok()
        .and_then(|d| std::fs::read_to_string(d.join("theme")).ok())
        .map(|s| ThemePref::parse(&s))
        .unwrap_or(ThemePref::System)
}

/// Persist the theme preference so the manual override survives a relaunch.
fn save_theme_pref(t: ThemePref) {
    if let Ok(d) = state::config_dir() {
        let _ = std::fs::write(d.join("theme"), t.as_str());
    }
}

/// Whether the floating desktop buddy was left enabled (defaults off — opt-in).
fn load_widget_enabled() -> bool {
    state::config_dir()
        .ok()
        .map(|d| d.join("widget_enabled").exists())
        .unwrap_or(false)
}

/// Persist the desktop-buddy on/off choice (presence of the marker file = on).
fn save_widget_enabled(on: bool) {
    if let Ok(d) = state::config_dir() {
        let path = d.join("widget_enabled");
        if on {
            let _ = std::fs::write(path, "1");
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Path to the `agent-buddy-widget` binary, preferring the copy beside this one
/// (so a bundle finds its sibling), falling back to a bare name on `PATH`.
fn widget_exe_path() -> std::path::PathBuf {
    let name = if cfg!(windows) {
        "agent-buddy-widget.exe"
    } else {
        "agent-buddy-widget"
    };
    if let Ok(here) = std::env::current_exe() {
        if let Some(dir) = here.parent() {
            let candidate = dir.join(name);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    std::path::PathBuf::from(name)
}

/// Spawn the desktop-buddy widget process. `BUDDY_WIDGET_MANAGED` tells it to
/// self-exit if this app dies, so it can't orphan on screen. Best-effort.
fn spawn_widget() -> Option<std::process::Child> {
    std::process::Command::new(widget_exe_path())
        .env("BUDDY_WIDGET_MANAGED", "1")
        .spawn()
        .ok()
}

struct App {
    tx: Sender<Cmd>,
    rx: Receiver<Msg>,
    status: Option<StatusReport>,
    status_err: Option<String>,
    last_action: Option<(bool, String)>,
    /// When the current `last_action` arrived — used to auto-dismiss the feedback
    /// line after a few seconds so a stale result doesn't linger.
    last_action_at: Option<Instant>,
    busy: bool,
    /// Live OTA transfer percentage while an update is in flight (`None` = idle,
    /// or a non-measurable phase like download/verify that shows a spinner).
    ota_progress: Option<u8>,
    /// Current firmware-update phase label (`None` = no update running).
    ota_phase: Option<String>,
    /// Current stage label while an in-place app self-update is running (`None` =
    /// not updating). When set, the content pane shows a full install overlay.
    update_stage: Option<String>,
    /// Which content page the nav rail has selected.
    page: Page,
    /// Light/dark/system preference, remembered across launches.
    theme: ThemePref,
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
    /// True while the "really uninstall?" confirmation is showing (Settings).
    pending_uninstall: bool,
    /// Whether the window held OS focus on the previous frame. A `false → true`
    /// transition (the user returning to the app) triggers an update recheck.
    was_focused: bool,
    /// When we last asked the daemon to re-poll for updates — throttles the
    /// focus-driven recheck so rapid focus flapping can't hammer GitHub.
    last_update_recheck: Option<Instant>,
    /// The connection state the tray icon currently shows — so we only redraw the
    /// menu-bar glyph when it actually changes, not every frame. Unused on Linux,
    /// which has no tray.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    tray_state: TrayState,
    /// The floating desktop-buddy widget child process, while it's showing. The
    /// widget is a separate process (a transparent main viewport — egui can't
    /// make a child viewport transparent), spawned/killed from here.
    widget_proc: Option<std::process::Child>,
    /// Whether the desktop buddy should be showing, remembered across launches.
    widget_enabled: bool,
}

/// What a tray menu item does when clicked.
enum TrayAction {
    Open,
    Start,
    Stop,
    ToggleWidget,
    Uninstall,
    Quit,
}

/// Connection state the tray icon reflects, so the menu-bar glyph shows status
/// at a glance (filled = linked, ring = running-but-unlinked, faint = off).
/// Unused on Linux, which has no tray.
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(target_os = "linux", allow(dead_code))]
enum TrayState {
    Off,
    Idle,
    Linked,
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
impl TrayState {
    fn from_status(st: Option<&StatusReport>) -> Self {
        match st {
            None => TrayState::Off,
            Some(s) if s.device_connected => TrayState::Linked,
            Some(_) => TrayState::Idle,
        }
    }
    fn tooltip(self) -> &'static str {
        match self {
            TrayState::Off => "Agent Buddy — gateway off",
            TrayState::Idle => "Agent Buddy — buddy not linked",
            TrayState::Linked => "Agent Buddy — buddy linked",
        }
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Install the bundled icon font before anything paints, or the first
        // frames render every icon as a missing-glyph box.
        install_fonts(&cc.egui_ctx);
        let (tx, rx_cmd) = std::sync::mpsc::channel::<Cmd>();
        let (tx_msg, rx) = std::sync::mpsc::channel::<Msg>();
        spawn_worker(cc.egui_ctx.clone(), rx_cmd, tx_msg);
        let _ = tx.send(Cmd::Refresh); // fetch immediately, don't wait for the tick
        // Ask the daemon to re-poll GitHub now so the update banners reflect the
        // newest release at launch, not its last (≤6h-old) cached check.
        let _ = tx.send(Cmd::RecheckUpdates);
        // Keep the installed gateway in lock-step with this app: if we ship a
        // newer one, re-stage + restart it. Silent unless it actually updates.
        let _ = tx.send(Cmd::Maintain);

        let (tray, tray_rx) = init_tray(&cc.egui_ctx);

        // Bring up the floating desktop buddy if it was left enabled.
        let widget_enabled = load_widget_enabled();
        let widget_proc = if widget_enabled { spawn_widget() } else { None };

        let detected_ssid = client::current_ssid();
        App {
            tx,
            rx,
            status: None,
            status_err: None,
            last_action: None,
            last_action_at: None,
            busy: false,
            ota_progress: None,
            ota_phase: None,
            update_stage: None,
            page: Page::Overview,
            theme: load_theme_pref(),
            ssid_autofilled: detected_ssid.is_some(),
            ssid: detected_ssid.unwrap_or_default(),
            pass: String::new(),
            show_pass: false,
            tray,
            tray_rx,
            pending_uninstall: false,
            was_focused: true,
            last_update_recheck: Some(Instant::now()),
            tray_state: TrayState::Off,
            widget_proc,
            widget_enabled,
        }
    }

    /// Show or hide the floating desktop buddy, remembering the choice. Spawning
    /// is best-effort; the widget single-instances itself, so a redundant spawn
    /// is harmless.
    fn set_widget_enabled(&mut self, on: bool) {
        if on == self.widget_enabled && (!on || self.widget_proc.is_some()) {
            return;
        }
        self.widget_enabled = on;
        save_widget_enabled(on);
        if on {
            if self.widget_proc.is_none() {
                self.widget_proc = spawn_widget();
            }
        } else {
            self.kill_widget();
        }
    }

    /// Stop the buddy widget process if we started one.
    fn kill_widget(&mut self) {
        if let Some(mut child) = self.widget_proc.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Redraw the menu-bar icon when the connection state changes, so it reflects
    /// linked / running / off at a glance. No-op where there's no tray.
    #[cfg(not(target_os = "linux"))]
    fn refresh_tray_icon(&mut self, st: Option<&StatusReport>) {
        let next = TrayState::from_status(st);
        if next != self.tray_state {
            self.tray_state = next;
            if let Some(tray) = &self.tray {
                let _ = tray.set_icon(Some(tray_icon_image(next)));
                let _ = tray.set_tooltip(Some(next.tooltip()));
            }
        }
    }
    #[cfg(target_os = "linux")]
    fn refresh_tray_icon(&mut self, _st: Option<&StatusReport>) {}

    fn send(&mut self, cmd: Cmd) {
        self.busy = true;
        self.last_action = None;
        self.last_action_at = None;
        // Drop any prior error so a stale message can't sit next to a fresh
        // action's spinner.
        self.status_err = None;
        let _ = self.tx.send(cmd);
    }

    /// A status poll that does *not* mark the UI busy. Used for the manual
    /// "Retry": `Cmd::Refresh` produces no `Msg::Action`, so routing it through
    /// `send()` (which sets `busy`) would strand the "Working…" spinner on
    /// forever. Every command that goes through `send()` produces an `Action`
    /// that clears `busy`; bare refreshes must not.
    fn refresh(&self) {
        let _ = self.tx.send(Cmd::Refresh);
    }

    /// Ask the daemon to re-poll GitHub for updates, but at most once per 30s so
    /// repeated focus changes don't hammer the network. Like `refresh`, this
    /// stays off the `busy` path — it's a background nudge, not a user action.
    fn recheck_updates_throttled(&mut self) {
        let due = self
            .last_update_recheck
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(30));
        if due {
            self.last_update_recheck = Some(Instant::now());
            let _ = self.tx.send(Cmd::RecheckUpdates);
        }
    }

    /// Switch pages, clearing any transient feedback so a result from one page
    /// doesn't bleed onto the next.
    fn set_page(&mut self, page: Page) {
        if self.page != page {
            self.last_action = None;
            self.last_action_at = None;
            self.status_err = None;
        }
        self.page = page;
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
                    self.ota_phase = None;
                    // A failed self-update reports here; clear the overlay so the
                    // panel comes back. (A *successful* one never reaches this —
                    // the worker exits the process to hand off to the swap helper.)
                    self.update_stage = None;
                    self.last_action = Some((ok, text));
                    self.last_action_at = Some(Instant::now());
                }
                Msg::OtaProgress(pct) => {
                    self.ota_progress = Some(pct);
                }
                Msg::OtaPhase(s) => {
                    self.ota_phase = Some(s);
                }
                Msg::UpdateStage(s) => {
                    self.update_stage = Some(s);
                }
            }
        }
    }

    /// Resolve whether the effective theme is dark, given the OS's current
    /// choice (the cue for the "System" preference).
    fn effective_dark(&self, sys_dark: bool) -> bool {
        match self.theme {
            ThemePref::Light => false,
            ThemePref::Dark => true,
            ThemePref::System => sys_dark,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.drain();
        self.handle_tray(ctx);
        // Keep the status ticking even if the user is idle.
        ctx.request_repaint_after(Duration::from_secs(2));

        // When the user comes back to the window, re-poll for updates so the
        // banners reflect the newest release rather than the daemon's last
        // cached check. Throttled, so focus flapping can't spam GitHub.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if focused && !self.was_focused {
            self.recheck_updates_throttled();
        }
        self.was_focused = focused;

        let sys_dark = frame.info().system_theme == Some(eframe::Theme::Dark);
        let dark = self.effective_dark(sys_dark);
        let p = if dark { Pal::dark() } else { Pal::light() };
        apply_style(ctx, &p, dark);

        // Work from an owned snapshot so page bodies can both read status and
        // mutate self (send commands, edit form fields) without borrow fights.
        let status = self.status.clone();
        let running = status.is_some();
        self.refresh_tray_icon(status.as_ref());

        // Without a running gateway only Overview (install prompt) and Settings
        // are meaningful — don't strand the user on an empty Wi-Fi/Gateway page.
        if !running && !matches!(self.page, Page::Settings) {
            self.page = Page::Overview;
        }

        self.sidebar(ctx, &p, dark, status.as_ref());

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(p.bg))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                self.content_header(ui, &p, status.as_ref());
                hairline(ui, &p);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::none()
                            .inner_margin(egui::Margin::symmetric(26.0, 20.0))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                // An in-flight in-place update owns the whole
                                // pane — no navigating away mid-swap.
                                if let Some(stage) = self.update_stage.clone() {
                                    self.self_update_overlay(ui, &p, &stage);
                                    return;
                                }
                                match self.page {
                                    Page::Overview => self.page_overview(ui, &p, status.as_ref()),
                                    Page::Wifi => self.page_wifi(ui, &p, status.as_ref()),
                                    Page::Gateway => self.page_gateway(ui, &p),
                                    Page::Settings => self.page_settings(ui, &p, status.as_ref()),
                                }
                            });
                    });
            });
    }
}

// --- shell: sidebar + content header --------------------------------------
impl App {
    fn sidebar(&mut self, ctx: &egui::Context, p: &Pal, dark: bool, st: Option<&StatusReport>) {
        let running = st.is_some();
        egui::SidePanel::left("nav")
            .exact_width(196.0)
            .resizable(false)
            .show_separator_line(true)
            .frame(
                egui::Frame::none()
                    .fill(p.surface)
                    .inner_margin(egui::Margin::symmetric(14.0, 18.0)),
            )
            .show(ctx, |ui| {
                // Brand lockup: a teal mark drawn in code (no asset to ship) + the
                // wordmark.
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
                    ui.painter()
                        .rect_filled(rect, egui::Rounding::same(9.0), p.accent);
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        ic::STAR,
                        icon_font(15.0),
                        p.on_accent,
                    );
                    ui.add_space(9.0);
                    ui.vertical(|ui| {
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new("Agent Buddy")
                                .color(p.ink)
                                .size(16.0)
                                .family(bold()),
                        );
                        ui.label(
                            egui::RichText::new("Control panel")
                                .color(p.muted)
                                .size(10.5),
                        );
                    });
                });

                ui.add_space(18.0);

                // Nav rows. Wi-Fi and Gateway need a running gateway; gray them
                // out until then so a first-run user heads for the install prompt.
                if nav_item(ui, p, ic::OVERVIEW, "Overview", self.page == Page::Overview, true) {
                    self.set_page(Page::Overview);
                }
                ui.add_space(2.0);
                if nav_item(ui, p, ic::WIFI, "Wi-Fi", self.page == Page::Wifi, running) {
                    self.set_page(Page::Wifi);
                }
                ui.add_space(2.0);
                if nav_item(ui, p, ic::GATEWAY, "Gateway", self.page == Page::Gateway, running) {
                    self.set_page(Page::Gateway);
                }
                ui.add_space(2.0);
                if nav_item(ui, p, ic::SETTINGS, "Settings", self.page == Page::Settings, true) {
                    self.set_page(Page::Settings);
                }

                // Footer pinned to the bottom: a quick light/dark toggle + version.
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(env!("AGENT_BUDDY_VERSION"))
                            .color(p.faint)
                            .size(10.5),
                    );
                    ui.add_space(8.0);
                    let (glyph, label) = if dark {
                        (ic::SUN, "Light mode")
                    } else {
                        (ic::MOON, "Dark mode")
                    };
                    if nav_item(ui, p, glyph, label, false, true) {
                        // Toggling flips to the opposite explicit mode and pins it.
                        self.theme = if dark { ThemePref::Light } else { ThemePref::Dark };
                        save_theme_pref(self.theme);
                    }
                });
            });
    }

    /// The strip above the content: current page title on the left, a live
    /// connection chip on the right.
    fn content_header(&self, ui: &mut egui::Ui, p: &Pal, st: Option<&StatusReport>) {
        egui::Frame::none()
            .inner_margin(egui::Margin {
                left: 26.0,
                right: 26.0,
                top: 18.0,
                bottom: 14.0,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let title = match self.page {
                        Page::Overview => "Overview",
                        Page::Wifi => "Wi-Fi",
                        Page::Gateway => "Gateway",
                        Page::Settings => "Settings",
                    };
                    ui.label(egui::RichText::new(title).color(p.ink).size(20.0).family(bold()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (text, color) = match st {
                            None => ("Gateway off", p.muted),
                            Some(s) if s.device_connected => ("Buddy linked", p.good),
                            Some(_) => ("Buddy not linked", p.muted),
                        };
                        pill(ui, p, text, color);
                    });
                });
            });
    }
}

// --- pages ----------------------------------------------------------------
impl App {
    /// "A newer Agent Buddy is out" banner. When the release carries an installer
    /// for this platform and in-place update is supported, the action is a
    /// one-click **Update & restart** (download → verify → swap → relaunch);
    /// otherwise it falls back to a guided download of the release page. Renders
    /// nothing when no update is available or nothing is actionable.
    fn update_banner(&mut self, ui: &mut egui::Ui, p: &Pal, st: Option<&StatusReport>) {
        let Some(u) = st.and_then(|s| s.update.as_ref()).filter(|u| u.available) else {
            return;
        };
        let latest = u.latest.clone();
        let current = u.current.clone();
        let page_url = u.url.clone();
        // In-place only when we both have a platform installer URL and can do the
        // swap on this OS; otherwise guided download (needs the release-page url).
        let in_place = u
            .pkg_url
            .clone()
            .filter(|_| selfupdate::supported());
        if in_place.is_none() && page_url.is_empty() {
            return; // nothing actionable
        }
        let busy = self.busy;
        let mut want_self_update: Option<String> = None;

        card(ui, p, |ui| {
            ui.label(
                egui::RichText::new("Update available")
                    .color(p.accent)
                    .size(15.0)
                    .family(bold()),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!("Agent Buddy {latest} is out — you have v{current}."))
                    .color(p.muted)
                    .size(12.5),
            );
            ui.add_space(12.0);
            if let Some(pkg_url) = in_place {
                if primary_button(ui, p, &format!("Update to {latest} & restart"), !busy).clicked() {
                    want_self_update = Some(pkg_url);
                }
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Downloads, verifies, and installs automatically, then relaunches.",
                    )
                    .color(p.muted)
                    .size(11.0),
                );
            } else {
                #[cfg(target_os = "macos")]
                {
                    ui.label(
                        egui::RichText::new(
                            "Download it, then drag it into Applications to replace this version.",
                        )
                        .color(p.muted)
                        .size(11.0),
                    );
                    ui.add_space(8.0);
                }
                if primary_button(ui, p, &format!("Download {latest}"), true).clicked() {
                    open_url(&page_url);
                }
            }
        });
        if let Some(url) = want_self_update {
            self.start_self_update(url);
        }
        ui.add_space(10.0);
    }

    /// Kick off an in-place self-update: show the overlay and hand the package
    /// URL to the worker, which downloads, verifies, swaps, and relaunches.
    fn start_self_update(&mut self, url: String) {
        self.update_stage = Some("Starting update…".to_string());
        self.send(Cmd::SelfUpdate { url });
    }

    /// Full-pane overlay shown while the app is replacing itself.
    fn self_update_overlay(&mut self, ui: &mut egui::Ui, p: &Pal, stage: &str) {
        ui.add_space(40.0);
        card(ui, p, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                ui.add(egui::Spinner::new().size(28.0).color(p.accent));
                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new("Updating Agent Buddy")
                        .color(p.ink)
                        .size(17.0)
                        .family(bold()),
                );
                ui.add_space(6.0);
                ui.label(egui::RichText::new(stage).color(p.muted).size(13.0));
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(
                        "Keep this window open — the app restarts itself when it’s done.",
                    )
                    .color(p.faint)
                    .size(11.0),
                );
                ui.add_space(12.0);
                // Abandoning the download is harmless: the swap helper only spawns
                // after a successful download+verify, so /Applications is untouched.
                if ghost_button(ui, p, "Cancel", true).clicked() {
                    self.update_stage = None;
                    self.busy = false;
                }
                ui.add_space(8.0);
            });
        });
    }

    fn page_overview(&mut self, ui: &mut egui::Ui, p: &Pal, st: Option<&StatusReport>) {
        // Lead with the app-update banner so a waiting update is the first thing
        // seen (it renders nothing when none is available).
        self.update_banner(ui, p, st);

        // A firmware update owns the page — the buddy "disconnects" over BLE
        // during the flash, so nothing else here is meaningful meanwhile. Shown
        // for any live phase, not just the transfer: the download and verify
        // phases have no percentage, so they spin under a phase label instead.
        if self.ota_progress.is_some() || self.ota_phase.is_some() {
            let pct = self.ota_progress;
            let phase = self.ota_phase.clone();
            card(ui, p, |ui| {
                ui.label(
                    egui::RichText::new("Updating firmware")
                        .color(p.ink)
                        .size(15.0)
                        .family(bold()),
                );
                ui.add_space(10.0);
                if let Some(pct) = pct {
                    ui.add(
                        egui::ProgressBar::new(pct as f32 / 100.0)
                            .desired_height(10.0)
                            .fill(p.accent)
                            .text(format!("{pct}%")),
                    );
                } else {
                    // No measurable denominator yet (downloading / verifying).
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(16.0).color(p.accent));
                    });
                }
                if let Some(phase) = phase {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(phase).color(p.muted).size(12.5));
                }
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "Keep your buddy powered and nearby — it reboots when done. If the \
                         update is interrupted, your buddy stays safe on its current version.",
                    )
                    .color(p.faint)
                    .size(11.5),
                );
            });
            return;
        }

        let Some(s) = st else {
            self.install_card(ui, p);
            return;
        };

        // Two status tiles, side by side.
        ui.columns(2, |cols| {
            stat_tile(&mut cols[0], p, "GATEWAY", "Running", p.good, true);
            let (val, color, ok) = if s.device_connected {
                ("Linked", p.good, true)
            } else {
                ("Not linked", p.muted, false)
            };
            stat_tile(&mut cols[1], p, "DEVICE", val, color, ok);
        });
        ui.add_space(10.0);

        // Details card: the buddy's vitals + the firmware-update action.
        let mut want_update: Option<(String, Option<String>, Option<String>)> = None;
        card(ui, p, |ui| {
            if !s.device_connected {
                ui.label(
                    egui::RichText::new(disconnected_hint(s))
                        .color(p.muted)
                        .size(12.0),
                );
                ui.add_space(6.0);
                hairline(ui, p);
                ui.add_space(6.0);
            }

            metric(ui, p, "Owner", &s.owner);
            metric(ui, p, "Tokens today", &fmt_count(s.tokens_today));
            match fmt_sessions(s) {
                (text, true) => metric(ui, p, "Sessions", &text),
                (text, false) => metric_colored(ui, p, "Sessions", &text, p.muted),
            }
            if let Some(fw) = &s.device_fw {
                metric(ui, p, "Firmware", fw);
            }
            if let (Some(ssid), Some(ip)) = (&s.device_ssid, &s.device_ip) {
                metric(ui, p, "On Wi-Fi", &format!("{ssid} · {ip}"));
                match s.device_online {
                    Some(true) => metric_colored(ui, p, "Internet", "Online", p.good),
                    Some(false) => {
                        metric_colored(ui, p, "Internet", "Joined, no internet", p.bad)
                    }
                    None => metric_colored(ui, p, "Internet", "Checking…", p.muted),
                }
                // OTA firmware update — needs Wi-Fi (ip known). The image can come
                // from the copy bundled with this app OR a newer one published to
                // GitHub Releases (downloaded at flash time, so a device can update
                // without the user updating the app). Pick the newest available and
                // only push the primary button when it actually beats what the buddy
                // runs; otherwise confirm it's current, or — when the buddy didn't
                // report a comparable version — allow a manual flash.
                let board = s
                    .device_board
                    .clone()
                    .unwrap_or_else(|| ota::DEFAULT_BOARD.to_string());
                // Each candidate carries (parsed_version, version_string, bin_url,
                // checksum_url). The bundled image is local (trusted, no checksum);
                // the release image carries the published .sha256 so the flash path
                // can verify the download before applying it.
                let cand = |ver: Option<String>, url: Option<String>, sha: Option<String>| {
                    ver.and_then(|v| update::parse_version(&v).map(|pv| (pv, v, url, sha)))
                };
                let bundled = cand(ota::bundled_firmware_version(&board), None, None);
                let release = cand(
                    s.firmware_latest.as_ref().map(|f| f.version.clone()),
                    s.firmware_latest.as_ref().map(|f| f.url.clone()),
                    s.firmware_latest.as_ref().and_then(|f| f.sha256_url.clone()),
                );
                let best = match (bundled, release) {
                    (Some(b), Some(r)) => Some(if r.0 > b.0 { r } else { b }),
                    (Some(b), None) => Some(b),
                    (None, Some(r)) => Some(r),
                    (None, None) => None,
                };
                if let Some((_, best_ver, best_url, best_sha)) = best {
                    let newer = s
                        .device_fw
                        .as_deref()
                        .map(|d| update::is_newer(&best_ver, d))
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
                            p,
                            &format!("Update firmware to {best_ver}"),
                            !self.busy,
                        )
                        .clicked()
                        {
                            want_update = Some((board.clone(), best_url, best_sha));
                        }
                    } else if device_known {
                        metric_colored(ui, p, "Firmware update", "up to date", p.good);
                    } else if primary_button(ui, p, "Update firmware", !self.busy).clicked() {
                        want_update = Some((board.clone(), best_url, best_sha));
                    }
                    // Prime the macOS Local Network prompt: the OTA flow's first
                    // packet trips it, so explain why before the user is surprised.
                    #[cfg(target_os = "macos")]
                    if newer || !device_known {
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(
                                "On the first update, macOS asks to allow Agent Buddy on your \
                                 local network — that’s how it reaches your buddy over Wi-Fi.",
                            )
                            .color(p.faint)
                            .size(11.0),
                        );
                    }
                }
            }
        });
        if let Some((board, url, sha256_url)) = want_update {
            self.send(Cmd::UpdateFirmware {
                board,
                url,
                sha256_url,
            });
        }

        // First-run pairing guidance while the buddy isn't linked.
        if !s.device_connected {
            ui.add_space(10.0);
            self.pairing_card(ui, p);
        }

        // Recent activity, if any.
        if !s.entries.is_empty() {
            ui.add_space(10.0);
            card(ui, p, |ui| {
                ui.label(
                    egui::RichText::new("RECENT ACTIVITY")
                        .color(p.muted)
                        .size(10.5)
                        .family(bold()),
                );
                ui.add_space(4.0);
                for e in s.entries.iter().take(6) {
                    ui.label(egui::RichText::new(format!("·  {e}")).color(p.ink).size(12.5));
                }
            });
        }

        self.action_feedback(ui, p);
    }

    /// The get-started panel shown when no gateway is running yet.
    fn install_card(&mut self, ui: &mut egui::Ui, p: &Pal) {
        card(ui, p, |ui| {
            ui.label(
                egui::RichText::new("Get started")
                    .color(p.ink)
                    .size(16.0)
                    .family(bold()),
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "The gateway isn’t running yet. Install it once and it stays on — \
                     keeping your buddy linked across reboots and even while this window \
                     is closed.",
                )
                .color(p.muted)
                .size(12.5),
            );
            ui.add_space(12.0);
            if primary_button(ui, p, "Install & start gateway", !self.busy).clicked() {
                self.send(Cmd::InstallStart);
            }
            if let Some(err) = &self.status_err {
                if !err.contains("isn’t running") {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(err).color(p.bad).size(11.0));
                }
            }
        });
        self.action_feedback(ui, p);
    }

    /// First-run pairing / onboarding guidance, shown while the buddy isn't
    /// linked. Explains the 6-digit-code confirmation flow and offers a deep
    /// link to the Bluetooth settings pane plus a manual retry.
    fn pairing_card(&mut self, ui: &mut egui::Ui, p: &Pal) {
        card(ui, p, |ui| {
            ui.label(
                egui::RichText::new("Pair your buddy")
                    .color(p.ink)
                    .size(15.0)
                    .family(bold()),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Power on your buddy and keep it nearby. The first time, it shows a \
                     6-digit code and your OS pops a Bluetooth window — confirm that the codes \
                     match to pair. After that it reconnects on its own.",
                )
                .color(p.muted)
                .size(12.5),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ghost_button(ui, p, "Open Bluetooth settings", true).clicked() {
                    open_bluetooth_settings();
                }
                if ghost_button(ui, p, "Retry", !self.busy).clicked() {
                    self.refresh();
                }
            });
        });
    }

    fn page_wifi(&mut self, ui: &mut egui::Ui, p: &Pal, st: Option<&StatusReport>) {
        let connected = st.map(|s| s.device_connected).unwrap_or(false);
        let online = st
            .map(|s| s.device_ssid.is_some() && s.device_online == Some(true))
            .unwrap_or(false);

        card(ui, p, |ui| {
            ui.label(
                egui::RichText::new("Provision Wi-Fi")
                    .color(p.ink)
                    .size(15.0)
                    .family(bold()),
            );
            ui.add_space(2.0);
            if let (true, Some(s)) = (online, st) {
                let ssid = s.device_ssid.as_deref().unwrap_or("");
                ui.label(
                    egui::RichText::new(format!(
                        "Your buddy is online on “{ssid}”. Send new credentials below to move it \
                         to a different network."
                    ))
                    .color(p.muted)
                    .size(12.5),
                );
            } else {
                ui.label(
                    egui::RichText::new(
                        "Send your network to the buddy so it can update over the air.",
                    )
                    .color(p.muted)
                    .size(12.5),
                );
            }
            ui.add_space(12.0);
            self.wifi_form(ui, p, connected);
        });
        self.action_feedback(ui, p);
    }

    /// The network/password inputs + send button.
    fn wifi_form(&mut self, ui: &mut egui::Ui, p: &Pal, connected: bool) {
        field_label(ui, p, "Network");
        text_field(ui, &mut self.ssid, "Wi-Fi name", false);
        if !self.ssid_autofilled {
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(
                    "Couldn’t read your current network automatically — type your Wi-Fi name.",
                )
                .color(p.muted)
                .size(11.0),
            );
        }
        ui.add_space(8.0);

        field_label(ui, p, "Password");
        text_field(ui, &mut self.pass, "Wi-Fi password", !self.show_pass);
        ui.add_space(3.0);
        ui.checkbox(
            &mut self.show_pass,
            egui::RichText::new("Show password").size(11.0).color(p.muted),
        );

        ui.add_space(12.0);
        let can_send = connected && !self.busy && !self.ssid.trim().is_empty();
        if primary_button(ui, p, "Send to buddy", can_send).clicked() {
            let (ssid, pass) = (self.ssid.trim().to_string(), self.pass.clone());
            self.send(Cmd::Provision { ssid, pass });
        }
        if !connected {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Wake the buddy and wait for “linked” first.")
                    .color(p.muted)
                    .size(11.0),
            );
        }
    }

    fn page_gateway(&mut self, ui: &mut egui::Ui, p: &Pal) {
        let running = self.status.is_some();
        card(ui, p, |ui| {
            status_row(ui, p, "Gateway", running, if running { "running" } else { "stopped" });
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(
                    "The gateway is the always-on background service that keeps your buddy \
                     linked and relays every device action. It survives reboots and this window \
                     closing.",
                )
                .color(p.muted)
                .size(12.5),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ghost_button(ui, p, "Restart", !self.busy).clicked() {
                    self.send(Cmd::Restart);
                }
                if ghost_button(ui, p, "Stop", !self.busy).clicked() {
                    self.send(Cmd::Stop);
                }
                let start_label = if running { "Running" } else { "Start" };
                if ghost_button(ui, p, start_label, !self.busy && !running).clicked() {
                    self.send(Cmd::Start);
                }
            });
            if running {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("Kept alive automatically.")
                        .color(p.muted)
                        .size(11.0),
                );
            }
        });
        self.action_feedback(ui, p);
    }

    fn page_settings(&mut self, ui: &mut egui::Ui, p: &Pal, st: Option<&StatusReport>) {
        // Appearance.
        card(ui, p, |ui| {
            ui.label(
                egui::RichText::new("Appearance")
                    .color(p.ink)
                    .size(15.0)
                    .family(bold()),
            );
            ui.add_space(8.0);
            field_label(ui, p, "Theme");
            if let Some(choice) = theme_segmented(ui, p, self.theme) {
                self.theme = choice;
                save_theme_pref(self.theme);
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("“System” follows your operating system’s light/dark setting.")
                    .color(p.muted)
                    .size(11.0),
            );
        });

        // Desktop buddy — the floating mascot. The control here works on every
        // platform (the tray toggle isn't available on Linux), so there's always
        // a way to turn it on/off.
        ui.add_space(10.0);
        let mut want_widget = self.widget_enabled;
        card(ui, p, |ui| {
            ui.label(
                egui::RichText::new("Desktop buddy")
                    .color(p.ink)
                    .size(15.0)
                    .family(bold()),
            );
            ui.add_space(8.0);
            ui.checkbox(&mut want_widget, "Show the floating desktop buddy");
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "A small always-on-top character that reacts to your coding sessions. Drag it anywhere; nudge it to a screen edge to tuck it away.",
                )
                .color(p.muted)
                .size(11.0),
            );
        });
        if want_widget != self.widget_enabled {
            self.set_widget_enabled(want_widget);
        }

        // Agent — which coding harness the buddy follows. Switching re-wires that
        // harness's hooks and re-themes the device. Only shown with a running
        // gateway (it's what supplies the available-agents list).
        if let Some(s) = st {
            if !s.available_agents.is_empty() {
                ui.add_space(10.0);
                let current = s.active_agent.clone();
                let agents = s.available_agents.clone();
                let current_name = agents
                    .iter()
                    .find(|a| a.id == current)
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| current.clone());
                let busy = self.busy;
                let mut chosen: Option<String> = None;
                card(ui, p, |ui| {
                    ui.label(egui::RichText::new("Agent").color(p.ink).size(15.0).family(bold()));
                    ui.add_space(8.0);
                    field_label(ui, p, "Coding agent the buddy follows");
                    ui.add_enabled_ui(!busy, |ui| {
                        egui::ComboBox::from_id_source("agent_selector")
                            .selected_text(current_name)
                            .width((ui.available_width() - 8.0).max(160.0))
                            .show_ui(ui, |ui| {
                                for a in &agents {
                                    if ui.selectable_label(a.id == current, &a.name).clicked()
                                        && a.id != current
                                    {
                                        chosen = Some(a.id.clone());
                                    }
                                }
                            });
                    });
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Switching re-wires that agent’s hooks and re-themes your buddy.",
                        )
                        .color(p.muted)
                        .size(11.0),
                    );
                });
                if let Some(id) = chosen {
                    self.send(Cmd::SetAgent { id });
                }
            }
        }

        // App update banner — one-click in-place update where supported, else a
        // guided download. (Also shown at the top of Overview.)
        ui.add_space(10.0);
        self.update_banner(ui, p, st);

        // About.
        ui.add_space(10.0);
        card(ui, p, |ui| {
            ui.label(
                egui::RichText::new("About")
                    .color(p.ink)
                    .size(15.0)
                    .family(bold()),
            );
            ui.add_space(8.0);
            metric(ui, p, "Version", env!("AGENT_BUDDY_VERSION"));
            if let Some(s) = st {
                if let Some(fw) = &s.device_fw {
                    metric(ui, p, "Buddy firmware", fw);
                }
            }
            ui.add_space(8.0);
            ui.collapsing(
                egui::RichText::new("Third-party licenses").color(p.muted).size(12.0),
                |ui| {
                    ui.label(
                        egui::RichText::new(THIRD_PARTY_LICENSES)
                            .color(p.muted)
                            .monospace()
                            .size(10.5),
                    );
                },
            );
        });

        // Uninstall — removes everything Agent Buddy installed; gated behind an
        // inline confirmation so the click is informed, not a trap.
        ui.add_space(10.0);
        card(ui, p, |ui| {
            ui.label(
                egui::RichText::new("Uninstall")
                    .color(p.ink)
                    .size(15.0)
                    .family(bold()),
            );
            ui.add_space(6.0);
            if self.pending_uninstall {
                ui.label(
                    egui::RichText::new(
                        "Removes your coding agent’s hooks, the background gateway and its \
                         service, the login item, the app launcher, and saved settings. \
                         Your buddy device and its firmware are not touched.",
                    )
                    .color(p.muted)
                    .size(12.0),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if primary_button_compact(ui, p, "Uninstall everything", !self.busy).clicked() {
                        self.pending_uninstall = false;
                        self.send(Cmd::Uninstall);
                    }
                    if ghost_button(ui, p, "Cancel", !self.busy).clicked() {
                        self.pending_uninstall = false;
                    }
                });
            } else {
                ui.label(
                    egui::RichText::new("Remove Agent Buddy and everything it installed.")
                        .color(p.muted)
                        .size(12.0),
                );
                ui.add_space(10.0);
                if ghost_button(ui, p, "Uninstall Agent Buddy…", !self.busy).clicked() {
                    self.pending_uninstall = true;
                }
            }
        });

        self.action_feedback(ui, p);
    }

    /// Transient feedback from the last action — a spinner while busy, then a
    /// check/cross result line. Lives at the foot of whichever page issued it.
    fn action_feedback(&mut self, ui: &mut egui::Ui, p: &Pal) {
        // How long a result lingers before it auto-dismisses.
        const DISMISS_AFTER: Duration = Duration::from_secs(6);
        if self.busy {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                ui.label(egui::RichText::new("Working…").color(p.muted).size(12.0));
            });
            return;
        }
        // Drop a stale result so it doesn't sit around forever.
        if self.last_action_at.is_some_and(|at| at.elapsed() >= DISMISS_AFTER) {
            self.last_action = None;
            self.last_action_at = None;
        }
        if let Some((ok, text)) = self.last_action.clone() {
            let (mark, color) = if ok {
                (ic::CHECK, p.good)
            } else {
                (ic::CROSS, p.bad)
            };
            ui.add_space(12.0);
            ui.horizontal_top(|ui| {
                ui.label(egui::RichText::new(mark).color(color).font(icon_font(13.0)));
                ui.add_space(6.0);
                ui.label(egui::RichText::new(text).color(color).size(12.0));
            });
        }
    }
}

// --- tray -----------------------------------------------------------------
impl App {
    /// Apply any pending tray-menu clicks, and turn a window-close into a
    /// hide-to-tray (so the panel tucks away while the gateway keeps running).
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
                TrayAction::ToggleWidget => {
                    let on = !self.widget_enabled;
                    self.set_widget_enabled(on);
                }
                TrayAction::Uninstall => {
                    // Surface the window on Settings with the confirmation up,
                    // rather than tearing down from a menu click without warning.
                    self.pending_uninstall = true;
                    self.page = Page::Settings;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayAction::Quit => {
                    self.kill_widget();
                    std::process::exit(0);
                }
            }
        }

        // With a tray, the [x] hides to it instead of quitting; the gateway is
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
    menu.append(&MenuItem::with_id("start", "Start gateway", true, None))?;
    menu.append(&MenuItem::with_id("stop", "Stop gateway", true, None))?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id("widget", "Show/hide desktop buddy", true, None))?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id("uninstall", "Uninstall Agent Buddy…", true, None))?;
    menu.append(&MenuItem::with_id("quit", "Quit Agent Buddy", true, None))?;

    let builder = tray_icon::TrayIconBuilder::new()
        .with_tooltip(TrayState::Off.tooltip())
        .with_menu(Box::new(menu))
        .with_icon(tray_icon_image(TrayState::Off));
    // On macOS a template image lets the system tint the glyph to match the menu
    // bar (light/dark, reduce-transparency) — so it never looks pasted-on. State
    // must read from shape, not colour, because the tint overrides our RGB.
    #[cfg(target_os = "macos")]
    let builder = builder.with_icon_as_template(true);
    let tray = builder.build()?;
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
                "widget" => TrayAction::ToggleWidget,
                "uninstall" => TrayAction::Uninstall,
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

/// A 32×32 RGBA tray glyph drawn in code (no asset to ship) whose *shape* encodes
/// connection state: Linked → filled disc, Idle → outline ring, Off → faint ring.
/// On macOS it's a template (the system tints it), so state must read from shape;
/// elsewhere it carries the brand teal (a muted grey when off), since those
/// platforms show our colours as-is. Rim and ring edges are anti-aliased.
#[cfg(not(target_os = "linux"))]
fn tray_icon_image(state: TrayState) -> tray_icon::Icon {
    const N: u32 = 32;
    let mut rgba = vec![0u8; (N * N * 4) as usize];
    let c = (N as f32 - 1.0) / 2.0;
    let r = c; // outer radius
    let ring_inner = r - 5.0; // ~5px ring thickness when not filled
    let filled = matches!(state, TrayState::Linked);
    // Template images ignore RGB (alpha is the mask); colour only matters off-mac.
    #[cfg(target_os = "macos")]
    let (cr, cg, cb) = (0u8, 0u8, 0u8);
    #[cfg(not(target_os = "macos"))]
    let (cr, cg, cb): (u8, u8, u8) = match state {
        TrayState::Off => (0x8A, 0x8F, 0x90), // muted grey
        _ => (0x0D, 0x94, 0x88),              // brand teal
    };
    let max_alpha: f32 = if matches!(state, TrayState::Off) { 0.5 } else { 1.0 };
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            let d = (dx * dx + dy * dy).sqrt();
            let outer = (r - d).clamp(0.0, 1.0); // 1 inside, 0 outside, AA at rim
            let cov = if filled {
                outer
            } else {
                // ring = inside the outer rim AND outside the inner rim.
                outer.min((d - ring_inner).clamp(0.0, 1.0))
            };
            if cov > 0.0 {
                let i = ((y * N + x) * 4) as usize;
                rgba[i] = cr;
                rgba[i + 1] = cg;
                rgba[i + 2] = cb;
                rgba[i + 3] = (cov * max_alpha * 255.0).round() as u8;
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
                Ok(Cmd::SelfUpdate { url }) => {
                    // In-place app update: download → verify → swap → relaunch.
                    // Streams stage labels for the overlay. On success the helper
                    // is staged and waiting on our PID, so we exit(0) — a clean
                    // exit keeps launchd from respawning the *old* bundle, letting
                    // the helper swap it and reopen the new one.
                    let txp = tx.clone();
                    let ctxp = ctx.clone();
                    match selfupdate::install_and_relaunch(&url, |s| {
                        let _ = txp.send(Msg::UpdateStage(s.to_string()));
                        ctxp.request_repaint();
                    }) {
                        Ok(()) => {
                            let _ = tx.send(Msg::UpdateStage("Relaunching…".to_string()));
                            ctx.request_repaint();
                            // Give the overlay a beat to paint before we vanish.
                            thread::sleep(Duration::from_millis(400));
                            std::process::exit(0);
                        }
                        Err(e) => {
                            let _ = tx.send(Msg::Action(false, format!("update failed: {e}")));
                            let _ =
                                tx.send(Msg::Status(client::status().map_err(|e| e.to_string())));
                        }
                    }
                }
                Ok(Cmd::UpdateFirmware {
                    board,
                    url,
                    sha256_url,
                }) => {
                    // Long-running with live progress, so it can't go through the
                    // one-shot `handle()`; stream OtaProgress, then the outcome.
                    // Source the image: when a URL was chosen, download the newer
                    // release image first (this is what lets a device update
                    // without the user updating the app); otherwise read the copy
                    // bundled with this app.
                    let bytes: Result<Vec<u8>, String> = match &url {
                        Some(u) => {
                            // No byte count to bar against during the fetch — show
                            // the phase and let the panel spin rather than parking
                            // a misleading fake 0%.
                            let _ =
                                tx.send(Msg::OtaPhase("Downloading the latest firmware…".into()));
                            ctx.request_repaint();
                            // Verify the download against the release's published
                            // checksum (when present) before it's ever flashed.
                            update::download_firmware_verified(u, sha256_url.as_deref())
                                .map_err(|e| format!("couldn’t download firmware: {e}"))
                        }
                        None => match ota::bundled_firmware_path(&board) {
                            Some(path) => std::fs::read(&path)
                                .map_err(|e| format!("couldn’t read bundled firmware: {e}")),
                            None => {
                                Err("no firmware bundled with this app to install".to_string())
                            }
                        },
                    };
                    let action = match bytes {
                        Ok(bytes) => {
                            let _ = tx.send(Msg::OtaPhase("Sending to your buddy…".into()));
                            let txp = tx.clone();
                            match client::update_firmware(&bytes, &board, |pct| {
                                let _ = txp.send(Msg::OtaProgress(pct));
                                // The final ack triggers an on-device MD5 verify +
                                // reboot the bar can't track — name that phase.
                                if pct >= 100 {
                                    let _ =
                                        txp.send(Msg::OtaPhase("Verifying & rebooting…".into()));
                                }
                                ctx.request_repaint();
                            }) {
                                Ok(()) => {
                                    (true, "Firmware updated — your buddy is rebooting.".to_string())
                                }
                                Err(e) => (false, format!("{e}\n{OTA_FAIL_HINT}")),
                            }
                        }
                        Err(e) => (false, e),
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
        // Background nudge: ask the daemon to re-poll GitHub now. No outcome to
        // report; the worker's follow-up status snapshot (and subsequent polls)
        // pick up the refreshed result.
        Cmd::RecheckUpdates => {
            let _ = client::recheck_updates();
            None
        }
        // Handled directly in the worker loop (streams progress); never reaches here.
        Cmd::UpdateFirmware { .. } => None,
        Cmd::SelfUpdate { .. } => None,
        Cmd::Provision { ssid, pass } => Some(match client::provision_wifi(&ssid, &pass) {
            // The gateway resolves Ok only after the device confirms it *stored*
            // the credentials, so this is "saved to the buddy", not merely sent.
            // Whether it actually joined shows up on the Overview's "On Wi-Fi"
            // line once the device announces a network.
            Ok(()) => (true, format!("saved Wi-Fi “{ssid}” to the buddy")),
            Err(e) => (false, e.to_string()),
        }),
        Cmd::InstallStart => Some(
            match setup::daemon_exe_path().and_then(|exe| setup::service_install_and_start(&exe)) {
                // Gateway is the must-have. Then wire the coding-agent hooks:
                // without them the daemon runs but never receives session/usage
                // events, so the buddy shows nothing — the CLI `setup` wires them
                // and a GUI install must match (surfaced, not best-effort).
                // Finally register this GUI as a clickable desktop app
                // (best-effort — a failure there doesn't undo the install). The
                // message stays customer-facing: success is a plain confirmation,
                // not a dump of internal paths; only a hook failure needs detail.
                Ok(_note) => match setup::wire_claude_hooks() {
                    Ok(_) => {
                        let _ = setup::register_desktop_app();
                        (true, "Gateway installed and running — your buddy is connected.".into())
                    }
                    Err(e) => {
                        let _ = setup::register_desktop_app();
                        (
                            true,
                            format!(
                                "Gateway installed and running, but it couldn’t connect your \
                                 coding agent: {e}"
                            ),
                        )
                    }
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
        Cmd::SetAgent { id } => Some(match client::set_agent(&id) {
            Ok(()) => (true, format!("switched to {id}")),
            Err(e) => (false, e.to_string()),
        }),
        // Startup maintenance: only speak up if it actually updated the gateway;
        // an up-to-date daemon (or a dev build) stays silent.
        Cmd::Maintain => match setup::refresh_daemon_if_outdated() {
            Ok(Some(note)) => Some((true, note)),
            Ok(None) => None,
            // Don't alarm at launch over a best-effort refresh; the daemon's own
            // startup reconciliation is the backstop.
            Err(_) => None,
        },
        Cmd::Uninstall => Some(match setup::uninstall() {
            Ok(summary) => (true, format!("{summary}\n  Quit Agent Buddy to finish.")),
            Err(e) => (false, e.to_string()),
        }),
    }
}

/// Turn the gateway's connect diagnostics into one actionable next step for a
/// not-connected buddy. Order matters: a missing/denied radio trumps everything
/// (nothing else can work), then the gateway's classified last error, then the
/// generic "power it on" fallback.
fn disconnected_hint(s: &StatusReport) -> String {
    if !s.bluetooth_available {
        return format!("Turn on Bluetooth on this {THIS_COMPUTER}, then wait a moment.");
    }
    if s.bluetooth_permitted == Some(false) {
        return format!("Allow Bluetooth for Agent Buddy in {BT_SETTINGS_PATH}.");
    }
    if let Some(err) = &s.last_connect_error {
        let low = err.to_lowercase();
        if low.contains("pair") || low.contains("auth") || low.contains("encrypt") {
            return "Confirm the 6-digit code shown on your buddy to finish pairing.".into();
        }
        if low.contains("permission") || low.contains("denied") || low.contains("unauthorized") {
            return format!("Allow Bluetooth for Agent Buddy in {BT_SETTINGS_PATH}.");
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
/// Push the active palette into egui's base style so built-in widgets (text
/// fields, checkboxes, scrollbars, separators) match. Called every frame; egui
/// caches, so re-applying an unchanged style is cheap.
fn apply_style(ctx: &egui::Context, p: &Pal, dark: bool) {
    let mut style = (*ctx.style()).clone();
    style.visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    style.visuals.panel_fill = p.bg;
    style.visuals.window_fill = p.card;
    style.visuals.override_text_color = Some(p.ink);
    style.visuals.extreme_bg_color = p.field; // text-field well
    style.visuals.hyperlink_color = p.accent;
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, p.hair);

    for w in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        w.rounding = egui::Rounding::same(8.0);
    }
    style.visuals.widgets.inactive.bg_fill = p.field;
    style.visuals.widgets.inactive.weak_bg_fill = p.field;
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, p.hair);
    style.visuals.widgets.hovered.bg_fill = p.field;
    style.visuals.widgets.hovered.weak_bg_fill = p.field;
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, p.accent);
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, p.accent);

    style.visuals.selection.bg_fill = p.accent.linear_multiply(0.25);
    style.visuals.selection.stroke = egui::Stroke::new(1.0, p.accent);

    style.spacing.button_padding = egui::vec2(14.0, 9.0);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.interact_size.y = 28.0;

    ctx.set_style(style);
}

/// A clickable left-nav row: optional accent bar + tint when active, a soft
/// hover, a leading glyph. Returns whether it was clicked this frame.
fn nav_item(
    ui: &mut egui::Ui,
    p: &Pal,
    icon: &str,
    label: &str,
    active: bool,
    enabled: bool,
) -> bool {
    let h = 36.0;
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), h),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let hovered = enabled && resp.hovered();
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, egui::Rounding::same(9.0), p.nav_active);
        let bar = egui::Rect::from_min_size(
            rect.left_top() + egui::vec2(0.0, 8.0),
            egui::vec2(3.0, h - 16.0),
        );
        painter.rect_filled(bar, egui::Rounding::same(2.0), p.accent);
    } else if hovered {
        painter.rect_filled(rect, egui::Rounding::same(9.0), p.hair);
    }
    let color = if !enabled {
        p.faint
    } else if active {
        p.accent
    } else {
        p.ink
    };
    // Icon and label are painted separately: the icon needs the Lucide family,
    // the label the proportional one, and a fixed label offset keeps every row's
    // text aligned regardless of icon width.
    painter.text(
        rect.left_center() + egui::vec2(15.0, 0.0),
        egui::Align2::CENTER_CENTER,
        icon,
        icon_font(15.0),
        color,
    );
    painter.text(
        rect.left_center() + egui::vec2(34.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.5),
        color,
    );
    resp.clicked()
}

/// The soft drop shadow that lifts cards and tiles off the page. A blurred,
/// slightly-inset penumbra (negative spread keeps it from haloing past the
/// rounded corners) tinted by the palette so light/dark each read right.
fn card_shadow(p: &Pal) -> egui::epaint::Shadow {
    egui::epaint::Shadow {
        offset: egui::vec2(0.0, 2.0),
        blur: 12.0,
        spread: -2.0,
        color: p.shadow,
    }
}

/// A bordered rounded panel with padding — the building block of every section.
fn card(ui: &mut egui::Ui, p: &Pal, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(p.card)
        .rounding(egui::Rounding::same(13.0))
        .stroke(egui::Stroke::new(1.0, p.hair))
        .shadow(card_shadow(p))
        .inner_margin(egui::Margin::same(16.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

/// A status tile: small uppercase title, a colored dot, and a big value.
fn stat_tile(ui: &mut egui::Ui, p: &Pal, title: &str, value: &str, color: egui::Color32, ok: bool) {
    egui::Frame::none()
        .fill(p.card)
        .rounding(egui::Rounding::same(13.0))
        .stroke(egui::Stroke::new(1.0, p.hair))
        .shadow(card_shadow(p))
        .inner_margin(egui::Margin::same(15.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new(title)
                    .color(p.muted)
                    .size(10.5)
                    .family(bold()),
            );
            ui.add_space(7.0);
            ui.horizontal(|ui| {
                dot(ui, if ok { color } else { p.faint }, 4.0);
                ui.add_space(6.0);
                ui.label(egui::RichText::new(value).color(color).size(17.0).family(bold()));
            });
        });
}

/// A small filled status dot, vertically centered on its row. Crisper than a
/// glyph at this size and needs no font glyph at all.
fn dot(ui: &mut egui::Ui, color: egui::Color32, r: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(r * 2.0, r * 2.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), r, color);
}

fn hairline(ui: &mut egui::Ui, p: &Pal) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, p.hair);
}

fn status_row(ui: &mut egui::Ui, p: &Pal, label: &str, ok: bool, value: &str) {
    let color = if ok { p.good } else { p.muted };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).color(p.ink).size(14.5).family(bold()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            pill(ui, p, value, color);
        });
    });
}

/// A small rounded badge: tinted fill, a leading status dot, and saturated text.
/// The dot makes the state legible by shape+position, not color alone — it stays
/// readable for red/green color-blind users and in grayscale.
fn pill(ui: &mut egui::Ui, p: &Pal, text: &str, color: egui::Color32) {
    egui::Frame::none()
        .fill(mix(color, p.card, 0.86))
        .rounding(egui::Rounding::same(7.0))
        .inner_margin(egui::Margin::symmetric(9.0, 4.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                dot(ui, color, 3.5);
                ui.add_space(5.0);
                ui.label(egui::RichText::new(text).color(color).size(11.5).family(bold()));
            });
        });
}

/// Linear blend from `a` toward `b` by `t` (0 = all `a`, 1 = all `b`).
fn mix(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let f = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8;
    egui::Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

fn metric(ui: &mut egui::Ui, p: &Pal, label: &str, value: &str) {
    metric_colored(ui, p, label, value, p.ink);
}

fn metric_colored(ui: &mut egui::Ui, p: &Pal, label: &str, value: &str, color: egui::Color32) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).color(p.muted).size(12.5));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(value).color(color).size(12.5).family(bold()));
        });
    });
    ui.add_space(2.0);
}

fn field_label(ui: &mut egui::Ui, p: &Pal, text: &str) {
    ui.label(egui::RichText::new(text).color(p.muted).size(11.0).family(bold()));
    ui.add_space(4.0);
}

/// A roomy full-width single-line input. The generous inner margin gives it a
/// height in the button family's rhythm rather than egui's cramped default.
/// `password` masks the text.
fn text_field(ui: &mut egui::Ui, value: &mut String, hint: &str, password: bool) -> egui::Response {
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint)
            .password(password)
            .desired_width(f32::INFINITY)
            .margin(egui::Margin::symmetric(11.0, 10.0))
            .vertical_align(egui::Align::Center),
    )
}

/// Full-width primary action button. The default CTA shape — one per card.
fn primary_button(ui: &mut egui::Ui, p: &Pal, text: &str, enabled: bool) -> egui::Response {
    primary_button_sized(ui, p, text, enabled, ui.available_width())
}

/// Intrinsic-width primary button, for use inside a `horizontal` row beside
/// another button (a full-width one would swallow the whole row).
fn primary_button_compact(ui: &mut egui::Ui, p: &Pal, text: &str, enabled: bool) -> egui::Response {
    primary_button_sized(ui, p, text, enabled, 0.0)
}

fn primary_button_sized(
    ui: &mut egui::Ui,
    p: &Pal,
    text: &str,
    enabled: bool,
    width: f32,
) -> egui::Response {
    let resp = ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(text).color(p.on_accent).size(14.0))
            .fill(p.accent)
            .rounding(egui::Rounding::same(8.0))
            .min_size(egui::vec2(width, 38.0)),
    );
    // egui won't recolor an explicit `.fill`, so paint the hover/press state on
    // top for tactile feedback: darker accent when pressed, a hair on hover.
    // (Same 8px rounding as the button beneath, so the overlay registers.)
    if enabled {
        let over = if resp.is_pointer_button_down_on() {
            Some(p.accent_hover)
        } else if resp.hovered() {
            Some(mix(p.accent, p.accent_hover, 0.5))
        } else {
            None
        };
        if let Some(fill) = over {
            ui.painter()
                .rect_filled(resp.rect, egui::Rounding::same(8.0), fill);
            ui.painter().text(
                resp.rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(14.0),
                p.on_accent,
            );
        }
    }
    resp
}

fn ghost_button(ui: &mut egui::Ui, p: &Pal, text: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(text).color(p.ink).size(13.0))
            .fill(p.card)
            .rounding(egui::Rounding::same(8.0))
            .stroke(egui::Stroke::new(1.0, p.hair))
            .min_size(egui::vec2(0.0, 36.0)),
    )
}

/// A three-up segmented control for System/Light/Dark. Returns the newly chosen
/// preference, or `None` if nothing was clicked this frame.
fn theme_segmented(ui: &mut egui::Ui, p: &Pal, current: ThemePref) -> Option<ThemePref> {
    let mut chosen = None;
    ui.horizontal(|ui| {
        for (t, label) in [
            (ThemePref::System, "System"),
            (ThemePref::Light, "Light"),
            (ThemePref::Dark, "Dark"),
        ] {
            let active = current == t;
            let (fill, txt, stroke) = if active {
                (p.accent, p.on_accent, p.accent)
            } else {
                (p.card, p.ink, p.hair)
            };
            let resp = ui.add(
                egui::Button::new(egui::RichText::new(label).color(txt).size(13.0))
                    .fill(fill)
                    .rounding(egui::Rounding::same(8.0))
                    .stroke(egui::Stroke::new(1.0, stroke))
                    .min_size(egui::vec2(90.0, 36.0)),
            );
            if resp.clicked() {
                chosen = Some(t);
            }
        }
    });
    chosen
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

/// Group a count with thousands separators for readability: 12345 → "12,345".
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
