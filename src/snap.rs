/// Bounding rectangle of a window in canvas coordinates, used for edge snap detection.
pub struct SnapRect {
    pub x_low: f64,
    pub x_high: f64,
    pub y_low: f64,
    pub y_high: f64,
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

/// Find the best snap candidate along one axis, filtering out windows that
/// don't overlap on the perpendicular axis (within `threshold` tolerance).
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

        if p.perp_high + p.threshold <= other_perp_low
            || other_perp_high + p.threshold <= p.perp_low
        {
            continue;
        }

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

        if p.same_edge {
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
    }

    best
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

        if p.perp_high + p.threshold <= other_perp_low
            || other_perp_high + p.threshold <= p.perp_low
        {
            continue;
        }

        if p.high_edge {
            // Right/bottom edge: snap to other's near edge with gap (opposite),
            // and to other's far edge exactly (same-edge alignment).
            try_candidate(
                &mut best,
                other_low - p.gap,
                (natural_edge - other_low).abs(),
                p.threshold,
            );
            if p.same_edge {
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
            try_candidate(
                &mut best,
                other_high + p.gap,
                (natural_edge - other_high).abs(),
                p.threshold,
            );
            if p.same_edge {
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
            (s.natural_at_engage - natural_pos, natural_pos - s.snapped_pos)
        } else {
            (natural_pos - s.natural_at_engage, s.snapped_pos - natural_pos)
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

/// Update snap state for a single edge during resize. Returns the final edge position.
pub fn update_edge(
    snap: &mut Option<AxisSnap>,
    cooldown: &mut Option<f64>,
    natural_edge: f64,
    p: &EdgeSnapParams<'_>,
) -> f64 {
    if let Some(ref s) = *snap {
        let (retreat, overshoot) = if s.snapped_pos > s.natural_at_engage {
            (s.natural_at_engage - natural_edge, natural_edge - s.snapped_pos)
        } else {
            (natural_edge - s.natural_at_engage, s.snapped_pos - natural_edge)
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
