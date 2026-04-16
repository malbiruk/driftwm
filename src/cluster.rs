//! Window clustering — on-demand connected components over snap-adjacency.
//!
//! A *cluster* is the connected component of the focused window in the
//! snap-adjacency graph — but the graph is never stored. Each query walks
//! current geometry via BFS, so there's nothing to rebuild when windows move,
//! resize, fullscreen, close, or get repositioned by external protocols.
//! "Edge-adjacent" matches the post-tightening semantics of `snap.rs`: two
//! windows share a side, with strictly positive perpendicular overlap,
//! separated by exactly `gap` on the parallel axis. Diagonal corner snaps
//! are intentionally excluded from both snap engage and cluster membership.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;

use driftwm::snap::SnapRect;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    // Kept for slice 2 (resize propagation) — the neighbor's side is always
    // the opposite of self's side in an edge adjacency.
    #[allow(dead_code)]
    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }
}

/// Which side of `a` is edge-adjacent to `b`? Returns `None` if the two
/// rectangles don't share an edge.
///
/// "Edge-adjacent" means: on one parallel coordinate, the relevant edge of
/// `a` equals the opposite edge of `b` shifted by `gap` (within `EPS`), AND
/// the perpendicular extents strictly overlap. Strict overlap rejects both
/// corner-touches (zero shared length) and diagonal corner snaps where the
/// two windows are flush-with-gap on both axes simultaneously.
pub fn adjacent_side(a: &SnapRect, b: &SnapRect, gap: f64) -> Option<Side> {
    const EPS: f64 = 1.0;

    let y_overlap = a.y_low < b.y_high && b.y_low < a.y_high;
    let x_overlap = a.x_low < b.x_high && b.x_low < a.x_high;

    if y_overlap {
        if ((a.x_high + gap) - b.x_low).abs() < EPS {
            return Some(Side::Right);
        }
        if (a.x_low - (b.x_high + gap)).abs() < EPS {
            return Some(Side::Left);
        }
    }
    if x_overlap {
        if ((a.y_high + gap) - b.y_low).abs() < EPS {
            return Some(Side::Bottom);
        }
        if (a.y_low - (b.y_high + gap)).abs() < EPS {
            return Some(Side::Top);
        }
    }

    None
}

/// Axis classification + initial rect for one cluster member, used by the
/// pure `resolve_cluster_shifts` helper. Production code constructs these
/// from `ClusterResizeMember` via `alive()` filtering; tests construct them
/// with literal `SnapRect`s and no smithay dependency.
#[derive(Clone, Copy, Debug)]
pub struct ResizeClassification {
    pub axis_x: Option<Side>,
    pub axis_y: Option<Side>,
    pub initial_rect: SnapRect,
}

/// Compute per-member translation vectors for one motion tick of a cluster
/// resize.
///
/// Three-phase algorithm:
///
/// 1. **Static shift** — each member with a non-`None` axis inherits a
///    shift from the edge deltas: `Right → +width_delta`,
///    `Left → -width_delta`, `Bottom → +height_delta`,
///    `Top → -height_delta`.
/// 2. **Bond-driven shift** — for each existing bond `(m, n)`, position
///    `n` flush with `m`'s current leading edge ± gap. Bonds are
///    processed in insertion order so chains propagate transitively
///    (A→B before B→C). No direction guard — bonded members follow
///    unconditionally, including on drag reversal and past their initial
///    position.
/// 3. **Push cascade** — for every pair `(M, N)` where `M` has a
///    non-zero shift, compute encroachment of `M` on `N`. If positive,
///    `N` is pushed by exactly the encroachment, and a new bond
///    `(M, N)` is recorded. Looped until stable.
///
/// Returns `(shifts, newly_formed_bonds)`. The caller persists
/// `newly_formed_bonds` into its snapshot so the next frame's phase 2
/// picks them up.
/// Return type for `resolve_cluster_shifts`: per-member shifts keyed by
/// index, plus any newly-formed bonds that the caller persists across frames.
pub type ShiftsAndBonds = (HashMap<usize, (i32, i32)>, Vec<(usize, usize)>);

