/// Bounding rectangle of a window in canvas coordinates, used for edge snap detection.
#[derive(Clone, Copy, Debug)]
pub struct SnapRect {
    pub x_low: f64,
    pub x_high: f64,
    pub y_low: f64,
    pub y_high: f64,
}

impl SnapRect {
    /// True when the two rects share positive-area overlap (touching edges
    /// don't count).
    pub fn overlaps(&self, other: &SnapRect) -> bool {
        self.x_low < other.x_high
            && other.x_low < self.x_high
            && self.y_low < other.y_high
            && other.y_low < self.y_high
    }
}

/// Parameters for snap candidate search along one axis.
pub struct SnapParams<'a> {
    pub extent: f64,
    pub perp_low: f64,
    pub perp_high: f64,
    pub horizontal: bool,
    pub others: &'a [SnapRect],
    pub gap: f64,
    pub threshold: f64,
    pub break_force: f64,
    pub same_edge: bool,
    pub edge_center: bool,
}

/// Per-axis snap state: tracks the snapped coordinate and the natural position
/// at the moment of engagement (used for directional break detection).
pub struct AxisSnap {
    pub snapped_pos: f64,
    pub natural_at_engage: f64,
}

/// Snap state for both axes plus cooldown after breaking a snap.
#[derive(Default)]
pub struct SnapState {
    pub x: Option<AxisSnap>,
    pub y: Option<AxisSnap>,
    pub cooldown_x: Option<f64>,
    pub cooldown_y: Option<f64>,
}

/// Try to beat the current best with a new candidate.
fn try_candidate(best: &mut Option<(f64, f64)>, snap_pos: f64, dist: f64, threshold: f64) {
    if dist < threshold && best.is_none_or(|(_, bd)| dist < bd) {
        *best = Some((snap_pos, dist));
    }
}

/// Find the best snap candidate along one axis.
///
/// Three candidate families:
/// - **Opposite-edge** (dragged right → other left, dragged left → other right)
///   docks the windows side-by-side with a gap; requires strict perpendicular
///   overlap (so a window only docks to neighbors it actually visually overlaps).
/// - **Same-edge** (left↔left, right↔right) aligns parallel edges exactly.
/// - **Edge-center** snaps the moved window's midpoint to the neighbor's
///   midpoint along the moved axis (centers align along the perp axis).
///
/// Both alignment families fire only when the windows are perpendicular-
/// *adjacent* within `gap + threshold` (the band where an opposite-edge snap
/// on the other axis would engage). Alignment is intentionally rejected on
/// any perp overlap — aligning left edges of two same-height windows would
/// just Z-stack them.
///
/// Returns `Some((snapped_origin, abs_distance))` for the closest candidate
/// within `threshold`, or `None`.
pub fn find_snap_candidate(natural_edge_low: f64, p: &SnapParams<'_>) -> Option<(f64, f64)> {
    let natural_edge_high = natural_edge_low + p.extent;
    let mut best: Option<(f64, f64)> = None;

    for other in p.others {
        let (other_low, other_high, other_perp_low, other_perp_high) = if p.horizontal {
            (other.x_low, other.x_high, other.y_low, other.y_high)
        } else {
            (other.y_low, other.y_high, other.x_low, other.x_high)
        };

        let perp_overlap = p.perp_high > other_perp_low && other_perp_high > p.perp_low;
        let alignment_eligible = !perp_overlap
            && perp_distance(p.perp_low, p.perp_high, other_perp_low, other_perp_high)
                < p.gap + p.threshold;
        let same_edge_eligible = p.same_edge && alignment_eligible;
        let edge_center_eligible = p.edge_center && alignment_eligible;

        if !perp_overlap && !same_edge_eligible && !edge_center_eligible {
            continue;
        }

        if perp_overlap {
            // Opposite-edge: dragged right edge → other left edge
            try_candidate(
                &mut best,
                other_low - p.gap - p.extent,
                (natural_edge_high - other_low).abs(),
                p.threshold,
            );

            // Opposite-edge: dragged left edge → other right edge
            try_candidate(
                &mut best,
                other_high + p.gap,
                (natural_edge_low - other_high).abs(),
                p.threshold,
            );
        }

        if same_edge_eligible {
            // Same-edge: left → left (no gap — edges align exactly)
            try_candidate(
                &mut best,
                other_low,
                (natural_edge_low - other_low).abs(),
                p.threshold,
            );

            // Same-edge: right → right
            try_candidate(
                &mut best,
                other_high - p.extent,
                (natural_edge_high - other_high).abs(),
                p.threshold,
            );
        }

        if edge_center_eligible {
            let other_center = (other_low + other_high) / 2.0;
            try_candidate(
                &mut best,
                other_center - p.extent / 2.0,
                (natural_edge_low + p.extent / 2.0 - other_center).abs(),
                p.threshold,
            );
        }
    }

    best
}

