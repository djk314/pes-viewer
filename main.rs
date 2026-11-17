//! Lynn PES (Brother) embroidery viewer & layer composer for Linux By Djk314.
//!
//! - Open .pes and .svg files with a dialog (Ctrl+O), drag & drop, or a command-line argument
//! - Add multiple .pes / .svg layers, drag, resize, rotate, and edit vector/Bezier nodes directly
//! - Export the current view to PNG or the merged layers to PES v6 (#PES0060)

use eframe::egui::{self, Align2, Color32, FontId, Key, Pos2, Rect, Sense, Shape, Stroke, Vec2};
use image::imageops::{self, FilterType};
use image::{Rgba, RgbaImage};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Data model & Vector Node / Bezier structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum StitchKind {
    Stitch,
    Jump,
}

#[derive(Debug, Clone, Copy)]
struct Stitch {
    /// Absolute position in 0.1 mm units (y grows downward).
    x: i32,
    y: i32,
    kind: StitchKind,
    /// Color block index (incremented on every color change).
    block: usize,
}

#[derive(Debug, Clone, Copy)]
struct VectorNode {
    /// Position de l'ancre en unités PEC (0.1 mm)
    pos: Pos2,
    /// Poignée entrante relative à `pos` (Vec2::ZERO si segment droit)
    handle_in: Vec2,
    /// Poignée sortante relative à `pos` (Vec2::ZERO si segment droit)
    handle_out: Vec2,
}

#[derive(Debug, Clone)]
struct VectorPath {
    color: u8,
    nodes: Vec<VectorNode>,
    closed: bool,
}

/// A polyline drawn with one thread color.
struct Run {
    /// PEC palette index.
    color: u8,
    pts: Vec<Pos2>,
    /// Per stitch (`pts.len() - 1` values): 0 = isolated line, 1 = part of a dense fill.
    fill: Vec<f32>,
}

struct Design {
    runs: Vec<Run>,
    jumps: Vec<[Pos2; 2]>,
    min: Pos2,
    max: Pos2,
    stitch_count: usize,
    color_blocks: usize,
}

struct EmbroideryLayer {
    name: String,
    stitches: Vec<Stitch>,
    palette: Vec<u8>,
    design: Design,
    /// Chemins vectoriels éditables (présents nativement pour les SVG, ou générés à la demande pour les PES)
    vector_paths: Option<Vec<VectorPath>>,
    offset_x: i32,
    offset_y: i32,
    scale_x: f32,
    scale_y: f32,
    rotation: f32, // en degrés (0..360)
    visible: bool,
}

impl EmbroideryLayer {
    fn base_size(&self) -> Vec2 {
        (self.design.max - self.design.min).max(Vec2::splat(1.0))
    }

    fn scaled_bounds(&self) -> (Pos2, Pos2) {
        let base = self.base_size();
        let scaled_w = base.x * self.scale_x;
        let scaled_h = base.y * self.scale_y;
        let local_center = Vec2::new(scaled_w * 0.5, scaled_h * 0.5);
        let lo = Vec2::new(self.offset_x as f32, self.offset_y as f32);
        let center = self.design.min + lo + local_center;

        let rad = self.rotation.to_radians();
        let cos_r = rad.cos();
        let sin_r = rad.sin();

        let half_w = scaled_w * 0.5;
        let half_h = scaled_h * 0.5;
        let corners = [
            Vec2::new(-half_w, -half_h),
            Vec2::new(half_w, -half_h),
            Vec2::new(half_w, half_h),
            Vec2::new(-half_w, half_h),
        ];

        let mut min = Pos2::new(f32::MAX, f32::MAX);
        let mut max = Pos2::new(f32::MIN, f32::MIN);
        for c in &corners {
            let rot_c = Vec2::new(c.x * cos_r - c.y * sin_r, c.x * sin_r + c.y * cos_r);
            let pt = center + rot_c;
            min = min.min(pt);
            max = max.max(pt);
        }
        (min, max)
    }

    fn transform_point(&self, p: Pos2) -> Pos2 {
        let base = self.base_size();
        let scaled_w = base.x * self.scale_x;
        let scaled_h = base.y * self.scale_y;
        let local_center = Vec2::new(scaled_w * 0.5, scaled_h * 0.5);
        let lo = Vec2::new(self.offset_x as f32, self.offset_y as f32);
        let center = self.design.min + lo + local_center;

        let local = p - self.design.min;
        let scaled = Vec2::new(local.x * self.scale_x, local.y * self.scale_y);
        let centered = scaled - local_center;

        let rad = self.rotation.to_radians();
        let cos_r = rad.cos();
        let sin_r = rad.sin();
        let rotated = Vec2::new(
            centered.x * cos_r - centered.y * sin_r,
            centered.x * sin_r + centered.y * cos_r,
        );

        center + rotated
    }

    /// Transformation inverse : convertit un point dans l'espace monde PEC vers l'espace local non-transformé du calque.
    fn inverse_transform_point(&self, world_p: Pos2) -> Pos2 {
        let base = self.base_size();
        let scaled_w = base.x * self.scale_x;
        let scaled_h = base.y * self.scale_y;
        let local_center = Vec2::new(scaled_w * 0.5, scaled_h * 0.5);
        let lo = Vec2::new(self.offset_x as f32, self.offset_y as f32);
        let center = self.design.min + lo + local_center;

        let rel = world_p - center;
        let rad = -self.rotation.to_radians();
        let cos_r = rad.cos();
        let sin_r = rad.sin();
        let unrotated = Vec2::new(
            rel.x * cos_r - rel.y * sin_r,
            rel.x * sin_r + rel.y * cos_r,
        );
        let scaled = unrotated + local_center;
        let local = Vec2::new(
            scaled.x / self.scale_x.max(1e-4),
            scaled.y / self.scale_y.max(1e-4),
        );
        self.design.min + local
    }

    fn contains_pes_point(&self, p_pes: Pos2) -> bool {
        let base = self.base_size();
        let local_pt = self.inverse_transform_point(p_pes) - self.design.min;
        let check_rect = Rect::from_min_size(Pos2::ZERO, base).expand(15.0 / self.scale_x.max(0.1));
        check_rect.contains(Pos2::new(local_pt.x, local_pt.y))
    }

    /// Convertit les points d'un calque `.pes` en chemins vectoriels éditables si ce n'est pas déjà fait.
    fn ensure_vector_paths(&mut self) {
        if self.vector_paths.is_some() {
            return;
        }
        let mut paths = Vec::new();
        for run in &self.design.runs {
            if run.pts.len() < 2 {
                continue;
            }
            let nodes: Vec<VectorNode> = run
                .pts
                .iter()
                .map(|&p| VectorNode {
                    pos: p,
                    handle_in: Vec2::ZERO,
                    handle_out: Vec2::ZERO,
                })
                .collect();
            paths.push(VectorPath {
                color: run.color,
                nodes,
                closed: false,
            });
        }
        self.vector_paths = Some(paths);
    }

    /// Recalcule les points de broderie (`stitches`, `palette`, `design`) à partir des `vector_paths` modifiés.
    fn rebuild_from_vector_paths(&mut self) {
        let Some(ref paths) = self.vector_paths else { return };
        let (stitches, palette) = sample_vector_paths_to_stitches(paths);
        if stitches.is_empty() {
            return;
        }
        // Préserve l'ancrage visuel dans l'espace monde si le bounding box local change
        let old_min = self.design.min;
        let new_design = build_design(&stitches, &palette);
        let delta_min = new_design.min - old_min;
        self.offset_x += (delta_min.x * self.scale_x).round() as i32;
        self.offset_y += (delta_min.y * self.scale_y).round() as i32;

        self.stitches = stitches;
        self.palette = palette;
        self.design = new_design;
    }

    fn transformed_stitches(&self, block_offset: usize, force_first_jump: bool) -> Vec<Stitch> {
        let base = self.base_size();
        let scaled_w = base.x * self.scale_x;
        let scaled_h = base.y * self.scale_y;
        let local_center = Vec2::new(scaled_w * 0.5, scaled_h * 0.5);
        let lo = Vec2::new(self.offset_x as f32, self.offset_y as f32);
        let center = self.design.min + lo + local_center;

        let rad = self.rotation.to_radians();
        let cos_r = rad.cos();
        let sin_r = rad.sin();

        self.stitches
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let local = Pos2::new(s.x as f32, s.y as f32) - self.design.min;
                let scaled = Vec2::new(local.x * self.scale_x, local.y * self.scale_y);
                let centered = scaled - local_center;
                let rotated = Vec2::new(
                    centered.x * cos_r - centered.y * sin_r,
                    centered.x * sin_r + centered.y * cos_r,
                );
                let final_pt = center + rotated;

                let x = final_pt.x.round() as i32;
                let y = final_pt.y.round() as i32;
                let kind = if i == 0 && force_first_jump {
                    StitchKind::Jump
                } else {
                    s.kind
                };
                Stitch {
                    x,
                    y,
                    kind,
                    block: block_offset + s.block,
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Bezier evaluation & Vector Path -> Stitch sampling
// ---------------------------------------------------------------------------

const SVG_STITCH_STEP_PEC: f32 = 25.0; // 2.5 mm entre deux piqûres d'aiguille
const SVG_PX_TO_PEC: f32 = 2.6458333;


fn flatten_cubic(p0: Pos2, p1: Pos2, p2: Pos2, p3: Pos2, tol: f32, out: &mut Vec<Pos2>) {
    let chord = p3 - p0;
    let len_sq = chord.length_sq();
    let d1 = if len_sq > 1e-6 {
        ((p1 - p0).x * chord.y - (p1 - p0).y * chord.x).abs() / len_sq.sqrt()
    } else {
        (p1 - p0).length()
    };
    let d2 = if len_sq > 1e-6 {
        ((p2 - p0).x * chord.y - (p2 - p0).y * chord.x).abs() / len_sq.sqrt()
    } else {
        (p2 - p0).length()
    };

    if d1.max(d2) <= tol {
        out.push(p3);
        return;
    }

    let p01 = p0.lerp(p1, 0.5);
    let p12 = p1.lerp(p2, 0.5);
    let p23 = p2.lerp(p3, 0.5);
    let p012 = p01.lerp(p12, 0.5);
    let p123 = p12.lerp(p23, 0.5);
    let p0123 = p012.lerp(p123, 0.5);

    flatten_cubic(p0, p01, p012, p0123, tol, out);
    flatten_cubic(p0123, p123, p23, p3, tol, out);
}

/// Génère une polyligne aplatie pour le segment entre `n0` et `n1`.
fn sample_segment_polyline(n0: &VectorNode, n1: &VectorNode) -> Vec<Pos2> {
    let mut poly = vec![n0.pos];
    if n0.handle_out == Vec2::ZERO && n1.handle_in == Vec2::ZERO {
        poly.push(n1.pos);
    } else {
        let p1 = n0.pos + n0.handle_out;
        let p2 = n1.pos + n1.handle_in;
        flatten_cubic(n0.pos, p1, p2, n1.pos, 1.2, &mut poly);
    }
    poly
}

/// Échantillonne une liste de `VectorPath` en `Stitch` et palette PEC.
fn sample_vector_paths_to_stitches(paths: &[VectorPath]) -> (Vec<Stitch>, Vec<u8>) {
    let mut stitches = Vec::new();
    let mut palette = Vec::new();
    let mut last_color: Option<u8> = None;

    for vp in paths {
        if vp.nodes.len() < 2 {
            continue;
        }
        if last_color != Some(vp.color) {
            palette.push(vp.color);
            last_color = Some(vp.color);
        }
        let block = palette.len().saturating_sub(1);

        let mut full_poly: Vec<Pos2> = Vec::new();
        let seg_count = if vp.closed { vp.nodes.len() } else { vp.nodes.len() - 1 };

        for i in 0..seg_count {
            let n0 = &vp.nodes[i];
            let n1 = &vp.nodes[(i + 1) % vp.nodes.len()];
            let seg_pts = sample_segment_polyline(n0, n1);
            if full_poly.is_empty() {
                full_poly.extend(seg_pts);
            } else {
                full_poly.extend(seg_pts.into_iter().skip(1));
            }
        }

        if full_poly.len() < 2 {
            continue;
        }

        let first = full_poly[0];
        stitches.push(Stitch {
            x: first.x.round() as i32,
            y: first.y.round() as i32,
            kind: StitchKind::Jump,
            block,
        });

        for pair in full_poly.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let dist = a.distance(b);
            if dist < 0.2 {
                continue;
            }
            let steps = (dist / SVG_STITCH_STEP_PEC).ceil().max(1.0) as usize;
            for k in 1..=steps {
                let t = k as f32 / steps as f32;
                let p = a.lerp(b, t);
                stitches.push(Stitch {
                    x: p.x.round() as i32,
                    y: p.y.round() as i32,
                    kind: StitchKind::Stitch,
                    block,
                });
            }
        }
    }

    (stitches, palette)
}

// ---------------------------------------------------------------------------
// File format helpers (.pes & .svg)
// ---------------------------------------------------------------------------

fn is_supported_embroidery_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("pes") || ext.eq_ignore_ascii_case("svg"))
        .unwrap_or(false)
}

