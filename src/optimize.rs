//! Stitch optimizer tuned for the Brother PR680W (6 needles, hoops 300x200 / 180x130 /
//! 100x100 / 60x40 mm, up to 1000 spm, trims on jump stitches).
//!
//! Goals: fewer needle penetrations, fewer jumps (every jump costs a trim + thread tails on
//! this machine), fewer colour changes, and never exceed what the machine can sew.
//!
//! Pipeline, per continuous run of stitches (a "run" starts at every jump / colour change):
//!  1. drop stitches shorter than `min_stitch_mm` (the machine's "short stitch deletion")
//!  2. Douglas-Peucker: remove points that are within `tolerance_mm` of a straight line
//!  3. re-split segments longer than `max_stitch_mm` into equal steps (hard cap 12 mm)
//!  4. group runs by thread colour (optional), order them with a nearest-neighbour tour:
//!     open runs may be reversed, closed runs may start at their nearest point
//!  5. runs separated by <= `join_gap_mm` are connected with one stitch instead of jump+trim
//!
//! Units: design coordinates are 0.1 mm.

use crate::{Stitch, StitchKind};

type P = [f32; 2];

/// Largest stitch length we ever emit (PEC limit is ~12.1 mm).
pub const HARD_MAX_STITCH_MM: f32 = 12.0;
/// Needles on the PR680W.
pub const PR680W_NEEDLES: usize = 6;
/// Above this many runs the nearest-neighbour ordering is skipped (O(n^2)).
const MAX_RUNS_FOR_REORDER: usize = 2500;

/// PR680W frames as (name, long side mm, short side mm), smallest first.
pub const PR680W_HOOPS: [(&str, f32, f32); 4] = [
    ("60 x 40 mm", 60.0, 40.0),
    ("100 x 100 mm", 100.0, 100.0),
    ("180 x 130 mm", 180.0, 130.0),
    ("300 x 200 mm", 300.0, 200.0),
];

#[derive(Clone, Debug)]
pub struct OptimizeOptions {
    /// Longest stitch allowed (1..=12 mm). Longer = fewer points; ~6 mm keeps satin/fill intact.
    pub max_stitch_mm: f32,
    /// Stitches shorter than this are removed.
    pub min_stitch_mm: f32,
    /// Max deviation allowed when removing "almost aligned" points.
    pub tolerance_mm: f32,
    /// Gaps up to this size are bridged with a stitch instead of a jump (0 = never).
    pub join_gap_mm: f32,
    /// Reorder runs inside each colour to shorten the travel.
    pub reorder: bool,
    /// Sew every colour once, even if it appears in several blocks (fewer colour changes,
    /// but changes the stacking order of overlapping areas).
    pub merge_colors: bool,
}