#[allow(clippy::type_complexity)]
pub fn resolve_cluster_shifts(
    members: &[ResizeClassification],
    width_delta: i32,
    height_delta: i32,
    gap: f64,
    existing_bonds: &[(usize, usize)],
) -> ShiftsAndBonds {
    let mut shifts: HashMap<usize, (i32, i32)> = HashMap::new();
    let empty_bonds = Vec::new();
    if members.is_empty() {
        return (shifts, empty_bonds);
    }

    // Phase 1: static shifts from axis classifications.
    for (i, m) in members.iter().enumerate() {
        let dx = match m.axis_x {
            Some(Side::Right) => width_delta,
            Some(Side::Left) => -width_delta,
            _ => 0,
        };
        let dy = match m.axis_y {
            Some(Side::Bottom) => height_delta,
            Some(Side::Top) => -height_delta,
            _ => 0,
        };
        if dx != 0 || dy != 0 {
            shifts.insert(i, (dx, dy));
        }
    }

    // Phase 2: bond-driven shifts.
    for &(m_idx, n_idx) in existing_bonds {
        if m_idx >= members.len() || n_idx >= members.len() {
            continue;
        }
        let (mdx, mdy) = shifts.get(&m_idx).copied().unwrap_or((0, 0));
        let m_init = &members[m_idx].initial_rect;
        let m_cur = translate_rect(m_init, mdx, mdy);
        let n_init = &members[n_idx].initial_rect;
        let (ndx, ndy) = shifts.get(&n_idx).copied().unwrap_or((0, 0));

        let mut new_ndx = ndx;
        let mut new_ndy = ndy;

        if n_init.x_low >= m_init.x_high {
            new_ndx = (m_cur.x_high + gap - n_init.x_low).ceil() as i32;
        } else if n_init.x_high <= m_init.x_low {
            new_ndx = (m_cur.x_low - gap - n_init.x_high).floor() as i32;
        }
        if n_init.y_low >= m_init.y_high {
            new_ndy = (m_cur.y_high + gap - n_init.y_low).ceil() as i32;
        } else if n_init.y_high <= m_init.y_low {
            new_ndy = (m_cur.y_low - gap - n_init.y_high).floor() as i32;
        }

        if (new_ndx, new_ndy) != (ndx, ndy) {
            shifts.insert(n_idx, (new_ndx, new_ndy));
        }
    }

    if shifts.is_empty() {
        return (shifts, empty_bonds);
    }

    // Phase 3: push cascade with bond recording.
    let mut new_bonds: Vec<(usize, usize)> = Vec::new();
    let mut new_bonds_set: HashSet<(usize, usize)> = HashSet::new();

    for _ in 0..(members.len() * 2) {
        let mut changed = false;

        for (i, m_entry) in members.iter().enumerate() {
            let Some(&(idx, idy)) = shifts.get(&i) else {
                continue;
            };
            if idx == 0 && idy == 0 {
                continue;
            }
            let m_init = m_entry.initial_rect;
            let m_cur = translate_rect(&m_init, idx, idy);

            for (j, n_entry) in members.iter().enumerate() {
                if j == i {
                    continue;
                }
                let (jdx, jdy) = shifts.get(&j).copied().unwrap_or((0, 0));
                let n_init = n_entry.initial_rect;
                let n_cur = translate_rect(&n_init, jdx, jdy);

                let push =
                    compute_push(&m_init, &m_cur, &n_init, &n_cur, (idx, idy), gap);
                if push == (0, 0) {
                    continue;
                }
                let new = (jdx + push.0, jdy + push.1);
                if new != (jdx, jdy) {
                    shifts.insert(j, new);
                    if new_bonds_set.insert((i, j)) {
                        new_bonds.push((i, j));
                    }
                    changed = true;
                }
            }
        }

        if !changed {
            break;
        }
    }

    (shifts, new_bonds)
}

fn translate_rect(r: &SnapRect, dx: i32, dy: i32) -> SnapRect {
    SnapRect {
        x_low: r.x_low + dx as f64,
        x_high: r.x_high + dx as f64,
        y_low: r.y_low + dy as f64,
        y_high: r.y_high + dy as f64,
    }
}

