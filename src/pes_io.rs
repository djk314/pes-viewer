//! PES export for versions 1 to 9, plus version detection for import.
//!
//! Layout follows the libpes wiki (https://github.com/frno7/libpes/wiki/PES-format):
//!   "#PESxxxx" | u32 absolute PEC offset | version specific PES header | PEC block
//!
//! * The PEC block (what Brother machines actually sew) is identical for every version.
//! * PES headers are written as documented for v1, v2, v3, v4, v5 and v6.
//! * Versions 7, 8 and 9 are not described by those references. They are written with
//!   their own signature (#PES0070 / 0080 / 0090) and the v6 header layout; machines read
//!   only the PEC block, so these files are sewable, but PE-Design may treat them as
//!   machine-only files.
//! * The PES section contains no editable objects (CSewSeg / CEmbOne): the stitch data
//!   lives in the PEC block.
//!
//! Import needs no per-version code: every version stores the PEC offset at byte 8, which
//! `parse_pes` in main.rs follows.

use crate::{build_design, encode_pec_stitches, merge_layers_data, render_1bit_thumbnail, thread_rgb};
use crate::optimize::{self, OptimizeOptions, Report};
use crate::{EmbroideryLayer, Stitch, PEC_HEADER_LEN};
use std::fs;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PesVersion {
    V1,
    V2,
    V3,
    V4,
    V5,
    V6,
    V7,
    V8,
    V9,
}

impl PesVersion {
    pub const ALL: [PesVersion; 9] = [
        PesVersion::V1,
        PesVersion::V2,
        PesVersion::V3,
        PesVersion::V4,
        PesVersion::V5,
        PesVersion::V6,
        PesVersion::V7,
        PesVersion::V8,
        PesVersion::V9,
    ];

    pub fn number(self) -> u8 {
        self as u8 + 1
    }

    pub fn magic(self) -> &'static [u8; 8] {
        match self {
            PesVersion::V1 => b"#PES0001",
            PesVersion::V2 => b"#PES0020",
            PesVersion::V3 => b"#PES0030",
            PesVersion::V4 => b"#PES0040",
            PesVersion::V5 => b"#PES0050",
            PesVersion::V6 => b"#PES0060",
            PesVersion::V7 => b"#PES0070",
            PesVersion::V8 => b"#PES0080",
            PesVersion::V9 => b"#PES0090",
        }
    }

    pub fn label(self) -> String {
        let m = std::str::from_utf8(self.magic()).unwrap_or("");
        let note = if self.number() >= 7 { ", v6 layout" } else { "" };
        format!("PES v{} ({m}{note})", self.number())
    }
}

/// Returns e.g. "#PES0060" for display, or None if the data is not a PES file.
pub fn detect_version(data: &[u8]) -> Option<String> {
    if data.len() < 12 || &data[0..4] != b"#PES" || !data[4..8].iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(String::from_utf8_lossy(&data[0..8]).into_owned())
}

/// Writes the merged layers. With `opt`, stitches are first optimized for the PR680W and the
/// optimization report is returned.
pub fn export_pes(
    layers: &[EmbroideryLayer],
    path: &Path,
    ver: PesVersion,
    opt: Option<&OptimizeOptions>,
) -> Result<Option<Report>, String> {
    let (stitches, palette) = merge_layers_data(layers)?;
    let (stitches, palette, report) = match opt {
        Some(o) => {
            let (s, p, r) = optimize::optimize(&stitches, &palette, o);
            (s, p, Some(r))
        }
        None => (stitches, palette, None),
    };
    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let bytes = build_pes(&stitches, &palette, ver, &name)?;
    fs::write(path, bytes).map_err(|e| e.to_string())?;
    Ok(report)
}

// ---------------------------------------------------------------------------

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn raw(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    /// u8 length + ASCII bytes (non ASCII replaced, max 255 bytes).
    fn str8(&mut self, s: &str) {
        let clean: Vec<u8> = s.bytes().map(|b| if b.is_ascii() && b >= 0x20 { b } else { b'_' }).take(255).collect();
        self.u8(clean.len() as u8);
        self.raw(&clean);
    }
}

const HOOP_W_MM: u16 = 130;
const HOOP_H_MM: u16 = 180;

fn hoop(w: &mut W) {
    w.u16(HOOP_W_MM);
    w.u16(HOOP_H_MM);
}

