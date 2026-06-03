//! Persisted configuration and in-memory aggregate session state.

use crate::protocol::SessionInfo;
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Returns the per-user config/state directory, creating it if needed.
/// macOS: ~/Library/Application Support/agent-buddy
/// Linux: ~/.config/agent-buddy
/// Windows: %APPDATA%\agent-buddy
pub fn config_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("com", "anthropic", "agent-buddy")
        .context("could not determine a config directory")?;
    let dir = dirs.config_dir().to_path_buf();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// On-disk daemon config. Small and hand-editable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Owner first name, sent to the device on connect. Defaults to the OS
    /// username.
    pub owner: String,
    /// BLE peripheral id (platform-specific string) to prefer when more than
    /// one buddy is in range. `None` = connect to the first match.
    pub preferred_device: Option<String>,
    /// Daily token counter, reset at local midnight.
    #[serde(default)]
    pub tokens_today: u64,
    /// `YYYY-MM-DD` (local) the `tokens_today` counter belongs to.
    #[serde(default)]
    pub tokens_day: String,
    /// Tool-name regex whose permission prompts route to the buddy, as chosen by
    /// `setup --tools`. `None` = use the built-in default. Persisted so hook
    /// reconciliation (daemon startup / app update) preserves a custom choice
    /// instead of resetting it to the default.
    #[serde(default)]
    pub hook_matcher: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            owner: whoami::realname()
                .split_whitespace()
                .next()
                .unwrap_or("there")
                .to_string(),
            preferred_device: None,
            tokens_today: 0,
            tokens_day: String::new(),
            hook_matcher: None,
        }
    }
}

impl Config {
    fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("config.json"))
    }

    pub fn load() -> Result<Config> {
        let path = Self::path()?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Config::default();
                cfg.save()?;
                Ok(cfg)
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Remember the BLE peripheral id of the buddy we just connected to and
    /// persist it, so a later daemon restart prefers the *same* device. No-op
    /// (and no disk write) when the id is already current.
    pub fn set_preferred_device(&mut self, id: &str) -> Result<()> {
        if self.preferred_device.as_deref() == Some(id) {
            return Ok(());
        }
        self.preferred_device = Some(id.to_string());
        self.save()
    }
}

/// Per-session liveness + telemetry, aggregated into the heartbeat.
#[derive(Debug, Clone, Default)]
struct Session {
    running: bool,
    waiting: bool,
    /// Working directory; its basename is the project label on the device.
    cwd: String,
    /// Short model name (`opus`/`sonnet`/`haiku`), latest seen on a turn.
    model: String,
    /// Cumulative output tokens for this session (from its transcript).
    out_tokens: u64,
    /// Last turn's context size (input+cache+output tokens).
    ctx_tokens: u64,
    /// Model context window for the last-seen model (200K, or 1M for *-1m).
    ctx_limit: u64,
    /// Monotonic activity tick for ordering the capped snapshot (newest first).
    last_active: u64,
}

/// Context-window default and the wide-context variant.
const CTX_DEFAULT: u64 = 200_000;
const CTX_WIDE: u64 = 1_000_000;

/// Map a full model id (e.g. `claude-opus-4-8` / `claude-sonnet-4-6[1m]`) to a
/// short family name for the device. Empty in → empty out.
pub fn model_short(model_id: &str) -> String {
    let m = model_id.to_ascii_lowercase();
    for fam in ["opus", "sonnet", "haiku"] {
        if m.contains(fam) {
            return fam.to_string();
        }
    }
    // Unknown but non-empty: keep a short slice so something shows.
    m.chars().take(7).collect()
}

/// Context window for a model id. Opus runs the wide (1M) window by default —
/// that's how we run it, and the transcript's model id never carries the `[1m]`
/// beta marker, so the family is the only signal we get. An explicit `1m` in the
/// id still forces wide for any family (e.g. a 1m Sonnet); everything else is the
/// 200K default.
pub fn ctx_limit_for(model_id: &str) -> u64 {
    let m = model_id.to_ascii_lowercase();
    if m.contains("1m") || m.contains("opus") {
        CTX_WIDE
    } else {
        CTX_DEFAULT
    }
}

/// Project label = basename of the cwd (final path component), '/' or '\'.
pub fn label_from_cwd(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_string()
}

