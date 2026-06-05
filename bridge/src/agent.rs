//! Agent profiles — the single source of truth for which AI coding harness the
//! bridge is driving.
//!
//! A profile is a declarative description of one harness (Claude Code, Codex,
//! Gemini, …): how to ingest its hook/log events, how to install its hooks, what
//! palette + info panels the device should show for it, and which animation pack
//! it uses. Profiles are *data*, so adding or updating a harness is a JSON edit
//! shipped via a release — not a recompile. New Rust is needed only for a
//! genuinely novel hook-config format, stdin format, or ingestion mechanism.
//!
//! The reference vocabulary (event→state maps, capabilities, config-format names,
//! the normalized state set) mirrors the open `clawd-on-desk` project's *spec*; the
//! artwork and profile JSON shipped here are our own.
//!
//! Layering (merged at load): embedded defaults (compiled in, infallible) <
//! release bundle (`config_dir()/agents/_bundled/`) < user overrides
//! (`config_dir()/agents/`). Highest `revision` wins per id; a user file wins ties.

use crate::state::config_dir;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Color — JSON as "#RRGGBB", on the wire as RGB565.
// ---------------------------------------------------------------------------

/// An sRGB color. Serialized as a `#RRGGBB` hex string for hand-editability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Color { r, g, b }
    }

    /// Pack into a 16-bit RGB565 value — the format the device draws in.
    pub fn rgb565(self) -> u16 {
        let r = (self.r as u16 >> 3) & 0x1f;
        let g = (self.g as u16 >> 2) & 0x3f;
        let b = (self.b as u16 >> 3) & 0x1f;
        (r << 11) | (g << 5) | b
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b))
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let h = s.trim().trim_start_matches('#');
        if h.len() != 6 {
            return Err(serde::de::Error::custom(format!("bad color {s:?} (want #RRGGBB)")));
        }
        let parse = |i: usize| u8::from_str_radix(&h[i..i + 2], 16);
        match (parse(0), parse(2), parse(4)) {
            (Ok(r), Ok(g), Ok(b)) => Ok(Color { r, g, b }),
            _ => Err(serde::de::Error::custom(format!("bad color {s:?}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Palette — the device theme. 5 base colors (firmware `Palette`) + 4 accents.
// ---------------------------------------------------------------------------

/// The device theme colors. The first five map 1:1 onto the firmware `Palette`
/// struct (body/bg/text/textDim/ink); the four accents replace the scattered
/// `HOT`/`PANEL`/`SELBAR`/`GREEN` constants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Palette {
    pub body: Color,
    pub bg: Color,
    pub text: Color,
    pub text_dim: Color,
    pub ink: Color,
    #[serde(default = "default_hot")]
    pub hot: Color,
    #[serde(default = "default_panel")]
    pub panel: Color,
    #[serde(default = "default_sel")]
    pub sel: Color,
    #[serde(default = "default_ok")]
    pub ok: Color,
}

// Firmware defaults (RGB565 0xFA20 / 0x2104 / 0x4208 / TFT_GREEN) re-expanded to
// the nearest sRGB so an un-customized profile reproduces today's exact look.
fn default_hot() -> Color {
    Color::new(0xFF, 0x44, 0x00) // → RGB565 0xFA20, the firmware's historical HOT
}
fn default_panel() -> Color {
    Color::new(0x20, 0x20, 0x20)
}
fn default_sel() -> Color {
    Color::new(0x40, 0x40, 0x40)
}
fn default_ok() -> Color {
    Color::new(0x00, 0xFF, 0x00)
}

impl Palette {
    /// The five base colors as RGB565, in firmware `Palette` field order.
    pub fn base_rgb565(&self) -> [u16; 5] {
        [
            self.body.rgb565(),
            self.bg.rgb565(),
            self.text.rgb565(),
            self.text_dim.rgb565(),
            self.ink.rgb565(),
        ]
    }
}

// ---------------------------------------------------------------------------
// Normalized state vocabulary — every harness's events collapse to these.
// ---------------------------------------------------------------------------

/// The state vocabulary shared across all harnesses. The bridge maps each into
/// the device's `(running, waiting, completed, prompt?)` model via `state_map`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum NormState {
    Idle,
    Sleeping,
    Thinking,
    Working,
    Error,
    Attention,
    Notification,
    Juggling,
    Sweeping,
    Carrying,
    CodexTurnEnd,
}

// ---------------------------------------------------------------------------
// Ingestion + install descriptors.
// ---------------------------------------------------------------------------

/// Which generic ingestion mechanism the daemon uses for this harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventSource {
    /// Hooks invoke `agent-buddy hook <EVENT> --agent <id>` over stdin.
    Hook,
    /// Hooks plus a JSONL session-log tail (Codex).
    HookLogPoll,
    /// A plugin POSTs events to our loopback HTTP listener.
    PluginEvent,
    /// An editor/agent extension POSTs events to our loopback HTTP listener.
    Extension,
}

/// The on-disk hook-config format a harness expects. The `setup` writers and the
/// hook stdin extractors key off this. `Unknown` keeps a future format parseable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigFormat {
    ClaudeCodeCompatible,
    CodexHooksJson,
    GeminiSettingsJson,
    CursorHooksJson,
    QwenSettingsJson,
    KiroAgentJson,
    AntigravityHooksJson,
    UserGlobalHooksJson,
    KimiToml,
    OpencodePlugin,
    PiExtension,
    OpenclawPlugin,
    HermesPlugin,
    #[serde(other)]
    Unknown,
}