/// The nine u16 fields shared by v2 and v3 (rotation, colours, grid flags...).
fn page_fields_v2_v3(w: &mut W) {
    for v in [0u16, 7, 19, 0, 1, 0, 100, 0, 0] {
        w.u16(v);
    }
}

fn descriptions(w: &mut W, name: &str) {
    w.str8(name); // design
    w.str8(""); // category
    w.str8(""); // author
    w.str8(""); // keywords
    w.str8(""); // comments
}

fn colour_list(w: &mut W, palette: &[u8]) {
    w.u16(palette.len() as u16);
    for &idx in palette {
        let rgb = thread_rgb(idx);
        w.str8(&idx.to_string());
        w.u8(rgb[0]);
        w.u8(rgb[1]);
        w.u8(rgb[2]);
        w.u8(0);
        w.u32(0x0A); // custom colour, not from a thread list
        w.str8(""); // description
        w.str8(""); // brand
        w.str8(""); // chart
    }
}

/// Header body after the first 12 bytes (signature + PEC offset).
fn pes_header_body(ver: PesVersion, palette: &[u8], name: &str) -> Vec<u8> {
    let mut w = W(Vec::new());
    match ver {
        PesVersion::V1 => {
            w.u16(1); // hoop 130x180
            w.u16(1); // use existing design area
            w.u16(0); // CSewSeg block count
        }
        PesVersion::V2 => {
            hoop(&mut w);
            page_fields_v2_v3(&mut w);
        }
        PesVersion::V3 => {
            w.u16(1);
            w.raw(b"01");
            hoop(&mut w);
            page_fields_v2_v3(&mut w);
        }
        PesVersion::V4 => {
            w.u16(1);
            w.raw(b"01");
            descriptions(&mut w, name);
            w.u8(0);
            w.u8(0);
            hoop(&mut w);
            for v in [0u16, 7, 19, 0, 1, 0, 100] {
                w.u16(v); // rotation, bg, fg, grid, axes, snap, interval
            }
            w.u8(1);
            w.raw(&[0; 7]);
            w.u16(1);
        }
        PesVersion::V5 => {
            w.u16(1);
            w.raw(b"01");
            descriptions(&mut w, name);
            w.u16(0); // optimize hoop change
            hoop(&mut w);
            for v in [0u16, 7, 19, 0, 1, 0, 100, 1, 0] {
                w.u16(v); // rotation, bg, fg, grid, axes, snap, interval, unknown, optimize entry/exit
            }
            tail_v5_v6(&mut w, palette);
        }
        PesVersion::V6 | PesVersion::V7 | PesVersion::V8 | PesVersion::V9 => {
            w.u16(1);
            w.raw(b"01");
            descriptions(&mut w, name);
            w.u16(0); // optimize hoop change
            w.u16(0); // design page is custom
            hoop(&mut w);
            for v in [0u16, 200, 200, 100, 100, 100, 7, 19, 0, 1, 0, 15, 1, 0] {
                w.u16(v); // rotation, design w/h, page w/h, unknown, bg, fg, grid, axes, snap, interval, unknown, optimize
            }
            tail_v5_v6(&mut w, palette);
        }
    }
    w.0
}

fn tail_v5_v6(w: &mut W, palette: &[u8]) {
    w.u8(0); // "from image" path length
    for v in [1.0f32, 0.0, 0.0, 1.0, 0.0, 0.0] {
        w.f32(v); // identity affine transform
    }
    w.u16(0); // programmable fill patterns
    w.u16(0); // motifs
    w.u16(0); // feather patterns
    colour_list(w, palette);
    w.u16(0); // number of objects (stitches live in the PEC block)
}

