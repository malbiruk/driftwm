//! Cluster snapshots captured at drag/resize grab start: cluster membership
//! plus per-member offsets / classifications for the motion loop. Only reads
//! `stage`, `decorations`, and `config.snap_gap`.

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

/// Cluster member (other than primary) captured at resize grab start.
/// Non-shifted members are stored too so the motion-time cascade can detect
/// post-shift overlap and pull them in.
///
/// `axis_x` / `axis_y` are `Some` for the *static* shift set (direct
/// neighbors of the primary's resized edge, plus downstream cluster
/// members). `None` means stationary unless cascade picks it up.
pub struct ClusterResizeMember {
    pub window: Window,
    pub initial_pos: Point<i32, Logical>,
    pub initial_rect: driftwm::layout::snap::SnapRect,
    pub axis_x: Option<driftwm::layout::cluster::Side>,
    pub axis_y: Option<driftwm::layout::cluster::Side>,
}

/// Frozen-at-grab-start cluster snapshot for `ResizeSurfaceGrab`.
///
/// `members` holds every cluster member except primary. `exclude` is the
/// *static* shift set only — stationary cluster members stay valid
/// `snap_targets` so e.g. `A.right` can snap to a still-life neighbor.
pub struct ClusterResizeSnapshot {
    pub members: Vec<ClusterResizeMember>,
    pub exclude: HashSet<WlSurface>,
    /// Sticky bonds formed during push cascade. Once `(m, n)` is bonded, `n`
    /// tracks `m`'s leading edge ± gap unconditionally — including drag
    /// reversal past `n`'s initial position. Vec preserves insertion order
    /// for transitive chains (A→B before B→C). Persists until grab end.
    pub bonds: Vec<(usize, usize)>,
    /// O(1) dedup mirror of `bonds`.
    bonds_set: HashSet<(usize, usize)>,
    /// Primary's rect frozen at grab start, for primary-push encroachment.
    pub primary_rect: driftwm::layout::snap::SnapRect,
    /// Active resize edges as raw bitmask (top=1, bottom=2, left=4, right=8).
    /// Combined with `primary_rect` + current deltas, reconstructs the
    /// primary's current rect each frame.
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
            primary_rect: driftwm::layout::snap::SnapRect {
                x_low: 0.0,
                x_high: 0.0,
                y_low: 0.0,
                y_high: 0.0,
            },
            resize_edges: 0,
        }
    }

    /// Compute shifts, reposition every affected cluster member, and re-map
    /// the primary to the top of the z-order so it stays on top of its own
    /// cluster. Writes go through stage and space together (the map_window
    /// contract, inlined here because the grab owns this snapshot, not
    /// `DriftWm`).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::disallowed_methods)]
    pub fn apply_member_shifts(
        &mut self,
        stage: &mut driftwm::stage::Stage<Window>,
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
            stage.map(m.window.clone(), new_pos);
            space.map_element(m.window.clone(), new_pos, false);
        }

        if !shifts.is_empty()
            && let Some(cur) = stage.position_of(primary)
        {
            stage.map(primary.clone(), cur);
            space.map_element(primary.clone(), cur, false);
        }
    }

    /// Per-member translation vector for one motion tick. Wraps
    /// `resolve_cluster_shifts`, persisting newly-formed bonds across frames.
    pub fn compute_shifts(
        &mut self,
        width_delta: i32,
        height_delta: i32,
        gap: f64,
    ) -> HashMap<usize, (i32, i32)> {
        use driftwm::layout::cluster::{ResizeClassification, resolve_cluster_shifts};
        use smithay::utils::IsAlive;

        // Dead windows get a degenerate rect that can't overlap in cascade,
        // preventing ghost-rect collisions from mid-drag unmaps.
        const DEAD_RECT: driftwm::layout::snap::SnapRect = driftwm::layout::snap::SnapRect {
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

        // Reconstruct primary's current rect from frozen initial + active
        // edges + deltas, so phase 2.5 can push members.
        let mut p_cur = self.primary_rect;
        if self.resize_edges & 8 != 0 {
            p_cur.x_high += width_delta as f64;
        }
        if self.resize_edges & 4 != 0 {
            p_cur.x_low -= width_delta as f64;
        }
        if self.resize_edges & 2 != 0 {
            p_cur.y_high += height_delta as f64;
        }
        if self.resize_edges & 1 != 0 {
            p_cur.y_low -= height_delta as f64;
        }
        let primary = Some((self.primary_rect, p_cur));

        let (shifts, new_bonds) = resolve_cluster_shifts(
            &classifications,
            width_delta,
            height_delta,
            gap,
            &self.bonds,
            primary,
        );
        for bond in new_bonds {
            if self.bonds_set.insert(bond) {
                self.bonds.push(bond);
            }
        }
        shifts
    }
}

