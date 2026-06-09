//! The floating desktop buddy — a frameless, transparent, always-on-top mascot
//! that reacts to your AI-tool session activity.
//!
//! This runs as its **own process** (`agent-buddy-widget`), not a window inside
//! the control panel: egui/eframe 0.28 can only make a process's *main* viewport
//! transparent — a child viewport renders an opaque black box (egui #3632), which
//! a spike confirmed on both the glow and wgpu backends. So the widget owns its
//! own `eframe::run_native` with a transparent main viewport and reuses the
//! library: it polls the daemon over the same IPC the control panel uses
//! (`client::status`), resolves the active agent's pack + palette from
//! [`agent::load_profiles`], and animates one of the seven `PersonaState`s.
//!
//! Appearance, in resolution order: per-state frame PNGs at
//! `config_dir()/packs/<pack-id>/<state>/<state>-N.png` (where a release or user
//! can drop real sprite-forge art), then a bundled default pack, then a
//! procedural fallback drawn from the agent's palette so the buddy always has a
//! face even with zero assets.

use crate::agent::{self, AgentProfile, Color, Palette};
use crate::ipc::StatusReport;
use eframe::egui;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

/// Outer size of the floating window, in points. The art (≤128–252px) is fit and
/// centered inside with headroom for accents (sparkles/heart above the head).
pub const WIDGET_SIZE: f32 = 200.0;
/// How long the base state must stay idle before the buddy falls asleep.
const SLEEP_AFTER: Duration = Duration::from_secs(60);
/// Default per-frame duration when a pack carries no `pipeline-meta.json`.
const DEFAULT_FRAME_MS: u32 = 90;
/// Visible sliver left on screen when the buddy is peeked-away at an edge.
const PEEK_SLIVER: f32 = 14.0;

// --- the bundled default pack ---------------------------------------------
// Filled in by `bundled.rs` (generated frames `include_bytes!`'d). Each entry is
// (state, &[frame_bytes], frame_ms). Empty until step C wires real assets, which
// is fine — the procedural fallback covers any state with no frames.
#[path = "widget_bundled.rs"]
mod bundled;

// --------------------------------------------------------------------------
// Persona state machine — mirrors the firmware `derive()` (main.cpp) plus the
// timed one-shot overlays (celebrate/heart/dizzy).
// --------------------------------------------------------------------------

/// Picks which of the seven animation states the buddy should show, from the
/// live `StatusReport`. Base state comes from session counts; brief one-shots
/// (celebrate/heart/dizzy) overlay it for a couple of seconds on an event edge.
struct BuddyAnim {
    cur: &'static str,
    cur_started: Instant,
    idle_since: Instant,
    one_shot: Option<(&'static str, Instant)>, // (state, until)
    prev_running: u32,
    prev_completed: bool,
    prev_error: bool,
}

impl BuddyAnim {
    fn new(now: Instant) -> Self {
        Self {
            cur: "idle",
            cur_started: now,
            idle_since: now,
            one_shot: None,
            prev_running: 0,
            prev_completed: false,
            prev_error: false,
        }
    }