/// How the harness shapes the JSON it hands a hook on stdin. Selects the field
/// extractor in `ingest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StdinFormat {
    PascalCase,
    CamelCase,
    CodexHookJson,
    GeminiHookJson,
    AntigravityHookJson,
    CursorHookJson,
    QwenHookJson,
    ClaudeCodeHookJson,
    #[serde(other)]
    Unknown,
}

/// Where a harness's hook config lives, resolved at install time. A path is
/// relative to `$HOME` unless `from_config_dir` is set (then relative to our own
/// config dir, for plugin manifests we own).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetPath {
    /// Path relative to `$HOME` (e.g. `.codex/hooks.json`).
    pub home_rel: String,
    #[serde(default)]
    pub from_config_dir: bool,
    /// Optional env var naming an override base dir (e.g. `"COPILOT_HOME"`).
    /// When set and non-empty, it replaces `$HOME` + the leading config segment
    /// of `home_rel` (e.g. `.copilot/`). Opt-in and harness-agnostic.
    #[serde(default)]
    pub home_env: Option<String>,
}

/// How to install this harness's hooks so it talks to us.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallRecipe {
    pub config_format: ConfigFormat,
    pub target_path: TargetPath,
    pub stdin_format: StdinFormat,
    /// Tool matcher for formats that gate per-tool (Claude `PreToolUse` etc.).
    #[serde(default)]
    pub matcher: Option<String>,
    /// The harness event names we register hooks for (only those it emits).
    #[serde(default)]
    pub events: Vec<String>,
}

/// JSONL session-log polling config (present iff `event_source = hook-log-poll`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    /// Directory holding session logs, `~`-prefixed (e.g. `~/.codex/sessions`).
    pub session_dir: String,
    /// Glob for the rolling log files (e.g. `rollout-*.jsonl`).
    pub file_pattern: String,
    pub poll_interval_ms: u64,
}

/// Per-OS process names, kept for future auto-detect of the running harness.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessNames {
    #[serde(default)]
    pub win: Vec<String>,
    #[serde(default)]
    pub mac: Vec<String>,
    #[serde(default)]
    pub linux: Vec<String>,
}

/// Harness capabilities — drives capability-gated UI (e.g. approve/deny only for
/// `permission_approval`) and is forwarded to the device as a bitmask.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub http_hook: bool,
    #[serde(default)]
    pub permission_approval: bool,
    #[serde(default)]
    pub notification_hook: bool,
    #[serde(default)]
    pub interactive_bubble: bool,
    #[serde(default)]
    pub session_end: bool,
    #[serde(default)]
    pub subagent: bool,
}