impl Default for OptimizeOptions {
    fn default() -> Self {
        Self {
            max_stitch_mm: 6.0,
            min_stitch_mm: 0.3,
            tolerance_mm: 0.1,
            join_gap_mm: 1.5,
            reorder: true,
            merge_colors: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub stitches_before: usize,
    pub stitches_after: usize,
    pub jumps_before: usize,
    pub jumps_after: usize,
    pub blocks_before: usize,
    pub blocks_after: usize,
    pub distinct_colors: usize,
    pub travel_before_mm: f32,
    pub travel_after_mm: f32,
    pub longest_stitch_mm: f32,
    pub width_mm: f32,
    pub height_mm: f32,
    /// Smallest PR680W hoop that fits the design, if any.
    pub hoop: Option<&'static str>,
    /// True when run ordering was skipped because there are too many runs.
    pub reorder_skipped: bool,
}

impl Report {
    pub fn saved_percent(&self) -> f32 {
        if self.stitches_before == 0 {
            0.0
        } else {
            100.0 * (self.stitches_before as f32 - self.stitches_after as f32) / self.stitches_before as f32
        }
    }
    /// Estimated sewing time in minutes at the given speed (stitches per minute).
    pub fn minutes_at(&self, spm: f32) -> f32 {
        self.stitches_after as f32 / spm.max(1.0)
    }
}

#[derive(Clone)]
struct Run {
    block: usize,
    color: u8,
    pts: Vec<P>,
    closed: bool,
}

fn dist(a: P, b: P) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

fn seg_dist(p: P, a: P, b: P) -> f32 {
    let (abx, aby) = (b[0] - a[0], b[1] - a[1]);
    let l2 = abx * abx + aby * aby;
    if l2 < 1e-9 {
        return dist(p, a);
    }
    let t = (((p[0] - a[0]) * abx + (p[1] - a[1]) * aby) / l2).clamp(0.0, 1.0);
    dist(p, [a[0] + abx * t, a[1] + aby * t])
}

// ---------------------------------------------------------------------------

fn extract_runs(stitches: &[Stitch], palette: &[u8]) -> Vec<Run> {
    let color_of = |block: usize| palette.get(block).or(palette.last()).copied().unwrap_or(1);
    let mut runs: Vec<Run> = Vec::new();
    let mut cur: Option<Run> = None;
    for s in stitches {
        let p = [s.x as f32, s.y as f32];
        let starts_new = s.kind == StitchKind::Jump || cur.as_ref().map_or(true, |r| r.block != s.block);
        if starts_new {
            if let Some(r) = cur.take() {
                runs.push(r);
            }
            cur = Some(Run { block: s.block, color: color_of(s.block), pts: vec![p], closed: false });
        } else if let Some(r) = cur.as_mut() {
            r.pts.push(p);
        }
    }
    if let Some(r) = cur {
        runs.push(r);
    }
    runs.retain(|r| r.pts.len() >= 2);
    runs
}

fn drop_short(pts: &[P], min: f32) -> Vec<P> {
    let mut out = vec![pts[0]];
    for &p in &pts[1..] {
        if dist(*out.last().unwrap(), p) >= min {
            out.push(p);
        }
    }
    let last = *pts.last().unwrap();
    if *out.last().unwrap() != last {
        if out.len() >= 2 {
            *out.last_mut().unwrap() = last; // keep the true end of the run
        } else {
            out.push(last);
        }
    }
    out
}

fn douglas_peucker(pts: &[P], tol: f32) -> Vec<P> {
    let n = pts.len();
    if n < 3 {
        return pts.to_vec();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut stack = vec![(0usize, n - 1)];
    while let Some((a, b)) = stack.pop() {
        if b <= a + 1 {
            continue;
        }
        let (mut md, mut mi) = (-1.0f32, a);
        for i in a + 1..b {
            let d = seg_dist(pts[i], pts[a], pts[b]);
            if d > md {
                md = d;
                mi = i;
            }
        }
        if md > tol {
            keep[mi] = true;
            stack.push((a, mi));
            stack.push((mi, b));
        }
    }
    (0..n).filter(|&i| keep[i]).map(|i| pts[i]).collect()
}

fn split_long(pts: &[P], max: f32) -> Vec<P> {
    let mut out = vec![pts[0]];
    for w in pts.windows(2) {
        let d = dist(w[0], w[1]);
        let n = (d / max).ceil().max(1.0) as usize;
        for k in 1..=n {
            let t = k as f32 / n as f32;
            let p = [(w[0][0] + (w[1][0] - w[0][0]) * t).round(), (w[0][1] + (w[1][1] - w[0][1]) * t).round()];
            if p != *out.last().unwrap() {
                out.push(p);
            }
        }
    }
    out
}

fn clean_run(r: &mut Run, min: f32, tol: f32, max: f32) {
    let closed = r.pts.len() >= 4 && dist(r.pts[0], *r.pts.last().unwrap()) < 2.0;
    let pts = drop_short(&r.pts, min);
    let pts = douglas_peucker(&pts, tol);
    r.pts = split_long(&pts, max);
    r.closed = closed && r.pts.len() >= 4;
    if r.closed {
        let first = r.pts[0];
        *r.pts.last_mut().unwrap() = first; // keep the loop exactly closed
    }
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

/// Cost and orientation of visiting `r` from `cur`: (distance, reversed, rotate_to).
fn visit_cost(r: &Run, cur: P) -> (f32, bool, usize) {
    if r.closed {
        let ring = r.pts.len() - 1;
        let stride = (ring / 64).max(1);
        let (mut bd, mut bi) = (f32::MAX, 0usize);
        let mut i = 0;
        while i < ring {
            let d = dist(cur, r.pts[i]);
            if d < bd {
                bd = d;
                bi = i;
            }
            i += stride;
        }
        (bd, false, bi)
    } else {
        let ds = dist(cur, r.pts[0]);
        let de = dist(cur, *r.pts.last().unwrap());
        if de < ds {
            (de, true, 0)
        } else {
            (ds, false, 0)
        }
    }
}

fn orient(r: &mut Run, reversed: bool, rotate: usize) {
    if reversed {
        r.pts.reverse();
    }
    if r.closed && rotate > 0 {
        r.pts.pop();
        r.pts.rotate_left(rotate);
        let first = r.pts[0];
        r.pts.push(first);
    }
}

fn nearest_neighbour_order(mut runs: Vec<Run>, start: P) -> Vec<Run> {
    let mut out = Vec::with_capacity(runs.len());
    let mut cur = start;
    while !runs.is_empty() {
        let mut best = (f32::MAX, false, 0usize, 0usize);
        for (i, r) in runs.iter().enumerate() {
            let (d, rev, rot) = visit_cost(r, cur);
            if d < best.0 {
                best = (d, rev, rot, i);
            }
        }
        let mut r = runs.swap_remove(best.3);
        orient(&mut r, best.1, best.2);
        cur = *r.pts.last().unwrap();
        out.push(r);
    }
    out
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

struct Counts {
    stitches: usize,
    jumps: usize,
    blocks: usize,
    travel_mm: f32,
    longest_mm: f32,
    min: P,
    max: P,
}

fn counts(s: &[Stitch]) -> Counts {
    let mut c = Counts {
        stitches: 0,
        jumps: 0,
        blocks: 0,
        travel_mm: 0.0,
        longest_mm: 0.0,
        min: [f32::MAX; 2],
        max: [f32::MIN; 2],
    };
    let mut prev: Option<P> = None;
    let mut last_block: Option<usize> = None;
    for st in s {
        let p = [st.x as f32, st.y as f32];
        c.min = [c.min[0].min(p[0]), c.min[1].min(p[1])];
        c.max = [c.max[0].max(p[0]), c.max[1].max(p[1])];
        if last_block != Some(st.block) {
            c.blocks += 1;
            last_block = Some(st.block);
        }
        match st.kind {
            StitchKind::Jump => {
                c.jumps += 1;
                if let Some(q) = prev {
                    c.travel_mm += dist(q, p) / 10.0;
                }
            }
            StitchKind::Stitch => {
                c.stitches += 1;
                if let Some(q) = prev {
                    c.longest_mm = c.longest_mm.max(dist(q, p) / 10.0);
                }
            }
        }
        prev = Some(p);
    }
    c
}

pub fn fitting_hoop(w_mm: f32, h_mm: f32) -> Option<&'static str> {
    let (long, short) = (w_mm.max(h_mm), w_mm.min(h_mm));
    PR680W_HOOPS
        .iter()
        .find(|(_, l, s)| long <= *l && short <= *s)
        .map(|(n, _, _)| *n)
}

// ---------------------------------------------------------------------------

pub fn optimize(stitches: &[Stitch], palette: &[u8], o: &OptimizeOptions) -> (Vec<Stitch>, Vec<u8>, Report) {
    let before = counts(stitches);
    let mut distinct: Vec<u8> = palette.to_vec();
    distinct.sort_unstable();
    distinct.dedup();

    let mut report = Report {
        stitches_before: before.stitches,
        jumps_before: before.jumps,
        blocks_before: before.blocks,
        travel_before_mm: before.travel_mm,
        distinct_colors: distinct.len(),
        ..Report::default()
    };

    let mut runs = extract_runs(stitches, palette);
    if runs.is_empty() {
        report.stitches_after = before.stitches;
        report.jumps_after = before.jumps;
        report.blocks_after = before.blocks;
        report.travel_after_mm = before.travel_mm;
        report.longest_stitch_mm = before.longest_mm;
        report.width_mm = (before.max[0] - before.min[0]).max(0.0) / 10.0;
        report.height_mm = (before.max[1] - before.min[1]).max(0.0) / 10.0;
        report.hoop = fitting_hoop(report.width_mm, report.height_mm);
        return (stitches.to_vec(), palette.to_vec(), report);
    }

    let max_mm = o.max_stitch_mm.clamp(1.0, HARD_MAX_STITCH_MM);
    let (min, tol, max) = (o.min_stitch_mm.max(0.0) * 10.0, o.tolerance_mm.max(0.0) * 10.0, max_mm * 10.0);
    let join = o.join_gap_mm.clamp(0.0, max_mm) * 10.0;

    for r in runs.iter_mut() {
        clean_run(r, min, tol, max);
    }
    runs.retain(|r| r.pts.len() >= 2);

    // group by colour (or by original block)
    struct Group {
        key: usize,
        color: u8,
        runs: Vec<Run>,
    }
    let mut groups: Vec<Group> = Vec::new();
    for r in runs {
        let key = if o.merge_colors { r.color as usize } else { r.block };
        match groups.iter_mut().find(|g| g.key == key) {
            Some(g) => g.runs.push(r),
            None => groups.push(Group { key, color: r.color, runs: vec![r] }),
        }
    }

    let total_runs: usize = groups.iter().map(|g| g.runs.len()).sum();
    let do_reorder = o.reorder && total_runs <= MAX_RUNS_FOR_REORDER;
    report.reorder_skipped = o.reorder && !do_reorder;

    let mut out: Vec<Stitch> = Vec::new();
    let mut new_palette: Vec<u8> = Vec::new();
    let mut cursor: P = groups[0].runs[0].pts[0];

    for (gi, g) in groups.into_iter().enumerate() {
        new_palette.push(g.color);
        let ordered = if do_reorder { nearest_neighbour_order(g.runs, cursor) } else { g.runs };
        let mut last: Option<P> = None;
        for run in ordered {
            let first = run.pts[0];
            let mut skip_first = false;
            let mut first_kind = StitchKind::Jump;
            if let Some(l) = last {
                let d = dist(l, first);
                if d < min.max(1.0) {
                    skip_first = true; // already there
                } else if d <= join {
                    first_kind = StitchKind::Stitch; // bridge instead of jump + trim
                }
            }
            for (i, &p) in run.pts.iter().enumerate() {
                let kind = if i == 0 { first_kind } else { StitchKind::Stitch };
                if i == 0 && skip_first {
                    continue;
                }
                if kind == StitchKind::Stitch {
                    if let Some(prev) = out.last() {
                        if prev.x == p[0] as i32 && prev.y == p[1] as i32 {
                            continue;
                        }
                    }
                }
                out.push(Stitch { x: p[0] as i32, y: p[1] as i32, kind, block: gi });
            }
            last = Some(*run.pts.last().unwrap());
        }
        if let Some(l) = last {
            cursor = l;
        }
    }

    let after = counts(&out);
    report.stitches_after = after.stitches;
    report.jumps_after = after.jumps;
    report.blocks_after = after.blocks;
    report.travel_after_mm = after.travel_mm;
    report.longest_stitch_mm = after.longest_mm;
    report.width_mm = (after.max[0] - after.min[0]).max(0.0) / 10.0;
    report.height_mm = (after.max[1] - after.min[1]).max(0.0) / 10.0;
    report.hoop = fitting_hoop(report.width_mm, report.height_mm);
    (out, new_palette, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(x: i32, y: i32, kind: StitchKind, block: usize) -> Stitch {
        Stitch { x, y, kind, block }
    }

    #[test]
    fn straight_line_collapses_and_respects_max_length() {
        let mut s = vec![st(0, 0, StitchKind::Jump, 0)];
        for i in 1..=40 {
            s.push(st(i * 25, 0, StitchKind::Stitch, 0)); // 1000 units = 100 mm
        }
        let (out, _, rep) = optimize(&s, &[20], &OptimizeOptions::default());
        assert!(rep.stitches_after < rep.stitches_before / 2);
        assert!(rep.longest_stitch_mm <= 6.01, "{}", rep.longest_stitch_mm);
        assert_eq!(out.last().map(|p| (p.x, p.y)), Some((1000, 0)));
    }

    #[test]
    fn close_runs_are_joined_without_a_jump() {
        let s = vec![
            st(0, 0, StitchKind::Jump, 0),
            st(100, 0, StitchKind::Stitch, 0),
            st(110, 0, StitchKind::Jump, 0), // 1 mm gap
            st(110, 100, StitchKind::Stitch, 0),
        ];
        let (_, _, rep) = optimize(&s, &[20], &OptimizeOptions::default());
        assert_eq!(rep.jumps_before, 2);
        assert_eq!(rep.jumps_after, 1);
    }

    #[test]
    fn same_colour_blocks_can_be_merged() {
        let s = vec![
            st(0, 0, StitchKind::Jump, 0),
            st(100, 0, StitchKind::Stitch, 0),
            st(0, 300, StitchKind::Jump, 1),
            st(100, 300, StitchKind::Stitch, 1),
            st(0, 600, StitchKind::Jump, 2),
            st(100, 600, StitchKind::Stitch, 2),
        ];
        let pal = [20u8, 5, 20];
        let opt = OptimizeOptions { merge_colors: true, ..OptimizeOptions::default() };
        let (out, new_pal, rep) = optimize(&s, &pal, &opt);
        assert_eq!(new_pal.len(), 2);
        assert_eq!(rep.blocks_after, 2);
        assert!(out.iter().all(|p| p.block < 2));
    }

    #[test]
    fn nearest_neighbour_shortens_travel() {
        // runs laid out so the given order zig-zags across the design
        let mut s = Vec::new();
        for (i, x) in [0, 900, 100, 800, 200].into_iter().enumerate() {
            let _ = i;
            s.push(st(x, 0, StitchKind::Jump, 0));
            s.push(st(x + 50, 0, StitchKind::Stitch, 0));
        }
        let off = OptimizeOptions { reorder: false, join_gap_mm: 0.0, ..OptimizeOptions::default() };
        let on = OptimizeOptions { reorder: true, join_gap_mm: 0.0, ..OptimizeOptions::default() };
        let (_, _, a) = optimize(&s, &[20], &off);
        let (_, _, b) = optimize(&s, &[20], &on);
        assert!(b.travel_after_mm < a.travel_after_mm);
    }

    #[test]
    fn hoop_selection() {
        assert_eq!(fitting_hoop(50.0, 30.0), Some("60 x 40 mm"));
        assert_eq!(fitting_hoop(250.0, 190.0), Some("300 x 200 mm"));
        assert_eq!(fitting_hoop(190.0, 250.0), Some("300 x 200 mm")); // rotated
        assert_eq!(fitting_hoop(310.0, 100.0), None);
    }
}