    /// Fold one status snapshot in. Detects event edges for the one-shots and
    /// recomputes the active state. `status` is `None` when the daemon is
    /// unreachable (treated as quiet → eventually sleep).
    fn tick(&mut self, status: Option<&StatusReport>, now: Instant) {
        let running = status.map(|s| s.sessions_running).unwrap_or(0);
        let waiting = status.map(|s| s.sessions_waiting).unwrap_or(0);
        let total = status.map(|s| s.sessions_total).unwrap_or(0);
        let completed = status.map(|s| s.recently_completed).unwrap_or(false);
        let errored = status.map(|s| s.recent_error).unwrap_or(false);

        // Base state (firmware `derive()`, adapted: a desktop widget reacts to a
        // single running session, where the device dashboard waited for ≥3).
        let mut base = if waiting > 0 {
            "attention"
        } else if running >= 1 {
            "busy"
        } else {
            "idle"
        };
        if base == "idle" {
            // Idle long enough → sleep. Any non-idle base resets the timer.
            if now.duration_since(self.idle_since) >= SLEEP_AFTER {
                base = "sleep";
            }
        } else {
            self.idle_since = now;
        }

        // One-shot edges. Error wins over completion; a single-session finish
        // reads as a happy "heart", a larger batch as "celebrate".
        if errored && !self.prev_error {
            self.one_shot = Some(("dizzy", now + Duration::from_secs(2)));
        } else if completed && !self.prev_completed {
            let st = if self.prev_running <= 1 { "heart" } else { "celebrate" };
            let dur = if st == "heart" { 2 } else { 3 };
            self.one_shot = Some((st, now + Duration::from_secs(dur)));
        }
        self.prev_running = running;
        self.prev_completed = completed;
        self.prev_error = errored;
        let _ = total;

        // Active = unexpired one-shot, else base.
        let active = match self.one_shot {
            Some((st, until)) if now < until => st,
            _ => {
                self.one_shot = None;
                base
            }
        };
        if active != self.cur {
            self.cur = active;
            self.cur_started = now;
        }
    }

    fn state(&self) -> &'static str {
        self.cur
    }

    /// Seconds the current state has been showing — drives frame index + the
    /// procedural motion phase.
    fn elapsed(&self, now: Instant) -> f32 {
        now.duration_since(self.cur_started).as_secs_f32()
    }
}

// --------------------------------------------------------------------------
// Loaded animation art for one state.
// --------------------------------------------------------------------------

struct Loaded {
    frames: Vec<egui::TextureHandle>,
    frame_ms: u32,
}

// --------------------------------------------------------------------------
// Which screen edge the buddy is docked against (for peek/hide).
// --------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Left,
    Right,
    Top,
}

// --------------------------------------------------------------------------
// The widget app.
// --------------------------------------------------------------------------

pub struct BuddyWidget {
    profiles: HashMap<String, AgentProfile>,
    active_agent: String,
    pack_id: String,
    palette: Palette,
    anim: BuddyAnim,
    /// Lazy per-state texture cache. `None` value = resolved to "no art, draw
    /// procedurally" so we don't retry the filesystem every frame.
    cache: HashMap<&'static str, Option<Loaded>>,
    // status polling
    rx: Receiver<Option<StatusReport>>,
    status: Option<StatusReport>,
    // window placement / peek
    docked: Option<Edge>,
    peeked: bool,
    last_hover: Instant,
    saved_pos: Option<egui::Pos2>,
}

impl BuddyWidget {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let now = Instant::now();
        let profiles = agent::load_profiles();
        let (active_agent, pack_id, palette) = resolve_active(&profiles, None);
        let rx = spawn_status_poll(cc.egui_ctx.clone());
        BuddyWidget {
            profiles,
            active_agent,
            pack_id,
            palette,
            anim: BuddyAnim::new(now),
            cache: HashMap::new(),
            rx,
            status: None,
            docked: None,
            peeked: false,
            last_hover: now,
            saved_pos: load_pos(),
        }
    }

    /// Swap to a new active agent's pack + palette and drop the texture cache so
    /// the right character loads lazily.
    fn set_agent(&mut self, id: &str) {
        let (active, pack, pal) = resolve_active(&self.profiles, Some(id));
        self.active_agent = active;
        self.pack_id = pack;
        self.palette = pal;
        self.cache.clear();
    }

    /// Ensure `state`'s frames are loaded (or marked procedural). Resolution:
    /// config-dir pack → bundled default → procedural.
    fn ensure_loaded(&mut self, ctx: &egui::Context, state: &'static str) {
        if self.cache.contains_key(state) {
            return;
        }
        let loaded = load_state_textures(ctx, &self.pack_id, state);
        self.cache.insert(state, loaded);
    }
}