/// Distance between two intervals `[lo1, hi1]` and `[lo2, hi2]`. Zero when
/// they overlap; otherwise the gap between the nearer endpoints.
fn perp_distance(lo1: f64, hi1: f64, lo2: f64, hi2: f64) -> f64 {
    if hi1 < lo2 {
        lo2 - hi1
    } else if hi2 < lo1 {
        lo1 - hi2
    } else {
        0.0
    }
}

/// Parameters for single-edge snap search (used during resize).
pub struct EdgeSnapParams<'a> {
    pub perp_low: f64,
    pub perp_high: f64,
    pub horizontal: bool,
    pub same_edge: bool,
    pub others: &'a [SnapRect],
    pub gap: f64,
    pub threshold: f64,
    pub break_force: f64,
    /// true = right/bottom edge, false = left/top edge.
    /// Controls gap direction: a high edge snaps to other_low with gap,
    /// a low edge snaps to other_high with gap.
    pub high_edge: bool,
}

/// Find the best snap target for a single edge (used during resize).
///
/// Unlike `find_snap_candidate` which snaps a whole window origin, this snaps
/// one active edge to nearby edges of other windows.
/// Returns `Some((snapped_edge_pos, distance))`.
pub fn find_edge_snap(natural_edge: f64, p: &EdgeSnapParams<'_>) -> Option<(f64, f64)> {
    let mut best: Option<(f64, f64)> = None;

    for other in p.others {
        let (other_low, other_high, other_perp_low, other_perp_high) = if p.horizontal {
            (other.x_low, other.x_high, other.y_low, other.y_high)
        } else {
            (other.y_low, other.y_high, other.x_low, other.x_high)
        };

        let perp_overlap = p.perp_high > other_perp_low && other_perp_high > p.perp_low;
        let same_edge_eligible = p.same_edge
            && !perp_overlap
            && perp_distance(p.perp_low, p.perp_high, other_perp_low, other_perp_high)
                < p.gap + p.threshold;

        if !perp_overlap && !same_edge_eligible {
            continue;
        }

        if p.high_edge {
            // Right/bottom edge: snap to other's near edge with gap (opposite),
            // and to other's far edge exactly (same-edge alignment).
            if perp_overlap {
                try_candidate(
                    &mut best,
                    other_low - p.gap,
                    (natural_edge - other_low).abs(),
                    p.threshold,
                );
            }
            if same_edge_eligible {
                try_candidate(
                    &mut best,
                    other_high,
                    (natural_edge - other_high).abs(),
                    p.threshold,
                );
            }
        } else {
            // Left/top edge: snap to other's far edge with gap (opposite),
            // and to other's near edge exactly (same-edge alignment).
            if perp_overlap {
                try_candidate(
                    &mut best,
                    other_high + p.gap,
                    (natural_edge - other_high).abs(),
                    p.threshold,
                );
            }
            if same_edge_eligible {
                try_candidate(
                    &mut best,
                    other_low,
                    (natural_edge - other_low).abs(),
                    p.threshold,
                );
            }
        }
    }

    best
}

