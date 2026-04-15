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
}
