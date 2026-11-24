//! Bitmap -> vector tracing, plus SVG export of editable vector paths.
//!
//! Pipeline (all pure Rust, no extra dependencies):
//!  1. downscale to a working resolution and quantize every pixel to the PEC thread palette
//!  2. keep the `max_colors` most used threads, remap the rest to the nearest kept one
//!  3. for every colour: trace the exact region boundaries (holes included) on the pixel grid
//!  4. round off the pixel staircase, simplify with Douglas-Peucker
//!  5. fit cubic Bezier handles (Catmull-Rom style) with corner detection
//!
//! The result is a `Vec<VectorPath>`, i.e. exactly what the SVG importer produces, so the
//! node/curve editor, stitch sampling and PES export all work on traced bitmaps.

use crate::{nearest_pec_color, thread_rgb, VectorNode, VectorPath, SVG_PX_TO_PEC};
use eframe::egui::{Pos2, Vec2};
use image::imageops::{self, FilterType};
use image::RgbaImage;
use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct VectorizeOptions {
    /// Maximum number of thread colours kept.
    pub max_colors: usize,
    /// Longest side (pixels) of the working bitmap. Larger = more detail, slower.
    pub work_size: u32,
    /// Douglas-Peucker tolerance in working pixels. Larger = fewer nodes.
    pub tolerance: f32,
    /// Turn angle (degrees) above which a node is a sharp corner instead of a smooth curve.
    pub corner_deg: f32,
    /// Regions smaller than this (working pixels squared) are discarded as noise.
    pub min_area: f32,
    /// Treat near-white pixels as background (no thread).
    pub drop_light_bg: bool,
    /// Size of the longest side of the result, in millimetres.
    pub target_mm: f32,
}

impl Default for VectorizeOptions {
    fn default() -> Self {
        Self {
            max_colors: 6,
            work_size: 400,
            tolerance: 0.8,
            corner_deg: 60.0,
            min_area: 12.0,
            drop_light_bg: true,
            target_mm: 80.0,
        }
    }
}

pub fn vectorize_file(path: &Path, opt: &VectorizeOptions) -> Result<Vec<VectorPath>, String> {
    let img = image::open(path).map_err(|e| format!("cannot read image: {e}"))?.to_rgba8();
    vectorize_image(&img, opt)
}

pub fn vectorize_image(img: &RgbaImage, opt: &VectorizeOptions) -> Result<Vec<VectorPath>, String> {
    if img.width() == 0 || img.height() == 0 {
        return Err("empty image".into());
    }
    let max_side = img.width().max(img.height());
    let small = if max_side > opt.work_size {
        let s = opt.work_size as f32 / max_side as f32;
        let nw = ((img.width() as f32 * s).round() as u32).max(1);
        let nh = ((img.height() as f32 * s).round() as u32).max(1);
        imageops::resize(img, nw, nh, FilterType::Triangle)
    } else {
        img.clone()
    };
    let (w, h) = (small.width() as usize, small.height() as usize);

    // ---- 1. quantize to PEC thread indices (0 = no thread) -------------------------
    let mut cache: HashMap<u32, u8> = HashMap::new();
    let mut labels = vec![0u8; w * h];
    for (i, px) in small.pixels().enumerate() {
        let [r, g, b, a] = px.0;
        if a < 128 || (opt.drop_light_bg && r >= 235 && g >= 235 && b >= 235) {
            continue;
        }
        let key = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
        labels[i] = *cache.entry(key).or_insert_with(|| nearest_pec_color(r, g, b));
    }

    // ---- 2. keep the most used colours ---------------------------------------------
    let mut counts = [0usize; 256];
    for &l in &labels {
        counts[l as usize] += 1;
    }
    let mut used: Vec<u8> = (1..=255u8).filter(|&c| counts[c as usize] > 0).collect();
    if used.is_empty() {
        return Err("no visible pixels found (is the image blank or all white?)".into());
    }
    used.sort_by_key(|&c| std::cmp::Reverse(counts[c as usize]));
    let kept: Vec<u8> = used.iter().copied().take(opt.max_colors.max(1)).collect();
    let mut remap = [0u8; 256];
    for &c in &used {
        remap[c as usize] = if kept.contains(&c) {
            c
        } else {
            let rgb = thread_rgb(c);
            *kept
                .iter()
                .min_by_key(|&&k| {
                    let q = thread_rgb(k);
                    let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2);
                    d(rgb[0], q[0]) + d(rgb[1], q[1]) + d(rgb[2], q[2])
                })
                .unwrap()
        };
    }
    for l in labels.iter_mut() {
        *l = remap[*l as usize];
    }

    // pixel -> PEC units (0.1 mm)
    let scale = opt.target_mm.max(1.0) * 10.0 / w.max(h) as f32;

    // ---- 3-5. trace every colour, biggest area first --------------------------------
    let mut order: Vec<u8> = kept.clone();
    order.sort_by_key(|&c| {
        std::cmp::Reverse(labels.iter().filter(|&&l| l == c).count())
    });

    let mut out: Vec<VectorPath> = Vec::new();
    for color in order {
        let mask: Vec<bool> = labels.iter().map(|&l| l == color).collect();
        for poly in trace_mask(&mask, w, h) {
            if polygon_area(&poly).abs() < opt.min_area {
                continue;
            }
            let pts: Vec<Pos2> = poly.iter().map(|&(x, y)| Pos2::new(x as f32, y as f32)).collect();
            let smooth = cut_corners(&cut_corners(&pts, 1.0), 1.0);
            let simple = simplify_closed(&smooth, opt.tolerance.max(0.05));
            if simple.len() < 3 {
                continue;
            }
            let nodes = fit_nodes(&simple, opt.corner_deg, scale);
            out.push(VectorPath { color, nodes, closed: true });
        }
    }
    if out.is_empty() {
        return Err("nothing left after tracing; lower the minimum area".into());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Boundary tracing
// ---------------------------------------------------------------------------

const DIRS: [(i32, i32); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)]; // right, down, left, up