fn load_layer_from_file(path: &Path) -> Result<EmbroideryLayer, String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let is_svg = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("svg"))
        .unwrap_or(false);

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Layer".into());

    if is_svg {
        let vector_paths = parse_svg_to_vector_paths(&data)?;
        let (stitches, palette) = sample_vector_paths_to_stitches(&vector_paths);
        if stitches.is_empty() {
            return Err("no stitches generated from SVG".into());
        }
        let design = build_design(&stitches, &palette);
        Ok(EmbroideryLayer {
            name,
            stitches,
            palette,
            design,
            vector_paths: Some(vector_paths),
            offset_x: 0,
            offset_y: 0,
            scale_x: 1.0,
            scale_y: 1.0,
            rotation: 0.0,
            visible: true,
        })
    } else {
        let (stitches, palette) = parse_pes(&data)?;
        let design = build_design(&stitches, &palette);
        Ok(EmbroideryLayer {
            name,
            stitches,
            palette,
            design,
            vector_paths: None,
            offset_x: 0,
            offset_y: 0,
            scale_x: 1.0,
            scale_y: 1.0,
            rotation: 0.0,
            visible: true,
        })
    }
}

fn parse_design_file(path: &Path) -> Result<(Vec<Stitch>, Vec<u8>), String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let is_svg = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("svg"))
        .unwrap_or(false);
    if is_svg {
        let vpaths = parse_svg_to_vector_paths(&data)?;
        let (stitches, palette) = sample_vector_paths_to_stitches(&vpaths);
        if stitches.is_empty() {
            return Err("no stitches found in SVG".into());
        }
        Ok((stitches, palette))
    } else {
        parse_pes(&data)
    }
}

// ---------------------------------------------------------------------------
// SVG parsing into editable Bezier VectorPaths
// ---------------------------------------------------------------------------

fn nearest_pec_color(r: u8, g: u8, b: u8) -> u8 {
    let mut best_idx = 20u8;
    let mut best_dist = u32::MAX;
    for (i, &hex) in PEC_PALETTE.iter().enumerate() {
        let pr = ((hex >> 16) & 0xFF) as i32;
        let pg = ((hex >> 8) & 0xFF) as i32;
        let pb = (hex & 0xFF) as i32;
        let dr = r as i32 - pr;
        let dg = g as i32 - pg;
        let db = b as i32 - pb;
        let dist = (dr * dr + dg * dg + db * db) as u32;
        if dist < best_dist {
            best_dist = dist;
            best_idx = (i + 1) as u8;
        }
    }
    best_idx
}

fn extract_usvg_paint_color(paint: &usvg::Paint) -> [u8; 3] {
    match paint {
        usvg::Paint::Color(c) => [c.red, c.green, c.blue],
        usvg::Paint::LinearGradient(g) => g
            .stops()
            .first()
            .map(|s| [s.color().red, s.color().green, s.color().blue])
            .unwrap_or([0, 0, 0]),
        usvg::Paint::RadialGradient(g) => g
            .stops()
            .first()
            .map(|s| [s.color().red, s.color().green, s.color().blue])
            .unwrap_or([0, 0, 0]),
        usvg::Paint::Pattern(_) => [0, 0, 0],
    }
}

fn collect_usvg_vector_paths(group: &usvg::Group, out_paths: &mut Vec<VectorPath>) {
    for node in group.children() {
        match node {
            usvg::Node::Group(sub) => {
                collect_usvg_vector_paths(sub, out_paths);
            }
            usvg::Node::Path(path) => {
                if !path.is_visible() {
                    continue;
                }
                let rgb = if let Some(stroke) = path.stroke() {
                    extract_usvg_paint_color(stroke.paint())
                } else if let Some(fill) = path.fill() {
                    extract_usvg_paint_color(fill.paint())
                } else {
                    [0, 0, 0]
                };
                let pec_color = nearest_pec_color(rgb[0], rgb[1], rgb[2]);

                let xf = path.abs_transform();
                let to_pec = |pt: usvg::tiny_skia_path::Point| -> Pos2 {
                    let tx = xf.sx * pt.x + xf.kx * pt.y + xf.tx;
                    let ty = xf.ky * pt.x + xf.sy * pt.y + xf.ty;
                    Pos2::new(tx * SVG_PX_TO_PEC, ty * SVG_PX_TO_PEC)
                };

                let mut cur_nodes: Vec<VectorNode> = Vec::new();
                for seg in path.data().segments() {
                    match seg {
                        usvg::tiny_skia_path::PathSegment::MoveTo(p) => {
                            if cur_nodes.len() >= 2 {
                                out_paths.push(VectorPath {
                                    color: pec_color,
                                    nodes: std::mem::take(&mut cur_nodes),
                                    closed: false,
                                });
                            } else {
                                cur_nodes.clear();
                            }
                            cur_nodes.push(VectorNode {
                                pos: to_pec(p),
                                handle_in: Vec2::ZERO,
                                handle_out: Vec2::ZERO,
                            });
                        }
                        usvg::tiny_skia_path::PathSegment::LineTo(p) => {
                            cur_nodes.push(VectorNode {
                                pos: to_pec(p),
                                handle_in: Vec2::ZERO,
                                handle_out: Vec2::ZERO,
                            });
                        }
                        usvg::tiny_skia_path::PathSegment::QuadTo(p1, p2) => {
                            if let Some(prev) = cur_nodes.last_mut() {
                                let p0 = prev.pos;
                                let qp1 = to_pec(p1);
                                let qp2 = to_pec(p2);
                                // Conversion quadratique -> cubique exacte
                                let c1 = p0.lerp(qp1, 2.0 / 3.0);
                                let c2 = qp2.lerp(qp1, 2.0 / 3.0);
                                prev.handle_out = c1 - p0;
                                cur_nodes.push(VectorNode {
                                    pos: qp2,
                                    handle_in: c2 - qp2,
                                    handle_out: Vec2::ZERO,
                                });
                            }
                        }
                        usvg::tiny_skia_path::PathSegment::CubicTo(p1, p2, p3) => {
                            if let Some(prev) = cur_nodes.last_mut() {
                                let c1 = to_pec(p1);
                                let c2 = to_pec(p2);
                                let end = to_pec(p3);
                                prev.handle_out = c1 - prev.pos;
                                cur_nodes.push(VectorNode {
                                    pos: end,
                                    handle_in: c2 - end,
                                    handle_out: Vec2::ZERO,
                                });
                            }
                        }
                        usvg::tiny_skia_path::PathSegment::Close => {
                            if cur_nodes.len() >= 2 {
                                // Si le dernier point est confondu avec le premier, on transfère sa poignée entrante
                                if cur_nodes.len() >= 3
                                    && cur_nodes.last().unwrap().pos.distance(cur_nodes[0].pos) < 0.2
                                {
                                    let last = cur_nodes.pop().unwrap();
                                    cur_nodes[0].handle_in = last.handle_in;
                                }
                                out_paths.push(VectorPath {
                                    color: pec_color,
                                    nodes: std::mem::take(&mut cur_nodes),
                                    closed: true,
                                });
                            } else {
                                cur_nodes.clear();
                            }
                        }
                    }
                }
                if cur_nodes.len() >= 2 {
                    out_paths.push(VectorPath {
                        color: pec_color,
                        nodes: cur_nodes,
                        closed: false,
                    });
                }
            }
            _ => {}
        }
    }
}

fn parse_svg_to_vector_paths(data: &[u8]) -> Result<Vec<VectorPath>, String> {
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(data, &opt).map_err(|e| format!("SVG parse error: {e}"))?;
    let mut paths = Vec::new();
    collect_usvg_vector_paths(tree.root(), &mut paths);
    if paths.is_empty() {
        return Err("no vector paths found in SVG".into());
    }
    Ok(paths)
}

// ---------------------------------------------------------------------------
// PES / PEC parsing
// ---------------------------------------------------------------------------

const PEC_HEADER_LEN: usize = 0x200;
const PEC_STITCH_OFFSET: usize = PEC_HEADER_LEN + 20;

fn parse_pes(data: &[u8]) -> Result<(Vec<Stitch>, Vec<u8>), String> {
    if data.len() < 12 || &data[0..4] != b"#PES" {
        return Err("not a PES file (missing #PES signature)".into());
    }
    let pec_offset = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let pec = data
        .get(pec_offset..)
        .ok_or_else(|| format!("PEC offset {pec_offset} is outside the file"))?;

    if pec.len() < PEC_STITCH_OFFSET || &pec[0..3] != b"LA:" {
        return Err("invalid or truncated PEC section".into());
    }

    let palette = if pec[48] == 0xFF {
        Vec::new()
    } else {
        let n = pec[48] as usize + 1;
        pec.get(49..49 + n).ok_or("truncated PEC palette")?.to_vec()
    };

    let thumb_off = u16::from_le_bytes([pec[PEC_HEADER_LEN + 2], pec[PEC_HEADER_LEN + 3]]) as usize;
    let mut end = pec.len();
    if thumb_off > 20 && PEC_HEADER_LEN + thumb_off <= pec.len() {
        end = PEC_HEADER_LEN + thumb_off;
    }

    let stitches = decode_stitches(&pec[PEC_STITCH_OFFSET..end]);
    if stitches.is_empty() {
        return Err("no stitches found".into());
    }
    Ok((stitches, palette))
}

fn sign7(v: u8) -> i32 {
    if v < 0x40 { v as i32 } else { v as i32 - 0x80 }
}

fn sign12(code: i32) -> i32 {
    if code & 0x800 != 0 { code - 0x1000 } else { code }
}

