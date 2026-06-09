//! Bundled default animation pack — frame PNGs `include_bytes!`'d so the widget
//! always has real art even with an empty config dir (the procedural fallback in
//! `widget.rs` covers anything this doesn't).
//!
//! Curated from a sprite-forge pack into `bridge/assets/widget/<state>/`
//! (claude-code, 4 frames/state @ 200ms). `frames(state)` returns
//! `(&[frame_bytes], frame_ms)` or `None` (→ procedural). Paths are relative to
//! this file (`src/`).

/// One bundled state's frames + per-frame duration.
type State = (&'static [&'static [u8]], u32);

/// Build a `&[&[u8]]` of `include_bytes!`'d frames for one state.
macro_rules! frames {
    ($($f:literal),+ $(,)?) => {
        &[$(include_bytes!($f) as &[u8]),+]
    };
}

/// Per-frame duration of the bundled pack (claude-code `pipeline-meta.json`).
const MS: u32 = 200;

pub fn frames(state: &str) -> Option<State> {
    Some(match state {
        "sleep" => (
            frames![
                "../assets/widget/sleep/sleep-1.png",
                "../assets/widget/sleep/sleep-2.png",
                "../assets/widget/sleep/sleep-3.png",
                "../assets/widget/sleep/sleep-4.png",
            ],
            MS,
        ),
        "idle" => (
            frames![
                "../assets/widget/idle/idle-1.png",
                "../assets/widget/idle/idle-2.png",
                "../assets/widget/idle/idle-3.png",
                "../assets/widget/idle/idle-4.png",
            ],
            MS,
        ),
        "busy" => (
            frames![
                "../assets/widget/busy/busy-1.png",
                "../assets/widget/busy/busy-2.png",
                "../assets/widget/busy/busy-3.png",
                "../assets/widget/busy/busy-4.png",
            ],
            MS,
        ),
        "attention" => (
            frames![
                "../assets/widget/attention/attention-1.png",
                "../assets/widget/attention/attention-2.png",
                "../assets/widget/attention/attention-3.png",
                "../assets/widget/attention/attention-4.png",
            ],
            MS,
        ),
        "celebrate" => (
            frames![
                "../assets/widget/celebrate/celebrate-1.png",
                "../assets/widget/celebrate/celebrate-2.png",
                "../assets/widget/celebrate/celebrate-3.png",
                "../assets/widget/celebrate/celebrate-4.png",
            ],
            MS,
        ),
        "dizzy" => (
            frames![
                "../assets/widget/dizzy/dizzy-1.png",
                "../assets/widget/dizzy/dizzy-2.png",
                "../assets/widget/dizzy/dizzy-3.png",
                "../assets/widget/dizzy/dizzy-4.png",
            ],
            MS,
        ),
        "heart" => (
            frames![
                "../assets/widget/heart/heart-1.png",
                "../assets/widget/heart/heart-2.png",
                "../assets/widget/heart/heart-3.png",
                "../assets/widget/heart/heart-4.png",
            ],
            MS,
        ),
        _ => return None,
    })
}