/// Traces all closed boundary loops (outer borders and holes) of a binary mask.
/// Loops follow pixel edges, so the result is exact; vertices are pixel corners.
fn trace_mask(mask: &[bool], w: usize, h: usize) -> Vec<Vec<(i32, i32)>> {
    let (wi, hi) = (w as i32, h as i32);
    let filled = |x: i32, y: i32| x >= 0 && y >= 0 && x < wi && y < hi && mask[(y * wi + x) as usize];

    // Directed edges, interior on the right-hand side.
    let mut edges: HashMap<(i32, i32), [bool; 4]> = HashMap::new();
    for y in 0..hi {
        for x in 0..wi {
            if !filled(x, y) {
                continue;
            }
            if !filled(x, y - 1) {
                edges.entry((x, y)).or_insert([false; 4])[0] = true;
            }
            if !filled(x + 1, y) {
                edges.entry((x + 1, y)).or_insert([false; 4])[1] = true;
            }
            if !filled(x, y + 1) {
                edges.entry((x + 1, y + 1)).or_insert([false; 4])[2] = true;
            }
            if !filled(x - 1, y) {
                edges.entry((x, y + 1)).or_insert([false; 4])[3] = true;
            }
        }
    }

    let mut keys: Vec<(i32, i32)> = edges.keys().copied().collect();
    keys.sort();

    let mut loops = Vec::new();
    for k in keys {
        for d in 0..4 {
            if !edges.get(&k).map_or(false, |e| e[d]) {
                continue;
            }
            let start = k;
            let (mut v, mut dir) = (k, d);
            let mut prev_dir: Option<usize> = None;
            let mut pts: Vec<(i32, i32)> = Vec::new();
            loop {
                if let Some(e) = edges.get_mut(&v) {
                    e[dir] = false;
                }
                if prev_dir != Some(dir) {
                    pts.push(v);
                }
                let nv = (v.0 + DIRS[dir].0, v.1 + DIRS[dir].1);
                if nv == start {
                    break;
                }
                // prefer a right turn (keeps diagonal neighbours separate), then straight, then left
                let next = [(dir + 1) % 4, dir, (dir + 3) % 4]
                    .into_iter()
                    .find(|&nd| edges.get(&nv).map_or(false, |e| e[nd]));
                match next {
                    Some(nd) => {
                        prev_dir = Some(dir);
                        v = nv;
                        dir = nd;
                    }
                    None => break,
                }
            }
            if pts.len() >= 3 {
                loops.push(pts);
            }
        }
    }
    loops
}