pub const CAP_HTTP_HOOK: u32 = 1 << 0;
pub const CAP_PERMISSION: u32 = 1 << 1;
pub const CAP_NOTIFICATION: u32 = 1 << 2;
pub const CAP_INTERACTIVE: u32 = 1 << 3;
pub const CAP_SESSION_END: u32 = 1 << 4;
pub const CAP_SUBAGENT: u32 = 1 << 5;

impl Capabilities {
    pub fn bits(&self) -> u32 {
        let mut b = 0;
        if self.http_hook {
            b |= CAP_HTTP_HOOK;
        }
        if self.permission_approval {
            b |= CAP_PERMISSION;
        }
        if self.notification_hook {
            b |= CAP_NOTIFICATION;
        }
        if self.interactive_bubble {
            b |= CAP_INTERACTIVE;
        }
        if self.session_end {
            b |= CAP_SESSION_END;
        }
        if self.subagent {
            b |= CAP_SUBAGENT;
        }
        b
    }
}

// Info-panel bits — which dashboards the device shows for this agent.
pub const PANEL_TOKENS: u32 = 1 << 0;
pub const PANEL_MODEL: u32 = 1 << 1;
pub const PANEL_SESSIONS: u32 = 1 << 2;
pub const PANEL_SUBAGENTS: u32 = 1 << 3;
pub const PANEL_CONTEXT: u32 = 1 << 4;
pub const PANEL_COST: u32 = 1 << 5;

/// Map a panel name to its wire bit. Unknown names are ignored (0).
fn panel_bit(name: &str) -> u32 {
    match name {
        "tokens" => PANEL_TOKENS,
        "model" => PANEL_MODEL,
        "sessions" => PANEL_SESSIONS,
        "subagents" => PANEL_SUBAGENTS,
        "context" => PANEL_CONTEXT,
        "cost" => PANEL_COST,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// The profile.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub process_names: ProcessNames,
    pub event_source: EventSource,
    /// Harness hook-event name → normalized state.
    #[serde(default)]
    pub event_map: HashMap<String, NormState>,
    /// JSONL log event key → normalized state (log-poll harnesses).
    #[serde(default)]
    pub log_event_map: HashMap<String, NormState>,
    #[serde(default)]
    pub capabilities: Capabilities,
    pub install: InstallRecipe,
    #[serde(default)]
    pub log_config: Option<LogConfig>,
    pub palette: Palette,
    /// Info-panel names the device should show (see `panel_bit`).
    #[serde(default)]
    pub panels: Vec<String>,
    /// Animation-pack id (the device's `/agents/<pack>` folder). Defaults to `id`.
    #[serde(default)]
    pub animation_pack: Option<String>,
    /// Monotonic profile revision; the highest across layers wins per id.
    #[serde(default)]
    pub revision: u32,
}

impl AgentProfile {
    /// The animation-pack id (falls back to the agent id).
    pub fn pack(&self) -> &str {
        self.animation_pack.as_deref().unwrap_or(&self.id)
    }

    /// The info-panel bitmask for the wire.
    pub fn panel_bits(&self) -> u32 {
        self.panels.iter().map(|p| panel_bit(p)).fold(0, |a, b| a | b)
    }

    /// Whether this build can actually install + drive this harness. Every
    /// shipped profile is installable (hook-config writers + bundled plugins for
    /// the plugin/extension harnesses), so only an `Unknown` config format — a
    /// future format a newer release profile names that this build doesn't
    /// understand — is hidden from the selector.
    pub fn supported(&self) -> bool {
        !matches!(self.install.config_format, ConfigFormat::Unknown)
    }
}

// ---------------------------------------------------------------------------
// State map — how each normalized state lands on the device's telemetry model.
// ---------------------------------------------------------------------------

/// What a normalized state does to a session's device-facing telemetry. The
/// device renders `(running, waiting, completed, prompt?)`; this collapses the
/// richer normalized vocabulary onto it. Shipped as data so it's tunable without
/// a recompile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateEffect {
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub waiting: bool,
    #[serde(default)]
    pub completed: bool,
    /// This state carries a pending permission prompt.
    #[serde(default)]
    pub prompt: bool,
    /// Leave running/waiting untouched (e.g. a transient notification).
    #[serde(default)]
    pub keep_state: bool,
    /// Short status line for the device.
    #[serde(default)]
    pub msg: String,
}

