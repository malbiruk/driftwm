//! Geometry for `fill-window`: grow a window in place to fill the free space
//! around it, stopping a gap short of neighboring windows and an outer-gap
//! inset short of the usable-area edge, and pulling edges back in where they
//! stick out past the usable area or into a neighbor. Pure frame-space canvas
//! math — the compositor glue in `state/fill.rs` supplies the rects and
//! converts the result back to a client size + map location.

use super::snap::SnapRect;

/// Grow `current` outward inside `bounds` (inset by `bounds_inset`), never
/// covering an obstacle and never crossing the `gap` margin around one, then
/// apply frame-space size constraints. `bounds_inset` is the screen-edge inset
/// and may be negative (a frame whose border hangs off the usable area); `gap`
/// is the spacing kept from obstacles. `min_size` / `max_size` use `0.0` as the
/// "unconstrained on this axis" sentinel. Returns `None` when `current` lies
/// entirely outside the inset bounds — such a window can't be filled without
/// being moved.
pub fn fill_rect(
    current: SnapRect,
    obstacles: &[SnapRect],
    bounds: SnapRect,
    bounds_inset: f64,
    gap: f64,
    min_size: (f64, f64),
    max_size: (f64, f64),
) -> Option<SnapRect> {
    let b = SnapRect {
        x_low: bounds.x_low + bounds_inset,
        x_high: bounds.x_high - bounds_inset,
        y_low: bounds.y_low + bounds_inset,
        y_high: bounds.y_high - bounds_inset,
    };
    if !current.overlaps(&b) {
        return None;
    }

    // Viewport clamp: pull any edge sticking out past the usable area back in.
    let clamped = SnapRect {
        x_low: current.x_low.max(b.x_low),
        x_high: current.x_high.min(b.x_high),
        y_low: current.y_low.max(b.y_low),
        y_high: current.y_high.min(b.y_high),
    };

    // Shrink out of partial overlaps: pull the least-travel single edge of each
    // overlapping obstacle back to a gap. Obstacles that can't be escaped this
    // way (an obstacle enclosing the window, or min-size blocking every pull)
    // keep overlapping and are ignored below.
    let resolved = resolve_overlaps(clamped, obstacles, gap, min_size);

    // An obstacle still overlapping after resolution can't be cleared by growth
    // (that would need a move), so it's ignored throughout; a resolved obstacle
    // no longer overlaps and rejoins as a normal blocker.
    let active: Vec<SnapRect> = obstacles
        .iter()
        .copied()
        .filter(|o| !o.overlaps(&resolved))
        .collect();

    // Axis order decides which of an L-shaped free region's arms a diagonal
    // blocker cedes; keep whichever order yields the larger area, ties to
    // horizontal-first.
    let hv = grow(resolved, &active, b, gap, true);
    let vh = grow(resolved, &active, b, gap, false);
    let target = if area(vh) > area(hv) { vh } else { hv };

    Some(apply_constraints(target, resolved, min_size, max_size))
}