/// In-memory rollup of all active Claude Code sessions plus the recent
/// transcript ticker. Lives inside the daemon; not persisted (except the
/// token counters, which flush to [`Config`]).
#[derive(Debug, Default)]
pub struct SessionState {
    sessions: HashMap<String, Session>,
    /// Recent one-line activity entries, newest first.
    entries: Vec<String>,
    /// Cumulative output tokens since the daemon started.
    pub tokens: u64,
    pub tokens_today: u64,
    /// Local `YYYY-MM-DD` the daily counter belongs to.
    pub tokens_day: String,
    /// Per-session running total of output tokens already counted, so repeated
    /// Stop/SubagentStop events for the same transcript don't double-count.
    session_tokens: HashMap<String, u64>,
    /// Epoch second until which a turn-completion (celebrate) is signaled.
    completed_until: i64,
    /// Monotonic counter stamped onto a session whenever it sees activity, so
    /// the snapshot can order by recency without pulling a clock in here.
    activity_tick: u64,
}

const MAX_ENTRIES: usize = 8;
/// Cap on per-session rows sent in a heartbeat, to bound the wire line size.
const MAX_SESSIONS: usize = 6;
/// Length of the short session id prefix carried on the wire.
const SHORT_ID_LEN: usize = 6;

/// A stable short prefix of a session id, for the device's select/echo key.
fn short_id(id: &str) -> String {
    id.chars().take(SHORT_ID_LEN).collect()
}

/// Context fill as a 0..100 percent, clamped. Zero limit/tokens → 0.
fn ctx_pct(ctx_tokens: u64, ctx_limit: u64) -> u8 {
    if ctx_limit == 0 || ctx_tokens == 0 {
        return 0;
    }
    ((ctx_tokens.saturating_mul(100) / ctx_limit).min(100)) as u8
}

impl SessionState {
    pub fn from_config(cfg: &Config) -> Self {
        SessionState {
            tokens_today: cfg.tokens_today,
            tokens_day: cfg.tokens_day.clone(),
            ..Default::default()
        }
    }

    pub fn session_started(&mut self, id: &str) {
        self.sessions.entry(id.to_string()).or_default();
    }

    pub fn session_ended(&mut self, id: &str) {
        self.sessions.remove(id);
        self.session_tokens.remove(id);
    }

    pub fn set_running(&mut self, id: &str, running: bool) {
        let tick = self.next_tick();
        let s = self.sessions.entry(id.to_string()).or_default();
        s.running = running;
        s.last_active = tick;
        if running {
            s.waiting = false;
        }
    }

    pub fn set_waiting(&mut self, id: &str, waiting: bool) {
        let tick = self.next_tick();
        let s = self.sessions.entry(id.to_string()).or_default();
        s.waiting = waiting;
        s.last_active = tick;
        if waiting {
            s.running = false;
        }
    }

    /// Record a session's working directory (its project label).
    pub fn set_cwd(&mut self, id: &str, cwd: &str) {
        if cwd.is_empty() {
            return;
        }
        let tick = self.next_tick();
        let s = self.sessions.entry(id.to_string()).or_default();
        s.cwd = cwd.to_string();
        s.last_active = tick;
    }

    /// Fold a completed turn into both the global token counters and this
    /// session's per-session telemetry (model, context size, output total).
    /// `total_out` is the session's cumulative output tokens (deduped via
    /// [`record_session_total`]).
    pub fn record_turn(
        &mut self,
        id: &str,
        total_out: u64,
        model: &str,
        ctx_tokens: u64,
        cwd: &str,
        today: &str,
    ) {
        self.record_session_total(id, total_out, today);
        let tick = self.next_tick();
        let s = self.sessions.entry(id.to_string()).or_default();
        s.out_tokens = total_out;
        s.last_active = tick;
        if !model.is_empty() {
            s.model = model_short(model);
            s.ctx_limit = ctx_limit_for(model);
        }
        if ctx_tokens > 0 {
            s.ctx_tokens = ctx_tokens;
        }
        if !cwd.is_empty() {
            s.cwd = cwd.to_string();
        }
    }

    fn next_tick(&mut self) -> u64 {
        self.activity_tick += 1;
        self.activity_tick
    }

