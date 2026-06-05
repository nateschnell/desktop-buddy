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
}
