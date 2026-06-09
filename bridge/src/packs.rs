//! Animation-pack authoring: encode RGB565 frames into the `.spr` format the
//! firmware streams (see `firmware/src/sprite.{h,cpp}`), and lay out a per-agent
//! pack directory the device reads from its asset store (`/agents/<id>/`).
//!
//! Two delivery paths use this:
//!   * **Manual SD copy** (recommended for the CYD): write a pack dir here, copy
//!     `/agents/*` onto the card. No device round-trip.
//!   * **BLE push** (single-agent updates / the FNK0104, which has no card):
//!     the bytes here are streamed via the `CharBegin/File/Chunk/...` commands
//!     (`protocol.rs`) to the device's asset transfer.
//!
//! The `.spr` format is the in-flash clawd RLE generalized to a file: a 16-byte
//! header, a `frameCount+1` table of byte offsets, then concatenated frames of
//! `(u16 value, u16 count)` RLE pairs. Little-endian throughout.

/// `'BSP1'` little-endian — must match `SPR_MAGIC` in `sprite.cpp`.
pub const SPR_MAGIC: u32 = 0x3150_5342;
/// Default transparent key (matches `CLAWD_TRANSPARENT`); runs of it are skipped.
pub const SPR_TRANSPARENT: u16 = 0x18C5;

/// One animation frame: `w*h` RGB565 pixels, row-major.
pub struct Frame {
    pub w: u16,
    pub h: u16,
    pub pixels: Vec<u16>,
}

/// Encode frames (all the same dimensions) into a `.spr` byte stream.
/// `frame_ms` is per-frame duration (0 ⇒ the firmware's default); `loop_anim`
/// sets the loop flag. Returns an error if frames disagree on size or a frame's
/// pixel count is wrong.
pub fn encode_spr(
    frames: &[Frame],
    transparent: u16,
    frame_ms: u16,
    loop_anim: bool,
) -> anyhow::Result<Vec<u8>> {
    if frames.is_empty() {
        anyhow::bail!("a pack needs at least one frame");
    }
    let (w, h) = (frames[0].w, frames[0].h);
    if w == 0 || h == 0 {
        anyhow::bail!("frame dimensions must be non-zero");
    }
    let expect = w as usize * h as usize;
    for (i, f) in frames.iter().enumerate() {
        if f.w != w || f.h != h {
            anyhow::bail!("frame {i} size {}x{} != {w}x{h}", f.w, f.h);
        }
        if f.pixels.len() != expect {
            anyhow::bail!("frame {i} has {} pixels, expected {expect}", f.pixels.len());
        }
    }

    // RLE-encode each frame to a byte payload; record per-frame byte offsets.
    let mut payload: Vec<u8> = Vec::new();
    let mut offsets: Vec<u32> = Vec::with_capacity(frames.len() + 1);
    for f in frames {
        offsets.push(payload.len() as u32);
        rle_encode_frame(&f.pixels, &mut payload);
    }
    offsets.push(payload.len() as u32); // sentinel = total payload size

    let mut out = Vec::with_capacity(16 + offsets.len() * 4 + payload.len());
    out.extend_from_slice(&SPR_MAGIC.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&(frames.len() as u16).to_le_bytes());
    out.extend_from_slice(&transparent.to_le_bytes());
    out.extend_from_slice(&frame_ms.to_le_bytes());
    // fmtFlags: low byte = format (0 = RLE16), high byte = flags (bit0 loop).
    let flags: u8 = if loop_anim { 1 } else { 0 };
    let fmt_flags: u16 = (flags as u16) << 8; // format 0
    out.extend_from_slice(&fmt_flags.to_le_bytes());
    for o in &offsets {
        out.extend_from_slice(&o.to_le_bytes());
    }
    out.extend_from_slice(&payload);
    Ok(out)
}

/// RLE one frame: emit (value, count) u16 pairs, splitting runs over 65535.
fn rle_encode_frame(pixels: &[u16], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < pixels.len() {
        let v = pixels[i];
        let mut run = 1usize;
        while i + run < pixels.len() && pixels[i + run] == v && run < 0xFFFF {
            run += 1;
        }
        out.extend_from_slice(&v.to_le_bytes());
        out.extend_from_slice(&(run as u16).to_le_bytes());
        i += run;
    }
}

