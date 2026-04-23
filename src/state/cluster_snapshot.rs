//! Cluster-aware snapshots captured at drag/resize grab start.
//!
//! At grab start we freeze: (a) which windows belong to the focused
//! window's cluster, and (b) per-member offsets or classification for
//! the motion loop. These snapshots are independent of the rest of
//! DriftWm — they only read `space`, `decorations`, and `config.snap_gap`.

use smithay::{
    desktop::{Space, Window},
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{Logical, Point, Size},
    wayland::seat::WaylandFocus,
};
use std::collections::{HashMap, HashSet};

use crate::decorations::WindowDecoration;

use super::DriftWm;

/// A cluster member (other than the primary) captured at resize grab start.
///
/// All cluster members are recorded, not just statically-shifted ones: the
/// motion-time cascade in `ResizeSurfaceGrab::motion` needs access to
/// non-shifted members' initial rects so it can detect post-shift overlap and
/// pull them into the shift set.
///
/// `axis_x` / `axis_y` are `Some` when this member is part of the *static*
/// shift set (steps 1+2 of the snapshot algorithm): direct neighbors of the
/// primary's resized edge, plus cluster members "downstream" of that edge
/// reachable via BFS through the full cluster graph. `None` means the member
/// stays stationary unless cascade picks it up.
pub struct ClusterResizeMember {
    pub window: Window,
    pub initial_pos: Point<i32, Logical>,
    pub initial_rect: driftwm::snap::SnapRect,
    pub axis_x: Option<crate::cluster::Side>,
    pub axis_y: Option<crate::cluster::Side>,
}

/// Frozen-at-grab-start cluster snapshot for `ResizeSurfaceGrab`.
///
/// `members` holds every cluster member except the primary. `exclude` is the
/// *static* shift set only (not the full cluster): stationary cluster
/// members remain valid `snap_targets`, so e.g. `A.right` can same-edge-snap
/// to the right edge of a cluster neighbor that isn't moving this frame.
pub struct ClusterResizeSnapshot {
    pub members: Vec<ClusterResizeMember>,
    pub exclude: HashSet<WlSurface>,
    /// Sticky bonds formed during the push cascade. Once `(m, n)` is bonded,
    /// `n` tracks `m`'s leading edge ± gap unconditionally on every
    /// subsequent frame — including drag reversal and past `n`'s initial
    /// position. Bonds persist until the grab ends (snapshot is dropped).
    /// Stored as a `Vec` to preserve insertion order, which determines
    /// cascade evaluation order for transitive chains (A→B before B→C).
    pub bonds: Vec<(usize, usize)>,
    /// Parallel set for O(1) bond dedup. Mirrors `bonds` contents.
    bonds_set: HashSet<(usize, usize)>,
    /// Primary window's rect frozen at grab start, used to compute
    /// primary-push encroachment in `resolve_cluster_shifts` phase 2.5.
    pub primary_rect: driftwm::snap::SnapRect,
    /// Which edges are active for this resize (raw u32 bitmask: top=1,
    /// bottom=2, left=4, right=8). Combined with `primary_rect` and the
    /// current deltas, this lets `compute_shifts` reconstruct the primary's
    /// current rect each frame without storing extra state.
    pub resize_edges: u32,
}

impl ClusterResizeSnapshot {
    /// Empty snapshot for single-window resize (no cluster members).
    pub fn empty() -> Self {
        Self {
            members: Vec::new(),
            exclude: HashSet::new(),
            bonds: Vec::new(),
            bonds_set: HashSet::new(),
            primary_rect: driftwm::snap::SnapRect {
                x_low: 0.0,
                x_high: 0.0,
                y_low: 0.0,
                y_high: 0.0,
            },
            resize_edges: 0,
        }
    }

    /// Compute shifts, reposition every affected cluster member, and
    /// re-map the primary to the tail of `Space::elements` so it stays
    /// on top of its own cluster. One call replaces the duplicated
    /// `compute_shifts` + `map_element` loop + primary re-map block that
    /// was previously inlined in both `ResizeSurfaceGrab::motion` and
    /// the gesture resize path.
    pub fn apply_member_shifts(
        &mut self,
        space: &mut Space<Window>,
        primary: &Window,
        initial_size: Size<i32, Logical>,
        new_w: i32,
        new_h: i32,
        gap: f64,
    ) {
        use smithay::utils::IsAlive;
        if self.members.is_empty() {
            return;
        }
        let width_delta = new_w - initial_size.w;
        let height_delta = new_h - initial_size.h;
        let shifts = self.compute_shifts(width_delta, height_delta, gap);

        for (i, (dx, dy)) in &shifts {
            let m = &self.members[*i];
            if !m.window.alive() {
                continue;
            }
            let new_pos = m.initial_pos + Point::from((*dx, *dy));
            space.map_element(m.window.clone(), new_pos, false);
        }

        if !shifts.is_empty()
            && let Some(cur) = space.element_location(primary)
        {
            space.map_element(primary.clone(), cur, false);
        }
    }