fn decode_stitches(d: &[u8]) -> Vec<Stitch> {
    let mut out = Vec::new();
    let (mut x, mut y) = (0i32, 0i32);
    let mut block = 0usize;
    let mut i = 0usize;

    while i < d.len() {
        let b1 = d[i];
        if b1 == 0xFF {
            break;
        }
        if b1 == 0xFE {
            block += 1;
            i += 3;
            continue;
        }

        let mut pen_up = false;

        let dx = if b1 & 0x80 != 0 {
            let Some(&b2) = d.get(i + 1) else { break };
            if b1 & 0x30 != 0 {
                pen_up = true;
            }
            i += 2;
            sign12((((b1 & 0x0F) as i32) << 8) | b2 as i32)
        } else {
            i += 1;
            sign7(b1)
        };

        let Some(&c1) = d.get(i) else { break };
        let dy = if c1 & 0x80 != 0 {
            let Some(&c2) = d.get(i + 1) else { break };
            if c1 & 0x30 != 0 {
                pen_up = true;
            }
            i += 2;
            sign12((((c1 & 0x0F) as i32) << 8) | c2 as i32)
        } else {
            i += 1;
            sign7(c1)
        };

        x += dx;
        y += dy;
        out.push(Stitch {
            x,
            y,
            kind: if pen_up { StitchKind::Jump } else { StitchKind::Stitch },
            block,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// PES v6 Encoding (Merged Layers)
// ---------------------------------------------------------------------------

fn encode_delta(d: i32, is_jump: bool, out: &mut Vec<u8>) {
    let mut rem = d;
    while rem.abs() > 2047 {
        let step = rem.signum() * 2047;
        encode_delta(step, true, out);
        rem -= step;
    }

    if !is_jump && (-64..=63).contains(&rem) {
        out.push((rem as u8) & 0x7F);
    } else {
        let flag = if is_jump { 0xA0 } else { 0x80 };
        let val = (rem as u16) & 0x0FFF;
        out.push(flag | ((val >> 8) as u8));
        out.push((val & 0xFF) as u8);
    }
}

fn encode_pec_stitches(stitches: &[Stitch], origin_x: i32, origin_y: i32) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut cur_x, mut cur_y) = (origin_x, origin_y);
    let mut cur_block = 0usize;
    let mut stop_toggle = 2u8;

    for s in stitches {
        while cur_block < s.block {
            out.extend_from_slice(&[0xFE, 0xB0, stop_toggle]);
            stop_toggle = if stop_toggle == 2 { 1 } else { 2 };
            cur_block += 1;
        }

        let dx = s.x - cur_x;
        let dy = s.y - cur_y;
        let is_jump = s.kind == StitchKind::Jump;

        encode_delta(dx, is_jump, &mut out);
        encode_delta(dy, is_jump, &mut out);

        cur_x = s.x;
        cur_y = s.y;
    }

    out.push(0xFF);
    out
}

fn merge_layers_data(layers: &[EmbroideryLayer]) -> Result<(Vec<Stitch>, Vec<u8>), String> {
    let mut merged_stitches: Vec<Stitch> = Vec::new();
    let mut merged_palette: Vec<u8> = Vec::new();

    for layer in layers.iter().filter(|l| l.visible) {
        if layer.stitches.is_empty() {
            continue;
        }
        let base_block = merged_palette.len();
        if layer.palette.is_empty() {
            merged_palette.push(20);
        } else {
            merged_palette.extend_from_slice(&layer.palette);
        }

        let transformed = layer.transformed_stitches(base_block, !merged_stitches.is_empty());
        merged_stitches.extend(transformed);
    }

    if merged_stitches.is_empty() {
        return Err("No visible stitches to export".into());
    }
    if merged_palette.len() > 255 {
        return Err("Maximum 255 color blocks exceeded".into());
    }
    Ok((merged_stitches, merged_palette))
}

fn render_1bit_thumbnail(design: &Design, block_filter: Option<usize>, stitches: &[Stitch]) -> [u8; 228] {
    let mut buf = [0u8; 228];
    let size = design.max - design.min;
    let scale = (42.0 / size.x.max(1.0)).min(32.0 / size.y.max(1.0));
    let ox = (48.0 - size.x * scale) * 0.5;
    let oy = (38.0 - size.y * scale) * 0.5;

    let mut prev: Option<(i32, i32)> = None;
    for s in stitches {
        let px = ((s.x as f32 - design.min.x) * scale + ox).round() as i32;
        let py = ((s.y as f32 - design.min.y) * scale + oy).round() as i32;
        if s.kind == StitchKind::Stitch && block_filter.map_or(true, |b| b == s.block) {
            if let Some((x0, y0)) = prev {
                let steps = (px - x0).abs().max((py - y0).abs()).max(1);
                for k in 0..=steps {
                    let x = x0 + (px - x0) * k / steps;
                    let y = y0 + (py - y0) * k / steps;
                    if (0..48).contains(&x) && (0..38).contains(&y) {
                        let byte_idx = (y as usize) * 6 + (x as usize) / 8;
                        let bit_idx = (x as usize) % 8;
                        buf[byte_idx] |= 1 << bit_idx;
                    }
                }
            }
        }
        prev = Some((px, py));
    }
    buf
}

fn export_pes_v6(layers: &[EmbroideryLayer], path: &Path) -> Result<(), String> {
    let (merged_stitches, merged_palette) = merge_layers_data(layers)?;
    let merged_design = build_design(&merged_stitches, &merged_palette);

    let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
    let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
    for s in &merged_stitches {
        min_x = min_x.min(s.x);
        min_y = min_y.min(s.y);
        max_x = max_x.max(s.x);
        max_y = max_y.max(s.y);
    }
    let width = (max_x - min_x).max(1) as i16;
    let height = (max_y - min_y).max(1) as i16;

    let pec_stitches = encode_pec_stitches(&merged_stitches, min_x, min_y);

    let mut pec = Vec::new();
    pec.extend_from_slice(b"LA:MERGED_DESIGN   ");
    pec.resize(48, 0x20);
    pec.push((merged_palette.len() - 1) as u8);
    pec.extend_from_slice(&merged_palette);
    pec.resize(PEC_HEADER_LEN, 0x20);

    let thumb_offset = (20 + pec_stitches.len()) as u16;
    pec.extend_from_slice(&[0x00, 0x00]);
    pec.extend_from_slice(&thumb_offset.to_le_bytes());
    pec.extend_from_slice(&[0x31, 0xFF, 0xF0, 0x00]);
    pec.extend_from_slice(&width.to_le_bytes());
    pec.extend_from_slice(&height.to_le_bytes());
    pec.extend_from_slice(&[0xE0, 0x01, 0xB0, 0x01]);
    pec.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

    pec.extend_from_slice(&pec_stitches);

    pec.extend_from_slice(&render_1bit_thumbnail(&merged_design, None, &merged_stitches));
    for b in 0..merged_palette.len() {
        pec.extend_from_slice(&render_1bit_thumbnail(&merged_design, Some(b), &merged_stitches));
    }

    let mut pes = Vec::new();
    pes.extend_from_slice(b"#PES0060");
    let pec_offset_pos = pes.len();
    pes.extend_from_slice(&0u32.to_le_bytes());

    pes.extend_from_slice(&0u16.to_le_bytes());
    pes.extend_from_slice(&1u16.to_le_bytes());
    pes.extend_from_slice(&[0, 0, 0, 0, 0]);
    pes.extend_from_slice(&0u16.to_le_bytes());

    let pec_offset = pes.len() as u32;
    pes[pec_offset_pos..pec_offset_pos + 4].copy_from_slice(&pec_offset.to_le_bytes());
    pes.extend_from_slice(&pec);

    fs::write(path, pes).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Design & Fill computation
// ---------------------------------------------------------------------------

const FILL_CELL: f32 = 30.0;
const FILL_STEP: f32 = 4.0;
const FILL_MIN: f32 = 150.0;
const FILL_FULL: f32 = 400.0;

fn compute_fill(runs: &mut [Run]) {
    let cell = |p: Pos2| ((p.x / FILL_CELL).floor() as i32, (p.y / FILL_CELL).floor() as i32);
    let mut dens: HashMap<(u8, i32, i32), f32> = HashMap::new();
    for run in runs.iter() {
        for pair in run.pts.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let len = (b - a).length();
            let n = ((len / FILL_STEP) as usize).max(1);
            let share = len / n as f32;
            for k in 0..n {
                let p = a + (b - a) * ((k as f32 + 0.5) / n as f32);
                let (cx, cy) = cell(p);
                *dens.entry((run.color, cx, cy)).or_insert(0.0) += share;
            }
        }
    }
    for run in runs.iter_mut() {
        let fill: Vec<f32> = run
            .pts
            .windows(2)
            .map(|pair| {
                let mid = pair[0] + (pair[1] - pair[0]) * 0.5;
                let (cx, cy) = cell(mid);
                let d = dens.get(&(run.color, cx, cy)).copied().unwrap_or(0.0);
                ((d - FILL_MIN) / (FILL_FULL - FILL_MIN)).clamp(0.0, 1.0)
            })
            .collect();
        run.fill = fill;
    }
}

fn build_design(stitches: &[Stitch], palette: &[u8]) -> Design {
    let color_of = |block: usize| palette.get(block).or(palette.last()).copied().unwrap_or(0);

    let mut runs: Vec<Run> = Vec::new();
    let mut jumps: Vec<[Pos2; 2]> = Vec::new();
    let mut cur: Vec<Pos2> = Vec::new();
    let mut cur_block = 0usize;
    let mut prev: Option<Pos2> = None;
    let mut min = Pos2::new(f32::MAX, f32::MAX);
    let mut max = Pos2::new(f32::MIN, f32::MIN);

    let flush = |cur: &mut Vec<Pos2>, block: usize, runs: &mut Vec<Run>| {
        if cur.len() >= 2 {
            runs.push(Run { color: color_of(block), pts: std::mem::take(cur), fill: Vec::new() });
        } else {
            cur.clear();
        }
    };

    for s in stitches {
        let p = Pos2::new(s.x as f32, s.y as f32);
        min = min.min(p);
        max = max.max(p);

        if s.block != cur_block {
            flush(&mut cur, cur_block, &mut runs);
            cur_block = s.block;
        }
        match s.kind {
            StitchKind::Jump => {
                flush(&mut cur, cur_block, &mut runs);
                if let Some(q) = prev {
                    jumps.push([q, p]);
                }
            }
            StitchKind::Stitch => {
                if cur.is_empty() {
                    if let Some(q) = prev {
                        cur.push(q);
                    }
                }
                cur.push(p);
            }
        }
        prev = Some(p);
    }
    flush(&mut cur, cur_block, &mut runs);
    compute_fill(&mut runs);

    Design {
        runs,
        jumps,
        min,
        max,
        stitch_count: stitches.iter().filter(|s| s.kind == StitchKind::Stitch).count(),
        color_blocks: stitches.last().map(|s| s.block + 1).unwrap_or(0),
    }
}

const PEC_PALETTE: [u32; 64] = [
    0x1a0a94, 0x0f75ff, 0x00934c, 0xbabdfe, 0xec0000, 0xe4995a, 0xcc48ab, 0xfdc4fa,
    0xdd84cd, 0x6bd38a, 0xe4a945, 0xffbd42, 0xffe600, 0x6cd900, 0xc1a941, 0xb5ad97,
    0xba9c5f, 0xfaf59e, 0x808080, 0x000000, 0x001cdf, 0xdf00b8, 0x626262, 0x69260d,
    0xff0060, 0xbf8200, 0xf39178, 0xff6805, 0xf0f0f0, 0xc832cd, 0xb0bf9b, 0x65bfeb,
    0xffba04, 0xfff06c, 0xfeca15, 0xf38101, 0x37a923, 0x23465f, 0xa6a695, 0xcebfa6,
    0x96aa02, 0xffe3c6, 0xff99d7, 0x007004, 0xedccfb, 0xc089d8, 0xe7d9b4, 0xe90e86,
    0xcf6829, 0x408615, 0xdb1797, 0xffa704, 0xb9ffff, 0x228927, 0xb612cd, 0x00aa00,
    0xfea9dc, 0xfed510, 0x0097df, 0xffff84, 0xcfe774, 0xffc864, 0xffc8c8, 0xffc8c8,
];

fn thread_rgb(pec_index: u8) -> [u8; 3] {
    match (pec_index as usize).checked_sub(1).and_then(|i| PEC_PALETTE.get(i)) {
        Some(&c) => [(c >> 16) as u8, (c >> 8) as u8, c as u8],
        None => [128, 128, 128],
    }
}

fn darken(c: [u8; 3], f: f32) -> [u8; 3] {
    [(c[0] as f32 * f) as u8, (c[1] as f32 * f) as u8, (c[2] as f32 * f) as u8]
}

fn lighten(c: [u8; 3], t: f32) -> [u8; 3] {
    let m = |v: u8| (v as f32 + (255.0 - v as f32) * t) as u8;
    [m(c[0]), m(c[1]), m(c[2])]
}

#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    Embroidery,
    Flat,
}

const THREAD_MM: f32 = 0.35;
const FLAT_MM: f32 = 0.32;
const FLAT_FILL_MM: f32 = 0.62;
const FLAT_MIN_PX: f32 = 1.5;

fn keep_stitch(i: usize, density: f32) -> bool {
    if density >= 0.999 {
        return true;
    }
    (((i + 1) as f32) * density).floor() != ((i as f32) * density).floor()
}

#[derive(Clone, Copy)]
struct Layer {
    width: f32,
    lit: f32,
    fixed: (f32, f32),
    color: [u8; 3],
    alpha: u8,
    ext: f32,
}

const LIGHT: (f32, f32) = (-0.55, -0.83);

fn thread_layers(base: [u8; 3]) -> [Layer; 6] {
    let l = |width, lit, fixed, color, alpha, ext| Layer { width, lit, fixed, color, alpha, ext };
    [
        l(1.00, 0.00, (0.20, 0.26), [0, 0, 0], 70, 0.30),
        l(1.00, 0.00, (0.0, 0.0), darken(base, 0.45), 255, 0.30),
        l(0.84, 0.04, (0.0, 0.0), darken(base, 0.72), 255, 0.22),
        l(0.62, 0.10, (0.0, 0.0), base, 255, 0.14),
        l(0.36, 0.18, (0.0, 0.0), lighten(base, 0.28), 255, 0.05),
        l(0.14, 0.24, (0.0, 0.0), lighten(base, 0.70), 220, -0.12),
    ]
}

type P2 = (f32, f32);

fn layer_line(l: &Layer, a: P2, b: P2, w: f32) -> Option<(P2, P2)> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len);
    let (nx, ny) = (-uy, ux);
    let lf = nx * LIGHT.0 + ny * LIGHT.1;
    let ox = l.fixed.0 * w + nx * lf * l.lit * w;
    let oy = l.fixed.1 * w + ny * lf * l.lit * w;
    let e = l.ext * w;
    Some(((a.0 + ox - ux * e, a.1 + oy - uy * e), (b.0 + ox + ux * e, b.1 + oy + uy * e)))
}