/// The seven persona-state filenames, in `PersonaState` order (firmware
/// `kStateNames`). A pack provides a `<name>.spr` for each state it animates.
pub const STATE_NAMES: [&str; 7] = [
    "sleep", "idle", "busy", "attention", "celebrate", "dizzy", "heart",
];

// ---------------------------------------------------------------------------
// Pack authoring: sprite-forge frame PNGs → `.spr`. Gated behind the `pack`
// feature (pulls in the `image` PNG decoder). Reads a sprite-forge pack dir
// (`packs/<id>/<state>/<state>-N.png` + `pipeline-meta.json`), resizes each
// state's frames to the device render box, quantizes RGBA→RGB565 (keying out
// transparency), and writes `<out>/<state>.spr` for each state present.
// ---------------------------------------------------------------------------
#[cfg(feature = "pack")]
mod build {
    use super::{encode_spr, Frame, STATE_NAMES, SPR_TRANSPARENT};
    use anyhow::{bail, Context, Result};
    use std::path::{Path, PathBuf};

    /// The firmware render box (sprite.cpp fits-and-centers within this, max 3×).
    /// We encode no larger than this so we never waste flash/RLE on pixels the
    /// device would only downscale.
    const BOX_W: u32 = 320;
    const BOX_H: u32 = 154;

    /// Options for [`build_pack`].
    pub struct BuildOpts {
        /// Target sprite height in pixels (aspect-preserved, clamped to the box).
        pub target_h: u16,
        /// Per-frame duration override. `None` ⇒ take it from each state's
        /// `pipeline-meta.json`, falling back to `0` (the firmware default).
        pub frame_ms: Option<u16>,
        /// Set the loop flag in each `.spr`.
        pub loop_anim: bool,
        /// Alpha below this maps to the transparent key (binary cut).
        pub alpha_threshold: u8,
        /// RGB565 transparent key (runs of it are skipped on render).
        pub transparent: u16,
    }

    impl Default for BuildOpts {
        fn default() -> Self {
            Self {
                target_h: 140,
                frame_ms: None,
                loop_anim: true,
                alpha_threshold: 128,
                transparent: SPR_TRANSPARENT,
            }
        }
    }

    /// Per-state summary returned to the CLI for printing + budget warnings.
    pub struct StateReport {
        pub state: &'static str,
        pub frames: usize,
        pub w: u16,
        pub h: u16,
        pub frame_ms: u16,
        pub bytes: usize,
    }

    /// Max *decoded* bytes per BLE chunk. The firmware's `xfer.h` chunk decode
    /// buffer is 300 bytes; a chunk that decodes larger is rejected. Stay at the
    /// cap so the push is as few round-trips as the device allows.
    pub const CHUNK_BYTES: usize = 300;

    /// Slice a `.spr` byte stream into base64 chunks, each decoding to at most
    /// [`CHUNK_BYTES`] bytes — ready to drop into `OutboundCmd::Chunk { d }`.
    pub fn spr_to_chunks(bytes: &[u8]) -> Vec<String> {
        use base64::Engine;
        bytes
            .chunks(CHUNK_BYTES)
            .map(|c| base64::engine::general_purpose::STANDARD.encode(c))
            .collect()
    }

    /// Quantize one RGBA pixel to RGB565, mapping near-transparent pixels to the
    /// transparent key and nudging any opaque pixel that would *collide* with the
    /// key (else real art would silently vanish on-device).
    pub fn rgba_to_rgb565_keyed(rgba: [u8; 4], transparent: u16, alpha_threshold: u8) -> u16 {
        if rgba[3] < alpha_threshold {
            return transparent;
        }
        // Round-to-nearest, not truncate, so colors don't skew dark.
        let r = (rgba[0] as u16 * 31 + 127) / 255;
        let g = (rgba[1] as u16 * 63 + 127) / 255;
        let b = (rgba[2] as u16 * 31 + 127) / 255;
        let v = (r << 11) | (g << 5) | b;
        if v == transparent {
            // Flip the low green bit — the smallest perceptual nudge — so this
            // opaque pixel is no longer read as the transparent key.
            v ^ 0x0020
        } else {
            v
        }
    }