    /// Compute the per-member translation vector for one motion tick.
    ///
    /// Wraps `cluster::resolve_cluster_shifts`, passing the accumulated
    /// bonds and merging any newly-formed bonds back. Takes `&mut self`
    /// so bonds persist across frames.
    pub fn compute_shifts(
        &mut self,
        width_delta: i32,
        height_delta: i32,
        gap: f64,
    ) -> HashMap<usize, (i32, i32)> {
        use crate::cluster::{ResizeClassification, resolve_cluster_shifts};
        use smithay::utils::IsAlive;

        // Dead windows get a degenerate rect that can't produce overlap
        // in the push cascade, preventing ghost-rect collisions from a
        // member that was unmapped mid-drag.
        const DEAD_RECT: driftwm::snap::SnapRect = driftwm::snap::SnapRect {
            x_low: f64::MAX / 2.0,
            x_high: f64::MAX / 2.0,
            y_low: f64::MAX / 2.0,
            y_high: f64::MAX / 2.0,
        };

        let classifications: Vec<ResizeClassification> = self
            .members
            .iter()
            .map(|m| {
                let alive = m.window.alive();
                ResizeClassification {
                    axis_x: if alive { m.axis_x } else { None },
                    axis_y: if alive { m.axis_y } else { None },
                    initial_rect: if alive { m.initial_rect } else { DEAD_RECT },
                }
            })
            .collect();

        // Reconstruct primary's current rect from the frozen initial rect and
        // the active edges + current deltas, so phase 2.5 can push members.
        let mut p_cur = self.primary_rect;
        if self.resize_edges & 8 != 0 { p_cur.x_high += width_delta as f64; }
        if self.resize_edges & 4 != 0 { p_cur.x_low -= width_delta as f64; }
        if self.resize_edges & 2 != 0 { p_cur.y_high += height_delta as f64; }
        if self.resize_edges & 1 != 0 { p_cur.y_low -= height_delta as f64; }
        let primary = Some((self.primary_rect, p_cur));

        let (shifts, new_bonds) =
            resolve_cluster_shifts(&classifications, width_delta, height_delta, gap, &self.bonds, primary);
        for bond in new_bonds {
            if self.bonds_set.insert(bond) {
                self.bonds.push(bond);
            }
        }
        shifts
    }
}

/// `(cluster_members, cluster_member_surfaces)` — snapshot for a move drag.
/// Members carry their canvas-offset-from-primary; surfaces are the exclude
/// set for `snap_targets` during the drag.
pub type ClusterDragSnapshot = (
    Vec<(Window, Point<i32, Logical>)>,
    HashSet<WlSurface>,
);

impl DriftWm {
    /// Build snap target rectangles for all windows except `primary` and
    /// anything in `cluster_excludes` (used during a multi-window cluster
    /// drag to stop members from snapping against each other). Widgets are
    /// always skipped.
    pub fn snap_targets(
        &self,
        primary: &WlSurface,
        cluster_excludes: &HashSet<WlSurface>,
    ) -> (Vec<driftwm::snap::SnapRect>, i32) {
        snap_targets_impl(&self.space, &self.decorations, primary, cluster_excludes)
    }

    /// Every non-widget window in the space with its `SnapRect`. Used to
    /// feed `cluster::cluster_of` at drag start. The surface is discarded —
    /// `Window` identity is what the BFS needs.
    pub fn all_windows_with_snap_rects(&self) -> Vec<(Window, driftwm::snap::SnapRect)> {
        self.space
            .elements()
            .filter_map(|w| {
                window_snap_rect(&self.space, &self.decorations, w)
                    .map(|(_, rect)| (w.clone(), rect))
            })
            .collect()
    }

