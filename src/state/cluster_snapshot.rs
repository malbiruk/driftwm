//! Cluster snapshots captured at drag/resize grab start: cluster membership
//! plus per-member offsets / classifications for the motion loop. Only reads
//! `stage`, `decorations`, and `config.snap_gap`.

use smithay::{
    desktop::Window,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{Logical, Point, Size},
    wayland::seat::WaylandFocus,
};
use std::collections::{HashMap, HashSet};

use crate::decorations::{DecorationKey, WindowDecoration};

use super::{DriftWm, StageWindow, SuspendedId};

/// A cluster member a grab holds across motion ticks. `PointerGrab`/`TouchGrab`
/// require `Send`, but `StageWindow` wraps a non-`Send` `Rc`, so a grab can't
/// store one directly. `Window` and `SuspendedId` are both `Send`; the grab
/// stores this and re-resolves it to a live `StageWindow` each tick (a member
/// closed mid-drag simply stops resolving).
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum ClusterMember {
    Client(Window),
    Suspended(SuspendedId),
}

impl ClusterMember {
    pub fn from_element(w: &StageWindow) -> Self {
        match w {
            StageWindow::Client(c) => Self::Client(c.clone()),
            StageWindow::Suspended(s) => Self::Suspended(s.id),
        }
    }

    /// A client dies with its surface; a stand-in only leaves on dismissal
    /// (which needs the pointer this grab already holds), so it's always live
    /// mid-drag — matching `StageWindow`'s own `IsAlive`.
    pub fn is_alive(&self) -> bool {
        match self {
            Self::Client(w) => smithay::utils::IsAlive::alive(w),
            Self::Suspended(_) => true,
        }
    }

    /// Resolve back to the live stage element, or `None` if it left the stage.
    pub fn resolve(&self, stage: &driftwm::stage::Stage<StageWindow>) -> Option<StageWindow> {
        match self {
            Self::Client(w) => {
                smithay::utils::IsAlive::alive(w).then(|| StageWindow::Client(w.clone()))
            }
            Self::Suspended(id) => stage
                .windows()
                .filter_map(|w| w.suspended())
                .find(|s| s.id == *id)
                .map(|s| StageWindow::Suspended(s.clone())),
        }
    }
}

/// Cluster member (other than primary) captured at resize grab start.
/// Non-shifted members are stored too so the motion-time cascade can detect
/// post-shift overlap and pull them in.
///
/// `axis_x` / `axis_y` are `Some` for the *static* shift set (direct
/// neighbors of the primary's resized edge, plus downstream cluster
/// members). `None` means stationary unless cascade picks it up.
pub struct ClusterResizeMember {
    pub window: ClusterMember,
    pub initial_pos: Point<i32, Logical>,
    pub initial_rect: driftwm::layout::snap::SnapRect,
    pub axis_x: Option<driftwm::layout::cluster::Side>,
    pub axis_y: Option<driftwm::layout::cluster::Side>,
}

impl ClusterResizeMember {
    /// A member is in the static-shift (exclude) set exactly when it has an
    /// axis classification; stationary members stay snap targets.
    pub fn is_shifted(&self) -> bool {
        self.axis_x.is_some() || self.axis_y.is_some()
    }
}

/// Frozen-at-grab-start cluster snapshot for `ResizeSurfaceGrab`.
///
/// `members` holds every cluster member except primary. The snap-target
/// exclude set is the *static* shift set only (members with an axis
/// classification, see [`ClusterResizeMember::is_shifted`]) — stationary
/// members stay valid `snap_targets` so e.g. `A.right` can snap to a
/// still-life neighbor; grabs resolve it live via [`Self::exclude_set`].
pub struct ClusterResizeSnapshot {
    pub members: Vec<ClusterResizeMember>,
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