    /// The frame PNGs for one state, in numeric order. Handles both engines'
    /// naming (`idle-1.png` and `idle-001.png`) and ignores the sheets/raw PNGs
    /// that also live in the state dir.
    fn state_frame_paths(state_dir: &Path, state: &str) -> Result<Vec<PathBuf>> {
        let prefix = format!("{state}-");
        let mut frames: Vec<(u32, PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(state_dir)
            .with_context(|| format!("reading {}", state_dir.display()))?
        {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".png") else {
                continue;
            };
            let Some(num) = stem.strip_prefix(&prefix) else {
                continue;
            };
            // Only `<state>-<digits>.png` — numeric, so it sorts -2 before -10.
            if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            if let Ok(n) = num.parse::<u32>() {
                frames.push((n, path));
            }
        }
        frames.sort_by_key(|(n, _)| *n);
        Ok(frames.into_iter().map(|(_, p)| p).collect())
    }

    /// Fit `(sw, sh)` to `target_h`, aspect-preserved, clamped to the render box.
    pub fn fit_dims(sw: u32, sh: u32, target_h: u16) -> (u32, u32) {
        let th = (target_h as u32).clamp(1, BOX_H);
        let mut w = ((sw as u64 * th as u64) / sh.max(1) as u64) as u32;
        let mut h = th;
        if w > BOX_W {
            // Too wide at that height — constrain by width instead.
            h = ((sh as u64 * BOX_W as u64) / sw.max(1) as u64) as u32;
            w = BOX_W;
        }
        (w.max(1), h.max(1))
    }

    /// Resize an RGBA image to `(tw, th)` (Lanczos3) and quantize to a `Frame`.
    pub fn frame_from_rgba(
        img: &image::RgbaImage,
        tw: u32,
        th: u32,
        transparent: u16,
        alpha_threshold: u8,
    ) -> Frame {
        let resized =
            image::imageops::resize(img, tw, th, image::imageops::FilterType::Lanczos3);
        let pixels = resized
            .pixels()
            .map(|p| rgba_to_rgb565_keyed(p.0, transparent, alpha_threshold))
            .collect();
        Frame {
            w: tw as u16,
            h: th as u16,
            pixels,
        }
    }

    /// Load + resize + quantize every frame of a state into uniform `Frame`s.
    /// All frames share one target size (derived from the first frame) so the
    /// character never pulses and `encode_spr` accepts them.
    fn load_state_frames(
        paths: &[PathBuf],
        target_h: u16,
        alpha_threshold: u8,
        transparent: u16,
    ) -> Result<Vec<Frame>> {
        let first = image::open(&paths[0])
            .with_context(|| format!("decoding {}", paths[0].display()))?
            .to_rgba8();
        let (tw, th) = fit_dims(first.width(), first.height(), target_h);
        let mut frames = Vec::with_capacity(paths.len());
        for path in paths {
            let img = image::open(path)
                .with_context(|| format!("decoding {}", path.display()))?
                .to_rgba8();
            frames.push(frame_from_rgba(&img, tw, th, transparent, alpha_threshold));
        }
        Ok(frames)
    }

    /// Per-frame duration (ms) from a state's `pipeline-meta.json`, if present.
    fn meta_frame_ms(state_dir: &Path) -> Option<u16> {
        let txt = std::fs::read_to_string(state_dir.join("pipeline-meta.json")).ok()?;
        let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
        v.get("duration_ms")
            .or_else(|| v.get("duration"))
            .and_then(|x| x.as_u64())
            .map(|x| x.min(u16::MAX as u64) as u16)
    }

    /// Build a sprite-forge pack dir into `<out_pack_dir>/<state>.spr` files.
    /// Skips states with no frames; errors only if the source has no states.
    pub fn build_pack(
        src_pack_dir: &Path,
        out_pack_dir: &Path,
        opts: &BuildOpts,
    ) -> Result<Vec<StateReport>> {
        std::fs::create_dir_all(out_pack_dir)
            .with_context(|| format!("creating {}", out_pack_dir.display()))?;
        let mut reports = Vec::new();
        for state in STATE_NAMES {
            let state_dir = src_pack_dir.join(state);
            if !state_dir.is_dir() {
                continue;
            }
            let paths = state_frame_paths(&state_dir, state)?;
            if paths.is_empty() {
                continue;
            }
            let frames =
                load_state_frames(&paths, opts.target_h, opts.alpha_threshold, opts.transparent)?;
            let frame_ms = opts
                .frame_ms
                .or_else(|| meta_frame_ms(&state_dir))
                .unwrap_or(0);
            let spr = encode_spr(&frames, opts.transparent, frame_ms, opts.loop_anim)
                .with_context(|| format!("encoding state '{state}'"))?;
            let out = out_pack_dir.join(format!("{state}.spr"));
            std::fs::write(&out, &spr).with_context(|| format!("writing {}", out.display()))?;
            reports.push(StateReport {
                state,
                frames: frames.len(),
                w: frames[0].w,
                h: frames[0].h,
                frame_ms,
                bytes: spr.len(),
            });
        }
        if reports.is_empty() {
            bail!(
                "no animation states found under {} — expected <state>/ dirs with \
                 <state>-N.png frames (run sprite-forge first)",
                src_pack_dir.display()
            );
        }
        Ok(reports)
    }
}