    /// Snapshot the focused window's cluster for a move drag.
    ///
    /// Returns both the member offsets (from the primary's canvas position)
    /// AND the exclude set of member surfaces. Both are frozen at drag start:
    /// cluster membership doesn't change mid-drag and offsets are invariant
    /// over motion, snap, and cross-output teleport.
    //
    // smithay's `Window` wraps `Arc<WindowInner>` which contains an
    // `AtomicF64` (scale), so clippy flags it as an interior-mutable hash
    // key. Here that's a false positive: `Window: Hash + Eq` are both
    // implemented on the `Arc` pointer identity, which is stable under any
    // interior mutation of `WindowInner`. Safe to use as a HashSet member.
    #[allow(clippy::mutable_key_type)]
    pub fn cluster_snapshot_for_drag(
        &self,
        window: &Window,
        primary_pos: Point<i32, Logical>,
    ) -> ClusterDragSnapshot {
        let rects = self.all_windows_with_snap_rects();
        let component = crate::cluster::cluster_of(window, &rects, self.config.snap_gap);

        let mut members = Vec::new();
        let mut surfaces = HashSet::new();
        for m in component {
            if &m == window {
                continue;
            }
            let Some(pos) = self.space.element_location(&m) else {
                continue;
            };
            let offset = pos - primary_pos;
            if let Some(s) = m.wl_surface() {
                surfaces.insert(s.into_owned());
            }
            members.push((m, offset));
        }
        (members, surfaces)
    }

    /// Snapshot the focused window's cluster for a resize drag.
    ///
    /// Three-step classification, per active resize edge:
    ///
    /// 1. **Seed** with the primary's direct neighbors on that edge
    ///    (first hop only — `adjacent_side(primary, m) == Some(side)`).
    /// 2. **BFS expand** from the seed through the full cluster graph
    ///    (any adjacency direction), adding any member whose rect is
    ///    *downstream* of the primary's edge. For a right drag, downstream
    ///    means `m.x_low >= primary.x_high` — i.e. the member sits on or
    ///    past the primary's right edge and can be pushed by it.
    /// 3. Members not in any static shift set are still stored (with
    ///    `axis_x = None`, `axis_y = None`) so that the motion-time
    ///    cascade can detect and resolve overlap between shifted and
    ///    non-shifted members.
    ///
    /// Steps 1+2 are static (computed once at grab start). Step 3 runs per
    /// motion tick inside `ResizeSurfaceGrab::motion`.
    //
    // Same Arc-identity rationale as `cluster_snapshot_for_drag`: smithay's
    // `Window` is hashed by Arc pointer and is stable under interior mutation.
    #[allow(clippy::mutable_key_type)]
    pub fn cluster_snapshot_for_resize(
        &self,
        window: &Window,
        edges: xdg_toplevel::ResizeEdge,
    ) -> ClusterResizeSnapshot {
        use crate::cluster::{Side, adjacent_side, cluster_of};
        use crate::grabs::{has_bottom, has_left, has_right, has_top};
        use driftwm::snap::SnapRect;

        let rects = self.all_windows_with_snap_rects();
        let gap = self.config.snap_gap;
        let full = cluster_of(window, &rects, gap);

        // Primary's SnapRect for the downstream filter. Bail with an empty
        // snapshot if the primary somehow has no rect (widget, unmapped).
        let Some(primary_rect) = rects
            .iter()
            .find(|(w, _)| w == window)
            .map(|(_, r)| *r)
        else {
            return ClusterResizeSnapshot::empty();
        };

        // Rect lookup for BFS inner loop. Keys are cloned Arcs — cheap.
        let rect_of: HashMap<Window, SnapRect> =
            rects.iter().map(|(w, r)| (w.clone(), *r)).collect();

        // Compute the static shift set for a single side.
        let compute_shift_set = |side: Side| -> HashSet<Window> {
            // Step 1: direct neighbors of primary on this side.
            let mut set: HashSet<Window> = HashSet::new();
            let mut queue: std::collections::VecDeque<Window> =
                std::collections::VecDeque::new();
            for (w, r) in &rects {
                if w == window {
                    continue;
                }
                if adjacent_side(&primary_rect, r, gap) == Some(side) {
                    set.insert(w.clone());
                    queue.push_back(w.clone());
                }
            }

            // Step 2: BFS through full cluster graph, adding members whose
            // rect is downstream of the primary's `side` edge.
            let is_downstream = |r: &SnapRect| -> bool {
                match side {
                    Side::Right => r.x_low >= primary_rect.x_high,
                    Side::Left => r.x_high <= primary_rect.x_low,
                    Side::Bottom => r.y_low >= primary_rect.y_high,
                    Side::Top => r.y_high <= primary_rect.y_low,
                }
            };

            while let Some(current) = queue.pop_front() {
                let Some(cur_rect) = rect_of.get(&current).copied() else {
                    continue;
                };
                for (w, r) in &rects {
                    if w == window || set.contains(w) || !full.contains(w) {
                        continue;
                    }
                    if adjacent_side(&cur_rect, r, gap).is_none() {
                        continue;
                    }
                    if !is_downstream(r) {
                        continue;
                    }
                    set.insert(w.clone());
                    queue.push_back(w.clone());
                }
            }

            set
        };

        let right_set = if has_right(edges) {
            compute_shift_set(Side::Right)
        } else {
            HashSet::new()
        };
        let left_set = if has_left(edges) {
            compute_shift_set(Side::Left)
        } else {
            HashSet::new()
        };
        let bottom_set = if has_bottom(edges) {
            compute_shift_set(Side::Bottom)
        } else {
            HashSet::new()
        };
        let top_set = if has_top(edges) {
            compute_shift_set(Side::Top)
        } else {
            HashSet::new()
        };

        // Exclude set = union of static shift sets only. Stationary cluster
        // members stay as valid snap targets so the primary can same-edge
        // or opposite-edge snap against them.
        let mut exclude: HashSet<WlSurface> = HashSet::new();
        for set in [&right_set, &left_set, &bottom_set, &top_set] {
            for w in set {
                if let Some(s) = w.wl_surface() {
                    exclude.insert(s.into_owned());
                }
            }
        }

        // Build the member list from the FULL cluster. Members with no
        // static shift on either axis still ride along so the cascade can
        // reach them.
        let mut members = Vec::new();
        for m in full {
            if &m == window {
                continue;
            }
            let Some(initial_pos) = self.space.element_location(&m) else {
                continue;
            };
            let Some(initial_rect) = rect_of.get(&m).copied() else {
                continue;
            };
            let axis_x = if right_set.contains(&m) {
                Some(Side::Right)
            } else if left_set.contains(&m) {
                Some(Side::Left)
            } else {
                None
            };
            let axis_y = if bottom_set.contains(&m) {
                Some(Side::Bottom)
            } else if top_set.contains(&m) {
                Some(Side::Top)
            } else {
                None
            };
            members.push(ClusterResizeMember {
                window: m,
                initial_pos,
                initial_rect,
                axis_x,
                axis_y,
            });
        }

        ClusterResizeSnapshot {
            members,
            exclude,
            bonds: Vec::new(),
            bonds_set: HashSet::new(),
            primary_rect,
            resize_edges: edges as u32,
        }
    }
}

