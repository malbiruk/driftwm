//! Smart placement of a new window adjacent to a focused window's cluster.
//!
//! Algorithm: BFS the snap-cluster (closer members first). For each member
//! try *full-fit* (new range ⊆ member's parallel range) then *partial-fit*
//! (overhang into free space). First valid candidate wins.
//!
//! The two-tier collapse: L1 ("clean edge") and L2 ("blocker but full snap
//! fits in the gap") share `clamp(M.center, feasible_range)` — wide range
//! gives L1 (centered on M), narrow sticks to the closer end. L3 ("overhang
//! into free space outside M") is the partial-fit pass.
//!
//! Edge ordering is per-member: the viewport-near edge for the focused
//! window may not be the same direction for a neighbor several columns away.

use std::collections::{HashSet, VecDeque};

use crate::layout::cluster::adjacent_side;
use crate::layout::snap::SnapRect;

/// Below this `|vc - m.center|`, viewport gives no clear directional bias
/// (treat as centered on M). Above, the user has deliberately panned and
/// that direction wins over heuristics.
const VIEWPORT_DEADZONE: f64 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn x_high(&self) -> f64 {
        self.x + self.w
    }
    pub fn y_high(&self) -> f64 {
        self.y + self.h
    }
    pub fn cx(&self) -> f64 {
        self.x + self.w / 2.0
    }
    pub fn cy(&self) -> f64 {
        self.y + self.h / 2.0
    }
    fn to_snap(self) -> SnapRect {
        SnapRect {
            x_low: self.x,
            x_high: self.x_high(),
            y_low: self.y,
            y_high: self.y_high(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Edge {
    Right,
    Bottom,
    Left,
    Top,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum FitMode {
    /// New range ⊆ member's parallel range; no overhang.
    Full,
    /// Positive overlap with member, but new range extends past into free space.
    Partial,
}

/// Place `(new_w, new_h)` adjacent to the focused window's cluster.
/// Returns new rect's top-left (frame coords), or `None` if no fit.
///
/// `windows` is every window's frame rect; `cluster_eligible` is the
/// indices that may serve as anchors (excludes widgets/fullscreen).
/// Ineligible windows still act as obstacles. `viewport_center` is
/// canvas-space; `gap` matches the cluster definition.
pub fn place_auto(
    windows: &[Rect],
    focused_idx: usize,
    cluster_eligible: &HashSet<usize>,
    new_w: f64,
    new_h: f64,
    viewport_center: (f64, f64),
    gap: f64,
) -> Option<(f64, f64)> {
    if focused_idx >= windows.len() || !cluster_eligible.contains(&focused_idx) {
        return None;
    }
    let order = bfs_cluster(windows, focused_idx, cluster_eligible, gap);
    // Bbox of THIS cluster only — distant unrelated clusters shouldn't
    // bias growth direction.
    let cluster_set: HashSet<usize> = order.iter().copied().collect();
    let focused_cluster_bbox = cluster_bbox(windows, &cluster_set);

    // Each anchor exhausts Full + Partial before BFS moves on. So a
    // focused-window Partial beats a neighbor's clean Full — anchor
    // proximity beats fit cleanliness so the new window stays close to
    // where the user is working.
    for m_idx in order {
        let m = windows[m_idx];
        let edges = edge_order_for(
            &m,
            viewport_center,
            windows,
            m_idx,
            cluster_eligible,
            gap,
            focused_cluster_bbox,
        );
        let in_deadzone = (viewport_center.0 - m.cx()).abs() < VIEWPORT_DEADZONE
            && (viewport_center.1 - m.cy()).abs() < VIEWPORT_DEADZONE;

        if in_deadzone {
            // No direction signal — fit quality first (Full > Partial),
            // then compact-cluster attachment count, then edge order.
            // Pulls a 4th window into an L-cluster's corner (forming 2x2)
            // instead of extending a line.
            for mode in [FitMode::Full, FitMode::Partial] {
                let mut best: Option<(usize, (f64, f64))> = None;
                for &edge in &edges {
                    if let Some(pos) = try_place(&m, windows, m_idx, new_w, new_h, edge, mode, gap)
                    {
                        let cand = Rect {
                            x: pos.0,
                            y: pos.1,
                            w: new_w,
                            h: new_h,
                        };
                        let attach =
                            count_attachments(&cand, windows, m_idx, cluster_eligible, gap);
                        if best.is_none_or(|(c, _)| attach > c) {
                            best = Some((attach, pos));
                        }
                    }
                }
                if let Some((_, pos)) = best {
                    return Some(pos);
                }
            }
        } else {
            // Deliberate pan: direction beats fit cleanliness. For each
            // edge, try Full then Partial on the SAME edge before moving
            // on — Partial on the requested edge beats Full on a wrong
            // edge.
            for edge in edges {
                for mode in [FitMode::Full, FitMode::Partial] {
                    if let Some(pos) = try_place(&m, windows, m_idx, new_w, new_h, edge, mode, gap)
                    {
                        return Some(pos);
                    }
                }
            }
        }
    }
    None
}

/// Snap-attachments from `candidate` to existing cluster members,
/// excluding `m_idx` (always adjacent by construction). The compact-cluster
/// scorer uses this to prefer placements touching multiple windows.
fn count_attachments(
    candidate: &Rect,
    windows: &[Rect],
    m_idx: usize,
    eligible: &HashSet<usize>,
    gap: f64,
) -> usize {
    let cand = candidate.to_snap();
    windows
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != m_idx && eligible.contains(&i))
        .filter(|(_, w)| adjacent_side(&cand, &w.to_snap(), gap).is_some())
        .count()
}

/// Edge order with 2D-growth heuristic: prefer edges on the free axis.
/// If `m` already has a neighbor on one axis, prefer the perpendicular
/// axis so the cluster grows 2D rather than continuing the line. Falls
/// back to viewport order when both axes (or neither) are occupied.
///
/// Only fires inside the viewport deadzone — deliberate pan always wins.
/// Within an axis, ordering is inherited from `edge_order`.
fn edge_order_for(
    m: &Rect,
    vc: (f64, f64),
    windows: &[Rect],
    m_idx: usize,
    eligible: &HashSet<usize>,
    gap: f64,
    focused_cluster_bbox: Option<(f64, f64)>,
) -> [Edge; 4] {
    let base = edge_order(m, vc);
    let in_deadzone =
        (vc.0 - m.cx()).abs() < VIEWPORT_DEADZONE && (vc.1 - m.cy()).abs() < VIEWPORT_DEADZONE;
    if !in_deadzone {
        return base;
    }

    let m_snap = m.to_snap();
    let mut x_occupied = false;
    let mut y_occupied = false;
    // Iterating any `eligible` window is equivalent to iterating M's
    // cluster: `adjacent_side` returns Some only for cluster members.
    for (i, w) in windows.iter().enumerate() {
        if i == m_idx || !eligible.contains(&i) {
            continue;
        }
        match adjacent_side(&m_snap, &w.to_snap(), gap) {
            Some(crate::layout::cluster::Side::Left | crate::layout::cluster::Side::Right) => {
                x_occupied = true
            }
            Some(crate::layout::cluster::Side::Top | crate::layout::cluster::Side::Bottom) => {
                y_occupied = true
            }
            None => (),
        }
    }

    // Axis-occupation: M occupied on exactly one axis → grow perpendicular.
    if x_occupied != y_occupied {
        return reshuffle(&base, /* prefer_y = */ x_occupied);
    }

    // Cluster-bbox-aspect: M alone or both-axes occupied. Wider-than-tall
    // cluster prefers vertical growth and vice versa, pulling toward a
    // square. Without this rule, a finished NxN keeps appending columns.
    const BBOX_EPS: f64 = 1.0;
    if let Some((bw, bh)) = focused_cluster_bbox {
        if bw > bh + BBOX_EPS {
            return reshuffle(&base, true);
        }
        if bh > bw + BBOX_EPS {
            return reshuffle(&base, false);
        }
    }

    base
}

fn reshuffle(base: &[Edge; 4], prefer_y: bool) -> [Edge; 4] {
    let is_preferred = |e: Edge| {
        if prefer_y {
            matches!(e, Edge::Top | Edge::Bottom)
        } else {
            matches!(e, Edge::Left | Edge::Right)
        }
    };
    let mut out = [Edge::Right; 4];
    let mut i = 0;
    for &e in base {
        if is_preferred(e) {
            out[i] = e;
            i += 1;
        }
    }
    for &e in base {
        if !is_preferred(e) {
            out[i] = e;
            i += 1;
        }
    }
    out
}

/// (width, height) of the eligible cluster's bounding box. `None` if empty.
fn cluster_bbox(windows: &[Rect], eligible: &HashSet<usize>) -> Option<(f64, f64)> {
    let mut x_low = f64::INFINITY;
    let mut x_high = f64::NEG_INFINITY;
    let mut y_low = f64::INFINITY;
    let mut y_high = f64::NEG_INFINITY;
    let mut any = false;
    for (i, w) in windows.iter().enumerate() {
        if !eligible.contains(&i) {
            continue;
        }
        x_low = x_low.min(w.x);
        x_high = x_high.max(w.x_high());
        y_low = y_low.min(w.y);
        y_high = y_high.max(w.y_high());
        any = true;
    }
    if any {
        Some((x_high - x_low, y_high - y_low))
    } else {
        None
    }
}

/// BFS the snap-adjacency graph restricted to `eligible`. Within each
/// layer, neighbors visit in order of canvas distance to `start` — closer first.
fn bfs_cluster(windows: &[Rect], start: usize, eligible: &HashSet<usize>, gap: f64) -> Vec<usize> {
    let mut visited: HashSet<usize> = HashSet::new();
    let mut order: Vec<usize> = Vec::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    let start_rect = windows[start].to_snap();
    queue.push_back(start);
    visited.insert(start);

    while let Some(idx) = queue.pop_front() {
        order.push(idx);
        let r = windows[idx].to_snap();
        let mut next: Vec<(usize, f64)> = (0..windows.len())
            .filter(|i| !visited.contains(i) && eligible.contains(i))
            .filter(|&i| adjacent_side(&r, &windows[i].to_snap(), gap).is_some())
            .map(|i| {
                let w = windows[i];
                let dx = w.cx() - (start_rect.x_low + start_rect.x_high) / 2.0;
                let dy = w.cy() - (start_rect.y_low + start_rect.y_high) / 2.0;
                (i, dx * dx + dy * dy)
            })
            .collect();
        next.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        for (i, _) in next {
            visited.insert(i);
            queue.push_back(i);
        }
    }
    order
}

/// Edge order around `m`: viewport-facing edge first, then clockwise.
/// Inside the deadzone returns a default order; `place_auto`'s scorer
/// picks the actual direction in that case.
fn edge_order(m: &Rect, vc: (f64, f64)) -> [Edge; 4] {
    let dx = vc.0 - m.cx();
    let dy = vc.1 - m.cy();
    if dx.abs() < VIEWPORT_DEADZONE && dy.abs() < VIEWPORT_DEADZONE {
        return [Edge::Right, Edge::Bottom, Edge::Left, Edge::Top];
    }
    let primary = if dx.abs() >= dy.abs() {
        if dx >= 0.0 { Edge::Right } else { Edge::Left }
    } else if dy >= 0.0 {
        Edge::Bottom
    } else {
        Edge::Top
    };
    rotate_cw(primary)
}

fn rotate_cw(e: Edge) -> [Edge; 4] {
    match e {
        Edge::Right => [Edge::Right, Edge::Bottom, Edge::Left, Edge::Top],
        Edge::Bottom => [Edge::Bottom, Edge::Left, Edge::Top, Edge::Right],
        Edge::Left => [Edge::Left, Edge::Top, Edge::Right, Edge::Bottom],
        Edge::Top => [Edge::Top, Edge::Right, Edge::Bottom, Edge::Left],
    }
}

/// Place a new rect against `m`'s `edge`. Returns top-left or `None`.
///
/// Parallel/perpendicular projection: Right/Left → par=y, perp=x;
/// Top/Bottom → par=x, perp=y. Forbidden intervals come from other windows
/// whose perp extent (gap-padded) overlaps the new rect's perp extent.
/// Within each free interval, the candidate is `clamp(M.par_center,
/// feasible_range)`. Full constrains feasible to inside M's parallel range;
/// Partial only requires positive overlap with M.
#[allow(clippy::too_many_arguments)]
fn try_place(
    m: &Rect,
    windows: &[Rect],
    m_idx: usize,
    new_w: f64,
    new_h: f64,
    edge: Edge,
    mode: FitMode,
    gap: f64,
) -> Option<(f64, f64)> {
    let (m_par_lo, m_par_hi, new_perp_lo, new_perp_hi, new_par_len) = match edge {
        Edge::Right => {
            let perp_lo = m.x_high() + gap;
            (m.y, m.y_high(), perp_lo, perp_lo + new_w, new_h)
        }
        Edge::Left => {
            let perp_hi = m.x - gap;
            (m.y, m.y_high(), perp_hi - new_w, perp_hi, new_h)
        }
        Edge::Bottom => {
            let perp_lo = m.y_high() + gap;
            (m.x, m.x_high(), perp_lo, perp_lo + new_h, new_w)
        }
        Edge::Top => {
            let perp_hi = m.y - gap;
            (m.x, m.x_high(), perp_hi - new_h, perp_hi, new_w)
        }
    };
    let m_par_center = (m_par_lo + m_par_hi) / 2.0;

    let mut forbidden: Vec<(f64, f64)> = Vec::new();
    for (i, w) in windows.iter().enumerate() {
        if i == m_idx {
            continue;
        }
        let (w_par_lo, w_par_hi, w_perp_lo, w_perp_hi) = match edge {
            Edge::Right | Edge::Left => (w.y, w.y_high(), w.x, w.x_high()),
            Edge::Bottom | Edge::Top => (w.x, w.x_high(), w.y, w.y_high()),
        };
        if w_perp_hi + gap <= new_perp_lo || w_perp_lo >= new_perp_hi + gap {
            continue;
        }
        forbidden.push((w_par_lo - new_par_len - gap, w_par_hi + gap));
    }
    let free = compute_free_intervals(&forbidden);

    let mut best: Option<(f64, f64)> = None;
    for (a, b) in free {
        // Full-fit feasible: par_lo s.t. new range ⊆ [a,b] AND M's parallel
        // range. `b` is inclusive — forbidden intervals are open, snap
        // contact with the next blocker is allowed.
        let full_lo = a.max(m_par_lo);
        let full_hi = b.min(m_par_hi - new_par_len);
        let full_feasible = full_hi >= full_lo;

        let (lo_anchor, hi_anchor) = match mode {
            FitMode::Full => {
                if !full_feasible {
                    continue;
                }
                (full_lo, full_hi)
            }
            FitMode::Partial => {
                if full_feasible {
                    continue; // Full pass would have returned this.
                }
                // par_lo ∈ [a, b] AND positive overlap with M.
                let lo = a.max(m_par_lo - new_par_len + 1e-9);
                let hi = b.min(m_par_hi - 1e-9);
                if hi < lo {
                    continue;
                }
                (lo, hi)
            }
        };
        let new_par_lo = (m_par_center - new_par_len / 2.0).clamp(lo_anchor, hi_anchor);
        let new_par_center = new_par_lo + new_par_len / 2.0;
        let dist = (new_par_center - m_par_center).abs();
        if best.is_none_or(|(_, d)| dist < d) {
            best = Some((new_par_lo, dist));
        }
    }

    let new_par_lo = best?.0;
    let pos = match edge {
        Edge::Right | Edge::Left => (new_perp_lo, new_par_lo),
        Edge::Bottom | Edge::Top => (new_par_lo, new_perp_lo),
    };
    Some(pos)
}

/// Complement of `forbidden` on the real line; overlapping inputs merge.
/// Result is in increasing order, possibly unbounded on either end.
fn compute_free_intervals(forbidden: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut sorted: Vec<(f64, f64)> = forbidden.iter().copied().filter(|&(a, b)| b > a).collect();
    sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (a, b) in sorted {
        if let Some(last) = merged.last_mut()
            && a <= last.1
        {
            last.1 = last.1.max(b);
            continue;
        }
        merged.push((a, b));
    }

    let mut free = Vec::new();
    let mut prev = f64::NEG_INFINITY;
    for (a, b) in merged {
        if a > prev {
            free.push((prev, a));
        }
        prev = b;
    }
    if prev < f64::INFINITY {
        free.push((prev, f64::INFINITY));
    }
    free
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    fn place(
        windows: &[Rect],
        focused_idx: usize,
        new_w: f64,
        new_h: f64,
        vc: (f64, f64),
    ) -> Option<(f64, f64)> {
        let mut eligible = HashSet::new();
        for i in 0..windows.len() {
            eligible.insert(i);
        }
        place_auto(windows, focused_idx, &eligible, new_w, new_h, vc, 4.0)
    }

    #[test]
    fn no_obstacles_centers_on_focused_right_edge() {
        // F = 200x200 at origin. Viewport to the right. Expect Right edge,
        // full-fit, centered along F's y-range.
        let ws = vec![r(0.0, 0.0, 200.0, 200.0)];
        let pos = place(&ws, 0, 100.0, 100.0, (1000.0, 100.0)).unwrap();
        // Right edge: x = 200 + 4 = 204. y centered on F.cy=100: 100 - 50 = 50.
        assert_eq!(pos, (204.0, 50.0));
    }

    #[test]
    fn primary_edge_follows_viewport() {
        // Same F, viewport below. Expect Bottom edge.
        let ws = vec![r(0.0, 0.0, 200.0, 200.0)];
        let pos = place(&ws, 0, 100.0, 100.0, (100.0, 1000.0)).unwrap();
        // Bottom edge: y = 200 + 4 = 204. x centered on F.cx=100: 100 - 50 = 50.
        assert_eq!(pos, (50.0, 204.0));
    }

    #[test]
    fn full_fit_squeezes_below_blocker() {
        // F = 400x400 at origin, blocker on right strip at top half.
        // Right strip free interval: y >= 50 + 4 = 54.
        // F.cy = 200. New window h=100 fits in [54, 400] (length 346).
        // Anchor y = clamp(200 - 50, [54, 400 - 100=300]) = clamp(150, 54, 300) = 150.
        let ws = vec![
            r(0.0, 0.0, 400.0, 400.0),
            r(204.0, 0.0, 100.0, 50.0), // blocker at top-right
        ];
        let pos = place(&ws, 0, 50.0, 100.0, (1000.0, 200.0)).unwrap();
        assert_eq!(pos, (404.0, 150.0));
    }

    #[test]
    fn partial_fit_when_full_doesnt_fit() {
        // Wall every edge so all four Full attempts fail. The right
        // blocker leaves a free interval below it that's too short for
        // full-fit (intersection with F.y_range = [108, 200], length 92
        // < new_h=100) but long enough for partial-fit (overhangs F's
        // bottom into the free interval below F.y_high=200).
        let ws = vec![
            r(0.0, 0.0, 200.0, 200.0),        // F
            r(204.0, -1000.0, 100.0, 1104.0), // right blocker, y=[-1000, 104]
            r(0.0, 204.0, 200.0, 100.0),      // bottom block
            r(-104.0, 0.0, 100.0, 200.0),     // left block
            r(0.0, -104.0, 200.0, 100.0),     // top block
        ];
        let pos = place(&ws, 0, 50.0, 100.0, (1000.0, 100.0)).unwrap();
        // Right Partial: lo=108 (free interval start), par_lo = clamp(50, 108, 200) = 108.
        assert_eq!(pos, (204.0, 108.0));
    }

    #[test]
    fn partial_fit_snaps_to_both_anchor_and_blocker() {
        // Setup:
        //   B (offset right of A's column) snapped to A's top.
        //   A is focused, viewport above A → tries Top edge.
        // The new window C should land at A's top, slightly left of B —
        // snap-flush with A's top AND B's left edge.
        let ws = vec![
            r(0.0, 0.0, 200.0, 200.0),     // A (focused)
            r(50.0, -204.0, 200.0, 200.0), // B above A, offset right by 50
        ];
        let pos = place(&ws, 0, 200.0, 200.0, (100.0, -200.0)).unwrap();
        // Forbidden x for Top: (B.x - new_w - gap, B.x_high + gap) = (-154, 254).
        // Free x: (-inf, -154] ∪ [254, inf). Full-fit infeasible (C ⊆ A.x not in free).
        // Partial: par_lo = clamp(A.cx - new_w/2 = 0, -200+ε, -154) = -154.
        // Position (-154, -204) — snap-adjacent to A (x overlap [0, 46]) AND B (gap=4).
        assert_eq!(pos, (-154.0, -204.0));
    }

    #[test]
    fn viewport_partial_fit_beats_full_fit_on_wrong_edge() {
        // Viewport clearly points Right (outside deadzone). Right has only
        // a Partial fit (overhang); Bottom has a clean Full fit. The user's
        // direction signal wins — the algorithm picks Right Partial rather
        // than detouring to Bottom for a cleaner fit.
        let ws = vec![
            r(0.0, 0.0, 200.0, 200.0),        // F (focused)
            r(204.0, -1000.0, 100.0, 1104.0), // right blocker, y=[-1000, 104]
        ];
        let pos = place(&ws, 0, 50.0, 100.0, (1000.0, 100.0)).unwrap();
        // Right Partial: free interval [108, ∞), par_lo = 108.
        assert_eq!(pos, (204.0, 108.0));
    }

    #[test]
    fn falls_back_to_next_edge_when_blocked() {
        // F = 200x200, fully-blocking wall on the right. Right edge fails
        // both Full and Partial; must fall through to next edge in CW order
        // (Bottom, since viewport is to the right → primary=Right → CW Bottom next).
        let ws = vec![
            r(0.0, 0.0, 200.0, 200.0),
            r(204.0, -1000.0, 100.0, 3000.0), // tall wall right of F
        ];
        let pos = place(&ws, 0, 50.0, 50.0, (1000.0, 100.0)).unwrap();
        // Bottom edge: y = 204. x centered on F.cx=100: 100-25=75.
        assert_eq!(pos, (75.0, 204.0));
    }

    #[test]
    fn expands_to_neighbor_when_focused_is_surrounded() {
        // F surrounded on right (W1) and bottom (W2). W1 has a free right
        // edge that auto can use. Cluster: F → W1 (right), F → W2 (bottom).
        // Order: focused first, then closer neighbor.
        // Focused Right blocked (W1 there). Bottom blocked (W2). Left/Top
        // open but new window fits there too — we choose CW from Right.
        // Wait, viewport is to the right → Right is primary → order is
        // [Right, Bottom, Left, Top]. Right blocked by W1; Bottom blocked
        // by W2; Left at x=-50-4=-54 free (no obstacle), centered y=50.
        // Full-fit on focused's Left → expected (-54, 50).
        let ws = vec![
            r(0.0, 0.0, 200.0, 200.0),
            r(204.0, 0.0, 100.0, 200.0), // right
            r(0.0, 204.0, 200.0, 100.0), // bottom
        ];
        let pos = place(&ws, 0, 50.0, 100.0, (1000.0, 100.0)).unwrap();
        assert_eq!(pos, (-54.0, 50.0));
    }

    #[test]
    fn returns_none_when_focused_not_eligible() {
        let ws = vec![r(0.0, 0.0, 100.0, 100.0)];
        let eligible = HashSet::new(); // empty
        assert!(place_auto(&ws, 0, &eligible, 50.0, 50.0, (0.0, 0.0), 4.0).is_none());
    }

    #[test]
    fn obstacles_block_but_arent_anchors() {
        // F at origin. Widget W1 (not eligible) immediately right of F.
        // BFS won't expand into W1, so anchor stays focused. W1 is still an
        // obstacle, so Right is blocked. Falls through to Bottom.
        let ws = vec![
            r(0.0, 0.0, 200.0, 200.0),
            r(204.0, 0.0, 100.0, 200.0), // widget-like obstacle
        ];
        let mut eligible = HashSet::new();
        eligible.insert(0); // only focused is eligible
        let pos = place_auto(&ws, 0, &eligible, 50.0, 50.0, (1000.0, 100.0), 4.0).unwrap();
        // Right blocked → CW next (Bottom): (75, 204).
        assert_eq!(pos, (75.0, 204.0));
    }

    #[test]
    fn neighbor_anchor_when_focused_completely_walled_off() {
        // F surrounded on all 4 sides. BFS expands to W_right (closest in
        // viewport direction). With viewport clearly to the right (outside
        // deadzone), the 2D-growth heuristic does not override viewport
        // bias — Right edge of W_right wins.
        let ws = vec![
            r(0.0, 0.0, 200.0, 200.0),    // F
            r(204.0, 0.0, 100.0, 200.0),  // W_right (cluster member)
            r(-104.0, 0.0, 100.0, 200.0), // W_left
            r(0.0, 204.0, 200.0, 100.0),  // W_bottom
            r(0.0, -104.0, 200.0, 100.0), // W_top
        ];
        let pos = place(&ws, 0, 50.0, 100.0, (1000.0, 100.0)).unwrap();
        // W_right at (204, 0, 100, 200). Right edge: x = 308. Centered y = 50.
        assert_eq!(pos, (308.0, 50.0));
    }

    #[test]
    fn left_edge_with_diagonal_neighbor_blocking_strip() {
        // Layout (gap=4):
        //     C        ← focused (above B)
        //   A B
        // Spawn next to C, viewport leftish. Left edge of C has A in the
        // strip but A's y range doesn't conflict with C's y range — they
        // can stack vertically. Earlier `hi = b - new_par_len` formula
        // rejected this and fell through to Right.
        let ws = vec![
            r(0.0, 100.0, 100.0, 100.0),   // A
            r(104.0, 100.0, 100.0, 100.0), // B
            r(104.0, -4.0, 100.0, 100.0),  // C (focused, above B)
        ];
        let pos = place(&ws, 2, 100.0, 100.0, (-1000.0, -4.0)).unwrap();
        // C has Bottom neighbor (B), so X axis preferred (free axis).
        // Left edge of C: new at (0, -4), snap-flush with A's top edge.
        assert_eq!(pos, (0.0, -4.0));
    }

    #[test]
    fn growth_prefers_perpendicular_axis_when_one_axis_already_occupied() {
        // F has an existing Left neighbor (cluster mate). Viewport sits at
        // F's center so the base edge_order would default to Right (and
        // the new window would extend the line). The heuristic should
        // prefer Top/Bottom instead.
        let ws = vec![
            r(0.0, 0.0, 200.0, 200.0),    // F
            r(-204.0, 0.0, 200.0, 200.0), // Left neighbor (cluster mate)
        ];
        let pos = place(&ws, 0, 100.0, 100.0, (100.0, 100.0)).unwrap();
        // CW from default (Right when vc≈center) → Bottom is the first
        // perpendicular-axis edge tried; full-fit succeeds there.
        assert_eq!(pos, (50.0, 204.0));
    }

    #[test]
    fn compact_cluster_forms_2x2_from_l_shape() {
        // Layout:
        //   A B
        //   . C       (focused = C)
        // Spawning a 4th window with vc≈center: compact-cluster prefers
        // Left-of-C (touches both C and A) over Right-of-C (touches only C),
        // forming a 2x2 square instead of an L extending right.
        let ws = vec![
            r(0.0, 0.0, 100.0, 100.0),     // A
            r(104.0, 0.0, 100.0, 100.0),   // B
            r(104.0, 104.0, 100.0, 100.0), // C (focused)
        ];
        let pos = place(&ws, 2, 100.0, 100.0, (154.0, 154.0)).unwrap();
        // Compact picks Left of C → new at (0, 104), snap-flush with both
        // C (right edge) and A (bottom edge).
        assert_eq!(pos, (0.0, 104.0));
    }

    #[test]
    fn wider_cluster_prefers_vertical_growth() {
        // 3x4 grid (4 wide, 3 tall). focused = bottom-right, both axes
        // occupied, so axis-occupation gives no signal. Without the
        // bbox-aspect rule, default order picks Right and the cluster
        // appends another 3-tall column. With it, vertical growth wins —
        // new window goes Below the focused, starting to fill out a
        // 4th row toward a 4x4 square.
        let ws = vec![
            r(0.0, 0.0, 100.0, 100.0),     // (0,0)
            r(104.0, 0.0, 100.0, 100.0),   // (1,0)
            r(208.0, 0.0, 100.0, 100.0),   // (2,0)
            r(312.0, 0.0, 100.0, 100.0),   // (3,0)
            r(0.0, 104.0, 100.0, 100.0),   // (0,1)
            r(104.0, 104.0, 100.0, 100.0), // (1,1)
            r(208.0, 104.0, 100.0, 100.0), // (2,1)
            r(312.0, 104.0, 100.0, 100.0), // (3,1)
            r(0.0, 208.0, 100.0, 100.0),   // (0,2)
            r(104.0, 208.0, 100.0, 100.0), // (1,2)
            r(208.0, 208.0, 100.0, 100.0), // (2,2)
            r(312.0, 208.0, 100.0, 100.0), // (3,2) focused (bottom-right)
        ];
        let pos = place(&ws, 11, 100.0, 100.0, (362.0, 258.0)).unwrap();
        // Bbox 4 wide × 3 tall → prefer Y. Bottom of focused at (312, 312).
        assert_eq!(pos, (312.0, 312.0));
    }

    #[test]
    fn growth_falls_back_to_viewport_when_both_axes_occupied() {
        // F has neighbors on both axes. Heuristic disabled → viewport
        // tiebreak picks Right (vc to the right of F).
        let ws = vec![
            r(0.0, 0.0, 200.0, 200.0),    // F (Right edge of F is open)
            r(-204.0, 0.0, 200.0, 200.0), // Left neighbor
            r(0.0, -204.0, 200.0, 200.0), // Top neighbor
        ];
        let pos = place(&ws, 0, 100.0, 100.0, (1000.0, 100.0)).unwrap();
        assert_eq!(pos, (204.0, 50.0));
    }

    #[test]
    fn free_interval_complement_basic() {
        let free = compute_free_intervals(&[(0.0, 10.0), (20.0, 30.0)]);
        assert_eq!(free.len(), 3);
        assert_eq!(free[0].0, f64::NEG_INFINITY);
        assert_eq!(free[0].1, 0.0);
        assert_eq!(free[1], (10.0, 20.0));
        assert_eq!(free[2].0, 30.0);
        assert!(free[2].1.is_infinite());
    }

    #[test]
    fn free_interval_merges_overlapping() {
        let free = compute_free_intervals(&[(0.0, 10.0), (5.0, 15.0)]);
        // Merged: [0, 15]. Free: (-inf, 0) ∪ (15, inf).
        assert_eq!(free.len(), 2);
        assert_eq!(free[0].1, 0.0);
        assert_eq!(free[1].0, 15.0);
    }
}