pub type StateMap = HashMap<NormState, StateEffect>;

/// The compiled-in default normalized-state → device-telemetry table.
pub const DEFAULT_STATE_MAP: &str = include_str!("../assets/state_map.json");

/// Load the state map, embedded default overlaid by an optional
/// `config_dir()/state_map.json` (release- or user-supplied). A malformed
/// override is ignored in favor of the embedded default.
pub fn load_state_map() -> StateMap {
    let mut map: StateMap = serde_json::from_str(DEFAULT_STATE_MAP)
        .expect("embedded state_map.json is malformed (build bug)");
    if let Ok(dir) = config_dir() {
        if let Ok(bytes) = std::fs::read(dir.join("state_map.json")) {
            match serde_json::from_slice::<StateMap>(&bytes) {
                Ok(over) => map.extend(over),
                Err(e) => tracing::warn!("ignoring malformed state_map.json override: {e}"),
            }
        }
    }
    map
}

/// A lightweight id+name pair for the app's agent selector / `StatusReport`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
}

// ---------------------------------------------------------------------------
// Embedded defaults + layered load.
// ---------------------------------------------------------------------------

/// Compiled-in default profiles. Every supported harness ships here so a fresh
/// install works offline with no files on disk. (id, json) pairs.
pub const DEFAULT_PROFILES: &[(&str, &str)] = &[
    ("claude-code", include_str!("../assets/agents/claude-code.json")),
    ("codex", include_str!("../assets/agents/codex.json")),
    ("gemini-cli", include_str!("../assets/agents/gemini-cli.json")),
    ("copilot-cli", include_str!("../assets/agents/copilot-cli.json")),
    ("cursor-agent", include_str!("../assets/agents/cursor-agent.json")),
    ("kiro-cli", include_str!("../assets/agents/kiro-cli.json")),
    ("kimi-cli", include_str!("../assets/agents/kimi-cli.json")),
    ("qwen-code", include_str!("../assets/agents/qwen-code.json")),
    ("opencode", include_str!("../assets/agents/opencode.json")),
    ("antigravity-cli", include_str!("../assets/agents/antigravity-cli.json")),
    ("pi", include_str!("../assets/agents/pi.json")),
    ("openclaw", include_str!("../assets/agents/openclaw.json")),
    ("hermes", include_str!("../assets/agents/hermes.json")),
    ("codebuddy", include_str!("../assets/agents/codebuddy.json")),
];

/// The default active agent id (preconfigured out of the box).
pub const DEFAULT_AGENT: &str = "claude-code";

/// Parse and validate every embedded profile, returning them keyed by id. Panics
/// on a malformed embedded profile — they are compiled in and unit-tested, so a
/// failure here is a build-time bug, not a runtime condition.
fn embedded_profiles() -> HashMap<String, AgentProfile> {
    let mut out = HashMap::new();
    for (id, json) in DEFAULT_PROFILES {
        let p: AgentProfile = serde_json::from_str(json)
            .unwrap_or_else(|e| panic!("embedded profile {id} is malformed: {e}"));
        out.insert(p.id.clone(), p);
    }
    out
}