/// Free-function form of `DriftWm::snap_targets` for callers that already hold
/// a mutable borrow on another `DriftWm` field (e.g. `gesture_state`) and so
/// cannot call the `&self` method. Rust's borrow checker accepts disjoint field
/// borrows when they're passed as separate references.
pub(crate) fn snap_targets_impl(
    space: &Space<Window>,
    decorations: &HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        WindowDecoration,
    >,
    primary: &WlSurface,
    cluster_excludes: &HashSet<WlSurface>,
) -> (Vec<driftwm::snap::SnapRect>, i32) {
    let self_bar = if decorations.contains_key(&primary.id()) {
        driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT
    } else {
        0
    };
    let mut others = Vec::new();
    for w in space.elements() {
        let Some((surface, rect)) = window_snap_rect(space, decorations, w) else {
            continue;
        };
        if surface == *primary || cluster_excludes.contains(&surface) {
            continue;
        }
        others.push(rect);
    }
    (others, self_bar)
}

/// Compute the snap rectangle for a single window, returning its surface
/// alongside (needed by `snap_targets_impl` for exclusion checks). Returns
/// `None` for widgets, unmapped windows, or anything without a `wl_surface`.
/// Y-low includes the title-bar height for CSD-decorated windows.
fn window_snap_rect(
    space: &Space<Window>,
    decorations: &HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        WindowDecoration,
    >,
    w: &Window,
) -> Option<(WlSurface, driftwm::snap::SnapRect)> {
    let surface = w.wl_surface()?.into_owned();
    if driftwm::config::applied_rule(&surface).is_some_and(|r| r.widget) {
        return None;
    }
    let loc = space.element_location(w)?;
    let size = w.geometry().size;
    let bar = if decorations.contains_key(&surface.id()) {
        driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT
    } else {
        0
    };
    Some((
        surface,
        driftwm::snap::SnapRect {
            x_low: loc.x as f64,
            x_high: loc.x as f64 + size.w as f64,
            y_low: loc.y as f64 - bar as f64,
            y_high: loc.y as f64 + size.h as f64,
        },
    ))
}