/// Update snap state for a single axis. Returns the final position for that axis.
pub fn update_axis(
    snap: &mut Option<AxisSnap>,
    cooldown: &mut Option<f64>,
    natural_pos: f64,
    p: &SnapParams<'_>,
) -> f64 {
    if let Some(ref s) = *snap {
        // Directional break: retreat past engagement point OR overshoot past snap
        let (retreat, overshoot) = if s.snapped_pos > s.natural_at_engage {
            (
                s.natural_at_engage - natural_pos,
                natural_pos - s.snapped_pos,
            )
        } else {
            (
                natural_pos - s.natural_at_engage,
                s.snapped_pos - natural_pos,
            )
        };
        if retreat >= p.break_force || overshoot >= p.break_force {
            *cooldown = Some(s.snapped_pos);
            *snap = None;
            natural_pos
        } else {
            s.snapped_pos
        }
    } else {
        // Clear cooldown when natural position leaves threshold of cooldown coord
        if let Some(cd) = *cooldown
            && (natural_pos - cd).abs() > p.threshold
        {
            *cooldown = None;
        }

        // Try to find a new snap candidate (skip if on cooldown)
        if cooldown.is_none()
            && let Some((snapped_pos, _)) = find_snap_candidate(natural_pos, p)
        {
            *snap = Some(AxisSnap {
                snapped_pos,
                natural_at_engage: natural_pos,
            });
            return snapped_pos;
        }

        natural_pos
    }
}