/// Overlay on-disk profiles from `dir` onto `into`, keeping whichever has the
/// higher `revision` (ties: the incoming on-disk file wins, so a hand-edit
/// sticks). A malformed file is skipped and logged — a bad release asset must
/// never brick agent selection.
fn overlay_dir(dir: &std::path::Path, into: &mut HashMap<String, AgentProfile>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return, // missing dir is normal
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("skipping agent profile {}: {e}", path.display());
                continue;
            }
        };
        match serde_json::from_slice::<AgentProfile>(&bytes) {
            Ok(p) => {
                let keep = into
                    .get(&p.id)
                    .map(|cur| p.revision >= cur.revision)
                    .unwrap_or(true);
                if keep {
                    into.insert(p.id.clone(), p);
                }
            }
            Err(e) => tracing::warn!("skipping malformed agent profile {}: {e}", path.display()),
        }
    }
}

/// Load all profiles, merging embedded defaults < release bundle < user overrides.
pub fn load_profiles() -> HashMap<String, AgentProfile> {
    let mut profiles = embedded_profiles();
    if let Ok(dir) = config_dir() {
        overlay_dir(&dir.join("agents").join("_bundled"), &mut profiles);
        overlay_dir(&dir.join("agents"), &mut profiles);
    }
    profiles
}