#[cfg(feature = "pack")]
pub use build::{
    build_pack, fit_dims, frame_from_rgba, rgba_to_rgb565_keyed, spr_to_chunks, BuildOpts,
    StateReport, CHUNK_BYTES,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a `.spr` (mirror of `sprite.cpp`) so the test proves round-trip.
    fn decode_first_frame(spr: &[u8]) -> (u16, u16, Vec<u16>) {
        let u16le = |o: usize| u16::from_le_bytes([spr[o], spr[o + 1]]);
        let u32le = |o: usize| {
            u32::from_le_bytes([spr[o], spr[o + 1], spr[o + 2], spr[o + 3]])
        };
        assert_eq!(u32le(0), SPR_MAGIC);
        let w = u16le(4);
        let h = u16le(6);
        let frames = u16le(8);
        let payload_start = 16 + (frames as usize + 1) * 4;
        let off0 = u32le(16) as usize;
        let off1 = u32le(20) as usize;
        let mut px = Vec::new();
        let (mut i, end) = (payload_start + off0, payload_start + off1);
        while i + 4 <= end {
            let v = u16le(i);
            let c = u16le(i + 2);
            i += 4;
            for _ in 0..c {
                px.push(v);
            }
        }
        (w, h, px)
    }

    #[test]
    fn spr_round_trips() {
        // 2x2, two frames; mixed runs incl. a transparent run.
        let f0 = Frame { w: 2, h: 2, pixels: vec![0xF800, 0xF800, 0x18C5, 0x07E0] };
        let f1 = Frame { w: 2, h: 2, pixels: vec![0x001F, 0x001F, 0x001F, 0x001F] };
        let spr = encode_spr(&[f0, f1], SPR_TRANSPARENT, 100, true).unwrap();
        let (w, h, px) = decode_first_frame(&spr);
        assert_eq!((w, h), (2, 2));
        assert_eq!(px, vec![0xF800, 0xF800, 0x18C5, 0x07E0]);
        // Header fields land where sprite.cpp reads them.
        assert_eq!(u16::from_le_bytes([spr[8], spr[9]]), 2); // frameCount
        assert_eq!(u16::from_le_bytes([spr[10], spr[11]]), SPR_TRANSPARENT);
        assert_eq!(u16::from_le_bytes([spr[12], spr[13]]), 100); // frameMs
        assert_eq!(spr[15], 1); // flags high byte = loop
    }

    #[test]
    fn rejects_bad_frames() {
        assert!(encode_spr(&[], 0, 0, false).is_err());
        let bad = Frame { w: 2, h: 2, pixels: vec![0; 3] };
        assert!(encode_spr(&[bad], 0, 0, false).is_err());
    }

    #[test]
    fn solid_frame_is_one_run() {
        // A flat 4x4 encodes to a single (value,count=16) pair → 4 payload bytes.
        let f = Frame { w: 4, h: 4, pixels: vec![0x1234; 16] };
        let spr = encode_spr(&[f], 0, 0, false).unwrap();
        let payload_start = 16 + 2 * 4;
        assert_eq!(spr.len(), payload_start + 4);
    }

    #[cfg(feature = "pack")]
    mod pack_build {
        use super::super::*;

        #[test]
        fn quantizes_primaries() {
            let q = |rgba| rgba_to_rgb565_keyed(rgba, SPR_TRANSPARENT, 128);
            assert_eq!(q([255, 255, 255, 255]), 0xFFFF); // white
            assert_eq!(q([255, 0, 0, 255]), 0xF800); // pure red
            assert_eq!(q([0, 255, 0, 255]), 0x07E0); // pure green
            assert_eq!(q([0, 0, 255, 255]), 0x001F); // pure blue
            assert_eq!(q([0, 0, 0, 255]), 0x0000); // black
        }

        #[test]
        fn low_alpha_becomes_key() {
            // Below threshold → key, regardless of color.
            assert_eq!(rgba_to_rgb565_keyed([200, 30, 90, 0], SPR_TRANSPARENT, 128), SPR_TRANSPARENT);
            assert_eq!(rgba_to_rgb565_keyed([200, 30, 90, 127], SPR_TRANSPARENT, 128), SPR_TRANSPARENT);
            // At/above threshold → real color (not the key).
            assert_ne!(rgba_to_rgb565_keyed([200, 30, 90, 128], SPR_TRANSPARENT, 128), SPR_TRANSPARENT);
        }

        #[test]
        fn opaque_pixel_never_collides_with_key() {
            // Pick the exact RGB888 that quantizes back to the key value, then
            // assert an opaque pixel of it is nudged off the key.
            let r = ((SPR_TRANSPARENT >> 11) & 0x1F) as u32;
            let g = ((SPR_TRANSPARENT >> 5) & 0x3F) as u32;
            let b = (SPR_TRANSPARENT & 0x1F) as u32;
            let rgba = [
                (r * 255 / 31) as u8,
                (g * 255 / 63) as u8,
                (b * 255 / 31) as u8,
                255,
            ];
            // Sanity: this really would land on the key without the nudge.
            let raw = (((rgba[0] as u16 * 31 + 127) / 255) << 11)
                | (((rgba[1] as u16 * 63 + 127) / 255) << 5)
                | ((rgba[2] as u16 * 31 + 127) / 255);
            assert_eq!(raw, SPR_TRANSPARENT, "test setup: should hit the key raw");
            assert_ne!(rgba_to_rgb565_keyed(rgba, SPR_TRANSPARENT, 128), SPR_TRANSPARENT);
        }

        #[test]
        fn fit_dims_preserves_aspect_and_clamps() {
            // 256×256 → height 140 keeps it square.
            assert_eq!(fit_dims(256, 256, 140), (140, 140));
            // Very wide 800×100 at height 140 would be 1120px wide → clamp to 320.
            let (w, h) = fit_dims(800, 100, 140);
            assert_eq!(w, 320);
            assert!(h <= 154 && h < 140);
            // Target above the box height is clamped to the box.
            assert_eq!(fit_dims(100, 200, 250).1, 154);
        }

        #[test]
        fn chunks_decode_under_cap_and_reconstruct() {
            use base64::Engine;
            // A payload that isn't a multiple of the chunk size (tests the tail).
            let bytes: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
            let chunks = spr_to_chunks(&bytes);
            let mut rebuilt = Vec::new();
            for c in &chunks {
                let decoded = base64::engine::general_purpose::STANDARD.decode(c).unwrap();
                assert!(decoded.len() <= CHUNK_BYTES, "chunk decodes to {} > cap", decoded.len());
                rebuilt.extend_from_slice(&decoded);
            }
            assert_eq!(rebuilt, bytes);
            // 1000 / 300 = 4 chunks (300,300,300,100).
            assert_eq!(chunks.len(), 4);
        }

        #[test]
        fn rgba_frame_round_trips_through_spr() {
            // A 4×2 RGBA image with a transparent column → encode → decode and
            // confirm the pixels (incl. the key) survive intact.
            let mut img = image::RgbaImage::new(4, 2);
            img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255])); // red
            img.put_pixel(1, 0, image::Rgba([0, 0, 0, 0])); // transparent → key
            img.put_pixel(2, 0, image::Rgba([255, 255, 255, 255])); // white
            img.put_pixel(3, 0, image::Rgba([0, 0, 255, 255])); // blue
            for x in 0..4 {
                img.put_pixel(x, 1, image::Rgba([0, 255, 0, 255])); // green row
            }
            // No resize (target matches source height) → exact pixels.
            let frame = frame_from_rgba(&img, 4, 2, SPR_TRANSPARENT, 128);
            let spr = encode_spr(&[frame], SPR_TRANSPARENT, 100, true).unwrap();
            let (w, h, px) = super::decode_first_frame(&spr);
            assert_eq!((w, h), (4, 2));
            assert_eq!(
                px,
                vec![
                    0xF800, SPR_TRANSPARENT, 0xFFFF, 0x001F, // row 0
                    0x07E0, 0x07E0, 0x07E0, 0x07E0, // row 1
                ]
            );
        }
    }
}