fn paint_thread(painter: &egui::Painter, a: Pos2, b: Pos2, layers: &[Layer; 6], w: f32) {
    for l in layers {
        if let Some((p0, p1)) = layer_line(l, (a.x, a.y), (b.x, b.y), w) {
            let c = Color32::from_rgba_unmultiplied(l.color[0], l.color[1], l.color[2], l.alpha);
            painter.line_segment(
                [Pos2::new(p0.0, p0.1), Pos2::new(p1.0, p1.1)],
                Stroke::new((l.width * w).max(0.5), c),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// PNG export (raster renderer)
// ---------------------------------------------------------------------------

fn stamp(img: &mut RgbaImage, x: i32, y: i32, color: Rgba<u8>, thickness: u32) {
    let t = thickness as i32;
    let c = (t - 1) as f32 / 2.0;
    let r2 = (t as f32 / 2.0) * (t as f32 / 2.0);
    for j in 0..t {
        for i in 0..t {
            let (dx, dy) = (i as f32 - c, j as f32 - c);
            if dx * dx + dy * dy > r2 {
                continue;
            }
            let px = x - t / 2 + i;
            let py = y - t / 2 + j;
            if px >= 0 && py >= 0 && (px as u32) < img.width() && (py as u32) < img.height() {
                img.put_pixel(px as u32, py as u32, color);
            }
        }
    }
}

fn draw_line(img: &mut RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32, c: Rgba<u8>, t: u32) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (x0, y0);
    loop {
        stamp(img, x, y, c, t);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

fn line_pixels(w: u32, h: u32, x0: i32, y0: i32, x1: i32, y1: i32, t: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let ti = t as i32;
    let c = (ti - 1) as f32 / 2.0;
    let r2 = (t as f32 / 2.0) * (t as f32 / 2.0);
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (x0, y0);
    loop {
        for j in 0..ti {
            for i in 0..ti {
                let (fx, fy) = (i as f32 - c, j as f32 - c);
                if fx * fx + fy * fy > r2 {
                    continue;
                }
                let (px, py) = (x - ti / 2 + i, y - ti / 2 + j);
                if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                    out.push((px as u32, py as u32));
                }
            }
        }
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn blend_line(img: &mut RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32, rgb: [u8; 3], alpha: u8, t: u32) {
    let a = alpha as u32;
    for (px, py) in line_pixels(img.width(), img.height(), x0, y0, x1, y1, t) {
        let d = img.get_pixel_mut(px, py);
        for k in 0..3 {
            d.0[k] = ((rgb[k] as u32 * a + d.0[k] as u32 * (255 - a)) / 255) as u8;
        }
    }
}

fn export_png(
    layers: &[EmbroideryLayer],
    path: &Path,
    scale: f32,
    bg: [u8; 3],
    flat: bool,
    density: f32,
) -> Result<(), String> {
    let (merged_stitches, merged_palette) = merge_layers_data(layers)?;
    let design = build_design(&merged_stitches, &merged_palette);

    let margin = 20.0_f32;
    let size = design.max - design.min;
    let w = (size.x.max(1.0) * scale + 2.0 * margin).ceil() as u64;
    let h = (size.y.max(1.0) * scale + 2.0 * margin).ceil() as u64;
    if w * h > 200_000_000 {
        return Err(format!("image {w}x{h} is too large; lower the export scale"));
    }
    let mut img = RgbaImage::from_pixel(w as u32, h as u32, Rgba([bg[0], bg[1], bg[2], 255]));
    let w_thread = THREAD_MM * 10.0 * scale;
    let to_f = |p: Pos2| -> P2 {
        ((p.x - design.min.x) * scale + margin, (p.y - design.min.y) * scale + margin)
    };
    let ri = |p: P2| (p.0.round() as i32, p.1.round() as i32);

    for run in &design.runs {
        let base = thread_rgb(run.color);
        let rgba = Rgba([base[0], base[1], base[2], 255]);
        let thread_l = thread_layers(base);
        for (i, pair) in run.pts.windows(2).enumerate() {
            let (a, b) = (to_f(pair[0]), to_f(pair[1]));
            if flat {
                let t = run.fill.get(i).copied().unwrap_or(0.0);
                let mm = FLAT_MM + (FLAT_FILL_MM - FLAT_MM) * t;
                let th = ((mm * 10.0 * scale).round() as u32).max(1);
                let ((x0, y0), (x1, y1)) = (ri(a), ri(b));
                draw_line(&mut img, x0, y0, x1, y1, rgba, th);
                continue;
            }
            if !keep_stitch(i, density) {
                continue;
            }
            if w_thread >= 3.0 {
                for l in &thread_l {
                    if let Some((p0, p1)) = layer_line(l, a, b, w_thread) {
                        let th = ((l.width * w_thread).round() as u32).max(1);
                        let ((x0, y0), (x1, y1)) = (ri(p0), ri(p1));
                        if l.alpha == 255 {
                            let c = Rgba([l.color[0], l.color[1], l.color[2], 255]);
                            draw_line(&mut img, x0, y0, x1, y1, c, th);
                        } else {
                            blend_line(&mut img, x0, y0, x1, y1, l.color, l.alpha, th);
                        }
                    }
                }
            } else {
                let ((x0, y0), (x1, y1)) = (ri(a), ri(b));
                draw_line(&mut img, x0, y0, x1, y1, rgba, (w_thread.round() as u32).max(1));
            }
        }
    }
    img.save(path).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Thumbnails and the built-in file browser
// ---------------------------------------------------------------------------

const THUMB_SIZE: u32 = 160;
const PREVIEW_SIZE: u32 = 480;
const THUMB_BG: [u8; 3] = [215, 215, 220];

fn read_design(path: &Path) -> Result<Design, String> {
    let (stitches, palette) = parse_design_file(path)?;
    Ok(build_design(&stitches, &palette))
}

fn render_preview(design: &Design, size: u32, bg: [u8; 3]) -> RgbaImage {
    let big = size * 2;
    let pad = big as f32 * 0.06;
    let dsize = design.max - design.min;
    let avail = big as f32 - 2.0 * pad;
    let scale = (avail / dsize.x.max(1.0)).min(avail / dsize.y.max(1.0));
    let ox = (big as f32 - dsize.x * scale) / 2.0;
    let oy = (big as f32 - dsize.y * scale) / 2.0;
    let thickness = ((3.0 * scale).round() as u32).max(2);

    let mut img = RgbaImage::from_pixel(big, big, Rgba([bg[0], bg[1], bg[2], 255]));
    let to_px = |p: Pos2| -> (i32, i32) {
        (
            ((p.x - design.min.x) * scale + ox).round() as i32,
            ((p.y - design.min.y) * scale + oy).round() as i32,
        )
    };
    for run in &design.runs {
        let [r, g, b] = thread_rgb(run.color);
        for pair in run.pts.windows(2) {
            let (x0, y0) = to_px(pair[0]);
            let (x1, y1) = to_px(pair[1]);
            draw_line(&mut img, x0, y0, x1, y1, Rgba([r, g, b, 255]), thickness);
        }
    }
    imageops::resize(&img, size, size, FilterType::Triangle)
}

struct Thumb {
    tex: Option<egui::TextureHandle>,
    info: String,
}

fn make_thumb(ctx: &egui::Context, path: &Path, size: u32) -> Thumb {
    match read_design(path) {
        Ok(d) => {
            let img = render_preview(&d, size, THUMB_BG);
            let ci = egui::ColorImage::from_rgba_unmultiplied(
                [size as usize, size as usize],
                img.as_raw(),
            );
            let tex = ctx.load_texture(
                format!("{}@{size}", path.display()),
                ci,
                egui::TextureOptions::LINEAR,
            );
            let sz = d.max - d.min;
            let kb = fs::metadata(path).map(|m| m.len() as f64 / 1024.0).unwrap_or(0.0);
            Thumb {
                tex: Some(tex),
                info: format!(
                    "{} stitches | {} colors\n{:.1} x {:.1} mm | {:.1} KB",
                    d.stitch_count,
                    d.color_blocks,
                    sz.x / 10.0,
                    sz.y / 10.0,
                    kb
                ),
            }
        }
        Err(e) => Thumb { tex: None, info: e },
    }
}

fn short_name(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if name.chars().count() > 22 {
        let head: String = name.chars().take(20).collect();
        format!("{head}...")
    } else {
        name
    }
}

fn user_places() -> Vec<(String, PathBuf)> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Vec::new() };
    let mut out = vec![("Home".to_string(), home.clone())];
    let cfg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let dirs = fs::read_to_string(cfg.join("user-dirs.dirs")).unwrap_or_default();
    let home_str = home.to_string_lossy().into_owned();
    let lookup = |key: &str, fallback: &str| -> PathBuf {
        let prefix = format!("XDG_{key}_DIR=");
        for line in dirs.lines() {
            if let Some(rest) = line.trim().strip_prefix(prefix.as_str()) {
                let v = rest.trim().trim_matches('"').replace("$HOME", &home_str);
                return PathBuf::from(v);
            }
        }
        home.join(fallback)
    };
    for (label, key, fallback) in [
        ("Desktop", "DESKTOP", "Desktop"),
        ("Documents", "DOCUMENTS", "Documents"),
        ("Downloads", "DOWNLOAD", "Downloads"),
        ("Pictures", "PICTURES", "Pictures"),
    ] {
        let p = lookup(key, fallback);
        if p.is_dir() && p != home {
            out.push((label.to_string(), p.canonicalize().unwrap_or(p)));
        }
    }
    out
}

fn devices() -> Vec<(String, PathBuf)> {
    let mut out = vec![("File system".to_string(), PathBuf::from("/"))];
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    for line in mounts.lines() {
        let mut it = line.split_whitespace();
        let (_dev, mp) = (it.next(), it.next());
        let Some(mp) = mp else { continue };
        let mp = mp.replace("\\040", " ");
        if mp.starts_with("/media/") || mp.starts_with("/run/media/") || mp.starts_with("/mnt/") {
            let path = PathBuf::from(&mp);
            if path.is_dir() && !out.iter().any(|(_, p)| p == &path) {
                let label = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| mp.clone());
                out.push((label, path));
            }
        }
    }
    out
}

struct Browser {
    places: Vec<(String, PathBuf)>,
    devices: Vec<(String, PathBuf)>,
    dir: PathBuf,
    folders: Vec<PathBuf>,
    files: Vec<PathBuf>,
    selected: Option<PathBuf>,
    path_text: String,
    error: String,
    add_as_layer: bool,
}

impl Browser {
    fn new(dir: PathBuf, add_as_layer: bool) -> Self {
        let mut b = Self {
            places: user_places(),
            devices: devices(),
            dir: PathBuf::new(),
            folders: Vec::new(),
            files: Vec::new(),
            selected: None,
            path_text: String::new(),
            error: String::new(),
            add_as_layer,
        };
        b.navigate(dir);
        b
    }

    fn navigate(&mut self, dir: PathBuf) {
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                self.error = format!("{}: {e}", dir.display());
                return;
            }
        };
        let mut folders = Vec::new();
        let mut files = Vec::new();
        for e in rd.filter_map(|e| e.ok()) {
            let path = e.path();
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if path.is_dir() {
                folders.push(path);
            } else if is_supported_embroidery_file(&path) {
                files.push(path);
            }
        }
        let key = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().to_lowercase());
        folders.sort_by_key(key);
        files.sort_by_key(key);

        self.dir = dir.canonicalize().unwrap_or(dir);
        self.path_text = self.dir.display().to_string();
        self.folders = folders;
        self.files = files;
        self.error.clear();
    }
}

// ---------------------------------------------------------------------------
// Application & Node Editor State
// ---------------------------------------------------------------------------

const ACTUAL_SIZE_ZOOM: f32 = 96.0 / 254.0;

#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    None,
    MoveLayer,
    ResizeLayer,
    RotateLayer,
    PanCanvas,
    DragNode,
    DragHandleIn,
    DragHandleOut,
}

#[derive(Clone, Copy, PartialEq)]
struct NodeSelection {
    path_idx: usize,
    node_idx: usize,
}

struct PesApp {
    layers: Vec<EmbroideryLayer>,
    active_layer: Option<usize>,
    drag_mode: DragMode,
    drag_accum: Vec2,
    drag_start_angle: f32,
    layer_start_rotation: f32,
    lock_aspect: bool,

    // Mode édition de nœuds / courbes de Bézier
    node_edit_mode: bool,
    selected_node: Option<NodeSelection>,

    current: Option<PathBuf>,
    files: Vec<PathBuf>,
    scroll_to_current: bool,
    title_dirty: bool,
    status: String,

    zoom: f32,
    offset: Vec2,
    fit_pending: bool,

    bg: [u8; 3],
    density: f32,
    show_jumps: bool,
    mode: ViewMode,
    export_scale: f32,

    browser: Option<Browser>,
    thumbs: HashMap<PathBuf, Thumb>,
    preview: Option<(PathBuf, Thumb)>,
}