impl eframe::App for BuddyWidget {
    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] {
        // MUST be transparent — this is the whole point. The window only shows
        // the buddy's own non-transparent pixels.
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();

        // Drain status; swap agent if it changed.
        while let Ok(s) = self.rx.try_recv() {
            self.status = s;
        }
        if let Some(s) = &self.status {
            if !s.active_agent.is_empty() && s.active_agent != self.active_agent {
                let id = s.active_agent.clone();
                self.set_agent(&id);
            }
        }

        self.anim.tick(self.status.as_ref(), now);
        let state = self.anim.state();
        self.ensure_loaded(ctx, state);

        let palette = self.palette.clone();
        let elapsed = self.anim.elapsed(now);

        let frame_ms = self
            .cache
            .get(state)
            .and_then(|o| o.as_ref())
            .map(|l| l.frame_ms)
            .unwrap_or(DEFAULT_FRAME_MS);

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::TRANSPARENT))
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                // The whole window drags the buddy and is the hover target.
                let resp = ui.interact(
                    rect,
                    ui.id().with("buddy-drag"),
                    egui::Sense::click_and_drag(),
                );
                if resp.dragged() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if resp.drag_stopped() {
                    persist_pos(ctx);
                }
                if resp.hovered() {
                    self.last_hover = now;
                }

                // Paint: real frames if loaded, else procedural.
                let painted = self
                    .cache
                    .get(state)
                    .and_then(|o| o.as_ref())
                    .map(|l| {
                        let n = l.frames.len().max(1);
                        let idx = ((elapsed * 1000.0 / frame_ms as f32) as usize) % n;
                        paint_texture(ui, rect, &l.frames[idx]);
                    })
                    .is_some();
                if !painted {
                    paint_procedural(ui.painter(), rect, &palette, state, elapsed);
                }
            });

        self.update_peek(ctx, now);
        self.maybe_save_pos(ctx);

        // Animate at the pack's cadence, not 60fps — low CPU.
        ctx.request_repaint_after(Duration::from_millis(frame_ms.clamp(33, 200) as u64));
    }
}

// --------------------------------------------------------------------------
// Window placement: persistence + peek/hide.
// --------------------------------------------------------------------------

impl BuddyWidget {
    /// Detect docking against a screen edge and slide the buddy mostly off when
    /// idle, peeking back on hover. Best-effort: egui exposes the monitor size
    /// but not its origin, so this is accurate on the primary monitor.
    fn update_peek(&mut self, ctx: &egui::Context, now: Instant) {
        let (outer, monitor) = ctx.input(|i| {
            let v = i.viewport();
            (v.outer_rect, v.monitor_size)
        });
        let (Some(outer), Some(monitor)) = (outer, monitor) else {
            return;
        };
        // Which edge (if any) has the buddy been *shoved past*? Peek only engages
        // when part of the window is already hanging off the screen — a deliberate
        // drag. Resting flush in a corner (a window's edge exactly at the screen
        // boundary, e.g. bottom-right) is a normal placement and must stay fully
        // visible, not auto-hide.
        let off = PEEK_SLIVER;
        self.docked = if outer.min.x <= -off {
            Some(Edge::Left)
        } else if outer.max.x >= monitor.x + off {
            Some(Edge::Right)
        } else if outer.min.y <= -off {
            Some(Edge::Top)
        } else {
            None
        };

        let hovering = now.duration_since(self.last_hover) < Duration::from_millis(400);
        let want_peek = self.docked.is_some() && !hovering;
        if want_peek == self.peeked {
            return; // already in the desired state
        }
        let Some(edge) = self.docked else { return };
        let target = match (edge, want_peek) {
            (Edge::Left, true) => egui::pos2(PEEK_SLIVER - WIDGET_SIZE, outer.min.y),
            (Edge::Left, false) => egui::pos2(0.0, outer.min.y),
            (Edge::Right, true) => egui::pos2(monitor.x - PEEK_SLIVER, outer.min.y),
            (Edge::Right, false) => egui::pos2(monitor.x - WIDGET_SIZE, outer.min.y),
            (Edge::Top, true) => egui::pos2(outer.min.x, PEEK_SLIVER - WIDGET_SIZE),
            (Edge::Top, false) => egui::pos2(outer.min.x, 0.0),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(target));
        self.peeked = want_peek;
    }