fn polygon_area(p: &[(i32, i32)]) -> f32 {
    let n = p.len();
    let mut s = 0i64;
    for i in 0..n {
        let (x0, y0) = p[i];
        let (x1, y1) = p[(i + 1) % n];
        s += x0 as i64 * y1 as i64 - x1 as i64 * y0 as i64;
    }
    s as f32 * 0.5
}

// ---------------------------------------------------------------------------
// Smoothing, simplification, Bezier fitting
// ---------------------------------------------------------------------------

/// Corner cutting with a bounded cut distance: rounds the pixel staircase into diagonals
/// while keeping real corners nearly sharp.
fn cut_corners(poly: &[Pos2], max_cut: f32) -> Vec<Pos2> {
    let n = poly.len();
    if n < 3 {
        return poly.to_vec();
    }
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        let d = b - a;
        let l = d.length();
        if l < 1e-6 {
            continue;
        }
        let u = d / l;
        let k = (l * 0.25).min(max_cut);
        out.push(a + u * k);
        out.push(b - u * k);
    }
    out
}

fn seg_dist(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let l2 = ab.length_sq();
    if l2 < 1e-12 {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / l2).clamp(0.0, 1.0);
    p.distance(a + ab * t)
}

/// Douglas-Peucker on a closed polygon (split at the two farthest-apart anchor points).
fn simplify_closed(pts: &[Pos2], tol: f32) -> Vec<Pos2> {
    let n = pts.len();
    if n <= 4 {
        return pts.to_vec();
    }
    let mut far = 0usize;
    let mut fd = -1.0f32;
    for i in 1..n {
        let d = pts[i].distance(pts[0]);
        if d > fd {
            fd = d;
            far = i;
        }
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[far] = true;
    let mut stack = vec![(0usize, far), (far, n)];
    while let Some((lo, hi)) = stack.pop() {
        if hi <= lo + 1 {
            continue;
        }
        let (a, b) = (pts[lo % n], pts[hi % n]);
        let mut md = -1.0f32;
        let mut mi = lo;
        for i in lo + 1..hi {
            let d = seg_dist(pts[i % n], a, b);
            if d > md {
                md = d;
                mi = i;
            }
        }
        if md > tol {
            keep[mi % n] = true;
            stack.push((lo, mi));
            stack.push((mi, hi));
        }
    }
    (0..n).filter(|&i| keep[i]).map(|i| pts[i]).collect()
}

/// Catmull-Rom style handles; sharp turns become corners (zero handles).
fn fit_nodes(poly: &[Pos2], corner_deg: f32, scale: f32) -> Vec<VectorNode> {
    let n = poly.len();
    let mut nodes = Vec::with_capacity(n);
    for i in 0..n {
        let prev = poly[(i + n - 1) % n];
        let cur = poly[i];
        let next = poly[(i + 1) % n];
        let v1 = cur - prev;
        let v2 = next - cur;
        let (l1, l2) = (v1.length(), v2.length());

        let (mut hin, mut hout) = (Vec2::ZERO, Vec2::ZERO);
        if l1 > 1e-6 && l2 > 1e-6 {
            let cos = (v1.dot(v2) / (l1 * l2)).clamp(-1.0, 1.0);
            let turn = cos.acos().to_degrees();
            let tangent = next - prev;
            if turn <= corner_deg && tangent.length() > 1e-6 {
                let t = tangent.normalized();
                hout = t * (l2 / 3.0);
                hin = -t * (l1 / 3.0);
            }
        }
        nodes.push(VectorNode {
            pos: Pos2::new(cur.x * scale, cur.y * scale),
            handle_in: hin * scale,
            handle_out: hout * scale,
        });
    }
    nodes
}

// ---------------------------------------------------------------------------
// Helpers for exporting
// ---------------------------------------------------------------------------

/// Applies a point transform (e.g. a layer's move/scale/rotate) to paths, handles included.
pub fn transform_paths(paths: &[VectorPath], f: &dyn Fn(Pos2) -> Pos2) -> Vec<VectorPath> {
    paths
        .iter()
        .map(|vp| VectorPath {
            color: vp.color,
            closed: vp.closed,
            nodes: vp
                .nodes
                .iter()
                .map(|n| {
                    let p = f(n.pos);
                    VectorNode {
                        pos: p,
                        handle_in: if n.handle_in == Vec2::ZERO { Vec2::ZERO } else { f(n.pos + n.handle_in) - p },
                        handle_out: if n.handle_out == Vec2::ZERO { Vec2::ZERO } else { f(n.pos + n.handle_out) - p },
                    }
                })
                .collect(),
        })
        .collect()
}

/// Serializes paths as an SVG document (stroked outlines in thread colours).
/// Coordinates use the same px <-> PEC ratio as the SVG importer, so files round-trip.
pub fn paths_to_svg(paths: &[VectorPath]) -> String {
    let mut min = Pos2::new(f32::MAX, f32::MAX);
    let mut max = Pos2::new(f32::MIN, f32::MIN);
    for vp in paths {
        for n in &vp.nodes {
            min = min.min(n.pos);
            max = max.max(n.pos);
        }
    }
    if min.x > max.x {
        min = Pos2::ZERO;
        max = Pos2::new(1.0, 1.0);
    }
    let px = |p: Pos2| ((p.x - min.x) / SVG_PX_TO_PEC + 2.0, (p.y - min.y) / SVG_PX_TO_PEC + 2.0);
    let w = (max.x - min.x) / SVG_PX_TO_PEC + 4.0;
    let h = (max.y - min.y) / SVG_PX_TO_PEC + 4.0;

    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w:.2}\" height=\"{h:.2}\" viewBox=\"0 0 {w:.2} {h:.2}\">\n"
    ));
    for vp in paths {
        if vp.nodes.len() < 2 {
            continue;
        }
        let rgb = thread_rgb(vp.color);
        let mut d = String::new();
        let (x0, y0) = px(vp.nodes[0].pos);
        d.push_str(&format!("M{x0:.2} {y0:.2}"));
        let segs = if vp.closed { vp.nodes.len() } else { vp.nodes.len() - 1 };
        for i in 0..segs {
            let a = &vp.nodes[i];
            let b = &vp.nodes[(i + 1) % vp.nodes.len()];
            let (bx, by) = px(b.pos);
            if a.handle_out == Vec2::ZERO && b.handle_in == Vec2::ZERO {
                d.push_str(&format!(" L{bx:.2} {by:.2}"));
            } else {
                let (c1x, c1y) = px(a.pos + a.handle_out);
                let (c2x, c2y) = px(b.pos + b.handle_in);
                d.push_str(&format!(" C{c1x:.2} {c1y:.2} {c2x:.2} {c2y:.2} {bx:.2} {by:.2}"));
            }
        }
        if vp.closed {
            d.push_str(" Z");
        }
        s.push_str(&format!(
            "  <path d=\"{d}\" fill=\"none\" stroke=\"#{:02x}{:02x}{:02x}\" stroke-width=\"1\" stroke-linejoin=\"round\"/>\n",
            rgb[0], rgb[1], rgb[2]
        ));
    }
    s.push_str("</svg>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    #[test]
    fn traces_a_square_with_a_hole() {
        let mut img = RgbaImage::from_pixel(40, 40, Rgba([255, 255, 255, 255]));
        for y in 5..35 {
            for x in 5..35 {
                let hole = (15..25).contains(&x) && (15..25).contains(&y);
                if !hole {
                    img.put_pixel(x, y, Rgba([200, 0, 0, 255]));
                }
            }
        }
        let paths = vectorize_image(&img, &VectorizeOptions::default()).unwrap();
        assert_eq!(paths.len(), 2, "outer border + hole");
        assert!(paths.iter().all(|p| p.closed && p.nodes.len() >= 3));
    }

    #[test]
    fn blank_image_is_an_error() {
        let img = RgbaImage::from_pixel(10, 10, Rgba([255, 255, 255, 255]));
        assert!(vectorize_image(&img, &VectorizeOptions::default()).is_err());
    }
}
