//! agent-buddy-widget — the floating desktop buddy.
//!
//! A standalone process (spawned/killed by the control panel, or run directly)
//! whose whole window is a frameless, transparent, always-on-top mascot. It must
//! be its own process because eframe 0.28 can only make a process's *main*
//! viewport transparent (a child viewport renders opaque black — egui #3632).
//! All the rendering + state logic lives in `agent_buddy::widget`; this binary is
//! just the window/process shell.

#![cfg_attr(windows, windows_subsystem = "windows")] // no console window on Windows

use agent_buddy::widget::{self, BuddyWidget, WIDGET_SIZE};
use eframe::egui;

fn main() -> eframe::Result<()> {
    // Single-instance: a second widget would stack a duplicate mascot. Hold an
    // advisory lock for the process lifetime; bail cleanly if one already runs.
    let _lock = match acquire_lock() {
        Ok(l) => l,
        Err(_) => {
            eprintln!("agent-buddy-widget is already running.");
            return Ok(());
        }
    };

    // Don't outlive the parent control panel: if it dies without killing us,
    // exit so we can't orphan on screen.
    spawn_parent_watch();

    let mut builder = egui::ViewportBuilder::default()
        .with_title("Agent Buddy")
        .with_inner_size([WIDGET_SIZE, WIDGET_SIZE])
        .with_transparent(true)
        .with_decorations(false)
        .with_always_on_top()
        .with_resizable(false)
        .with_taskbar(false);
    if let Some(p) = widget::load_pos() {
        builder = builder.with_position([p.x, p.y]);
    }

    let options = eframe::NativeOptions {
        viewport: builder,
        // The buddy owns its own placement; never let eframe restore a stale one.
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        "Agent Buddy Widget",
        options,
        Box::new(|cc| Ok(Box::new(BuddyWidget::new(cc)))),
    )
}

/// Exclusive advisory lock at `config_dir()/widget.lock` (keep the handle alive).
#[cfg(unix)]
fn acquire_lock() -> Result<std::fs::File, Box<dyn std::error::Error>> {
    use std::os::unix::io::AsRawFd;
    let path = agent_buddy::state::config_dir()?.join("widget.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(Box::new(std::io::Error::last_os_error()));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn acquire_lock() -> Result<std::fs::File, Box<dyn std::error::Error>> {
    let path = agent_buddy::state::config_dir()?.join("widget.lock");
    Ok(std::fs::OpenOptions::new().create(true).write(true).open(&path)?)
}

/// On unix, poll our parent pid; once it's reparented to init (pid 1, i.e. the
/// control panel exited) tear down so the buddy doesn't linger orphaned.
#[cfg(unix)]
fn spawn_parent_watch() {
    // Only the control-panel-managed instance self-exits when its parent dies
    // (the app sets this when it spawns us). A direct `./agent-buddy-widget` run
    // — e.g. for testing — never watches, so a transient shell exit can't kill it.
    if std::env::var_os("BUDDY_WIDGET_MANAGED").is_none() {
        return;
    }
    let start_ppid = unsafe { libc::getppid() };
    if start_ppid <= 1 {
        return;
    }
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let ppid = unsafe { libc::getppid() };
        if ppid != start_ppid || ppid <= 1 {
            std::process::exit(0);
        }
    });
}

#[cfg(not(unix))]
fn spawn_parent_watch() {}