impl PesApp {
    fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Self {
            layers: Vec::new(),
            active_layer: None,
            drag_mode: DragMode::None,
            drag_accum: Vec2::ZERO,
            drag_start_angle: 0.0,
            layer_start_rotation: 0.0,
            lock_aspect: true,
            node_edit_mode: false,
            selected_node: None,
            current: None,
            files: Vec::new(),
            scroll_to_current: false,
            title_dirty: true,
            status: String::new(),
            zoom: 1.0,
            offset: Vec2::ZERO,
            fit_pending: true,
            bg: [190, 190, 195],
            density: 1.0,
            show_jumps: false,
            mode: ViewMode::Embroidery,
            export_scale: 3.0,
            browser: None,
            thumbs: HashMap::new(),
            preview: None,
        };
        if let Some(p) = initial {
            app.load(p, false);
        }
        app
    }

    fn load(&mut self, path: PathBuf, append: bool) {
        match load_layer_from_file(&path) {
            Ok(layer) => {
                if !append {
                    self.layers.clear();
                    self.fit_pending = true;
                    self.refresh_dir(&path);
                    self.current = Some(path);
                    self.scroll_to_current = true;
                    self.title_dirty = true;
                }
                self.layers.push(layer);
                self.active_layer = Some(self.layers.len() - 1);
                self.selected_node = None;
                self.browser = None;
                self.status.clear();
            }
            Err(e) => self.status = format!("{}: {e}", path.display()),
        }
    }

    fn delete_selected_node(&mut self) {
        let (Some(layer_idx), Some(sel)) = (self.active_layer, self.selected_node) else {
            return;
        };
        let Some(layer) = self.layers.get_mut(layer_idx) else { return };
        let Some(ref mut vpaths) = layer.vector_paths else { return };

        if let Some(vp) = vpaths.get_mut(sel.path_idx) {
            if vp.nodes.len() > 2 && sel.node_idx < vp.nodes.len() {
                vp.nodes.remove(sel.node_idx);
                self.selected_node = Some(NodeSelection {
                    path_idx: sel.path_idx,
                    node_idx: sel.node_idx.saturating_sub(1),
                });
                layer.rebuild_from_vector_paths();
            } else if vp.nodes.len() <= 2 && vpaths.len() > 1 {
                vpaths.remove(sel.path_idx);
                self.selected_node = None;
                layer.rebuild_from_vector_paths();
            }
        }
    }

    fn toggle_selected_node_bezier(&mut self) {
        let (Some(layer_idx), Some(sel)) = (self.active_layer, self.selected_node) else {
            return;
        };
        let Some(layer) = self.layers.get_mut(layer_idx) else { return };
        let Some(ref mut vpaths) = layer.vector_paths else { return };

        if let Some(vp) = vpaths.get_mut(sel.path_idx) {
            let count = vp.nodes.len();
            if sel.node_idx >= count {
                return;
            }
            let is_curved = vp.nodes[sel.node_idx].handle_in != Vec2::ZERO
                || vp.nodes[sel.node_idx].handle_out != Vec2::ZERO;
            if is_curved {
                vp.nodes[sel.node_idx].handle_in = Vec2::ZERO;
                vp.nodes[sel.node_idx].handle_out = Vec2::ZERO;
            } else {
                let prev_pos = if sel.node_idx > 0 {
                    vp.nodes[sel.node_idx - 1].pos
                } else if vp.closed {
                    vp.nodes[count - 1].pos
                } else {
                    vp.nodes[sel.node_idx].pos - Vec2::new(20.0, 0.0)
                };
                let next_pos = if sel.node_idx + 1 < count {
                    vp.nodes[sel.node_idx + 1].pos
                } else if vp.closed {
                    vp.nodes[0].pos
                } else {
                    vp.nodes[sel.node_idx].pos + Vec2::new(20.0, 0.0)
                };
                let tangent = (next_pos - prev_pos) * 0.25;
                vp.nodes[sel.node_idx].handle_in = -tangent;
                vp.nodes[sel.node_idx].handle_out = tangent;
            }
            layer.rebuild_from_vector_paths();
        }
    }

    fn combined_bounds(&self) -> Option<(Pos2, Pos2)> {
        let mut min = Pos2::new(f32::MAX, f32::MAX);
        let mut max = Pos2::new(f32::MIN, f32::MIN);
        let mut any = false;
        for l in self.layers.iter().filter(|l| l.visible) {
            let (lmin, lmax) = l.scaled_bounds();
            min = min.min(lmin);
            max = max.max(lmax);
            any = true;
        }
        if !any {
            for l in &self.layers {
                let (lmin, lmax) = l.scaled_bounds();
                min = min.min(lmin);
                max = max.max(lmax);
                any = true;
            }
        }
        any.then_some((min, max))
    }

    fn refresh_dir(&mut self, path: &Path) {
        let Some(dir) = path.parent() else { return };
        let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
        let mut files: Vec<PathBuf> = fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| is_supported_embroidery_file(p))
                    .collect()
            })
            .unwrap_or_default();
        files.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        self.files = files;
    }

    fn step(&mut self, delta: i32) {
        if self.files.is_empty() {
            return;
        }
        let idx = self
            .current
            .as_ref()
            .and_then(|c| self.files.iter().position(|f| f == c))
            .unwrap_or(0) as i32;
        let n = self.files.len() as i32;
        let next = (idx + delta).rem_euclid(n) as usize;
        let path = self.files[next].clone();
        self.load(path, false);
    }

    fn open_browser(&mut self, add_as_layer: bool) {
        let dir = self
            .current
            .as_ref()
            .and_then(|p| p.parent())
            .filter(|d| !d.as_os_str().is_empty())
            .map(|d| d.to_path_buf())
            .or_else(|| std::env::current_dir().ok())
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut b = Browser::new(dir, add_as_layer);
        b.selected = self.current.clone().filter(|c| {
            b.files.contains(&c.canonicalize().unwrap_or(c.clone())) || b.files.contains(c)
        });
        self.browser = Some(b);
    }

    fn browser_ui(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        open: &mut Option<(PathBuf, bool)>,
        want_system_dialog: &mut Option<bool>,
    ) {
        let Some(mut br) = self.browser.take() else { return };
        let mut close = false;
        let mut nav_to: Option<PathBuf> = None;
        let mut select: Option<PathBuf> = None;
        let mut open_req: Option<PathBuf> = None;

        ui.horizontal(|ui| {
            if ui.button("Up").clicked() {
                if let Some(parent) = br.dir.parent() {
                    nav_to = Some(parent.to_path_buf());
                }
            }
            if ui.button("Home").clicked() {
                if let Some(h) = std::env::var_os("HOME") {
                    nav_to = Some(PathBuf::from(h));
                }
            }
            let w = (ui.available_width() - 190.0).max(100.0);
            let r = ui.add(egui::TextEdit::singleline(&mut br.path_text).desired_width(w));
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                nav_to = Some(PathBuf::from(br.path_text.clone()));
            }
            if ui.button("System dialog...").clicked() {
                *want_system_dialog = Some(br.add_as_layer);
            }
            if ui.button("Close").clicked() {
                close = true;
            }
        });
        if !br.error.is_empty() {
            ui.colored_label(Color32::from_rgb(230, 120, 80), &br.error);
        }
        ui.separator();

        egui::SidePanel::left("browser_places")
            .resizable(true)
            .default_width(170.0)
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.strong("Places");
                    for (label, path) in &br.places {
                        if ui.selectable_label(br.dir == *path, label).clicked() {
                            nav_to = Some(path.clone());
                        }
                    }
                    ui.add_space(10.0);
                    ui.strong("Devices");
                    for (label, path) in &br.devices {
                        if ui.selectable_label(br.dir == *path, label).clicked() {
                            nav_to = Some(path.clone());
                        }
                    }
                });
            });

        egui::SidePanel::right("browser_preview")
            .resizable(true)
            .default_width(320.0)
            .show_inside(ui, |ui| {
                let Some(sel) = br.selected.clone() else {
                    ui.add_space(8.0);
                    ui.label("Select a file to preview it.\nDouble-click to open.");
                    return;
                };
                if self.preview.as_ref().map(|(p, _)| p != &sel).unwrap_or(true) {
                    let t = make_thumb(ctx, &sel, PREVIEW_SIZE);
                    self.preview = Some((sel.clone(), t));
                }
                ui.strong(short_name(&sel));
                if let Some((_, t)) = &self.preview {
                    let side = ui.available_width().min(PREVIEW_SIZE as f32);
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
                    if let Some(tex) = &t.tex {
                        ui.painter().image(
                            tex.id(),
                            rect,
                            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }
                    ui.label(&t.info);
                }
                ui.add_space(6.0);
                let btn_label = if br.add_as_layer { "Add Layer" } else { "Open" };
                if ui.button(btn_label).clicked() {
                    open_req = Some(sel);
                }
            });

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if let Some(parent) = br.dir.parent() {
                if ui.selectable_label(false, "[dir]  ..").double_clicked() {
                    nav_to = Some(parent.to_path_buf());
                }
            }
            for d in &br.folders {
                let name = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if ui
                    .selectable_label(false, format!("[dir]  {name}"))
                    .on_hover_text("Double-click to open")
                    .double_clicked()
                {
                    nav_to = Some(d.clone());
                }
            }
            if br.folders.is_empty() && br.files.is_empty() {
                ui.label("No sub-folders, .pes or .svg files here.");
            }
            ui.add_space(6.0);

            let cell = Vec2::new(176.0, 194.0);
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            let mut budget = 4;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(6.0);
                for f in &br.files {
                    let (rect, resp) = ui.allocate_exact_size(cell, Sense::click());
                    let resp = resp.on_hover_text(
                        f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    );
                    let selected = br.selected.as_ref() == Some(f);
                    let painter = ui.painter_at(rect);
                    let fill = if selected {
                        ui.visuals().selection.bg_fill
                    } else if resp.hovered() {
                        ui.visuals().widgets.hovered.weak_bg_fill
                    } else {
                        Color32::TRANSPARENT
                    };
                    painter.rect_filled(rect, 4.0, fill);

                    let img_rect =
                        Rect::from_min_size(rect.min + Vec2::new(8.0, 6.0), Vec2::splat(THUMB_SIZE as f32));
                    if ui.is_rect_visible(rect) {
                        if !self.thumbs.contains_key(f) {
                            if budget > 0 {
                                budget -= 1;
                                let t = make_thumb(ctx, f, THUMB_SIZE);
                                self.thumbs.insert(f.clone(), t);
                            } else {
                                ctx.request_repaint();
                            }
                        }
                        match self.thumbs.get(f) {
                            Some(Thumb { tex: Some(tex), .. }) => {
                                painter.image(tex.id(), img_rect, uv, Color32::WHITE);
                            }
                            Some(_) => {
                                painter.rect_filled(img_rect, 2.0, Color32::from_gray(70));
                                painter.text(
                                    img_rect.center(),
                                    Align2::CENTER_CENTER,
                                    "unreadable",
                                    FontId::proportional(13.0),
                                    Color32::LIGHT_RED,
                                );
                            }
                            None => {
                                painter.rect_filled(img_rect, 2.0, Color32::from_gray(60));
                            }
                        }
                    }
                    painter.text(
                        Pos2::new(rect.center().x, rect.max.y - 14.0),
                        Align2::CENTER_CENTER,
                        short_name(f),
                        FontId::proportional(13.0),
                        ui.visuals().text_color(),
                    );

                    if resp.clicked() {
                        select = Some(f.clone());
                    }
                    if resp.double_clicked() {
                        open_req = Some(f.clone());
                    }
                }
            });
        });

        if !ctx.wants_keyboard_input() && nav_to.is_none() {
            if ctx.input(|i| i.key_pressed(Key::Enter)) {
                if let Some(sel) = &br.selected {
                    open_req = Some(sel.clone());
                }
            }
            if ctx.input(|i| i.key_pressed(Key::Backspace)) {
                if let Some(parent) = br.dir.parent() {
                    nav_to = Some(parent.to_path_buf());
                }
            }
        }

        if let Some(d) = nav_to {
            br.navigate(d);
            br.selected = None;
        }
        if let Some(f) = select {
            br.selected = Some(f);
        }
        if let Some(f) = open_req {
            *open = Some((f, br.add_as_layer));
            close = true;
        }
        if !close {
            self.browser = Some(br);
        }
    }

    fn pick_file(&mut self, add_as_layer: bool) {
        let title = if add_as_layer { "Add PES or SVG layer" } else { "Open a PES or SVG file" };
        if let Some(p) = rfd::FileDialog::new()
            .set_title(title)
            .add_filter("Embroidery & Vector (PES, SVG)", &["pes", "PES", "svg", "SVG"])
            .add_filter("PES embroidery", &["pes", "PES"])
            .add_filter("SVG vector", &["svg", "SVG"])
            .pick_file()
        {
            self.load(p, add_as_layer);
        }
    }

    fn save_png(&mut self) {
        if self.layers.is_empty() {
            return;
        }
        let default_name = self
            .current
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| format!("{}.png", s.to_string_lossy()))
            .unwrap_or_else(|| "design.png".into());
        if let Some(out) = rfd::FileDialog::new()
            .set_title("Export PNG")
            .set_file_name(default_name)
            .add_filter("PNG image", &["png"])
            .save_file()
        {
            self.status = match export_png(
                &self.layers,
                &out,
                self.export_scale,
                self.bg,
                self.mode == ViewMode::Flat,
                self.density,
            ) {
                Ok(()) => format!("Saved {}", out.display()),
                Err(e) => format!("Export failed: {e}"),
            };
        }
    }

    fn save_pes_v6(&mut self) {
        if self.layers.is_empty() {
            return;
        }
        let default_name = self
            .current
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| format!("{}_merged.pes", s.to_string_lossy()))
            .unwrap_or_else(|| "merged.pes".into());
        if let Some(out) = rfd::FileDialog::new()
            .set_title("Export Merged PES v6")
            .set_file_name(default_name)
            .add_filter("PES embroidery", &["pes"])
            .save_file()
        {
            self.status = match export_pes_v6(&self.layers, &out) {
                Ok(()) => format!("Exported PES v6: {}", out.display()),
                Err(e) => format!("PES export failed: {e}"),
            };
        }
    }
}