    /// Persist the window position when it settles (debounced; never while peeked
    /// off-screen, so we don't save a hidden position).
    fn maybe_save_pos(&mut self, ctx: &egui::Context) {
        if self.peeked {
            return;
        }
        let outer = ctx.input(|i| i.viewport().outer_rect);
        if let Some(r) = outer {
            let p = r.min;
            if self.saved_pos.map(|s| (s - p).length() > 1.0).unwrap_or(true) {
                self.saved_pos = Some(p);
                save_pos(p);
            }
        }
    }
}

// --------------------------------------------------------------------------
// Free helpers.
// --------------------------------------------------------------------------

fn c32(c: Color) -> egui::Color32 {
    egui::Color32::from_rgb(c.r, c.g, c.b)
}

/// Resolve the active agent id → (id, pack id, palette). Falls back to
/// `claude-code` then to any profile then to a default palette.
fn resolve_active(
    profiles: &HashMap<String, AgentProfile>,
    want: Option<&str>,
) -> (String, String, Palette) {
    let id = want
        .map(str::to_string)
        .filter(|id| profiles.contains_key(id))
        .or_else(|| profiles.contains_key("claude-code").then(|| "claude-code".to_string()))
        .or_else(|| profiles.keys().next().cloned())
        .unwrap_or_else(|| "claude-code".to_string());
    if let Some(p) = profiles.get(&id) {
        (id, p.pack().to_string(), p.palette.clone())
    } else {
        (id.clone(), id, default_palette())
    }
}

fn default_palette() -> Palette {
    // A neutral terracotta buddy when no profile is available at all.
    serde_json::from_value(serde_json::json!({
        "body": "#C55531", "bg": "#000000", "text": "#FFFFFF",
        "text_dim": "#808080", "ink": "#1A1A1A"
    }))
    .expect("valid default palette")
}

/// Load (and cache-decide) one state's frames. Filesystem pack first, then the
/// bundled default, then `None` (→ procedural).
fn load_state_textures(ctx: &egui::Context, pack_id: &str, state: &'static str) -> Option<Loaded> {
    if let Some(dir) = config_pack_state_dir(pack_id, state) {
        if let Some(loaded) = load_fs_frames(ctx, &dir, state) {
            return Some(loaded);
        }
    }
    // Bundled default (covers any pack id — a generic buddy).
    if let Some((bytes, ms)) = bundled::frames(state) {
        let frames: Vec<_> = bytes
            .iter()
            .enumerate()
            .filter_map(|(i, b)| decode_to_texture(ctx, b, &format!("buddy-{state}-b{i}")))
            .collect();
        if !frames.is_empty() {
            return Some(Loaded { frames, frame_ms: ms });
        }
    }
    None
}

/// `config_dir()/packs/<pack-id>/<state>/` if it exists.
fn config_pack_state_dir(pack_id: &str, state: &str) -> Option<PathBuf> {
    let dir = crate::state::config_dir().ok()?.join("packs").join(pack_id).join(state);
    dir.is_dir().then_some(dir)
}