fn y_overlap(a: &SnapRect, b: &SnapRect) -> bool {
    a.y_low < b.y_high && b.y_low < a.y_high
}

fn x_overlap(a: &SnapRect, b: &SnapRect) -> bool {
    a.x_low < b.x_high && b.x_low < a.x_high
}

/// How far should `N` be pushed to maintain `gap` distance from `M`'s
/// leading edge(s)? Returns `(0, 0)` unless `M`'s motion is encroaching
/// on `N`.
///
/// The **direction guards** (`n_init.x_low >= m_init.x_high` and friends)
/// require `N` to sit entirely past `M`'s *leading* edge at grab start.
/// This prevents a rightward-moving M from pushing a member that merely
/// overlapped M in x but sat on M's *trailing* side — which would cause
/// cross-axis cascade oscillation (e.g. an upward-moving C spuriously
/// pushing the tall A upward because A's y extent happened to overlap C's).
///
/// The y_overlap / x_overlap checks operate on the *current* rects
/// (`m_cur`, `n_cur`) so a member that only y-overlaps M after some
/// prior cascade step still collides correctly.
fn compute_push(
    m_init: &SnapRect,
    m_cur: &SnapRect,
    n_init: &SnapRect,
    n_cur: &SnapRect,
    (mdx, mdy): (i32, i32),
    gap: f64,
) -> (i32, i32) {
    let mut push = (0, 0);

    if mdx > 0
        && n_init.x_low >= m_init.x_high
        && y_overlap(m_cur, n_cur)
    {
        let encroach = m_cur.x_high + gap - n_cur.x_low;
        if encroach > 0.0 {
            push.0 = encroach.ceil() as i32;
        }
    } else if mdx < 0
        && n_init.x_high <= m_init.x_low
        && y_overlap(m_cur, n_cur)
    {
        let encroach = m_cur.x_low - gap - n_cur.x_high;
        if encroach < 0.0 {
            push.0 = encroach.floor() as i32;
        }
    }

    if mdy > 0
        && n_init.y_low >= m_init.y_high
        && x_overlap(m_cur, n_cur)
    {
        let encroach = m_cur.y_high + gap - n_cur.y_low;
        if encroach > 0.0 {
            push.1 = encroach.ceil() as i32;
        }
    } else if mdy < 0
        && n_init.y_high <= m_init.y_low
        && x_overlap(m_cur, n_cur)
    {
        let encroach = m_cur.y_low - gap - n_cur.y_high;
        if encroach < 0.0 {
            push.1 = encroach.floor() as i32;
        }
    }

    push
}