    /// Capped, ordered per-session snapshot for the heartbeat: waiting first,
    /// then running, then idle; within a group, most-recently-active first.
    /// Capped to [`MAX_SESSIONS`] so the wire line stays small.
    pub fn sessions_snapshot(&self) -> Vec<SessionInfo> {
        let mut rows: Vec<(&String, &Session)> = self.sessions.iter().collect();
        rows.sort_by(|(_, a), (_, b)| {
            // Lower rank sorts first: waiting(0) < running(1) < idle(2).
            let rank = |s: &Session| -> u8 {
                if s.waiting {
                    0
                } else if s.running {
                    1
                } else {
                    2
                }
            };
            rank(a)
                .cmp(&rank(b))
                .then(b.last_active.cmp(&a.last_active))
        });
        rows.into_iter()
            .take(MAX_SESSIONS)
            .map(|(id, s)| SessionInfo {
                id: short_id(id),
                cwd: label_from_cwd(&s.cwd),
                st: if s.waiting {
                    "wait"
                } else if s.running {
                    "run"
                } else {
                    "idle"
                },
                tok: s.out_tokens,
                m: s.model.clone(),
                ctx: ctx_pct(s.ctx_tokens, s.ctx_limit),
                ctok: s.ctx_tokens,
                clim: s.ctx_limit,
            })
            .collect()
    }

    /// Short id for a full session id, matching what a prompt's `sid` carries.
    pub fn short_id_of(id: &str) -> String {
        short_id(id)
    }

    pub fn push_entry(&mut self, line: impl Into<String>) {
        let line = line.into();
        if line.trim().is_empty() {
            return;
        }
        self.entries.insert(0, line);
        self.entries.truncate(MAX_ENTRIES);
    }

    /// Add output tokens, rolling the daily counter over at local midnight.
    /// `today` is the caller-supplied local `YYYY-MM-DD` (we avoid pulling a
    /// clock in here so it stays testable).
    pub fn add_tokens(&mut self, n: u64, today: &str) {
        self.tokens += n;
        if self.tokens_day != today {
            self.tokens_day = today.to_string();
            self.tokens_today = 0;
        }
        self.tokens_today += n;
    }

    /// Record a session's *cumulative* output-token total (from its transcript)
    /// and fold only the newly-seen delta into the counters. Idempotent: a
    /// repeated event with the same total adds zero. Handles Stop and
    /// SubagentStop firing for the same transcript without double-counting.
    pub fn record_session_total(&mut self, session: &str, total: u64, today: &str) {
        let prev = self.session_tokens.get(session).copied().unwrap_or(0);
        let delta = total.saturating_sub(prev);
        self.session_tokens.insert(session.to_string(), total);
        if delta > 0 {
            self.add_tokens(delta, today);
        }
    }

    /// Signal a turn just completed (celebrate) until `epoch`.
    pub fn mark_completed(&mut self, until_epoch: i64) {
        self.completed_until = until_epoch;
    }

    /// Whether the celebrate window is still open at `now` (epoch seconds).
    pub fn recently_completed(&self, now: i64) -> bool {
        now < self.completed_until
    }