/// Load `<state>-N.png` frames from a directory, sorted by frame number, reading
/// per-frame duration from a sibling `pipeline-meta.json` when present.
fn load_fs_frames(ctx: &egui::Context, dir: &Path, state: &str) -> Option<Loaded> {
    let mut nums: Vec<(u32, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_stem()?.to_str()?.to_string();
            let n = name.strip_prefix(state)?.trim_start_matches('-');
            n.parse::<u32>().ok().map(|num| (num, path))
        })
        .collect();
    nums.sort_by_key(|(n, _)| *n);
    if nums.is_empty() {
        return None;
    }
    let frames: Vec<_> = nums
        .iter()
        .filter_map(|(n, p)| {
            let bytes = std::fs::read(p).ok()?;
            decode_to_texture(ctx, &bytes, &format!("buddy-{state}-{n}"))
        })
        .collect();
    if frames.is_empty() {
        return None;
    }
    let frame_ms = meta_frame_ms(dir).unwrap_or(DEFAULT_FRAME_MS);
    Some(Loaded { frames, frame_ms })
}

/// Read `duration_ms` (or `duration`) from a `pipeline-meta.json` in `dir`.
fn meta_frame_ms(dir: &Path) -> Option<u32> {
    let txt = std::fs::read_to_string(dir.join("pipeline-meta.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    v.get("duration_ms")
        .or_else(|| v.get("duration"))
        .and_then(|d| d.as_u64())
        .map(|d| d as u32)
}

/// Decode PNG bytes → an egui texture (straight, un-premultiplied alpha).
fn decode_to_texture(ctx: &egui::Context, bytes: &[u8], name: &str) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
    Some(ctx.load_texture(name, color, egui::TextureOptions::LINEAR))
}

/// Paint a texture fit (aspect-preserved) and centered in `rect`.
fn paint_texture(ui: &egui::Ui, rect: egui::Rect, tex: &egui::TextureHandle) {
    let size = tex.size_vec2();
    let scale = (rect.width() / size.x).min(rect.height() / size.y).min(1.5);
    let draw = size * scale;
    let at = egui::Rect::from_center_size(rect.center(), draw);
    egui::Image::new((tex.id(), draw)).paint_at(ui, at);
}

// --------------------------------------------------------------------------
// Procedural fallback — a palette-tinted blob with a per-state face + accents,
// bobbing on a sine. Always available, no assets required.
// --------------------------------------------------------------------------

fn paint_procedural(
    p: &egui::Painter,
    rect: egui::Rect,
    pal: &Palette,
    state: &str,
    t: f32,
) {
    let body = c32(pal.body);
    let ink = c32(pal.ink);
    let center = rect.center();
    let unit = rect.width().min(rect.height());
    let r = unit * 0.30;

    // Per-state vertical motion + squash.
    let (bob, squash) = match state {
        "sleep" => ((t * 1.1).sin() * unit * 0.012, 0.0),
        "busy" => ((t * 9.0).sin() * unit * 0.05, (t * 9.0).cos() * 0.06),
        "attention" => (-(t * 6.0).sin().max(0.0) * unit * 0.10, 0.0),
        "celebrate" => (-(t * 5.0).sin().abs() * unit * 0.14, (t * 10.0).sin() * 0.08),
        "dizzy" => ((t * 7.0).sin() * unit * 0.02, 0.0),
        "heart" => ((t * 4.0).sin() * unit * 0.03, 0.0),
        _ => ((t * 2.2).sin() * unit * 0.02, 0.0), // idle
    };
    let bc = egui::pos2(center.x + if state == "dizzy" { (t * 7.0).sin() * unit * 0.03 } else { 0.0 }, center.y + bob);
    let rx = r * (1.0 + squash);
    let ry = r * (1.0 - squash);

    // Body (a soft rounded blob).
    let body_rect = egui::Rect::from_center_size(bc, egui::vec2(rx * 2.0, ry * 2.0));
    p.rect_filled(body_rect, ry, body);

    // Eyes.
    let eye_dx = rx * 0.42;
    let eye_dy = ry * 0.18;
    let eye_r = (unit * 0.035).max(2.0);
    let le = egui::pos2(bc.x - eye_dx, bc.y - eye_dy);
    let re = egui::pos2(bc.x + eye_dx, bc.y - eye_dy);
    let stroke = egui::Stroke::new((unit * 0.018).max(1.5), ink);
    match state {
        "sleep" => {
            // closed eyes — short arcs (drawn as small horizontal lines).
            p.line_segment([le - egui::vec2(eye_r, 0.0), le + egui::vec2(eye_r, 0.0)], stroke);
            p.line_segment([re - egui::vec2(eye_r, 0.0), re + egui::vec2(eye_r, 0.0)], stroke);
        }
        "dizzy" => {
            // X eyes.
            for e in [le, re] {
                p.line_segment([e - egui::vec2(eye_r, eye_r), e + egui::vec2(eye_r, eye_r)], stroke);
                p.line_segment([e - egui::vec2(eye_r, -eye_r), e + egui::vec2(eye_r, -eye_r)], stroke);
            }
        }
        "attention" => {
            // wide eyes.
            p.circle_filled(le, eye_r * 1.4, ink);
            p.circle_filled(re, eye_r * 1.4, ink);
        }
        _ => {
            // a blink every couple seconds for idle/busy/celebrate/heart
            let blink = matches!(state, "idle") && ((t % 3.0) < 0.12);
            if blink {
                p.line_segment([le - egui::vec2(eye_r, 0.0), le + egui::vec2(eye_r, 0.0)], stroke);
                p.line_segment([re - egui::vec2(eye_r, 0.0), re + egui::vec2(eye_r, 0.0)], stroke);
            } else {
                p.circle_filled(le, eye_r, ink);
                p.circle_filled(re, eye_r, ink);
            }
        }
    }

    // Accents.
    match state {
        "sleep" => {
            // rising "z"s above.
            for k in 0..3 {
                let ph = (t * 0.6 + k as f32 * 0.5).fract();
                let z = egui::pos2(bc.x + rx * 0.7 + k as f32 * unit * 0.04, bc.y - ry - ph * unit * 0.25);
                p.text(z, egui::Align2::CENTER_CENTER, "z", egui::FontId::proportional(unit * 0.07 * (1.0 - ph)), c32(pal.text_dim));
            }
        }
        "attention" => {
            let a = egui::pos2(bc.x, bc.y - ry - unit * 0.12);
            p.text(a, egui::Align2::CENTER_CENTER, "!", egui::FontId::proportional(unit * 0.16), c32(pal.hot));
        }
        "celebrate" => {
            for k in 0..6 {
                let ang = k as f32 * std::f32::consts::TAU / 6.0 + t * 2.0;
                let ph = (t * 1.5 + k as f32 * 0.3).fract();
                let rad = unit * (0.32 + ph * 0.14);
                let s = egui::pos2(bc.x + ang.cos() * rad, bc.y + ang.sin() * rad - unit * 0.05);
                p.circle_filled(s, (unit * 0.02 * (1.0 - ph)).max(1.0), c32(pal.ok));
            }
        }
        "heart" => {
            let pulse = 1.0 + (t * 4.0).sin() * 0.12;
            let h = egui::pos2(bc.x, bc.y - ry - unit * 0.12);
            p.circle_filled(h, unit * 0.045 * pulse, c32(pal.hot));
        }
        "dizzy" => {
            for k in 0..3 {
                let ang = t * 6.0 + k as f32 * std::f32::consts::TAU / 3.0;
                let s = egui::pos2(bc.x + ang.cos() * rx * 1.1, bc.y - ry - unit * 0.06 + ang.sin() * unit * 0.03);
                p.text(s, egui::Align2::CENTER_CENTER, "*", egui::FontId::proportional(unit * 0.08), c32(pal.text));
            }
        }
        _ => {}
    }
}

// --------------------------------------------------------------------------
// Status polling — a background thread mirroring the control panel's worker.
// --------------------------------------------------------------------------

fn spawn_status_poll(ctx: egui::Context) -> Receiver<Option<StatusReport>> {
    let (tx, rx): (Sender<Option<StatusReport>>, _) = std::sync::mpsc::channel();
    std::thread::spawn(move || loop {
        let s = crate::client::status().ok();
        if tx.send(s).is_err() {
            break;
        }
        ctx.request_repaint();
        std::thread::sleep(Duration::from_millis(1000));
    });
    rx
}

// --------------------------------------------------------------------------
// Position persistence (config_dir/widget_pos = "x,y").
// --------------------------------------------------------------------------

fn pos_path() -> Option<PathBuf> {
    crate::state::config_dir().ok().map(|d| d.join("widget_pos"))
}

/// Read a saved window position, used by the binary to seed `with_position`.
pub fn load_pos() -> Option<egui::Pos2> {
    let txt = std::fs::read_to_string(pos_path()?).ok()?;
    let (x, y) = txt.trim().split_once(',')?;
    Some(egui::pos2(x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn save_pos(p: egui::Pos2) {
    if let Some(path) = pos_path() {
        let _ = std::fs::write(path, format!("{},{}", p.x, p.y));
    }
}

fn persist_pos(ctx: &egui::Context) {
    if let Some(r) = ctx.input(|i| i.viewport().outer_rect) {
        save_pos(r.min);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::StatusReport;

    fn status(running: u32, waiting: u32, completed: bool, error: bool) -> StatusReport {
        StatusReport {
            sessions_running: running,
            sessions_waiting: waiting,
            sessions_total: running.max(waiting),
            recently_completed: completed,
            recent_error: error,
            active_agent: "claude-code".into(),
            ..Default::default()
        }
    }

    #[test]
    fn base_states_map_from_counts() {
        let t0 = Instant::now();
        let mut a = BuddyAnim::new(t0);
        a.tick(Some(&status(0, 0, false, false)), t0);
        assert_eq!(a.state(), "idle");
        a.tick(Some(&status(2, 0, false, false)), t0);
        assert_eq!(a.state(), "busy");
        a.tick(Some(&status(0, 1, false, false)), t0);
        assert_eq!(a.state(), "attention");
    }

    #[test]
    fn goes_to_sleep_after_idle_window() {
        let t0 = Instant::now();
        let mut a = BuddyAnim::new(t0);
        a.tick(Some(&status(0, 0, false, false)), t0);
        assert_eq!(a.state(), "idle");
        let later = t0 + SLEEP_AFTER + Duration::from_secs(1);
        a.tick(Some(&status(0, 0, false, false)), later);
        assert_eq!(a.state(), "sleep");
    }

    #[test]
    fn completion_edge_fires_one_shot_then_reverts() {
        let t0 = Instant::now();
        let mut a = BuddyAnim::new(t0);
        // Was running (2) → now a completion edge → celebrate (batch).
        a.tick(Some(&status(2, 0, false, false)), t0);
        a.tick(Some(&status(0, 0, true, false)), t0);
        assert_eq!(a.state(), "celebrate");
        // After the window, reverts to idle.
        let after = t0 + Duration::from_secs(4);
        a.tick(Some(&status(0, 0, true, false)), after);
        assert_eq!(a.state(), "idle");
    }

    #[test]
    fn single_session_finish_is_heart() {
        let t0 = Instant::now();
        let mut a = BuddyAnim::new(t0);
        a.tick(Some(&status(1, 0, false, false)), t0);
        a.tick(Some(&status(0, 0, true, false)), t0);
        assert_eq!(a.state(), "heart");
    }

    #[test]
    fn error_edge_is_dizzy_and_wins_over_completion() {
        let t0 = Instant::now();
        let mut a = BuddyAnim::new(t0);
        a.tick(Some(&status(1, 0, false, false)), t0);
        a.tick(Some(&status(0, 0, true, true)), t0);
        assert_eq!(a.state(), "dizzy");
    }
}