impl eframe::App for PesApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut open_path: Option<(PathBuf, bool)> = None;
        let mut want_dialog: Option<bool> = None;
        let mut want_save_png = false;
        let mut want_save_pes = false;
        let mut want_system_dialog: Option<bool> = None;
        let mut close_browser = false;
        let mut nav = 0i32;

        // ---- input -------------------------------------------------------
        ctx.input(|i| {
            if let Some(p) = i.raw.dropped_files.iter().find_map(|f| f.path.clone()) {
                open_path = Some((p, !self.layers.is_empty()));
            }
            if i.modifiers.command && i.key_pressed(Key::O) {
                want_dialog = Some(i.modifiers.shift);
            }
            if !self.node_edit_mode {
                if i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::PageDown) {
                    nav = 1;
                }
                if i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::PageUp) {
                    nav = -1;
                }
            }
            if i.key_pressed(Key::F) {
                self.fit_pending = true;
            }
            if i.key_pressed(Key::Escape) {
                close_browser = true;
                self.selected_node = None;
            }
            if self.node_edit_mode && (i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace)) {
                self.delete_selected_node();
            }
        });

        // ---- toolbar -----------------------------------------------------
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Open…").clicked() {
                    want_dialog = Some(false);
                }
                if ui
                    .add_enabled(!self.layers.is_empty(), egui::Button::new("+ Layer…"))
                    .clicked()
                {
                    want_dialog = Some(true);
                }
                ui.add_enabled_ui(self.files.len() > 1, |ui| {
                    if ui.button("Prev").clicked() {
                        nav = -1;
                    }
                    if ui.button("Next").clicked() {
                        nav = 1;
                    }
                });
                ui.separator();

                // Bouton d'activation de l'éditeur de nœuds / courbes de Bézier
                let can_edit_nodes = self.active_layer.is_some();
                if ui
                    .add_enabled(
                        can_edit_nodes,
                        egui::SelectableLabel::new(self.node_edit_mode, "✎ Edit Nodes"),
                    )
                    .on_hover_text("Éditer les points et courbes de Bézier du calque actif")
                    .clicked()
                {
                    self.node_edit_mode = !self.node_edit_mode;
                    if self.node_edit_mode {
                        if let Some(idx) = self.active_layer {
                            if let Some(layer) = self.layers.get_mut(idx) {
                                layer.ensure_vector_paths();
                            }
                        }
                    } else {
                        self.selected_node = None;
                    }
                }

                if self.node_edit_mode {
                    if ui
                        .add_enabled(self.selected_node.is_some(), egui::Button::new("Curve / Line"))
                        .on_hover_text("Basculer le nœud entre courbe de Bézier et angle droit")
                        .clicked()
                    {
                        self.toggle_selected_node_bezier();
                    }
                    if ui
                        .add_enabled(self.selected_node.is_some(), egui::Button::new("Delete Node"))
                        .on_hover_text("Supprimer le point sélectionné (Suppr)")
                        .clicked()
                    {
                        self.delete_selected_node();
                    }
                }

                ui.separator();
                if ui.button("Fit").clicked() {
                    self.fit_pending = true;
                }
                if ui.button("Actual size").clicked() {
                    self.zoom = ACTUAL_SIZE_ZOOM;
                    self.offset = Vec2::ZERO;
                }
                ui.separator();
                ui.selectable_value(&mut self.mode, ViewMode::Embroidery, "Embroidery");
                ui.selectable_value(&mut self.mode, ViewMode::Flat, "Flat 2D");
                ui.add_enabled(
                    self.mode == ViewMode::Embroidery,
                    egui::Slider::new(&mut self.density, 0.05..=1.0)
                        .text("density")
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                        .custom_parser(|t| {
                            t.trim().trim_end_matches('%').parse::<f64>().ok().map(|v| v / 100.0)
                        }),
                );
                ui.checkbox(&mut self.show_jumps, "Jumps");
                ui.label("Bg");
                ui.color_edit_button_srgb(&mut self.bg);
                ui.separator();
                ui.add(
                    egui::DragValue::new(&mut self.export_scale)
                        .range(1.0..=8.0)
                        .speed(0.1)
                        .prefix("x"),
                );
                if ui
                    .add_enabled(!self.layers.is_empty(), egui::Button::new("Save PNG…"))
                    .clicked()
                {
                    want_save_png = true;
                }
                if ui
                    .add_enabled(!self.layers.is_empty(), egui::Button::new("Export PES v6…"))
                    .clicked()
                {
                    want_save_pes = true;
                }
            });
        });

        // ---- status bar --------------------------------------------------
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.node_edit_mode {
                    ui.colored_label(
                        Color32::from_rgb(100, 210, 255),
                        "NODE MODE: Drag nodes/Bezier handles | Double-click segment to add node | Right-click or Del to remove | Alt+drag handle to break tangent",
                    );
                } else if let Some((dmin, dmax)) = self.combined_bounds() {
                    let size = dmax - dmin;
                    let total_stitches: usize = self
                        .layers
                        .iter()
                        .filter(|l| l.visible)
                        .map(|l| l.design.stitch_count)
                        .sum();
                    let total_blocks: usize = self
                        .layers
                        .iter()
                        .filter(|l| l.visible)
                        .map(|l| l.design.color_blocks)
                        .sum();
                    ui.label(format!(
                        "{} layer(s)  |  {} stitches  |  {} colors  |  {:.1} x {:.1} mm  |  {:.0}%",
                        self.layers.len(),
                        total_stitches,
                        total_blocks,
                        size.x / 10.0,
                        size.y / 10.0,
                        self.zoom / ACTUAL_SIZE_ZOOM * 100.0
                    ));
                }
                if !self.status.is_empty() {
                    ui.separator();
                    ui.colored_label(Color32::from_rgb(230, 160, 80), &self.status);
                }
            });
        });

        // ---- file list sidebar (left) ------------------------------------
        if !self.files.is_empty() && self.browser.is_none() {
            egui::SidePanel::left("files")
                .resizable(true)
                .default_width(200.0)
                .show(ctx, |ui| {
                    ui.strong("Files in folder");
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for f in &self.files {
                                let name = f
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                let selected = self.current.as_ref() == Some(f);
                                let r = ui.selectable_label(selected, name);
                                if selected && self.scroll_to_current {
                                    r.scroll_to_me(None);
                                }
                                if r.clicked() && !selected {
                                    open_path = Some((f.clone(), false));
                                }
                            }
                        });
                    self.scroll_to_current = false;
                });
        }

        // ---- layers sidebar (right) --------------------------------------
        if !self.layers.is_empty() && self.browser.is_none() {
            egui::SidePanel::right("layers_panel")
                .resizable(true)
                .default_width(265.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong("Layers");
                        if ui.button("+ Add").clicked() {
                            want_dialog = Some(true);
                        }
                        ui.checkbox(&mut self.lock_aspect, "Lock ratio");
                    });
                    ui.separator();

                    let mut swap: Option<(usize, usize)> = None;
                    let mut remove: Option<usize> = None;
                    let count = self.layers.len();
                    let lock_aspect = self.lock_aspect;

                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for idx in (0..count).rev() {
                            let is_active = self.active_layer == Some(idx);
                            let frame = egui::Frame::group(ui.style()).fill(if is_active {
                                ui.visuals().selection.bg_fill.linear_multiply(0.35)
                            } else {
                                Color32::TRANSPARENT
                            });

                            frame.show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut self.layers[idx].visible, "");
                                    if ui
                                        .selectable_label(is_active, &self.layers[idx].name)
                                        .clicked()
                                    {
                                        self.active_layer = Some(idx);
                                        self.selected_node = None;
                                        if self.node_edit_mode {
                                            self.layers[idx].ensure_vector_paths();
                                        }
                                    }
                                });

                                ui.horizontal(|ui| {
                                    let mut x_mm = self.layers[idx].offset_x as f32 / 10.0;
                                    let mut y_mm = self.layers[idx].offset_y as f32 / 10.0;
                                    ui.label("X:");
                                    if ui
                                        .add(egui::DragValue::new(&mut x_mm).speed(0.1).suffix(" mm"))
                                        .changed()
                                    {
                                        self.layers[idx].offset_x = (x_mm * 10.0).round() as i32;
                                    }
                                    ui.label("Y:");
                                    if ui
                                        .add(egui::DragValue::new(&mut y_mm).speed(0.1).suffix(" mm"))
                                        .changed()
                                    {
                                        self.layers[idx].offset_y = (y_mm * 10.0).round() as i32;
                                    }
                                });

                                ui.horizontal(|ui| {
                                    let mut rot = self.layers[idx].rotation;
                                    ui.label("Rot:");
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut rot)
                                                .range(0.0..=360.0)
                                                .speed(1.0)
                                                .suffix("°"),
                                        )
                                        .changed()
                                    {
                                        self.layers[idx].rotation = rot.rem_euclid(360.0);
                                    }
                                });

                                let base = self.layers[idx].base_size();
                                let mut w_mm = (base.x * self.layers[idx].scale_x) / 10.0;
                                let mut h_mm = (base.y * self.layers[idx].scale_y) / 10.0;
                                ui.horizontal(|ui| {
                                    ui.label("W:");
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut w_mm)
                                                .range(1.0..=1000.0)
                                                .speed(0.1)
                                                .suffix(" mm"),
                                        )
                                        .changed()
                                    {
                                        let new_sx = (w_mm * 10.0 / base.x).clamp(0.05, 20.0);
                                        self.layers[idx].scale_x = new_sx;
                                        if lock_aspect {
                                            self.layers[idx].scale_y = new_sx;
                                        }
                                    }
                                    ui.label("H:");
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut h_mm)
                                                .range(1.0..=1000.0)
                                                .speed(0.1)
                                                .suffix(" mm"),
                                        )
                                        .changed()
                                    {
                                        let new_sy = (h_mm * 10.0 / base.y).clamp(0.05, 20.0);
                                        self.layers[idx].scale_y = new_sy;
                                        if lock_aspect {
                                            self.layers[idx].scale_x = new_sy;
                                        }
                                    }
                                });

                                ui.horizontal(|ui| {
                                    ui.label(format!(
                                        "{:.0}% × {:.0}%",
                                        self.layers[idx].scale_x * 100.0,
                                        self.layers[idx].scale_y * 100.0
                                    ));
                                    if ui.small_button("Reset").clicked() {
                                        self.layers[idx].offset_x = 0;
                                        self.layers[idx].offset_y = 0;
                                        self.layers[idx].scale_x = 1.0;
                                        self.layers[idx].scale_y = 1.0;
                                        self.layers[idx].rotation = 0.0;
                                    }
                                    if idx + 1 < count && ui.small_button("↑").clicked() {
                                        swap = Some((idx, idx + 1));
                                    }
                                    if idx > 0 && ui.small_button("↓").clicked() {
                                        swap = Some((idx, idx - 1));
                                    }
                                    if count > 1 && ui.small_button("✕").clicked() {
                                        remove = Some(idx);
                                    }
                                });
                            });
                            ui.add_space(4.0);
                        }
                    });

                    if let Some((a, b)) = swap {
                        self.layers.swap(a, b);
                        if self.active_layer == Some(a) {
                            self.active_layer = Some(b);
                        } else if self.active_layer == Some(b) {
                            self.active_layer = Some(a);
                        }
                    }
                    if let Some(idx) = remove {
                        self.layers.remove(idx);
                        self.active_layer = self.layers.len().checked_sub(1);
                        self.selected_node = None;
                    }
                });
        }

        // ---- canvas ------------------------------------------------------
        let bg = Color32::from_rgb(self.bg[0], self.bg[1], self.bg[2]);
        let frame = if self.browser.is_some() {
            egui::Frame::central_panel(&ctx.style())
        } else {
            egui::Frame::none().fill(bg)
        };
        egui::CentralPanel::default()
            .frame(frame)
            .show(ctx, |ui| {
                if self.browser.is_some() {
                    self.browser_ui(ctx, ui, &mut open_path, &mut want_system_dialog);
                    return;
                }
                let (resp, painter) =
                    ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
                let rect: Rect = resp.rect;

                let Some((dmin, dmax)) = self.combined_bounds() else {
                    painter.text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "Open a .pes or .svg file (Ctrl+O) or drop files here",
                        FontId::proportional(18.0),
                        Color32::GRAY,
                    );
                    return;
                };
                let size = dmax - dmin;

                // Fit to window
                if self.fit_pending || (!self.node_edit_mode && resp.double_clicked()) {
                    let margin = 30.0;
                    let fx = (rect.width() - 2.0 * margin).max(1.0) / size.x.max(1.0);
                    let fy = (rect.height() - 2.0 * margin).max(1.0) / size.y.max(1.0);
                    self.zoom = fx.min(fy).clamp(0.01, 500.0);
                    self.offset = Vec2::ZERO;
                    self.fit_pending = false;
                }

                let center = rect.center();
                let dc = dmin + size / 2.0;
                let to_screen_with = |p: Pos2, offset: Vec2, zoom: f32| -> Pos2 {
                    center + offset + (p - dc) * zoom
                };
                let screen_to_pes_with = |s: Pos2, offset: Vec2, zoom: f32| -> Pos2 {
                    dc + (s - center - offset) / zoom
                };

                // -----------------------------------------------------------
                // MODE ÉDITION DE NŒUDS / COURBES DE BÉZIER
                // -----------------------------------------------------------
                if self.node_edit_mode {
                    if let Some(layer_idx) = self.active_layer {
                        if let Some(layer) = self.layers.get_mut(layer_idx) {
                            layer.ensure_vector_paths();
                        }

                        // 1. Clic droit sur un nœud -> Suppression immédiate
                        if resp.secondary_clicked() {
                            if let Some(click_pos) = resp.interact_pointer_pos() {
                                let mut to_remove: Option<NodeSelection> = None;
                                if let Some(layer) = self.layers.get(layer_idx) {
                                    if let Some(ref vpaths) = layer.vector_paths {
                                        for (pi, vp) in vpaths.iter().enumerate() {
                                            for (ni, node) in vp.nodes.iter().enumerate() {
                                                let sp = to_screen_with(
                                                    layer.transform_point(node.pos),
                                                    self.offset,
                                                    self.zoom,
                                                );
                                                if click_pos.distance(sp) <= 10.0 {
                                                    to_remove = Some(NodeSelection {
                                                        path_idx: pi,
                                                        node_idx: ni,
                                                    });
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                                if let Some(sel) = to_remove {
                                    self.selected_node = Some(sel);
                                    self.delete_selected_node();
                                }
                            }
                        }

                        // 2. Double-clic sur un segment -> Ajout d'un nouveau nœud
                        if resp.double_clicked() {
                            if let Some(click_pos) = resp.interact_pointer_pos() {
                                if let Some(layer) = self.layers.get_mut(layer_idx) {
                                    let local_click = layer.inverse_transform_point(
                                        screen_to_pes_with(click_pos, self.offset, self.zoom),
                                    );
                                    let mut best_hit: Option<(usize, usize, f32, Pos2, bool)> = None;

                                    if let Some(ref vpaths) = layer.vector_paths {
                                        for (pi, vp) in vpaths.iter().enumerate() {
                                            let seg_count = if vp.closed {
                                                vp.nodes.len()
                                            } else {
                                                vp.nodes.len().saturating_sub(1)
                                            };
                                            for si in 0..seg_count {
                                                let n0 = &vp.nodes[si];
                                                let n1 = &vp.nodes[(si + 1) % vp.nodes.len()];
                                                let is_bezier = n0.handle_out != Vec2::ZERO
                                                || n1.handle_in != Vec2::ZERO;
                                                let poly = sample_segment_polyline(n0, n1);
                                                for pair in poly.windows(2) {
                                                    let (a, b) = (pair[0], pair[1]);
                                                    let ab = b - a;
                                                    let t = if ab.length_sq() > 1e-6 {
                                                        ((local_click - a).dot(ab) / ab.length_sq())
                                                        .clamp(0.0, 1.0)
                                                    } else {
                                                        0.0
                                                    };
                                                    let proj = a + ab * t;
                                                    let screen_dist = click_pos.distance(
                                                        to_screen_with(
                                                            layer.transform_point(proj),
                                                                       self.offset,
                                                                       self.zoom,
                                                        ),
                                                    );
                                                    if screen_dist <= 12.0 {
                                                        if best_hit
                                                            .map_or(true, |(_, _, d, _, _)| screen_dist < d)
                                                            {
                                                                best_hit = Some((
                                                                    pi,
                                                                    si + 1,
                                                                    screen_dist,
                                                                    proj,
                                                                    is_bezier,
                                                                ));
                                                            }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    if let Some((pi, insert_idx, _, proj, is_bezier)) = best_hit {
                                        let (hin, hout) = if is_bezier {
                                            (Vec2::new(-15.0, 0.0), Vec2::new(15.0, 0.0))
                                        } else {
                                            (Vec2::ZERO, Vec2::ZERO)
                                        };
                                        if let Some(ref mut vpaths) = layer.vector_paths {
                                            vpaths[pi].nodes.insert(
                                                insert_idx,
                                                VectorNode {
                                                    pos: proj,
                                                    handle_in: hin,
                                                    handle_out: hout,
                                                },
                                            );
                                            self.selected_node = Some(NodeSelection {
                                                path_idx: pi,
                                                node_idx: insert_idx,
                                            });
                                        }
                                        layer.rebuild_from_vector_paths();
                                    }
                                }
                            }
                        }

                        // 3. Début de glisser-déposer (Poignée Bézier, Nœud d'ancrage, ou Panoramique)
                        if resp.drag_started_by(egui::PointerButton::Primary) {
                            if let Some(pos) = resp.interact_pointer_pos() {
                                let mut hit = false;
                                if let Some(layer) = self.layers.get(layer_idx) {
                                    if let Some(ref vpaths) = layer.vector_paths {
                                        // Priorité 1 : poignées de Bézier du nœud sélectionné
                                        if let Some(sel) = self.selected_node {
                                            if let Some(vp) = vpaths.get(sel.path_idx) {
                                                if let Some(node) = vp.nodes.get(sel.node_idx) {
                                                    if node.handle_in != Vec2::ZERO {
                                                        let h_in_s = to_screen_with(
                                                            layer.transform_point(
                                                                node.pos + node.handle_in,
                                                            ),
                                                            self.offset,
                                                            self.zoom,
                                                        );
                                                        if pos.distance(h_in_s) <= 11.0 {
                                                            self.drag_mode = DragMode::DragHandleIn;
                                                            hit = true;
                                                        }
                                                    }
                                                    if !hit && node.handle_out != Vec2::ZERO {
                                                        let h_out_s = to_screen_with(
                                                            layer.transform_point(
                                                                node.pos + node.handle_out,
                                                            ),
                                                            self.offset,
                                                            self.zoom,
                                                        );
                                                        if pos.distance(h_out_s) <= 11.0 {
                                                            self.drag_mode = DragMode::DragHandleOut;
                                                            hit = true;
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        // Priorité 2 : nœuds d'ancrage
                                        if !hit {
                                            for (pi, vp) in vpaths.iter().enumerate() {
                                                for (ni, node) in vp.nodes.iter().enumerate() {
                                                    let sp = to_screen_with(
                                                        layer.transform_point(node.pos),
                                                        self.offset,
                                                        self.zoom,
                                                    );
                                                    if pos.distance(sp) <= 10.0 {
                                                        self.selected_node = Some(NodeSelection {
                                                            path_idx: pi,
                                                            node_idx: ni,
                                                        });
                                                        self.drag_mode = DragMode::DragNode;
                                                        hit = true;
                                                        break;
                                                    }
                                                }
                                                if hit {
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                                if !hit {
                                    self.drag_mode = DragMode::PanCanvas;
                                }
                            }
                        }

                        // 4. Mise à jour pendant le glissement d'un nœud ou d'une poignée Bézier
                        if resp.dragged_by(egui::PointerButton::Primary) {
                            match self.drag_mode {
                                DragMode::DragNode => {
                                    if let (Some(pointer), Some(sel)) =
                                        (resp.interact_pointer_pos(), self.selected_node)
                                    {
                                        if let Some(layer) = self.layers.get_mut(layer_idx) {
                                            let local_pos = layer.inverse_transform_point(
                                                screen_to_pes_with(pointer, self.offset, self.zoom),
                                            );
                                            if let Some(ref mut vpaths) = layer.vector_paths {
                                                if let Some(vp) = vpaths.get_mut(sel.path_idx) {
                                                    if let Some(node) = vp.nodes.get_mut(sel.node_idx)
                                                    {
                                                        node.pos = local_pos;
                                                    }
                                                }
                                            }
                                            layer.rebuild_from_vector_paths();
                                        }
                                    }
                                }
                                DragMode::DragHandleIn => {
                                    if let (Some(pointer), Some(sel)) =
                                        (resp.interact_pointer_pos(), self.selected_node)
                                    {
                                        let break_sym = ctx.input(|i| i.modifiers.alt);
                                        if let Some(layer) = self.layers.get_mut(layer_idx) {
                                            let local_pos = layer.inverse_transform_point(
                                                screen_to_pes_with(pointer, self.offset, self.zoom),
                                            );
                                            if let Some(ref mut vpaths) = layer.vector_paths {
                                                if let Some(vp) = vpaths.get_mut(sel.path_idx) {
                                                    if let Some(node) = vp.nodes.get_mut(sel.node_idx)
                                                    {
                                                        node.handle_in = local_pos - node.pos;
                                                        if !break_sym
                                                            && node.handle_out != Vec2::ZERO
                                                        {
                                                            let out_len = node.handle_out.length();
                                                            if node.handle_in.length() > 1e-4 {
                                                                node.handle_out = -node
                                                                    .handle_in
                                                                    .normalized()
                                                                    * out_len;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            layer.rebuild_from_vector_paths();
                                        }
                                    }
                                }
                                DragMode::DragHandleOut => {
                                    if let (Some(pointer), Some(sel)) =
                                        (resp.interact_pointer_pos(), self.selected_node)
                                    {
                                        let break_sym = ctx.input(|i| i.modifiers.alt);
                                        if let Some(layer) = self.layers.get_mut(layer_idx) {
                                            let local_pos = layer.inverse_transform_point(
                                                screen_to_pes_with(pointer, self.offset, self.zoom),
                                            );
                                            if let Some(ref mut vpaths) = layer.vector_paths {
                                                if let Some(vp) = vpaths.get_mut(sel.path_idx) {
                                                    if let Some(node) = vp.nodes.get_mut(sel.node_idx)
                                                    {
                                                        node.handle_out = local_pos - node.pos;
                                                        if !break_sym
                                                            && node.handle_in != Vec2::ZERO
                                                        {
                                                            let in_len = node.handle_in.length();
                                                            if node.handle_out.length() > 1e-4 {
                                                                node.handle_in = -node
                                                                    .handle_out
                                                                    .normalized()
                                                                    * in_len;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            layer.rebuild_from_vector_paths();
                                        }
                                    }
                                }
                                DragMode::PanCanvas | DragMode::None => {
                                    self.offset += resp.drag_delta();
                                }
                                _ => {}
                            }
                        } else if resp.dragged_by(egui::PointerButton::Middle) {
                            self.offset += resp.drag_delta();
                        }

                        if resp.drag_stopped() {
                            self.drag_mode = DragMode::None;
                        }
                    }
                } else {
                    // -----------------------------------------------------------
                    // MODE NORMAL (Déplacement / Redimensionnement / Rotation)
                    // -----------------------------------------------------------
                    let mut active_handles: Option<(Pos2, Pos2)> = None;
                    if let Some(idx) = self.active_layer {
                        if let Some(layer) = self.layers.get(idx) {
                            if layer.visible {
                                let base = layer.base_size();
                                let scaled_w = base.x * layer.scale_x;
                                let scaled_h = base.y * layer.scale_y;
                                let local_center = Vec2::new(scaled_w * 0.5, scaled_h * 0.5);
                                let lo = Vec2::new(layer.offset_x as f32, layer.offset_y as f32);
                                let c_pes = layer.design.min + lo + local_center;

                                let rad = layer.rotation.to_radians();
                                let cos_r = rad.cos();
                                let sin_r = rad.sin();
                                let half_w = scaled_w * 0.5;
                                let half_h = scaled_h * 0.5;

                                let local_corners = [
                                    Vec2::new(-half_w, -half_h),
                                    Vec2::new(half_w, -half_h),
                                    Vec2::new(half_w, half_h),
                                    Vec2::new(-half_w, half_h),
                                ];
                                let screen_corners = local_corners.map(|c| {
                                    let rot_c = Vec2::new(
                                        c.x * cos_r - c.y * sin_r,
                                        c.x * sin_r + c.y * cos_r,
                                    );
                                    to_screen_with(c_pes + rot_c, self.offset, self.zoom)
                                });

                                let resize_handle = screen_corners[2];
                                let top_center_screen =
                                    screen_corners[0].lerp(screen_corners[1], 0.5);
                                let rot_dir = Vec2::new(-sin_r, cos_r);
                                let rotate_handle = top_center_screen - rot_dir * 25.0;

                                active_handles = Some((resize_handle, rotate_handle));

                                if let Some(hover_pos) = resp.hover_pos() {
                                    if hover_pos.distance(resize_handle) <= 12.0 {
                                        ctx.set_cursor_icon(egui::CursorIcon::ResizeNwSe);
                                    } else if hover_pos.distance(rotate_handle) <= 12.0 {
                                        ctx.set_cursor_icon(egui::CursorIcon::Grab);
                                    }
                                }
                            }
                        }
                    }

                    if resp.drag_started_by(egui::PointerButton::Primary) {
                        if let Some(pos) = resp.interact_pointer_pos() {
                            let mut hit_action = false;
                            if let Some((resize_handle, rotate_handle)) = active_handles {
                                if pos.distance(rotate_handle) <= 14.0 {
                                    self.drag_mode = DragMode::RotateLayer;
                                    if let Some(idx) = self.active_layer {
                                        let layer = &self.layers[idx];
                                        let base = layer.base_size();
                                        let scaled_w = base.x * layer.scale_x;
                                        let scaled_h = base.y * layer.scale_y;
                                        let local_center =
                                            Vec2::new(scaled_w * 0.5, scaled_h * 0.5);
                                        let lo = Vec2::new(
                                            layer.offset_x as f32,
                                            layer.offset_y as f32,
                                        );
                                        let center_screen = to_screen_with(
                                            layer.design.min + lo + local_center,
                                            self.offset,
                                            self.zoom,
                                        );
                                        let v = pos - center_screen;
                                        self.drag_start_angle = v.y.atan2(v.x);
                                        self.layer_start_rotation = layer.rotation;
                                    }
                                    hit_action = true;
                                } else if pos.distance(resize_handle) <= 14.0 {
                                    self.drag_mode = DragMode::ResizeLayer;
                                    hit_action = true;
                                }
                            }

                            if !hit_action {
                                let p_pes = screen_to_pes_with(pos, self.offset, self.zoom);
                                let mut clicked_layer = None;
                                if let Some(active) = self.active_layer {
                                    if let Some(l) = self.layers.get(active) {
                                        if l.visible && l.contains_pes_point(p_pes) {
                                            clicked_layer = Some(active);
                                        }
                                    }
                                }
                                if clicked_layer.is_none() {
                                    clicked_layer =
                                        self.layers.iter().enumerate().rev().find_map(|(idx, l)| {
                                            if l.visible && l.contains_pes_point(p_pes) {
                                                Some(idx)
                                            } else {
                                                None
                                            }
                                        });
                                }

                                self.active_layer = clicked_layer;
                                if self.active_layer.is_some() {
                                    self.drag_mode = DragMode::MoveLayer;
                                    self.drag_accum = Vec2::ZERO;
                                } else {
                                    self.drag_mode = DragMode::PanCanvas;
                                }
                            }
                        }
                    }

                    if resp.dragged_by(egui::PointerButton::Primary) {
                        match self.drag_mode {
                            DragMode::RotateLayer => {
                                ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                                if let (Some(idx), Some(pointer)) =
                                    (self.active_layer, resp.interact_pointer_pos())
                                {
                                    let (center_screen, start_rot) = {
                                        let layer = &self.layers[idx];
                                        let base = layer.base_size();
                                        let scaled_w = base.x * layer.scale_x;
                                        let scaled_h = base.y * layer.scale_y;
                                        let local_center =
                                            Vec2::new(scaled_w * 0.5, scaled_h * 0.5);
                                        let lo = Vec2::new(
                                            layer.offset_x as f32,
                                            layer.offset_y as f32,
                                        );
                                        (
                                            to_screen_with(
                                                layer.design.min + lo + local_center,
                                                self.offset,
                                                self.zoom,
                                            ),
                                            self.layer_start_rotation,
                                        )
                                    };

                                    let v = pointer - center_screen;
                                    let current_angle = v.y.atan2(v.x);
                                    let delta_angle =
                                        (current_angle - self.drag_start_angle).to_degrees();
                                    let mut new_rot = start_rot + delta_angle;
                                    if ctx.input(|i| i.modifiers.shift) {
                                        let snap = (new_rot / 45.0).round() * 45.0;
                                        if (new_rot - snap).abs() < 5.0 {
                                            new_rot = snap;
                                        }
                                    }
                                    self.layers[idx].rotation = new_rot.rem_euclid(360.0);
                                }
                            }
                            DragMode::ResizeLayer => {
                                ctx.set_cursor_icon(egui::CursorIcon::ResizeNwSe);
                                if let (Some(idx), Some(pointer)) =
                                    (self.active_layer, resp.interact_pointer_pos())
                                {
                                    let p_pes =
                                        screen_to_pes_with(pointer, self.offset, self.zoom);
                                    let keep_ratio =
                                        self.lock_aspect != ctx.input(|i| i.modifiers.shift);
                                    let layer = &mut self.layers[idx];
                                    let base = layer.base_size();
                                    let lo =
                                        Vec2::new(layer.offset_x as f32, layer.offset_y as f32);
                                    let local_center = Vec2::new(
                                        base.x * layer.scale_x * 0.5,
                                        base.y * layer.scale_y * 0.5,
                                    );
                                    let center_pes = layer.design.min + lo + local_center;

                                    let rel = p_pes - center_pes;
                                    let rad = -layer.rotation.to_radians();
                                    let cos_r = rad.cos();
                                    let sin_r = rad.sin();
                                    let unrotated = Vec2::new(
                                        rel.x * cos_r - rel.y * sin_r,
                                        rel.x * sin_r + rel.y * cos_r,
                                    );
                                    let target_w = (unrotated.x * 2.0).abs().max(10.0);
                                    let target_h = (unrotated.y * 2.0).abs().max(10.0);

                                    if keep_ratio {
                                        let s = (target_w / base.x)
                                            .max(target_h / base.y)
                                            .clamp(0.05, 20.0);
                                        layer.scale_x = s;
                                        layer.scale_y = s;
                                    } else {
                                        layer.scale_x = (target_w / base.x).clamp(0.05, 20.0);
                                        layer.scale_y = (target_h / base.y).clamp(0.05, 20.0);
                                    }
                                }
                            }
                            DragMode::MoveLayer => {
                                if let Some(idx) = self.active_layer {
                                    self.drag_accum += resp.drag_delta() / self.zoom;
                                    let dx = self.drag_accum.x.trunc() as i32;
                                    let dy = self.drag_accum.y.trunc() as i32;
                                    if dx != 0 || dy != 0 {
                                        self.layers[idx].offset_x += dx;
                                        self.layers[idx].offset_y += dy;
                                        self.drag_accum.x -= dx as f32;
                                        self.drag_accum.y -= dy as f32;
                                    }
                                }
                            }
                            DragMode::PanCanvas | DragMode::None => {
                                self.offset += resp.drag_delta();
                            }
                            _ => {}
                        }
                    } else if resp.dragged_by(egui::PointerButton::Middle) {
                        self.offset += resp.drag_delta();
                    }

                    if resp.drag_stopped() {
                        self.drag_mode = DragMode::None;
                    }
                }

                // Zoom around the cursor
                if resp.hovered() {
                    let scroll = ctx.input(|i| i.smooth_scroll_delta.y);
                    let pinch = ctx.input(|i| i.zoom_delta());
                    let factor = (scroll * 0.0015).exp() * pinch;
                    if (factor - 1.0).abs() > f32::EPSILON {
                        if let Some(cursor) = resp.hover_pos() {
                            let new_zoom = (self.zoom * factor).clamp(0.01, 500.0);
                            let ratio = new_zoom / self.zoom;
                            let p = cursor - rect.center();
                            self.offset = p - (p - self.offset) * ratio;
                            self.zoom = new_zoom;
                        }
                    }
                }

                // Paint
                let (zoom, offset) = (self.zoom, self.offset);
                let to_screen = |p: Pos2| -> Pos2 { center + offset + (p - dc) * zoom };
                let width = (THREAD_MM * 10.0 * zoom).max(1.0);
                let col = |c: [u8; 3]| Color32::from_rgb(c[0], c[1], c[2]);
                let view = rect.expand(width * 2.0 + 4.0);
                let visible = |a: Pos2, b: Pos2| Rect::from_two_pos(a, b).intersects(view);

                // Fabric weave in Embroidery mode
                if self.mode == ViewMode::Embroidery {
                    let sp = 6.0 * zoom;
                    if sp >= 5.0 && (rect.width() + rect.height()) / sp < 2000.0 {
                        let o = to_screen(dmin);
                        let dark = Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(0, 0, 0, 9));
                        let light = Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 8));
                        let mut x = o.x + ((rect.left() - o.x) / sp).floor() * sp;
                        while x <= rect.right() {
                            painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], dark);
                            x += sp;
                        }
                        let mut y = o.y + ((rect.top() - o.y) / sp).floor() * sp;
                        while y <= rect.bottom() {
                            painter.line_segment([Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)], light);
                            y += sp;
                        }
                    }
                }

                // Render each visible layer in Z-order
                for (idx, layer) in self.layers.iter().enumerate() {
                    if !layer.visible {
                        continue;
                    }
                    let design = &layer.design;

                    if self.show_jumps {
                        let jump_stroke =
                            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(160, 160, 160, 90));
                        for [a, b] in &design.jumps {
                            painter.line_segment(
                                [to_screen(layer.transform_point(*a)), to_screen(layer.transform_point(*b))],
                                jump_stroke,
                            );
                        }
                    }

                    if self.mode == ViewMode::Flat {
                        let w_thin = (FLAT_MM * 10.0 * zoom).max(FLAT_MIN_PX);
                        let w_fill = (FLAT_FILL_MM * 10.0 * zoom).max(FLAT_MIN_PX);
                        for run in &design.runs {
                            let color = col(thread_rgb(run.color));
                            if design.stitch_count <= 100_000 {
                                for (i, pair) in run.pts.windows(2).enumerate() {
                                    let (a, b) = (
                                        to_screen(layer.transform_point(pair[0])),
                                        to_screen(layer.transform_point(pair[1])),
                                    );
                                    if !visible(a, b) {
                                        continue;
                                    }
                                    let t = run.fill.get(i).copied().unwrap_or(0.0);
                                    let fw = w_thin + (w_fill - w_thin) * t;
                                    painter.line_segment([a, b], Stroke::new(fw, color));
                                    if fw > 2.0 {
                                        painter.circle_filled(a, fw * 0.5, color);
                                        painter.circle_filled(b, fw * 0.5, color);
                                    }
                                }
                            } else {
                                let pts: Vec<Pos2> = run
                                    .pts
                                    .iter()
                                    .map(|p| to_screen(layer.transform_point(*p)))
                                    .collect();
                                painter.add(Shape::line(pts, Stroke::new((w_thin + w_fill) * 0.5, color)));
                            }
                        }
                    } else {
                        let density = self.density;
                        let mut segs: Vec<(Pos2, Pos2, usize)> = Vec::new();
                        for (ri, run) in design.runs.iter().enumerate() {
                            for (i, pair) in run.pts.windows(2).enumerate() {
                                if !keep_stitch(i, density) {
                                    continue;
                                }
                                let (a, b) = (
                                    to_screen(layer.transform_point(pair[0])),
                                    to_screen(layer.transform_point(pair[1])),
                                );
                                if visible(a, b) {
                                    segs.push((a, b, ri));
                                }
                            }
                        }
                        let layer_sets: Vec<[Layer; 6]> = design
                            .runs
                            .iter()
                            .map(|r| thread_layers(thread_rgb(r.color)))
                            .collect();
                        let rich = width >= 3.0 && segs.len() <= 25_000;
                        for (a, b, ri) in segs {
                            let base = thread_rgb(design.runs[ri].color);
                            if rich {
                                paint_thread(&painter, a, b, &layer_sets[ri], width);
                                if width >= 6.0 {
                                    let hole = col(darken(base, 0.3));
                                    painter.circle_filled(a, width * 0.14, hole);
                                    painter.circle_filled(b, width * 0.14, hole);
                                }
                            } else {
                                painter.line_segment([a, b], Stroke::new(width, col(base)));
                            }
                        }
                    }

                    // Overlay d'édition sur le calque actif
                    if self.active_layer == Some(idx) {
                        if self.node_edit_mode {
                            // Affichage des nœuds vectoriels et poignées de Bézier
                            if let Some(ref vpaths) = layer.vector_paths {
                                let guide_stroke = Stroke::new(
                                    1.2_f32,
                                    Color32::from_rgba_unmultiplied(0, 180, 255, 160),
                                );
                                let tangent_stroke =
                                    Stroke::new(1.2_f32, Color32::from_rgb(50, 220, 120));

                                for (pi, vp) in vpaths.iter().enumerate() {
                                    let seg_count = if vp.closed {
                                        vp.nodes.len()
                                    } else {
                                        vp.nodes.len().saturating_sub(1)
                                    };
                                    for si in 0..seg_count {
                                        let n0 = &vp.nodes[si];
                                        let n1 = &vp.nodes[(si + 1) % vp.nodes.len()];
                                        let poly = sample_segment_polyline(n0, n1);
                                        let screen_poly: Vec<Pos2> = poly
                                            .into_iter()
                                            .map(|p| to_screen(layer.transform_point(p)))
                                            .collect();
                                        painter.add(Shape::line(screen_poly, guide_stroke));
                                    }

                                    for (ni, node) in vp.nodes.iter().enumerate() {
                                        let is_sel = self.selected_node
                                            == Some(NodeSelection {
                                                path_idx: pi,
                                                node_idx: ni,
                                            });
                                        let s_pos = to_screen(layer.transform_point(node.pos));

                                        // Si le nœud est sélectionné, dessine ses poignées de Bézier
                                        if is_sel {
                                            if node.handle_in != Vec2::ZERO {
                                                let h_in_s = to_screen(
                                                    layer.transform_point(
                                                        node.pos + node.handle_in,
                                                    ),
                                                );
                                                painter.line_segment(
                                                    [s_pos, h_in_s],
                                                    tangent_stroke,
                                                );
                                                painter.circle_filled(
                                                    h_in_s,
                                                    4.5,
                                                    Color32::from_rgb(50, 220, 120),
                                                );
                                                painter.circle_stroke(
                                                    h_in_s,
                                                    4.5,
                                                    Stroke::new(1.0_f32, Color32::WHITE),
                                                );
                                            }
                                            if node.handle_out != Vec2::ZERO {
                                                let h_out_s = to_screen(
                                                    layer.transform_point(
                                                        node.pos + node.handle_out,
                                                    ),
                                                );
                                                painter.line_segment(
                                                    [s_pos, h_out_s],
                                                    tangent_stroke,
                                                );
                                                painter.circle_filled(
                                                    h_out_s,
                                                    4.5,
                                                    Color32::from_rgb(50, 220, 120),
                                                );
                                                painter.circle_stroke(
                                                    h_out_s,
                                                    4.5,
                                                    Stroke::new(1.0_f32, Color32::WHITE),
                                                );
                                            }
                                        }

                                        let node_rect = Rect::from_center_size(
                                            s_pos,
                                            Vec2::splat(if is_sel { 9.0 } else { 7.0 }),
                                        );
                                        let fill = if is_sel {
                                            Color32::from_rgb(255, 190, 40)
                                        } else {
                                            Color32::from_rgb(40, 160, 255)
                                        };
                                        painter.rect_filled(node_rect, 1.5, fill);
                                        painter.rect_stroke(
                                            node_rect,
                                            1.5,
                                            Stroke::new(1.0_f32, Color32::WHITE),
                                        );
                                    }
                                }
                            }
                        } else {
                            // Boîte de transformation globale (déplacement, échelle, rotation)
                            let base = layer.base_size();
                            let scaled_w = base.x * layer.scale_x;
                            let scaled_h = base.y * layer.scale_y;
                            let local_center = Vec2::new(scaled_w * 0.5, scaled_h * 0.5);
                            let lo = Vec2::new(layer.offset_x as f32, layer.offset_y as f32);
                            let c_pes = layer.design.min + lo + local_center;

                            let rad = layer.rotation.to_radians();
                            let cos_r = rad.cos();
                            let sin_r = rad.sin();
                            let half_w = scaled_w * 0.5;
                            let half_h = scaled_h * 0.5;

                            let local_corners = [
                                Vec2::new(-half_w, -half_h),
                                Vec2::new(half_w, -half_h),
                                Vec2::new(half_w, half_h),
                                Vec2::new(-half_w, half_h),
                            ];
                            let sc = local_corners.map(|c| {
                                let rot_c = Vec2::new(
                                    c.x * cos_r - c.y * sin_r,
                                    c.x * sin_r + c.y * cos_r,
                                );
                                to_screen(c_pes + rot_c)
                            });

                            let accent = Color32::from_rgb(60, 160, 255);
                            for i in 0..4 {
                                painter.line_segment(
                                    [sc[i], sc[(i + 1) % 4]],
                                    Stroke::new(
                                        1.5_f32,
                                        Color32::from_rgba_unmultiplied(60, 160, 255, 190),
                                    ),
                                );
                            }

                            let resize_rect = Rect::from_center_size(sc[2], Vec2::splat(10.0));
                            painter.rect_filled(resize_rect, 2.0, accent);
                            painter.rect_stroke(
                                resize_rect,
                                2.0,
                                Stroke::new(1.0_f32, Color32::WHITE),
                            );

                            let top_center = sc[0].lerp(sc[1], 0.5);
                            let rot_dir = Vec2::new(-sin_r, cos_r);
                            let rot_handle = top_center - rot_dir * 25.0;
                            painter.line_segment(
                                [top_center, rot_handle],
                                Stroke::new(1.0_f32, accent),
                            );
                            let rot_rect = Rect::from_center_size(rot_handle, Vec2::splat(10.0));
                            painter.rect_filled(rot_rect, 5.0, Color32::from_rgb(50, 200, 100));
                            painter.rect_stroke(
                                rot_rect,
                                5.0,
                                Stroke::new(1.0_f32, Color32::WHITE),
                            );
                        }
                    }
                }
            });

        // ---- deferred actions --------------------------------------------
        if close_browser {
            self.browser = None;
        }
        if let Some(add_layer) = want_dialog {
            if self.browser.is_some() {
                self.browser = None;
            } else {
                self.open_browser(add_layer);
            }
        }
        if let Some(add_layer) = want_system_dialog {
            self.pick_file(add_layer);
        }
        if let Some((p, append)) = open_path {
            self.load(p, append);
        }
        if nav != 0 {
            self.step(nav);
        }
        if want_save_png {
            self.save_png();
        }
        if want_save_pes {
            self.save_pes_v6();
        }
        if self.title_dirty {
            let title = match &self.current {
                Some(p) => format!(
                    "{} - {}",
                    p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
                    app_name()
                ),
                None => app_name().to_string(),
            };
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
            self.title_dirty = false;
        }
    }
}

fn app_name() -> &'static str {
    include_str!("../Cargo.toml")
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("display_name")
                .and_then(|r| r.trim_start().strip_prefix('='))
                .map(|r| r.trim().trim_matches('"'))
        })
        .unwrap_or("PES Viewer")
}

fn main() -> eframe::Result<()> {
    let initial = std::env::args().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 750.0])
            .with_title(app_name())
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        app_name(),
        options,
        Box::new(move |cc| Ok(Box::new(PesApp::new(cc, initial)))),
    )
}