/// Sorted id+name summaries for the app selector. Active/default ("claude-code")
/// is not special-cased here; sort is by display name. Only `supported()`
/// harnesses are listed — agents whose integration isn't shippable yet (plugin
/// harnesses with no bundled plugin) are hidden so the user is never offered a
/// selection that would install nothing.
pub fn list_agents(profiles: &HashMap<String, AgentProfile>) -> Vec<AgentSummary> {
    let mut v: Vec<AgentSummary> = profiles
        .values()
        .filter(|p| p.supported())
        .map(|p| AgentSummary {
            id: p.id.clone(),
            name: p.name.clone(),
        })
        .collect();
    v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_profile_parses() {
        let p = embedded_profiles();
        assert_eq!(p.len(), DEFAULT_PROFILES.len());
        assert!(p.contains_key(DEFAULT_AGENT));
        // Each profile's id must match its registry key.
        for (id, _) in DEFAULT_PROFILES {
            assert!(p.contains_key(*id), "missing embedded profile {id}");
        }
    }

    #[test]
    fn color_hex_roundtrips_and_packs() {
        let c: Color = serde_json::from_str("\"#D97757\"").unwrap();
        assert_eq!(c, Color::new(0xD9, 0x77, 0x57));
        assert_eq!(serde_json::to_string(&c).unwrap(), "\"#D97757\"");
        // Pure red/green/blue land on the RGB565 extremes.
        assert_eq!(Color::new(255, 0, 0).rgb565(), 0xF800);
        assert_eq!(Color::new(0, 255, 0).rgb565(), 0x07E0);
        assert_eq!(Color::new(0, 0, 255).rgb565(), 0x001F);
        assert_eq!(Color::new(0, 0, 0).rgb565(), 0x0000);
        assert_eq!(Color::new(255, 255, 255).rgb565(), 0xFFFF);
    }

    #[test]
    fn bad_color_is_rejected() {
        assert!(serde_json::from_str::<Color>("\"#xyz\"").is_err());
        assert!(serde_json::from_str::<Color>("\"D97757\"").is_ok()); // '#' optional
        assert!(serde_json::from_str::<Color>("\"#12\"").is_err());
    }

    #[test]
    fn panel_and_cap_bits() {
        let p = load_profiles();
        let cc = &p["claude-code"];
        // Claude Code advertises permission + subagent + notification.
        let caps = cc.capabilities.bits();
        assert!(caps & CAP_PERMISSION != 0);
        assert!(caps & CAP_SUBAGENT != 0);
        // It shows the token + session panels at least.
        assert!(cc.panel_bits() & PANEL_TOKENS != 0);
    }

    #[test]
    fn default_accents_match_historical_firmware_constants() {
        // An un-customized profile must reproduce the firmware's old colors
        // exactly, so pushing the default agent's theme is a visual no-op.
        assert_eq!(default_hot().rgb565(), 0xFA20);
        assert_eq!(default_panel().rgb565(), 0x2104);
        assert_eq!(default_sel().rgb565(), 0x4208);
        assert_eq!(default_ok().rgb565(), 0x07E0);
        // Claude Code's base palette equals the firmware default Palette.
        let cc = &load_profiles()["claude-code"];
        assert_eq!(cc.palette.base_rgb565(), [0xC2A6, 0x0000, 0xFFFF, 0x8410, 0x0000]);
    }

    #[test]
    fn unknown_config_format_parses() {
        let v: ConfigFormat = serde_json::from_str("\"some-future-format\"").unwrap();
        assert_eq!(v, ConfigFormat::Unknown);
    }

    #[test]
    fn pack_defaults_to_id() {
        let p = load_profiles();
        assert_eq!(p["codex"].pack(), "codex");
    }

    #[test]
    fn every_shipped_agent_is_selectable() {
        // All 14 bundled profiles use a known, installable config format
        // (hook-config writers or a bundled plugin), so none are hidden.
        let p = load_profiles();
        for (id, prof) in &p {
            assert!(prof.supported(), "{id} should be selectable");
        }
        let listed = list_agents(&p);
        assert_eq!(listed.len(), p.len(), "every agent should appear in the selector");
        for id in ["claude-code", "opencode", "pi", "openclaw", "hermes"] {
            assert!(listed.iter().any(|a| a.id == id), "{id} missing from the selector");
        }
    }

    #[test]
    fn plugin_harnesses_are_non_gating_in_v1() {
        // The bundled plugins forward lifecycle state only — there's no reverse
        // permission bridge — so the plugin/extension harnesses must NOT advertise
        // permission_approval, or the device would show a tap-to-approve prompt
        // whose decision can never be delivered back (it would just TTL out).
        use ConfigFormat::*;
        for p in load_profiles().values() {
            if matches!(
                p.install.config_format,
                OpencodePlugin | PiExtension | OpenclawPlugin | HermesPlugin
            ) {
                assert!(
                    !p.capabilities.permission_approval,
                    "{} must be non-gating in v1 (no reverse permission bridge)",
                    p.id
                );
            }
        }
    }

    #[test]
    fn permission_approval_implies_a_reachable_gate() {
        // A profile that advertises permission_approval must be able to actually
        // gate: it has to map a PermissionRequest event to Attention (the only
        // NormState that builds a HookEvent::PermissionRequest). Otherwise the
        // device would show a tap-to-approve prompt whose verdict can't be
        // delivered (it would just TTL out). After the v1 caps reconciliation
        // only claude-code, qwen-code, codebuddy keep the capability.
        for p in load_profiles().values() {
            if p.capabilities.permission_approval {
                let gates = p.event_map.get("PermissionRequest") == Some(&NormState::Attention)
                    // Claude Code's gate is the hardcoded build_event path, which
                    // emits PermissionRequest directly (its PreToolUse → working).
                    || p.id == "claude-code";
                assert!(
                    gates,
                    "{} advertises permission_approval but has no reachable gate",
                    p.id
                );
            }
        }
    }

    #[test]
    fn state_map_covers_every_norm_state() {
        let m = load_state_map();
        use NormState::*;
        for s in [
            Idle, Sleeping, Thinking, Working, Error, Attention, Notification, Juggling, Sweeping,
            Carrying, CodexTurnEnd,
        ] {
            assert!(m.contains_key(&s), "state_map missing {s:?}");
        }
        assert!(m[&NormState::Attention].prompt);
        assert!(m[&NormState::Thinking].running);
        assert!(m[&NormState::CodexTurnEnd].completed);
        assert!(m[&NormState::Notification].keep_state);
    }

    #[test]
    fn every_profile_event_map_is_non_empty_and_maps_to_known_states() {
        // Each profile must declare at least one event mapping; deserialization
        // already validated the NormState values, so this guards authoring gaps.
        let p = load_profiles();
        for (id, prof) in &p {
            assert!(!prof.event_map.is_empty(), "{id} has an empty event_map");
        }
        // Codex carries a log_event_map for its JSONL tail.
        assert!(!p["codex"].log_event_map.is_empty());
        assert!(p["codex"].log_config.is_some());
    }
}