/// Connected component of `root` in the snap-adjacency graph of `windows`.
///
/// BFS over the edge relation defined by `adjacent_side`. Always contains at
/// least `root` itself, even if `root` isn't in `windows` or has no
/// neighbors. Generic over node identity so production code can pass
/// `smithay::desktop::Window` while tests use `&'static str`.
pub fn cluster_of<W>(root: &W, windows: &[(W, SnapRect)], gap: f64) -> HashSet<W>
where
    W: Clone + Eq + Hash,
{
    // O(1) rect lookup per popped node; without this the BFS does a linear
    // scan per pop, turning O(n²) into O(n²) + O(n²).
    let rects: HashMap<&W, &SnapRect> =
        windows.iter().map(|(w, r)| (w, r)).collect();

    let mut visited: HashSet<W> = HashSet::new();
    let mut queue: VecDeque<W> = VecDeque::new();
    visited.insert(root.clone());
    queue.push_back(root.clone());

    while let Some(w) = queue.pop_front() {
        let Some(w_rect) = rects.get(&w) else {
            continue;
        };
        for (other, other_rect) in windows {
            if visited.contains(other) {
                continue;
            }
            if adjacent_side(w_rect, other_rect, gap).is_some() {
                visited.insert(other.clone());
                queue.push_back(other.clone());
            }
        }
    }

    visited
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x_low: f64, y_low: f64, w: f64, h: f64) -> SnapRect {
        SnapRect {
            x_low,
            x_high: x_low + w,
            y_low,
            y_high: y_low + h,
        }
    }

    #[test]
    fn right_edge_with_gap() {
        let a = rect(0.0, 0.0, 100.0, 100.0);
        let b = rect(104.0, 20.0, 50.0, 50.0);
        assert_eq!(adjacent_side(&a, &b, 4.0), Some(Side::Right));
    }

    #[test]
    fn left_edge_with_gap() {
        let a = rect(200.0, 10.0, 80.0, 80.0);
        let b = rect(100.0, 20.0, 96.0, 50.0);
        assert_eq!(adjacent_side(&a, &b, 4.0), Some(Side::Left));
    }

    #[test]
    fn bottom_edge_with_gap() {
        let a = rect(0.0, 0.0, 100.0, 100.0);
        let b = rect(20.0, 108.0, 60.0, 60.0);
        assert_eq!(adjacent_side(&a, &b, 8.0), Some(Side::Bottom));
    }

    #[test]
    fn top_edge_with_gap() {
        let a = rect(10.0, 200.0, 80.0, 80.0);
        let b = rect(20.0, 100.0, 60.0, 92.0);
        assert_eq!(adjacent_side(&a, &b, 8.0), Some(Side::Top));
    }

    #[test]
    fn corner_touch_not_adjacent() {
        let a = rect(0.0, 0.0, 100.0, 100.0);
        let b = rect(100.0, 100.0, 50.0, 50.0);
        assert_eq!(adjacent_side(&a, &b, 0.0), None);
    }

    #[test]
    fn diagonal_gap_not_adjacent() {
        let a = rect(0.0, 0.0, 100.0, 100.0);
        let b = rect(108.0, 108.0, 80.0, 80.0);
        assert_eq!(adjacent_side(&a, &b, 8.0), None);
    }

    #[test]
    fn wrong_distance_not_adjacent() {
        let a = rect(0.0, 0.0, 100.0, 100.0);
        let b = rect(200.0, 20.0, 50.0, 50.0);
        assert_eq!(adjacent_side(&a, &b, 4.0), None);
    }

    #[test]
    fn sub_pixel_drift_tolerated() {
        let a = rect(0.0, 0.0, 100.0, 100.0);
        let b = rect(104.4, 20.0, 50.0, 50.0);
        assert_eq!(adjacent_side(&a, &b, 4.0), Some(Side::Right));
    }

    #[test]
    fn parallel_flush_but_perp_disjoint_not_adjacent() {
        // x_right_flush holds (b.x_low = 100+4), but b is far below a with
        // zero perpendicular overlap — not a corner touch either.
        let a = rect(0.0, 0.0, 100.0, 100.0);
        let b = rect(104.0, 300.0, 50.0, 50.0);
        assert_eq!(adjacent_side(&a, &b, 4.0), None);
    }

    #[test]
    fn cluster_chain_abc() {
        let ws = vec![
            ("a", rect(0.0, 0.0, 100.0, 100.0)),
            ("b", rect(104.0, 0.0, 100.0, 100.0)),
            ("c", rect(208.0, 0.0, 100.0, 100.0)),
        ];
        assert_eq!(cluster_of(&"a", &ws, 4.0), HashSet::from(["a", "b", "c"]));
    }

    #[test]
    fn cluster_chain_splits_when_c_moves_away() {
        let mut ws = vec![
            ("a", rect(0.0, 0.0, 100.0, 100.0)),
            ("b", rect(104.0, 0.0, 100.0, 100.0)),
            ("c", rect(208.0, 0.0, 100.0, 100.0)),
        ];
        ws[2].1 = rect(500.0, 0.0, 100.0, 100.0);
        assert_eq!(cluster_of(&"a", &ws, 4.0), HashSet::from(["a", "b"]));
    }

    #[test]
    fn cluster_diamond_no_double_visit() {
        // A adjacent to B and C; both adjacent to D. BFS must visit D exactly
        // once despite two distinct incoming edges.
        let ws = vec![
            ("a", rect(0.0, 0.0, 100.0, 100.0)),
            ("b", rect(104.0, 0.0, 100.0, 100.0)),
            ("c", rect(0.0, 104.0, 100.0, 100.0)),
            ("d", rect(104.0, 104.0, 100.0, 100.0)),
        ];
        assert_eq!(
            cluster_of(&"a", &ws, 4.0),
            HashSet::from(["a", "b", "c", "d"])
        );
    }

    #[test]
    fn cluster_singleton_isolated_window() {
        let ws = vec![
            ("a", rect(0.0, 0.0, 100.0, 100.0)),
            ("b", rect(500.0, 0.0, 100.0, 100.0)),
        ];
        assert_eq!(cluster_of(&"a", &ws, 4.0), HashSet::from(["a"]));
    }

    #[test]
    fn cluster_middle_of_chain_walks_both_directions() {
        // BFS from B must reach both A (earlier in iteration) and C (later).
        let ws = vec![
            ("a", rect(0.0, 0.0, 100.0, 100.0)),
            ("b", rect(104.0, 0.0, 100.0, 100.0)),
            ("c", rect(208.0, 0.0, 100.0, 100.0)),
        ];
        assert_eq!(cluster_of(&"b", &ws, 4.0), HashSet::from(["a", "b", "c"]));
    }

    fn classify(
        axis_x: Option<Side>,
        axis_y: Option<Side>,
        r: SnapRect,
    ) -> ResizeClassification {
        ResizeClassification { axis_x, axis_y, initial_rect: r }
    }

    #[test]
    fn resize_shifts_static_only_right_chain() {
        // A — B — C chain with B and C classified as Right-chain members.
        // Width grows by +30; both should shift by +30 in x, no y.
        let members = vec![
            classify(Some(Side::Right), None, rect(104.0, 0.0, 100.0, 100.0)),
            classify(Some(Side::Right), None, rect(208.0, 0.0, 100.0, 100.0)),
        ];
        let (shifts, _) = resolve_cluster_shifts(&members,30, 0, 4.0, &[]);
        assert_eq!(shifts.len(), 2);
        assert_eq!(shifts[&0], (30, 0));
        assert_eq!(shifts[&1], (30, 0));
    }

    #[test]
    fn resize_shifts_left_chain_negates_width_delta() {
        let members = vec![
            classify(Some(Side::Left), None, rect(-104.0, 0.0, 100.0, 100.0)),
        ];
        let (shifts, _) = resolve_cluster_shifts(&members,20, 0, 4.0, &[]);
        assert_eq!(shifts[&0], (-20, 0));
    }

    #[test]
    fn resize_shifts_zero_delta_produces_no_shifts() {
        let members = vec![
            classify(Some(Side::Right), None, rect(104.0, 0.0, 100.0, 100.0)),
            classify(None, None, rect(0.0, 104.0, 100.0, 100.0)),
        ];
        assert!(resolve_cluster_shifts(&members, 0, 0, 4.0, &[]).0.is_empty());
    }

    #[test]
    fn resize_cascade_pulls_in_overlap_on_shrink() {
        // D — E flush at gap=4. E static-shifts -30. E_cur.x_low = 74, so
        // encroach on D = 74 - 4 - 100 = -30 → push D by -30. Result matches
        // the old inheritance semantics for this geometry because the
        // encroachment happens to equal E's full travel.
        let members = vec![
            classify(None, None, rect(0.0, 0.0, 100.0, 100.0)),
            classify(Some(Side::Right), None, rect(104.0, 0.0, 100.0, 100.0)),
        ];
        let (shifts, _) = resolve_cluster_shifts(&members,-30, 0, 4.0, &[]);
        assert_eq!(shifts[&1], (-30, 0), "E gets the static shift");
        assert_eq!(
            shifts[&0],
            (-30, 0),
            "D pushed by -30 because E.x_low - gap is 30 past D.x_high",
        );
    }

    #[test]
    fn resize_cascade_propagates_transitively() {
        // Three 40×40 members at x=[0,40], [44,84], [88,128]. Member 2
        // static-shifts -50 (C_cur.x_low=38). C pushes B by -50 (encroach
        // 38-4-84=-50); B then pushes A by -44 on the next iteration, and
        // on a third pass A gets pushed another -6 to complete the train.
        // Net: everyone at -50. Tests that the cascade converges across
        // multiple shifted/shifted push rounds.
        let members = vec![
            classify(None, None, rect(0.0, 0.0, 40.0, 40.0)),
            classify(None, None, rect(44.0, 0.0, 40.0, 40.0)),
            classify(Some(Side::Right), None, rect(88.0, 0.0, 40.0, 40.0)),
        ];
        let (shifts, _) = resolve_cluster_shifts(&members,-50, 0, 4.0, &[]);
        assert_eq!(shifts.len(), 3);
        assert_eq!(shifts[&0], (-50, 0));
        assert_eq!(shifts[&1], (-50, 0));
        assert_eq!(shifts[&2], (-50, 0));
    }

    #[test]
    fn resize_cascade_leaves_non_overlapping_members_alone() {
        // Primary resizes right; B shifts +20. D is a far-away cluster
        // member with no y-overlap with B — no push because y_overlap on
        // the current rects fails.
        let members = vec![
            classify(Some(Side::Right), None, rect(104.0, 0.0, 100.0, 100.0)),
            classify(None, None, rect(0.0, 500.0, 100.0, 100.0)),
        ];
        let (shifts, _) = resolve_cluster_shifts(&members,20, 0, 4.0, &[]);
        assert_eq!(shifts.len(), 1);
        assert_eq!(shifts[&0], (20, 0));
        assert!(!shifts.contains_key(&1));
    }

    #[test]
    fn resize_shifts_corner_drag_member_in_both_axes() {
        let members = vec![
            classify(
                Some(Side::Right),
                Some(Side::Bottom),
                rect(104.0, 104.0, 100.0, 100.0),
            ),
        ];
        let (shifts, _) = resolve_cluster_shifts(&members,25, 15, 4.0, &[]);
        assert_eq!(shifts[&0], (25, 15));
    }

    #[test]
    fn push_engages_at_snap_contact_not_overlap() {
        // B at x=[104,204] shifts by width_delta (Right chain). N sits 54px
        // past B's right edge — 50 travel + 4 gap. At width_delta=50, B_cur
        // = [154,254], encroachment = 254+4-258 = 0 → no push (snap contact
        // but no encroachment). At width_delta=51, encroachment = 255+4-258
        // = 1 → N pushed by exactly 1, NOT by the full 51. This is the
        // difference between inheritance semantics (N jumps by +51) and
        // push semantics (N follows at snap contact).
        let members = vec![
            classify(Some(Side::Right), None, rect(104.0, 0.0, 100.0, 100.0)),
            classify(None, None, rect(258.0, 0.0, 100.0, 100.0)),
        ];

        let (shifts, _) = resolve_cluster_shifts(&members,50, 0, 4.0, &[]);
        assert_eq!(shifts[&0], (50, 0));
        assert!(
            shifts.get(&1).is_none_or(|s| *s == (0, 0)),
            "N not pushed at snap contact: {:?}",
            shifts.get(&1),
        );

        let (shifts, _) = resolve_cluster_shifts(&members,51, 0, 4.0, &[]);
        assert_eq!(shifts[&0], (51, 0));
        assert_eq!(
            shifts[&1],
            (1, 0),
            "N pushed by exactly the encroachment (1), not by B's full travel (51)",
        );
    }

    #[test]
    fn push_direction_guard_ignores_backward_neighbors() {
        // 2×2 grid. Primary A at (0,0,100x100) — not in members. Cluster:
        //   B (top-right, Right chain): x=[104,204], y=[0,100]
        //   C (bottom-left, no chain):  x=[0,100],   y=[104,204]
        //   D (bottom-right, Right chain): x=[104,204], y=[104,204]
        // Right drag by +20: B and D shift right. The direction guard on
        // compute_push forbids D (moving right) from pushing C, which sits
        // on D's LEFT side (C.x_low=0 < D.x_low=104). Likewise B ignores C.
        // Without the guard, C would get pulled into a spurious shift.
        let members = vec![
            classify(Some(Side::Right), None, rect(104.0, 0.0, 100.0, 100.0)),
            classify(None, None, rect(0.0, 104.0, 100.0, 100.0)),
            classify(Some(Side::Right), None, rect(104.0, 104.0, 100.0, 100.0)),
        ];
        let (shifts, _) = resolve_cluster_shifts(&members,20, 0, 4.0, &[]);
        assert_eq!(shifts[&0], (20, 0), "B shifts right");
        assert!(
            shifts.get(&1).is_none_or(|s| *s == (0, 0)),
            "C stays put — direction guard blocks backward pushes",
        );
        assert_eq!(shifts[&2], (20, 0), "D shifts right");
    }

    #[test]
    fn push_resolves_shifted_vs_shifted_collision() {
        // Bug 3 repro. Primary B at x=[104,204], y=[0,100] (NOT in members,
        // but its geometry is what classifies A and C below). Cluster:
        //   A tall (Left chain):  x=[0,100],   y=[0,200]
        //   C bottom (Bottom chain): x=[104,204], y=[104,200]
        // A.right ↔ C.left directly. Corner-drag B's lower-left inward
        // (width_delta=-20, height_delta=-20): static shifts give A=(20,0)
        // and C=(0,-20). After statics, A_cur=[20,120]x[0,200] and
        // C_cur=[104,204]x[84,180] overlap. The push cascade must detect
        // that A (mdx=+20) is encroaching on C and push C further right by
        // exactly the encroachment — C's final shift is (20, -20).
        //
        // Old algorithm: skipped shifted-vs-shifted pairs entirely (early
        // `if shifts.contains_key(&j) { continue }`), so this collision
        // never got resolved and A/C overlapped visually.
        let members = vec![
            classify(Some(Side::Left), None, rect(0.0, 0.0, 100.0, 200.0)),
            classify(None, Some(Side::Bottom), rect(104.0, 104.0, 100.0, 96.0)),
        ];
        let (shifts, _) = resolve_cluster_shifts(&members,-20, -20, 4.0, &[]);
        assert_eq!(shifts[&0], (20, 0), "A gets Left-chain static shift");
        assert_eq!(
            shifts[&1],
            (20, -20),
            "C gets Bottom static + cascaded +20 push from A's rightward motion",
        );
    }

    #[test]
    fn push_no_cross_axis_oscillation() {
        // Regression for the "A jumps up" bug. Layout:
        //   A tall left:      x=[0,100],   y=[0,400]   (Left chain)
        //   C bottom-right:   x=[104,204], y=[200,400]  (Bottom chain)
        //
        // Corner-drag B's bottom-left inward: width_delta=-30, height_delta=-50.
        // Static: A=(30,0), C=(0,-50). The old loose guard (n_init.y_high <=
        // m_init.y_high) let C push A upward because A.y_high(400) <=
        // C.y_high(400), causing cascade oscillation. The tight guard
        // (n_init.y_high <= m_init.y_low) requires A.y_high(400) <=
        // C.y_low(200) — fails, so C does NOT push A on the y axis.
        //
        // Meanwhile A (mdx=+30) DOES push C rightward: C.x_low(104) >=
        // A.x_high(100) ✓, encroach = 130+4-104 = 30. C.shift = (30, -50).
        // No further pushes. Deterministic regardless of iteration order.
        let members = vec![
            classify(Some(Side::Left), None, rect(0.0, 0.0, 100.0, 400.0)),
            classify(None, Some(Side::Bottom), rect(104.0, 200.0, 100.0, 200.0)),
        ];
        let (shifts, _) = resolve_cluster_shifts(&members,-30, -50, 4.0, &[]);
        assert_eq!(shifts[&0], (30, 0), "A shifts right (Left chain)");
        assert_eq!(
            shifts[&1],
            (30, -50),
            "C shifts up (Bottom chain) + cascaded push from A's right motion",
        );
    }

    #[test]
    fn bond_forms_on_push_and_persists() {
        // M at x=[0,100] shifts right by +60. N at x=[154,254] starts 54px
        // past M's right edge (50 travel + 4 gap). At +60, encroach=10 →
        // push N by 10 and form bond (0,1). Re-running with the same delta
        // and the bond in place gives the same result via bond-driven shift.
        let members = vec![
            classify(Some(Side::Right), None, rect(0.0, 0.0, 100.0, 100.0)),
            classify(None, None, rect(154.0, 0.0, 100.0, 100.0)),
        ];
        let mut bonds = Vec::new();
        let (shifts, new_bonds) = resolve_cluster_shifts(&members, 60, 0, 4.0, &bonds);
        assert_eq!(shifts[&0], (60, 0));
        assert_eq!(shifts[&1], (10, 0));
        assert!(!new_bonds.is_empty(), "bond should form on first push");
        bonds.extend(new_bonds);

        // Same delta again — bond-driven, same result
        let (shifts2, _) = resolve_cluster_shifts(&members, 60, 0, 4.0, &bonds);
        assert_eq!(shifts2[&1], (10, 0));
    }

    #[test]
    fn bonded_member_follows_reversal_past_initial() {
        // Forward: M shifts +60, N pushed to +10 (bond forms).
        // Reverse: M shifts -20 (past M's initial). N should follow M back,
        // ending at a NEGATIVE shift (past N's initial position).
        let members = vec![
            classify(Some(Side::Right), None, rect(0.0, 0.0, 100.0, 100.0)),
            classify(None, None, rect(154.0, 0.0, 100.0, 100.0)),
        ];
        let mut bonds = Vec::new();
        let (_, new_bonds) = resolve_cluster_shifts(&members, 60, 0, 4.0, &bonds);
        bonds.extend(new_bonds);

        // Reverse drag: width_delta = -20 (primary shrank past initial)
        let (shifts, _) = resolve_cluster_shifts(&members, -20, 0, 4.0, &bonds);
        // M.x_high = 100 + (-20) = 80. N flush: N.x_low = 80 + 4 = 84.
        // N.shift = 84 - 154 = -70.
        assert_eq!(shifts[&0], (-20, 0));
        assert_eq!(shifts[&1], (-70, 0), "N follows M past its own initial");
    }

    #[test]
    fn bond_chain_propagates_transitively() {
        // M0=[0,40] shifts right, pushes M1=[44,84], which pushes M2=[88,128].
        // Two bonds form: (0,1) and (1,2). On reversal, both follow.
        let members = vec![
            classify(Some(Side::Right), None, rect(0.0, 0.0, 40.0, 40.0)),
            classify(None, None, rect(44.0, 0.0, 40.0, 40.0)),
            classify(None, None, rect(88.0, 0.0, 40.0, 40.0)),
        ];
        let mut bonds = Vec::new();
        let (_, new_bonds) = resolve_cluster_shifts(&members, 50, 0, 4.0, &bonds);
        bonds.extend(new_bonds);
        assert!(bonds.len() >= 2, "both bonds should form");

        // Reverse: width_delta = -10.
        let (shifts, _) = resolve_cluster_shifts(&members, -10, 0, 4.0, &bonds);
        // M0.x_high = 40+(-10)=30. M1 flush at 34. M2 flush at 78.
        assert_eq!(shifts[&0], (-10, 0));
        assert_eq!(shifts[&1], (34 - 44, 0)); // = (-10, 0)
        assert_eq!(shifts[&2], (78 - 88, 0)); // = (-10, 0)
    }

    #[test]
    fn unbonded_member_still_stops_at_initial() {
        // M shifts right but never reaches N (N is too far away). No bond
        // forms. On reversal, N stays at initial (no shift).
        let members = vec![
            classify(Some(Side::Right), None, rect(0.0, 0.0, 100.0, 100.0)),
            classify(None, None, rect(500.0, 0.0, 100.0, 100.0)),
        ];
        let mut bonds = Vec::new();
        let (shifts, new_bonds) = resolve_cluster_shifts(&members, 30, 0, 4.0, &bonds);
        assert!(!shifts.contains_key(&1), "N not pushed (too far)");
        assert!(new_bonds.is_empty());
        bonds.extend(new_bonds);

        let (shifts2, _) = resolve_cluster_shifts(&members, -10, 0, 4.0, &bonds);
        assert!(!shifts2.contains_key(&1), "N still at initial, no bond");
    }
}