/// Snapshot for a move drag: members carry canvas-offset-from-primary;
/// surfaces are the exclude set for `snap_targets`.
pub type ClusterDragSnapshot = (Vec<(Window, Point<i32, Logical>)>, HashSet<WlSurface>);

impl DriftWm {
    /// Snap target rects for all windows except `primary` and
    /// `cluster_excludes` (latter used during multi-window cluster drags to
    /// stop members snapping against each other). Widgets, pinned, and
    /// fullscreen windows are never snap targets (see `snap_rect_for`).
    pub fn snap_targets(
        &self,
        primary: &WlSurface,
        cluster_excludes: &HashSet<WlSurface>,
    ) -> (Vec<driftwm::layout::snap::SnapRect>, i32, i32) {
        let dc = &self.config.decorations;
        let self_bar = if self.decorations.contains_key(&primary.id()) {
            dc.title_bar_height
        } else {
            0
        };
        let primary_rule = driftwm::config::applied_rule(primary);
        let primary_mode = driftwm::config::effective_decoration_mode(
            primary_rule.as_ref().and_then(|r| r.decoration.as_ref()),
            &dc.default_mode,
        );
        let self_bw =
            driftwm::config::effective_border_width(primary_rule.as_ref(), primary_mode, dc);

        // `snap_rect_for` is the single definition of a snappable window — it
        // already drops widgets, pinned, and fullscreen windows; here we only
        // additionally skip the primary itself and its frozen cluster.
        let mut others = Vec::new();
        for w in self.stage.windows() {
            let Some(surface) = w.wl_surface() else {
                continue;
            };
            if &*surface == primary || cluster_excludes.contains(&*surface) {
                continue;
            }
            if let Some(rect) = self.snap_rect_for(w) {
                others.push(rect);
            }
        }
        (others, self_bar, self_bw)
    }

    /// Every non-widget window with its `SnapRect`. Feeds `cluster_of` at
    /// drag start; BFS needs `Window` identity, not surface.
    pub fn all_windows_with_snap_rects(&self) -> Vec<(Window, driftwm::layout::snap::SnapRect)> {
        self.space
            .elements()
            .filter_map(|w| self.snap_rect_for(w).map(|rect| (w.clone(), rect)))
            .collect()
    }

    /// Border + title-bar inflated `SnapRect`. `None` for widgets / pinned /
    /// fullscreen / unmapped. Pinned and fullscreen windows live in screen space
    /// (a fullscreen window is parked at its output's camera origin), so they
    /// have no canvas snap rect — this excludes them from snapping, clustering,
    /// and all the viewport-relation queries built on top of it.
    pub fn snap_rect_for(&self, w: &Window) -> Option<driftwm::layout::snap::SnapRect> {
        if self.is_pinned(w) || self.is_window_fullscreen(w) {
            return None;
        }
        window_snap_rect(&self.stage, &self.decorations, &self.config.decorations, w)
            .map(|(_, r)| r)
    }

    /// Snapshot `w`'s current `SnapRect` into `stable_snap_rects`. Call on
    /// settled events: initial map, grab end, post-unfit recenter, fit/
    /// unfit-snapped cluster members. Fit/unfit primaries are cached by
    /// those paths directly (configure not yet acked, so `geometry().size`
    /// would be wrong here). The cached rect outlives mid-teardown geometry
    /// changes and is consulted by `first_spatially_related_in_history`.
    pub fn refresh_stable_snap_rect(&mut self, w: &Window) {
        let Some(rect) = self.snap_rect_for(w) else {
            return;
        };
        let Some(surface) = w.wl_surface() else {
            return;
        };
        self.stable_snap_rects.insert(surface.id(), rect);
    }