/// Shrink `c` to escape partial overlaps with `obstacles`. Obstacles are visited
/// in a fixed sorted order, re-checking overlap before each (an earlier shrink
/// may already have cleared a later one). For an obstacle the rect still
/// overlaps, the four single-edge pulls back to a gap are considered; a pull is
/// eligible when it moves the edge inward while leaving that axis no shorter
/// than its min-size floor (`0.0` → a 1px floor). The eligible pull with the
/// least edge travel wins (ties broken in x-high, x-low, y-high, y-low order).
/// When no pull is eligible — an obstacle enclosing the rect, or min-size
/// blocking every escape — the overlap is left in place. Shrinks only remove
/// overlaps and never create them, so this single pass terminates.
fn resolve_overlaps(
    mut c: SnapRect,
    obstacles: &[SnapRect],
    gap: f64,
    min_size: (f64, f64),
) -> SnapRect {
    let min_w = if min_size.0 > 0.0 { min_size.0 } else { 1.0 };
    let min_h = if min_size.1 > 0.0 { min_size.1 } else { 1.0 };

    let mut order: Vec<&SnapRect> = obstacles.iter().collect();
    order.sort_by(|a, b| {
        a.x_low
            .total_cmp(&b.x_low)
            .then_with(|| a.y_low.total_cmp(&b.y_low))
            .then_with(|| a.x_high.total_cmp(&b.x_high))
            .then_with(|| a.y_high.total_cmp(&b.y_high))
    });

    for o in order {
        if !c.overlaps(o) {
            continue;
        }
        // (resulting rect, remaining extent on the pulled axis, edge travel,
        // min extent for that axis) in tie-break order.
        let pulls = [
            (
                SnapRect {
                    x_high: o.x_low - gap,
                    ..c
                },
                (o.x_low - gap) - c.x_low,
                c.x_high - (o.x_low - gap),
                min_w,
            ),
            (
                SnapRect {
                    x_low: o.x_high + gap,
                    ..c
                },
                c.x_high - (o.x_high + gap),
                (o.x_high + gap) - c.x_low,
                min_w,
            ),
            (
                SnapRect {
                    y_high: o.y_low - gap,
                    ..c
                },
                (o.y_low - gap) - c.y_low,
                c.y_high - (o.y_low - gap),
                min_h,
            ),
            (
                SnapRect {
                    y_low: o.y_high + gap,
                    ..c
                },
                c.y_high - (o.y_high + gap),
                (o.y_high + gap) - c.y_low,
                min_h,
            ),
        ];
        let best = pulls
            .into_iter()
            .filter(|&(_, extent, travel, min)| travel > 0.0 && extent >= min)
            .min_by(|a, b| a.2.total_cmp(&b.2));
        if let Some((resolved, ..)) = best {
            c = resolved;
        }
    }
    c
}

fn area(r: SnapRect) -> f64 {
    (r.x_high - r.x_low) * (r.y_high - r.y_low)
}

fn grow(
    clamped: SnapRect,
    obstacles: &[SnapRect],
    bounds: SnapRect,
    gap: f64,
    horizontal_first: bool,
) -> SnapRect {
    let mut r = clamped;
    if horizontal_first {
        grow_horizontal(&mut r, obstacles, bounds, gap);
        grow_vertical(&mut r, obstacles, bounds, gap);
    } else {
        grow_vertical(&mut r, obstacles, bounds, gap);
        grow_horizontal(&mut r, obstacles, bounds, gap);
    }
    r
}

fn grow_horizontal(r: &mut SnapRect, obstacles: &[SnapRect], bounds: SnapRect, gap: f64) {
    let mut max_left = f64::NEG_INFINITY;
    let mut min_right = f64::INFINITY;
    for o in obstacles {
        // Blocks only if it overlaps the current perpendicular (vertical)
        // extent within the gap margin — a diagonal neighbor inside `gap`
        // still blocks. Each side undoes the gap the way the rect edge was
        // derived (low = `o + gap`, high = `o - gap`), so an edge already
        // snapped a gap from `o` compares exactly and re-fills identically.
        if o.y_high + gap <= r.y_low || o.y_low - gap >= r.y_high {
            continue;
        }
        if o.x_high <= r.x_low {
            max_left = max_left.max(o.x_high + gap);
        } else if o.x_low >= r.x_high {
            min_right = min_right.min(o.x_low - gap);
        } else {
            // A straddler inside the perpendicular gap ring fits neither
            // partition. Growing past a side it already protrudes beyond would
            // open a fresh sub-gap overhang, so freeze that side by feeding the
            // current edge into the guard below — which then holds it in place.
            if o.x_high > r.x_high {
                min_right = min_right.min(r.x_high);
            }
            if o.x_low < r.x_low {
                max_left = max_left.max(r.x_low);
            }
        }
    }
    // The outer min/max guard the neighbor-closer-than-gap case: growth only
    // ever moves an edge outward, never inward.
    r.x_low = r.x_low.min(max_left.max(bounds.x_low));
    r.x_high = r.x_high.max(min_right.min(bounds.x_high));
}

fn grow_vertical(r: &mut SnapRect, obstacles: &[SnapRect], bounds: SnapRect, gap: f64) {
    let mut max_top = f64::NEG_INFINITY;
    let mut min_bottom = f64::INFINITY;
    for o in obstacles {
        if o.x_high + gap <= r.x_low || o.x_low - gap >= r.x_high {
            continue;
        }
        if o.y_high <= r.y_low {
            max_top = max_top.max(o.y_high + gap);
        } else if o.y_low >= r.y_high {
            min_bottom = min_bottom.min(o.y_low - gap);
        } else {
            // Symmetric straddler freeze (see grow_horizontal).
            if o.y_high > r.y_high {
                min_bottom = min_bottom.min(r.y_high);
            }
            if o.y_low < r.y_low {
                max_top = max_top.max(r.y_low);
            }
        }
    }
    r.y_low = r.y_low.min(max_top.max(bounds.y_low));
    r.y_high = r.y_high.max(min_bottom.min(bounds.y_high));
}