    pub fn total(&self) -> u32 {
        self.sessions.len() as u32
    }
    pub fn running(&self) -> u32 {
        self.sessions.values().filter(|s| s.running).count() as u32
    }
    pub fn waiting(&self) -> u32 {
        self.sessions.values().filter(|s| s.waiting).count() as u32
    }
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Flush the token counters into the persisted config.
    pub fn sync_to_config(&self, cfg: &mut Config) {
        cfg.tokens_today = self.tokens_today;
        cfg.tokens_day = self.tokens_day.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_session_counts() {
        let mut st = SessionState::default();
        st.session_started("a");
        st.session_started("b");
        st.set_running("a", true);
        st.set_waiting("b", true);
        assert_eq!(st.total(), 2);
        assert_eq!(st.running(), 1);
        assert_eq!(st.waiting(), 1);
        st.session_ended("a");
        assert_eq!(st.total(), 1);
        assert_eq!(st.running(), 0);
    }

    #[test]
    fn tokens_roll_over_at_midnight() {
        let mut st = SessionState::default();
        st.add_tokens(100, "2026-05-29");
        st.add_tokens(50, "2026-05-29");
        assert_eq!(st.tokens_today, 150);
        assert_eq!(st.tokens, 150);
        st.add_tokens(10, "2026-05-30");
        assert_eq!(st.tokens_today, 10); // reset
        assert_eq!(st.tokens, 160); // cumulative unaffected
    }

    #[test]
    fn session_totals_fold_in_only_the_delta() {
        let mut st = SessionState::default();
        // First Stop: session has produced 500 tokens cumulatively.
        st.record_session_total("s1", 500, "2026-05-29");
        assert_eq!(st.tokens, 500);
        // A SubagentStop fires for the same transcript at the same total — must
        // NOT double-count.
        st.record_session_total("s1", 500, "2026-05-29");
        assert_eq!(st.tokens, 500);
        // Next turn grows the cumulative total to 800 → only +300 folds in.
        st.record_session_total("s1", 800, "2026-05-29");
        assert_eq!(st.tokens, 800);
        // A second session is independent.
        st.record_session_total("s2", 100, "2026-05-29");
        assert_eq!(st.tokens, 900);
        // Ending a session clears its memory so a reused id starts fresh.
        st.session_ended("s1");
        st.record_session_total("s1", 50, "2026-05-29");
        assert_eq!(st.tokens, 950);
    }

    #[test]
    fn model_short_maps_families_and_falls_back() {
        assert_eq!(model_short("claude-opus-4-8"), "opus");
        assert_eq!(model_short("claude-sonnet-4-6[1m]"), "sonnet");
        assert_eq!(model_short("claude-haiku-4-5-20251001"), "haiku");
        assert_eq!(model_short(""), "");
        assert_eq!(model_short("gpt-something"), "gpt-som"); // 7-char fallback
    }

    #[test]
    fn ctx_limit_opus_is_wide_others_default() {
        // Opus runs the 1M window by default (no `1m` marker in the transcript).
        assert_eq!(ctx_limit_for("claude-opus-4-8"), CTX_WIDE);
        assert_eq!(ctx_limit_for("claude-opus-4-8[1m]"), CTX_WIDE);
        // Non-opus families default to 200K…
        assert_eq!(ctx_limit_for("claude-sonnet-4-6"), CTX_DEFAULT);
        assert_eq!(ctx_limit_for("claude-haiku-4-5"), CTX_DEFAULT);
        // …unless the id explicitly flags a 1m variant.
        assert_eq!(ctx_limit_for("claude-sonnet-4-6-1m"), CTX_WIDE);
    }

    #[test]
    fn label_is_cwd_basename() {
        assert_eq!(
            label_from_cwd("/Users/x/buddy/claude-cyd-buddy"),
            "claude-cyd-buddy"
        );
        assert_eq!(label_from_cwd("/Users/x/buddy/"), "buddy"); // trailing slash
        assert_eq!(label_from_cwd("C:\\dev\\proj"), "proj");
        assert_eq!(label_from_cwd(""), "");
    }

    #[test]
    fn ctx_pct_clamps() {
        assert_eq!(ctx_pct(0, 200_000), 0);
        assert_eq!(ctx_pct(100_000, 200_000), 50);
        assert_eq!(ctx_pct(300_000, 200_000), 100); // clamp
        assert_eq!(ctx_pct(50_000, 0), 0); // no limit
    }

    #[test]
    fn snapshot_orders_and_caps() {
        let mut st = SessionState::default();
        // 7 sessions; only MAX_SESSIONS (6) come back.
        for i in 0..7 {
            st.session_started(&format!("s{i}"));
        }
        st.set_running("s1", true);
        st.set_waiting("s2", true);
        st.record_turn(
            "s3",
            1234,
            "claude-opus-4-8",
            100_000,
            "/home/me/proj3",
            "2026-06-01",
        );

        let snap = st.sessions_snapshot();
        assert_eq!(snap.len(), MAX_SESSIONS);
        // Waiting sorts ahead of running, both ahead of idle.
        assert_eq!(snap[0].st, "wait");
        assert_eq!(snap[1].st, "run");
        assert!(snap.iter().any(|s| s.st == "idle"));

        // Per-session telemetry landed on s3.
        let s3 = snap.iter().find(|s| s.id == "s3").unwrap();
        assert_eq!(s3.tok, 1234);
        assert_eq!(s3.m, "opus");
        assert_eq!(s3.cwd, "proj3");
        assert_eq!(s3.ctx, 10); // 100K / 1M (opus → wide window)
    }

    #[test]
    fn record_turn_still_counts_global_tokens_once() {
        let mut st = SessionState::default();
        st.record_turn("s1", 500, "claude-opus-4-8", 0, "", "2026-06-01");
        st.record_turn("s1", 500, "claude-opus-4-8", 0, "", "2026-06-01"); // dedupe
        assert_eq!(st.tokens, 500);
        st.record_turn("s1", 800, "claude-opus-4-8", 0, "", "2026-06-01");
        assert_eq!(st.tokens, 800);
    }

    #[test]
    fn entries_are_newest_first_and_capped() {
        let mut st = SessionState::default();
        for i in 0..12 {
            st.push_entry(format!("line {i}"));
        }
        assert_eq!(st.entries().len(), MAX_ENTRIES);
        assert_eq!(st.entries()[0], "line 11");
    }
}