    /// Snapshot the focused window's cluster for a move drag: member offsets
    /// from primary + exclude set of member surfaces. Frozen at drag start —
    /// cluster membership and offsets are invariant over motion / snap /
    /// cross-output teleport.
    //
    // smithay's `Window` wraps `Arc<WindowInner>` (contains AtomicF64), so
    // clippy flags interior-mutable hash key. False positive: Hash + Eq are
    // implemented on Arc pointer identity, stable under interior mutation.
    #[allow(clippy::mutable_key_type)]
    pub fn cluster_snapshot_for_drag(
        &self,
        window: &Window,
        primary_pos: Point<i32, Logical>,
    ) -> ClusterDragSnapshot {
        let rects = self.all_windows_with_snap_rects();
        let component = driftwm::layout::cluster::cluster_of(window, &rects, self.config.snap_gap);

        let mut members = Vec::new();
        let mut surfaces = HashSet::new();
        for m in component {
            if &m == window {
                continue;
            }
            let Some(pos) = self.stage.position_of(&m) else {
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

    /// Snapshot focused window's cluster for a resize drag.
    ///
    /// Three-step classification per active resize edge:
    ///   1. **Seed**: primary's direct neighbors on that edge.
    ///   2. **BFS expand**: through the full cluster graph, adding any
    ///      member whose rect is *downstream* of the primary's edge
    ///      (for Right: `m.x_low >= primary.x_high`).
    ///   3. Off-shift-set members are stored with `axis_x/axis_y = None` so
    ///      the motion-time cascade can still detect overlap with them.
    //
    // Same Arc-identity rationale as `cluster_snapshot_for_drag`.
    #[allow(clippy::mutable_key_type)]
    pub fn cluster_snapshot_for_resize(
        &self,
        window: &Window,
        edges: xdg_toplevel::ResizeEdge,
    ) -> ClusterResizeSnapshot {
        use crate::grabs::{has_bottom, has_left, has_right, has_top};
        use driftwm::layout::cluster::{Side, adjacent_side, cluster_of};
        use driftwm::layout::snap::SnapRect;

        let rects = self.all_windows_with_snap_rects();
        let gap = self.config.snap_gap;
        let full = cluster_of(window, &rects, gap);

        // Primary's SnapRect for downstream filter; bail empty if absent
        // (widget, unmapped).
        let Some(primary_rect) = rects.iter().find(|(w, _)| w == window).map(|(_, r)| *r) else {
            return ClusterResizeSnapshot::empty();
        };

        // Rect lookup keyed on cloned Arc — cheap.
        let rect_of: HashMap<Window, SnapRect> =
            rects.iter().map(|(w, r)| (w.clone(), *r)).collect();

        let compute_shift_set = |side: Side| -> HashSet<Window> {
            // Step 1: direct neighbors of primary on this side.
            let mut set: HashSet<Window> = HashSet::new();
            let mut queue: std::collections::VecDeque<Window> = std::collections::VecDeque::new();
            for (w, r) in &rects {
                if w == window {
                    continue;
                }
                if adjacent_side(&primary_rect, r, gap) == Some(side) {
                    set.insert(w.clone());
                    queue.push_back(w.clone());
                }
            }

            // Step 2: BFS the full cluster graph, adding downstream members.
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

        // Exclude = union of static shift sets only. Stationary members
        // stay as snap targets so the primary can snap against them.
        let mut exclude: HashSet<WlSurface> = HashSet::new();
        for set in [&right_set, &left_set, &bottom_set, &top_set] {
            for w in set {
                if let Some(s) = w.wl_surface() {
                    exclude.insert(s.into_owned());
                }
            }
        }

        // Members come from the FULL cluster — unshifted ones still ride
        // along so the cascade can reach them.
        let mut members = Vec::new();
        for m in full {
            if &m == window {
                continue;
            }
            let Some(initial_pos) = self.stage.position_of(&m) else {
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

/// `(surface, snap_rect)` for one window. Y-low includes title-bar height
/// for SSD windows; the rect is inflated by border_width on all four sides
/// so snap/cluster math operates on the visible footprint. `None` for
/// widgets / unmapped / surfaceless.
fn window_snap_rect(
    stage: &driftwm::stage::Stage<Window>,
    decorations: &HashMap<smithay::reexports::wayland_server::backend::ObjectId, WindowDecoration>,
    decoration_config: &driftwm::config::DecorationConfig,
    w: &Window,
) -> Option<(WlSurface, driftwm::layout::snap::SnapRect)> {
    let surface = w.wl_surface()?.into_owned();
    let applied = driftwm::config::applied_rule(&surface);
    if applied.as_ref().is_some_and(|r| r.widget) {
        return None;
    }
    let loc = stage.position_of(w)?;
    let size = w.geometry().size;
    let bar = if decorations.contains_key(&surface.id()) {
        decoration_config.title_bar_height
    } else {
        0
    };
    let mode = driftwm::config::effective_decoration_mode(
        applied.as_ref().and_then(|r| r.decoration.as_ref()),
        &decoration_config.default_mode,
    );
    let bw =
        driftwm::config::effective_border_width(applied.as_ref(), mode, decoration_config) as f64;
    Some((
        surface,
        driftwm::layout::snap::SnapRect {
            x_low: loc.x as f64 - bw,
            x_high: loc.x as f64 + size.w as f64 + bw,
            y_low: loc.y as f64 - bar as f64 - bw,
            y_high: loc.y as f64 + size.h as f64 + bw,
        },
    ))
}