fn build_pec(stitches: &[Stitch], palette: &[u8], name: &str) -> Result<Vec<u8>, String> {
    let design = build_design(stitches, palette);
    let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
    let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
    for s in stitches {
        min_x = min_x.min(s.x);
        min_y = min_y.min(s.y);
        max_x = max_x.max(s.x);
        max_y = max_y.max(s.y);
    }
    let width = (max_x - min_x).max(1);
    let height = (max_y - min_y).max(1);
    if width > i16::MAX as i32 || height > i16::MAX as i32 {
        return Err("design is larger than 3276 mm; PEC cannot store it".into());
    }
    if palette.is_empty() || palette.len() > 255 {
        return Err("PEC supports 1 to 255 colour blocks".into());
    }

    let body = encode_pec_stitches(stitches, min_x, min_y);

    let mut pec = W(Vec::new());
    // --- first 512 bytes ---
    let label: String = name
        .chars()
        .filter(|c| c.is_ascii() && !c.is_control())
        .take(16)
        .collect();
    pec.raw(format!("LA:{label:<16}").as_bytes()); // 19 bytes
    pec.u8(0x0D);
    pec.raw(&[0x20; 11]);
    pec.u8(0x20);
    pec.raw(&[0xFF, 0x00]);
    pec.u8(6); // thumbnail width in bytes
    pec.u8(38); // thumbnail height in pixels
    pec.raw(&[0x20, 0x20, 0x20, 0x20, 0x64, 0x20, 0x00, 0x20, 0x00, 0x20, 0x20, 0x20]);
    pec.u8((palette.len() - 1) as u8);
    pec.raw(palette);
    pec.0.resize(PEC_HEADER_LEN, 0x20);

    // --- second part, 20 bytes ---
    let thumb_offset = (20 + body.len()) as u32;
    pec.raw(&[0x00, 0x00]);
    pec.raw(&thumb_offset.to_le_bytes()[..3]); // 24-bit offset
    pec.raw(&[0x31, 0xFF, 0xF0]);
    pec.raw(&(width as i16).to_le_bytes());
    pec.raw(&(height as i16).to_le_bytes());
    pec.raw(&[0xE0, 0x01, 0xB0, 0x01]);
    pec.raw(&[0x90, 0x00, 0x90, 0x00]); // origin offsets (design starts at its minimum)

    pec.raw(&body);

    // --- thumbnails: one overall + one per colour block ---
    pec.raw(&render_1bit_thumbnail(&design, None, stitches));
    for b in 0..palette.len() {
        pec.raw(&render_1bit_thumbnail(&design, Some(b), stitches));
    }
    Ok(pec.0)
}

pub fn build_pes(stitches: &[Stitch], palette: &[u8], ver: PesVersion, name: &str) -> Result<Vec<u8>, String> {
    let pec = build_pec(stitches, palette, name)?;
    let body = pes_header_body(ver, palette, name);
    let pec_offset = (12 + body.len()) as u32;

    let mut out = Vec::with_capacity(pec_offset as usize + pec.len());
    out.extend_from_slice(ver.magic());
    out.extend_from_slice(&pec_offset.to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&pec);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_pes, StitchKind};

    fn sample() -> (Vec<Stitch>, Vec<u8>) {
        let mk = |x, y, kind, block| Stitch { x, y, kind, block };
        let s = vec![
            mk(100, 100, StitchKind::Jump, 0),
            mk(130, 100, StitchKind::Stitch, 0),
            mk(130, 160, StitchKind::Stitch, 0),
            mk(400, 400, StitchKind::Jump, 1),
            mk(420, 380, StitchKind::Stitch, 1),
            mk(380, 380, StitchKind::Stitch, 1),
        ];
        (s, vec![20, 5])
    }

    #[test]
    fn every_version_round_trips_through_the_reader() {
        let (st, pal) = sample();
        for v in PesVersion::ALL {
            let bytes = build_pes(&st, &pal, v, "TEST").unwrap();
            assert_eq!(&bytes[0..8], v.magic());
            assert_eq!(detect_version(&bytes).as_deref(), Some(std::str::from_utf8(v.magic()).unwrap()));
            let (back, bpal) = parse_pes(&bytes).unwrap_or_else(|e| panic!("{}: {e}", v.label()));
            assert_eq!(bpal, pal, "{}", v.label());
            assert_eq!(back.len(), st.len(), "{}", v.label());
            for (a, b) in back.windows(2).zip(st.windows(2)) {
                assert_eq!(a[1].x - a[0].x, b[1].x - b[0].x);
                assert_eq!(a[1].y - a[0].y, b[1].y - b[0].y);
                assert_eq!(a[1].kind, b[1].kind);
                assert_eq!(a[1].block, b[1].block);
            }
        }
    }

    #[test]
    fn pec_offset_points_at_label() {
        let (st, pal) = sample();
        for v in PesVersion::ALL {
            let bytes = build_pes(&st, &pal, v, "TEST").unwrap();
            let off = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
            assert_eq!(&bytes[off..off + 3], b"LA:", "{}", v.label());
        }
    }
}