/// Apply edge snapping to an active resize operation.
///
/// Mutates `new_w`/`new_h` in place based on which edges are active.
/// `edges_mask` uses the xdg_toplevel resize edge bit layout (top=1, bottom=2, left=4, right=8).
///
/// The dragged window's *visible* edges (raw frame ± `self_bar` / `self_bw`)
/// are what get snapped against `others` — which are already inflated by their
/// own border width in `window_snap_rect`. Edges are inflated on the way in,
/// deflated on the way back to `new_w` / `new_h` (raw content size).
#[allow(clippy::too_many_arguments)]
pub fn snap_resize_edges(
    snap: &mut SnapState,
    edges_mask: u32,
    initial_location: (i32, i32),
    initial_size: (i32, i32),
    self_bar: i32,
    self_bw: i32,
    new_w: &mut i32,
    new_h: &mut i32,
    others: &[SnapRect],
    zoom: f64,
    gap: f64,
    snap_distance: f64,
    snap_break_force: f64,
    same_edge: bool,
) {
    let effective_distance = snap_distance / zoom;
    let effective_break = snap_break_force / zoom;
    let (loc_x, loc_y) = (initial_location.0 as f64, initial_location.1 as f64);
    let (init_w, init_h) = (initial_size.0 as f64, initial_size.1 as f64);
    let bw = self_bw as f64;

    let has_top = edges_mask & 1 != 0;
    let has_bottom = edges_mask & 2 != 0;
    let has_left = edges_mask & 4 != 0;
    let has_right = edges_mask & 8 != 0;

    // When a Y edge is already held-snapped, use the snapped visible position
    // instead of the natural (cursor-driven) one. Otherwise break_force drift
    // in the natural height could let the X-edge snap engage against a target
    // the window doesn't visually overlap — spurious corner snap.
    // `snap.y.snapped_pos` stores the visible (inflated) edge after the call
    // below, so we use it directly without further adjustment.
    let visual_top = if has_top {
        snap.y
            .as_ref()
            .map_or(loc_y + init_h - *new_h as f64 - self_bar as f64 - bw, |s| {
                s.snapped_pos
            })
    } else {
        loc_y - self_bar as f64 - bw
    };
    let visual_bottom = if has_bottom {
        snap.y
            .as_ref()
            .map_or(loc_y + *new_h as f64 + bw, |s| s.snapped_pos)
    } else {
        loc_y + init_h + bw
    };

    if has_right {
        let natural_right = loc_x + *new_w as f64 + bw;
        let hp = EdgeSnapParams {
            perp_low: visual_top,
            perp_high: visual_bottom,
            horizontal: true,
            same_edge,
            others,
            gap,
            threshold: effective_distance,
            break_force: effective_break,
            high_edge: true,
        };
        let snapped = update_edge(&mut snap.x, &mut snap.cooldown_x, natural_right, &hp);
        *new_w = (snapped - loc_x - bw) as i32;
    } else if has_left {
        let fixed_right = loc_x + init_w;
        let natural_left = fixed_right - *new_w as f64 - bw;
        let hp = EdgeSnapParams {
            perp_low: visual_top,
            perp_high: visual_bottom,
            horizontal: true,
            same_edge,
            others,
            gap,
            threshold: effective_distance,
            break_force: effective_break,
            high_edge: false,
        };
        let snapped = update_edge(&mut snap.x, &mut snap.cooldown_x, natural_left, &hp);
        *new_w = (fixed_right - snapped - bw) as i32;
    }

    // Visible X range for the Y-edge snap's perpendicular check. The X block
    // above has already updated *new_w to reflect any X snap, so we can derive
    // the visible range from that. A left-edge resize anchors to the right
    // side (fixed_right), so its visible X range is NOT (loc_x, loc_x + new_w).
    let (x_perp_low, x_perp_high) = if has_left {
        (loc_x + init_w - *new_w as f64 - bw, loc_x + init_w + bw)
    } else if has_right {
        (loc_x - bw, loc_x + *new_w as f64 + bw)
    } else {
        (loc_x - bw, loc_x + init_w + bw)
    };

    if has_bottom {
        let natural_bottom = loc_y + *new_h as f64 + bw;
        let vp = EdgeSnapParams {
            perp_low: x_perp_low,
            perp_high: x_perp_high,
            horizontal: false,
            same_edge,
            others,
            gap,
            threshold: effective_distance,
            break_force: effective_break,
            high_edge: true,
        };
        let snapped = update_edge(&mut snap.y, &mut snap.cooldown_y, natural_bottom, &vp);
        *new_h = (snapped - loc_y - bw) as i32;
    } else if has_top {
        let fixed_bottom = loc_y + init_h;
        let natural_top = fixed_bottom - *new_h as f64 - self_bar as f64 - bw;
        let vp = EdgeSnapParams {
            perp_low: x_perp_low,
            perp_high: x_perp_high,
            horizontal: false,
            same_edge,
            others,
            gap,
            threshold: effective_distance,
            break_force: effective_break,
            high_edge: false,
        };
        let snapped = update_edge(&mut snap.y, &mut snap.cooldown_y, natural_top, &vp);
        *new_h = (fixed_bottom - snapped - self_bar as f64 - bw) as i32;
    }

    *new_w = (*new_w).max(1);
    *new_h = (*new_h).max(1);
}

/// Update snap state for a single edge during resize. Returns the final edge position.
pub fn update_edge(
    snap: &mut Option<AxisSnap>,
    cooldown: &mut Option<f64>,
    natural_edge: f64,
    p: &EdgeSnapParams<'_>,
) -> f64 {
    if let Some(ref s) = *snap {
        let (retreat, overshoot) = if s.snapped_pos > s.natural_at_engage {
            (
                s.natural_at_engage - natural_edge,
                natural_edge - s.snapped_pos,
            )
        } else {
            (
                natural_edge - s.natural_at_engage,
                s.snapped_pos - natural_edge,
            )
        };
        if retreat >= p.break_force || overshoot >= p.break_force {
            *cooldown = Some(s.snapped_pos);
            *snap = None;
            natural_edge
        } else {
            s.snapped_pos
        }
    } else {
        if let Some(cd) = *cooldown
            && (natural_edge - cd).abs() > p.threshold
        {
            *cooldown = None;
        }

        if cooldown.is_none()
            && let Some((snapped_pos, _)) = find_edge_snap(natural_edge, p)
        {
            *snap = Some(AxisSnap {
                snapped_pos,
                natural_at_engage: natural_edge,
            });
            return snapped_pos;
        }

        natural_edge
    }
}