fn apply_constraints(
    target: SnapRect,
    resolved: SnapRect,
    min_size: (f64, f64),
    max_size: (f64, f64),
) -> SnapRect {
    let target_w = target.x_high - target.x_low;
    let target_h = target.y_high - target.y_low;
    let w = clamp_axis(target_w, min_size.0, max_size.0);
    let h = clamp_axis(target_h, min_size.1, max_size.1);
    let x_low = anchor_low(target.x_low, target.x_high, resolved.x_low, w, target_w);
    let y_low = anchor_low(target.y_low, target.y_high, resolved.y_low, h, target_h);
    SnapRect {
        x_low,
        x_high: x_low + w,
        y_low,
        y_high: y_low + h,
    }
}

/// `0.0` bounds are unconstrained; the low bound floors at 1px. Ordered so a
/// contradictory client (min > max) yields the max rather than panicking.
fn clamp_axis(len: f64, min: f64, max: f64) -> f64 {
    let lo = if min > 0.0 { min } else { 1.0 };
    let hi = if max > 0.0 { max } else { f64::INFINITY };
    len.max(lo).min(hi)
}

/// When a max-size cap shrinks the axis below the grown length, keep the
/// overlap-resolved low edge (give up growth on the high side first); when a
/// min-size floor forces it larger, keep the target's low edge and let it
/// overflow on the high side.
fn anchor_low(
    target_low: f64,
    target_high: f64,
    resolved_low: f64,
    len: f64,
    target_len: f64,
) -> f64 {
    if len < target_len {
        resolved_low.min(target_high - len).max(target_low)
    } else {
        target_low
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn rect(x_low: f64, y_low: f64, w: f64, h: f64) -> SnapRect {
        SnapRect {
            x_low,
            x_high: x_low + w,
            y_low,
            y_high: y_low + h,
        }
    }

    const UNCONSTRAINED: (f64, f64) = (0.0, 0.0);

    /// Bounds a comfortable 1000×1000 room with a 10px gap → free region
    /// [10,990]².
    fn room() -> SnapRect {
        rect(0.0, 0.0, 1000.0, 1000.0)
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    fn rect_approx(a: SnapRect, b: SnapRect) -> bool {
        approx(a.x_low, b.x_low)
            && approx(a.x_high, b.x_high)
            && approx(a.y_low, b.y_low)
            && approx(a.y_high, b.y_high)
    }

    #[test]
    fn no_obstacles_grows_to_inset_bounds() {
        let cur = rect(400.0, 400.0, 100.0, 100.0);
        let out = fill_rect(cur, &[], room(), 10.0, 10.0, UNCONSTRAINED, UNCONSTRAINED).unwrap();
        assert!(rect_approx(out, rect(10.0, 10.0, 980.0, 980.0)));
    }

    #[test]
    fn right_obstacle_stops_at_gap() {
        let cur = rect(100.0, 400.0, 100.0, 100.0);
        let neighbor = rect(600.0, 300.0, 200.0, 300.0);
        let out = fill_rect(
            cur,
            &[neighbor],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        // Right edge stops a gap short of the neighbor's left edge (590),
        // other edges reach the inset bounds.
        assert!(approx(out.x_high, 590.0), "x_high = {}", out.x_high);
        assert!(approx(out.x_low, 10.0));
        assert!(approx(out.y_low, 10.0));
        assert!(approx(out.y_high, 990.0));
    }

    #[test]
    fn neighbor_closer_than_gap_edge_never_pulls_inward() {
        // Neighbour's left edge (505) is less than a gap from the window's
        // right edge (500): the right edge must stay, not retreat.
        let cur = rect(100.0, 400.0, 400.0, 100.0);
        let neighbor = rect(505.0, 300.0, 200.0, 300.0);
        let out = fill_rect(
            cur,
            &[neighbor],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(approx(out.x_high, 500.0), "x_high = {}", out.x_high);
    }

    #[test]
    fn fully_containing_obstacle_is_ignored() {
        let cur = rect(400.0, 400.0, 200.0, 200.0);
        // Encloses the window on every side — no single-edge pull escapes it, so
        // fill ignores it and grows over it to the full inset bounds (the old
        // "obstacle overlapping the window" behavior).
        let enclosing = rect(300.0, 300.0, 400.0, 400.0);
        let out = fill_rect(
            cur,
            &[enclosing],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(rect_approx(out, rect(10.0, 10.0, 980.0, 980.0)));
    }

    #[test]
    fn partial_overlap_resolves_to_gap_then_grows() {
        // Window pokes into a neighbor on its right; the right edge pulls back to
        // exactly a gap short of the neighbor, and the other three edges grow to
        // the inset bounds.
        let cur = rect(400.0, 400.0, 200.0, 100.0); // [400,600]×[400,500]
        let neighbor = rect(550.0, 300.0, 200.0, 300.0); // [550,750]×[300,600]
        let out = fill_rect(
            cur,
            &[neighbor],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(approx(out.x_high, 540.0), "x_high = {}", out.x_high); // 550 − gap
        assert!(approx(out.x_low, 10.0), "x_low = {}", out.x_low);
        assert!(approx(out.y_low, 10.0));
        assert!(approx(out.y_high, 990.0));
    }

    #[test]
    fn left_partial_overlap_resolves_to_gap() {
        // Neighbor on the left: the left edge pulls out to a gap past the
        // neighbor's right edge, then the free sides grow to bounds.
        let cur = rect(400.0, 400.0, 200.0, 200.0); // [400,600]²
        let neighbor = rect(250.0, 300.0, 200.0, 400.0); // [250,450]×[300,700]
        let out = fill_rect(
            cur,
            &[neighbor],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(approx(out.x_low, 460.0), "x_low = {}", out.x_low); // 450 + gap
        assert!(approx(out.x_high, 990.0));
        assert!(approx(out.y_low, 10.0));
        assert!(approx(out.y_high, 990.0));
    }

    #[test]
    fn minimal_travel_edge_is_chosen() {
        // A corner overlap where the horizontal escape travels less (30) than the
        // vertical one (60): the x-high edge is pulled, and vertical growth is
        // left free (reaches the bounds rather than the y-escape at 540).
        let cur = rect(400.0, 400.0, 200.0, 200.0); // [400,600]²
        let neighbor = rect(580.0, 550.0, 200.0, 200.0); // [580,780]×[550,750]
        let out = fill_rect(
            cur,
            &[neighbor],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(approx(out.x_high, 570.0), "x_high = {}", out.x_high); // 580 − gap
        assert!(approx(out.y_high, 990.0), "y_high = {}", out.y_high);
    }

    #[test]
    fn tie_break_prefers_x_high() {
        // Symmetric corner overlap: the x-high and y-high pulls travel equally
        // (20 each). The deterministic tie-break takes x-high first, so vertical
        // growth is untouched.
        let cur = rect(400.0, 400.0, 200.0, 200.0); // [400,600]²
        let neighbor = rect(590.0, 590.0, 200.0, 200.0); // [590,790]²
        let out = fill_rect(
            cur,
            &[neighbor],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(approx(out.x_high, 580.0), "x_high = {}", out.x_high); // 590 − gap
        assert!(approx(out.y_high, 990.0), "y_high = {}", out.y_high);
    }

    #[test]
    fn min_size_can_block_every_resolving_pull() {
        // Neighbor overlaps the right; the only escaping pull (x-high) would leave
        // a 140px-wide rect. Unconstrained that resolves; a 200px min width blocks
        // it, so the obstacle is ignored and growth covers it instead.
        let cur = rect(400.0, 400.0, 200.0, 200.0); // [400,600]²
        let neighbor = rect(550.0, 300.0, 200.0, 400.0); // [550,750]×[300,700]

        let free = fill_rect(
            cur,
            &[neighbor],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(approx(free.x_high, 540.0), "x_high = {}", free.x_high);

        let blocked = fill_rect(
            cur,
            &[neighbor],
            room(),
            10.0,
            10.0,
            (200.0, 0.0),
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(rect_approx(blocked, rect(10.0, 10.0, 980.0, 980.0)));
    }

    #[test]
    fn two_overlaps_resolved_in_one_pass() {
        // One neighbor on the right, one below, each overlapping a corner of the
        // window; both edges pull back to a gap in a single pass and the window
        // grows into the free top-left.
        let cur = rect(400.0, 400.0, 200.0, 200.0); // [400,600]²
        let right = rect(550.0, 300.0, 200.0, 600.0); // [550,750]×[300,900]
        let below = rect(300.0, 550.0, 600.0, 200.0); // [300,900]×[550,750]
        let out = fill_rect(
            cur,
            &[right, below],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(approx(out.x_high, 540.0), "x_high = {}", out.x_high); // 550 − gap
        assert!(approx(out.y_high, 540.0), "y_high = {}", out.y_high); // 550 − gap
        assert!(approx(out.x_low, 10.0));
        assert!(approx(out.y_low, 10.0));
    }

    #[test]
    fn shrink_rechecks_incidentally_resolved_obstacle() {
        // Pulling the x-high edge to clear the first (nearer) neighbor also lifts
        // the window clear of the second one, whose perpendicular pull would
        // otherwise have spuriously shrunk the window vertically. The re-check
        // skips it, so vertical growth reaches the bounds.
        let cur = rect(400.0, 400.0, 200.0, 200.0); // [400,600]²
        let first = rect(500.0, 300.0, 300.0, 400.0); // [500,800]×[300,700]
        let second = rect(520.0, 550.0, 300.0, 300.0); // [520,820]×[550,850]
        let out = fill_rect(
            cur,
            &[first, second],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(approx(out.x_high, 490.0), "x_high = {}", out.x_high); // 500 − gap
        assert!(approx(out.y_high, 990.0), "y_high = {}", out.y_high);
    }

    #[test]
    fn neighbor_below_within_gap_does_not_block_horizontal_growth() {
        // A neighbor sits a few px below the window (inside the gap margin) with
        // the same x-range: it doesn't overlap, so it isn't shrunk against, and it
        // fits neither the left-of nor right-of partition for horizontal growth —
        // horizontal growth must proceed to the bounds. Downward growth stays put
        // (the outer max-guard keeps the too-close edge from retreating).
        let cur = rect(100.0, 100.0, 100.0, 100.0); // [100,200]²
        let neighbor = rect(100.0, 205.0, 100.0, 200.0); // [100,200]×[205,405]
        let out = fill_rect(
            cur,
            &[neighbor],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(approx(out.x_low, 10.0), "x_low = {}", out.x_low);
        assert!(approx(out.x_high, 990.0), "x_high = {}", out.x_high);
        assert!(approx(out.y_low, 10.0), "y_low = {}", out.y_low);
        assert!(approx(out.y_high, 200.0), "y_high = {}", out.y_high); // not pulled to 195
    }

    #[test]
    fn straddler_protruding_right_freezes_only_right_growth() {
        // A neighbor a few px below (inside the gap margin) straddles the
        // window's x-range and protrudes past its right edge. Growing right
        // would open a sub-gap overhang across x∈[200,400]; only the right edge
        // freezes, the left still grows to the bounds.
        let cur = rect(100.0, 100.0, 100.0, 100.0); // [100,200]²
        let neighbor = rect(150.0, 205.0, 250.0, 200.0); // [150,400]×[205,405]
        let out = fill_rect(
            cur,
            &[neighbor],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(approx(out.x_high, 200.0), "x_high = {}", out.x_high);
        assert!(approx(out.x_low, 10.0), "x_low = {}", out.x_low);
    }

    #[test]
    fn straddler_protruding_left_freezes_only_left_growth() {
        // Mirror image: the straddler protrudes past the window's left edge, so
        // only the left edge freezes and the right grows to the bounds.
        let cur = rect(100.0, 100.0, 100.0, 100.0); // [100,200]²
        let neighbor = rect(50.0, 205.0, 100.0, 200.0); // [50,150]×[205,405]
        let out = fill_rect(
            cur,
            &[neighbor],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(approx(out.x_low, 100.0), "x_low = {}", out.x_low);
        assert!(approx(out.x_high, 990.0), "x_high = {}", out.x_high);
    }

    #[test]
    fn straddler_exactly_gap_away_does_not_freeze() {
        // The same protruding straddler, but parked exactly a gap below (not
        // sub-gap): it falls outside the perpendicular gap ring, so it is never
        // considered and horizontal growth reaches the bounds unfrozen.
        let cur = rect(100.0, 100.0, 100.0, 100.0); // [100,200]²
        let neighbor = rect(150.0, 210.0, 250.0, 200.0); // y_low = 200 + gap
        let out = fill_rect(
            cur,
            &[neighbor],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(approx(out.x_high, 990.0), "x_high = {}", out.x_high);
        assert!(approx(out.x_low, 10.0), "x_low = {}", out.x_low);
    }

    #[test]
    fn diagonal_within_gap_margin_blocks() {
        // Neighbour sits below-right, not vertically overlapping the window yet,
        // but within the gap margin perpendicular to horizontal growth — it must
        // still cap the right edge.
        let cur = rect(100.0, 100.0, 100.0, 100.0);
        // window y: [100,200]. neighbor y_low = 205 → within gap (10) of 200.
        let neighbor = rect(600.0, 205.0, 200.0, 200.0);
        let out = fill_rect(
            cur,
            &[neighbor],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        assert!(approx(out.x_high, 590.0), "x_high = {}", out.x_high);
    }

    #[test]
    fn l_shaped_free_space_picks_larger_area_axis_order() {
        // One obstacle blocks horizontal growth low-down; growing vertical
        // first frees the full-height narrow arm, horizontal first frees the
        // full-width short arm. Pick the larger.
        let cur = rect(100.0, 100.0, 100.0, 100.0);
        // Blocks the right for y in [300, 990]; leaves the top-right open.
        let obstacle = rect(400.0, 300.0, 590.0, 690.0);
        let out = fill_rect(
            cur,
            &[obstacle],
            room(),
            10.0,
            10.0,
            UNCONSTRAINED,
            UNCONSTRAINED,
        )
        .unwrap();
        // Neither candidate may overlap the obstacle.
        assert!(!out.overlaps(&obstacle));
        // Vertical-first arm: full height [10,990], width capped at 390 (gap
        // before obstacle) = area 980*380. Horizontal-first: full width [10,990]
        // but height capped at 290 = 980*280. Vertical-first is larger.
        assert!(
            approx(out.y_low, 10.0) && approx(out.y_high, 990.0),
            "{out:?}"
        );
        assert!(approx(out.x_high, 390.0), "x_high = {}", out.x_high);
    }

    #[test]
    fn partially_out_window_shrinks_back_to_bounds() {
        // Window overhangs the left and top of the usable area; fill pulls those
        // edges in to the inset bounds while growing the others out.
        let cur = rect(-200.0, -200.0, 400.0, 400.0);
        let out = fill_rect(cur, &[], room(), 10.0, 10.0, UNCONSTRAINED, UNCONSTRAINED).unwrap();
        assert!(rect_approx(out, rect(10.0, 10.0, 980.0, 980.0)));
    }

    #[test]
    fn fully_outside_returns_none() {
        let cur = rect(1200.0, 1200.0, 100.0, 100.0);
        assert!(fill_rect(cur, &[], room(), 10.0, 10.0, UNCONSTRAINED, UNCONSTRAINED).is_none());
    }

    #[test]
    fn min_size_floor_anchors_low_edge() {
        // A min width larger than the free region: keep the target's low edge
        // and overflow on the high side.
        let cur = rect(400.0, 400.0, 100.0, 100.0);
        let out = fill_rect(cur, &[], room(), 10.0, 10.0, (2000.0, 0.0), UNCONSTRAINED).unwrap();
        assert!(approx(out.x_low, 10.0), "x_low = {}", out.x_low);
        assert!(approx(out.x_high - out.x_low, 2000.0));
    }

    #[test]
    fn max_size_cap_keeps_left_edge() {
        // Window sits at the left; a max width smaller than the free region caps
        // growth on the right, keeping the viewport-clamped left edge.
        let cur = rect(10.0, 400.0, 100.0, 100.0);
        let out = fill_rect(cur, &[], room(), 10.0, 10.0, UNCONSTRAINED, (300.0, 0.0)).unwrap();
        assert!(approx(out.x_low, 10.0), "x_low = {}", out.x_low);
        assert!(approx(out.x_high, 310.0), "x_high = {}", out.x_high);
    }

    #[test]
    fn respects_gap_against_bounds() {
        let cur = rect(400.0, 400.0, 100.0, 100.0);
        let out = fill_rect(cur, &[], room(), 25.0, 25.0, UNCONSTRAINED, UNCONSTRAINED).unwrap();
        assert!(approx(out.x_low, 25.0) && approx(out.x_high, 975.0));
        assert!(approx(out.y_low, 25.0) && approx(out.y_high, 975.0));
    }

    /// A rect strategy inside a generous canvas, sizes kept positive.
    fn any_rect() -> impl Strategy<Value = SnapRect> {
        (
            -500.0..500.0f64,
            -500.0..500.0f64,
            20.0..600.0f64,
            20.0..600.0f64,
        )
            .prop_map(|(x, y, w, h)| rect(x, y, w, h))
    }

    fn inset_bounds(bounds: SnapRect, inset: f64) -> SnapRect {
        SnapRect {
            x_low: bounds.x_low + inset,
            x_high: bounds.x_high - inset,
            y_low: bounds.y_low + inset,
            y_high: bounds.y_high - inset,
        }
    }

    /// `inner` is inside `outer` up to a float tolerance.
    fn within(inner: SnapRect, outer: SnapRect) -> bool {
        const EPS: f64 = 1e-6;
        inner.x_low >= outer.x_low - EPS
            && inner.x_high <= outer.x_high + EPS
            && inner.y_low >= outer.y_low - EPS
            && inner.y_high <= outer.y_high + EPS
    }

    /// `inner` contains `outer` up to a float tolerance.
    fn contains(inner: SnapRect, outer: SnapRect) -> bool {
        const EPS: f64 = 1e-6;
        inner.x_low <= outer.x_low + EPS
            && inner.x_high >= outer.x_high - EPS
            && inner.y_low <= outer.y_low + EPS
            && inner.y_high >= outer.y_high - EPS
    }

    proptest! {
        #[test]
        fn result_within_bounds_only_keeps_unresolvable_overlaps(
            current in any_rect(),
            obstacles in prop::collection::vec(any_rect(), 0..6),
            gap in 0.0..30.0f64,
        ) {
            let bounds = rect(-800.0, -800.0, 1600.0, 1600.0);
            if let Some(out) = fill_rect(current, &obstacles, bounds, gap, gap, UNCONSTRAINED, UNCONSTRAINED) {
                let b = inset_bounds(bounds, gap);
                // Unconstrained, so the result fits the gap-inset bounds.
                prop_assert!(within(out, b), "result {out:?} escaped inset bounds {b:?}");

                // Growth starts from the shrunk (resolved) rect, so the result
                // contains that — not necessarily the raw viewport clamp.
                let clamped = SnapRect {
                    x_low: current.x_low.max(b.x_low),
                    x_high: current.x_high.min(b.x_high),
                    y_low: current.y_low.max(b.y_low),
                    y_high: current.y_high.min(b.y_high),
                };
                let resolved = resolve_overlaps(clamped, &obstacles, gap, UNCONSTRAINED);
                prop_assert!(contains(out, resolved), "result {out:?} lost resolved rect {resolved:?}");

                // The result may only overlap obstacles the shrink phase could
                // not escape (they still overlap the resolved rect); every
                // resolvable obstacle is cleared.
                for o in &obstacles {
                    if !resolved.overlaps(o) {
                        prop_assert!(!out.overlaps(o), "result {out:?} overlaps resolvable obstacle {o:?}");
                    }
                }
            }
        }

        #[test]
        fn idempotent_from_steady_state(
            current in any_rect(),
            obstacles in prop::collection::vec(any_rect(), 0..6),
            gap in 0.0..30.0f64,
        ) {
            let bounds = rect(-800.0, -800.0, 1600.0, 1600.0);
            if let Some(first) = fill_rect(current, &obstacles, bounds, gap, gap, UNCONSTRAINED, UNCONSTRAINED) {
                // Once a fill no longer overlaps any obstacle — the steady state a
                // fill reaches unless it had to grow over an unresolvable
                // enclosing obstacle — re-filling reproduces it exactly.
                if obstacles.iter().all(|o| !first.overlaps(o)) {
                    let second = fill_rect(first, &obstacles, bounds, gap, gap, UNCONSTRAINED, UNCONSTRAINED)
                        .expect("filled rect still intersects bounds");
                    prop_assert!(rect_approx(first, second), "not idempotent: {first:?} -> {second:?}");
                }
            }
        }
    }
}