    /// The static-shift members resolved to live stage elements — the snap
    /// exclude set for the active resize. Rebuilt per tick since a grab can't
    /// hold `StageWindow`s.
    #[allow(clippy::mutable_key_type)]
    pub fn exclude_set(&self, stage: &driftwm::stage::Stage<StageWindow>) -> HashSet<StageWindow> {
        self.members
            .iter()
            .filter(|m| m.is_shifted())
            .filter_map(|m| m.window.resolve(stage))
            .collect()
    }

    /// Compute shifts, reposition every affected cluster member, and re-map
    /// the primary to the top of the z-order so it stays on top of its own
    /// cluster. Writes go through the stage (the map_window contract, inlined
    /// here because the grab owns this snapshot, not `DriftWm`).
    #[allow(clippy::too_many_arguments)]
    pub fn apply_member_shifts(
        &mut self,
        stage: &mut driftwm::stage::Stage<crate::state::StageWindow>,
        primary: &StageWindow,
        initial_size: Size<i32, Logical>,
        new_w: i32,
        new_h: i32,
        gap: f64,
    ) {
        if self.members.is_empty() {
            return;
        }
        let width_delta = new_w - initial_size.w;
        let height_delta = new_h - initial_size.h;
        let shifts = self.compute_shifts(width_delta, height_delta, gap);

        for (i, (dx, dy)) in &shifts {
            let m = &self.members[*i];
            let Some(element) = m.window.resolve(stage) else {
                continue;
            };
            // Shifting a member re-anchors it, invalidating any fill restore point.
            stage.clear_fill(&element);
            let new_pos = m.initial_pos + Point::from((*dx, *dy));
            stage.map(element, new_pos);
        }

        if !shifts.is_empty()
            && let Some(cur) = stage.position_of(primary)
        {
            stage.map(primary.clone(), cur);
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
                let alive = m.window.is_alive();
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

/// Snapshot for a move drag: each cluster member (other than primary) with its
/// canvas offset from the primary. Members may be suspended stand-ins. The
/// grab converts these into `Send`-safe [`ClusterMember`]s and derives the
/// `snap_targets` exclude set from them each tick.
pub type ClusterDragSnapshot = Vec<(StageWindow, Point<i32, Logical>)>;

impl DriftWm {
    /// Snap target rects for all windows except `primary` and
    /// `cluster_excludes` (latter used during multi-window cluster drags to
    /// stop members snapping against each other). Widgets, pinned, and
    /// fullscreen windows are never snap targets (see `snap_rect_for`).
    #[allow(clippy::mutable_key_type)]
    pub fn snap_targets(
        &self,
        primary: &StageWindow,
        cluster_excludes: &HashSet<StageWindow>,
    ) -> (Vec<driftwm::layout::snap::SnapRect>, i32, i32) {
        let self_bar = self.window_ssd_bar(primary);
        let self_bw = self.element_border_width(primary);

        // `snap_rect_for` is the single definition of a snappable window — it
        // already drops widgets, pinned, and fullscreen windows; here we only
        // additionally skip the primary itself and its frozen cluster.
        let mut others = Vec::new();
        for w in self.stage.windows() {
            if w == primary || cluster_excludes.contains(w) {
                continue;
            }
            if let Some(rect) = self.snap_rect_for(w) {
                others.push(rect);
            }
        }
        (others, self_bar, self_bw)
    }

    /// Snap a dragged element's natural content top-left to nearby windows,
    /// mirroring the client move-grab magnetic snap. `primary` identifies the
    /// dragged element (its size + SSD bar + border set the snapped extent);
    /// `excludes` is its frozen cluster, kept out of the target set. Returns the
    /// snapped content top-left. Callers gate on `config.snap_enabled`; the
    /// suspended and client move grabs share this so their snap is identical.
    #[allow(clippy::mutable_key_type)]
    pub(crate) fn snap_move_location(
        &self,
        primary: &StageWindow,
        zoom: f64,
        natural: Point<f64, Logical>,
        snap: &mut driftwm::layout::snap::SnapState,
        excludes: &HashSet<StageWindow>,
    ) -> Point<f64, Logical> {
        use driftwm::layout::snap::{SnapParams, update_axis};

        let effective_distance = self.config.snap_distance / zoom;
        let effective_break = self.config.snap_break_force / zoom;
        let gap = self.config.snap_gap;
        let (others, self_bar, self_bw) = self.snap_targets(primary, excludes);
        let size = primary.geometry().size;

        // Inflate self's extent by `self_bw` on each side (and the SSD bar on
        // top) so the snap math operates on the same visible-frame coords as
        // `others`, which `window_snap_rect` already inflated.
        let extent_x = size.w as f64 + 2.0 * self_bw as f64;
        let extent_y = size.h as f64 + self_bar as f64 + 2.0 * self_bw as f64;
        let visual_x = natural.x - self_bw as f64;
        let visual_y = natural.y - self_bar as f64 - self_bw as f64;

        // Perpendicular ranges track the *visual* window, not the raw cursor:
        // a held-snapped axis can let the cursor drift by up to break_force
        // while the window stays pinned, which would otherwise spawn spurious
        // corner snaps on the other axis.
        let visual_y_for_perp = snap.y.as_ref().map_or(visual_y, |s| s.snapped_pos);
        let params_x = SnapParams {
            extent: extent_x,
            perp_low: visual_y_for_perp,
            perp_high: visual_y_for_perp + extent_y,
            horizontal: true,
            others: &others,
            gap,
            threshold: effective_distance,
            break_force: effective_break,
            same_edge: self.config.snap_corners,
            edge_center: self.config.snap_centers,
        };
        let final_visual_x = update_axis(&mut snap.x, &mut snap.cooldown_x, visual_x, &params_x);

        let visual_x_for_perp = snap.x.as_ref().map_or(visual_x, |s| s.snapped_pos);
        let params_y = SnapParams {
            extent: extent_y,
            perp_low: visual_x_for_perp,
            perp_high: visual_x_for_perp + extent_x,
            horizontal: false,
            others: &others,
            gap,
            threshold: effective_distance,
            break_force: effective_break,
            same_edge: self.config.snap_corners,
            edge_center: self.config.snap_centers,
        };
        let final_visual_y = update_axis(&mut snap.y, &mut snap.cooldown_y, visual_y, &params_y);

        Point::from((
            final_visual_x + self_bw as f64,
            final_visual_y + self_bar as f64 + self_bw as f64,
        ))
    }

    /// Every snappable element with its `SnapRect`, keyed by `StageWindow` so
    /// suspended stand-ins traverse `cluster_of` as ordinary members. Feeds
    /// `cluster_of` at drag start; BFS needs element identity, not surface.
    #[allow(clippy::mutable_key_type)]
    pub fn all_windows_with_snap_rects(
        &self,
    ) -> Vec<(StageWindow, driftwm::layout::snap::SnapRect)> {
        self.stage
            .windows()
            .filter_map(|w| self.snap_rect_for(w).map(|rect| (w.clone(), rect)))
            .collect()
    }

    /// Border + title-bar inflated `SnapRect` for any stage element. `None` for
    /// widgets / pinned / fullscreen / unmapped. Pinned and fullscreen windows
    /// live in screen space (a fullscreen window is parked at its output's
    /// camera origin), so they have no canvas snap rect — this excludes them
    /// from snapping, clustering, and all the viewport-relation queries built
    /// on top of it. A suspended stand-in's rect equals what the live window's
    /// was: body rect + `window_ssd_bar` strip (0 for a barless CSD-origin
    /// stand-in) + the global-default border, so a client snaps to it exactly
    /// as it did to the window it replaced.
    pub fn snap_rect_for(&self, w: &StageWindow) -> Option<driftwm::layout::snap::SnapRect> {
        if self.is_pinned(w) || self.is_window_fullscreen(w) {
            return None;
        }
        match w {
            StageWindow::Client(c) => {
                window_snap_rect(&self.stage, &self.decorations, &self.config.decorations, c)
                    .map(|(_, r)| r)
            }
            StageWindow::Suspended(s) => {
                let loc = self.stage.position_of(w)?;
                let size = s.size.get();
                let bar = self.window_ssd_bar(w) as f64;
                let bw = self.default_border_width() as f64;
                Some(driftwm::layout::snap::SnapRect {
                    x_low: loc.x as f64 - bw,
                    x_high: loc.x as f64 + size.w as f64 + bw,
                    y_low: loc.y as f64 - bar - bw,
                    y_high: loc.y as f64 + size.h as f64 + bw,
                })
            }
        }
    }

    /// Where an element's visible frame (border + SSD title bar + content) sits
    /// on the canvas — the *navigation* rect. Now that stand-ins snap, this
    /// coincides with [`Self::snap_rect_for`]; kept as a named alias so
    /// navigation / IPC callsites read by intent.
    pub fn visual_frame_rect(&self, w: &StageWindow) -> Option<driftwm::layout::snap::SnapRect> {
        self.snap_rect_for(w)
    }

    /// Snapshot `w`'s current `SnapRect` into `stable_snap_rects`. Call on
    /// settled events: initial map, grab end, post-unfit recenter, fit/
    /// unfit-snapped cluster members. Fit/unfit primaries are cached by
    /// those paths directly (configure not yet acked, so `geometry().size`
    /// would be wrong here). The cached rect outlives mid-teardown geometry
    /// changes and is consulted by `first_spatially_related_in_history`.
    pub fn refresh_stable_snap_rect(&mut self, w: &StageWindow) {
        let Some(rect) = self.snap_rect_for(w) else {
            return;
        };
        // A stand-in has no surface to key the cache on — and needs none: its
        // `snap_rect_for` is always live-correct (no configure lag to outlast).
        let Some(surface) = w.wl_surface() else {
            return;
        };
        self.stable_snap_rects.insert(surface.id(), rect);
    }

    /// Snapshot the focused element's cluster for a move drag: each member with
    /// its canvas offset from the primary. Frozen at drag start — cluster
    /// membership and offsets are invariant over motion / snap / cross-output
    /// teleport. Members may include suspended stand-ins, which ride along at
    /// their fixed offset.
    //
    // `StageWindow` (like `Window`) hashes on pointer identity over an
    // interior-mutable payload; the lint's concern doesn't apply.
    #[allow(clippy::mutable_key_type)]
    pub fn cluster_snapshot_for_drag(
        &self,
        window: &StageWindow,
        primary_pos: Point<i32, Logical>,
    ) -> ClusterDragSnapshot {
        let rects = self.all_windows_with_snap_rects();
        let component = driftwm::layout::cluster::cluster_of(window, &rects, self.config.snap_gap);

        let mut members = Vec::new();
        for m in component {
            if &m == window {
                continue;
            }
            let Some(pos) = self.stage.position_of(&m) else {
                continue;
            };
            let offset = pos - primary_pos;
            members.push((m, offset));
        }
        members
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
        window: &StageWindow,
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

        // Rect lookup keyed on cloned element identity — cheap.
        let rect_of: HashMap<StageWindow, SnapRect> =
            rects.iter().map(|(w, r)| (w.clone(), *r)).collect();

        let compute_shift_set = |side: Side| -> HashSet<StageWindow> {
            // Step 1: direct neighbors of primary on this side.
            let mut set: HashSet<StageWindow> = HashSet::new();
            let mut queue: std::collections::VecDeque<StageWindow> =
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

        // Members come from the FULL cluster — unshifted ones still ride
        // along so the cascade can reach them. The static-shift (exclude) set
        // is recovered later from each member's axis classification.
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
                window: ClusterMember::from_element(&m),
                initial_pos,
                initial_rect,
                axis_x,
                axis_y,
            });
        }

        ClusterResizeSnapshot {
            members,
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
    stage: &driftwm::stage::Stage<crate::state::StageWindow>,
    decorations: &HashMap<DecorationKey, WindowDecoration>,
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
    let bar = if decorations.contains_key(&DecorationKey::Surface(surface.id())) {
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
