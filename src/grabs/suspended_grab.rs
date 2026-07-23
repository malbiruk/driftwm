//! Lightweight pointer grabs for moving and resizing suspended windows.
//!
//! A suspended window has no client, so the surface-driven [`MoveSurfaceGrab`]
//! and [`ResizeSurfaceGrab`] (which hold a concrete `Window` and drive
//! client configures) don't apply. These grabs update the stage position and
//! the size `Cell`; the render pass rebuilds the chrome/label from the new
//! size, with no configure/ack. They snap and cluster the same way the client
//! grabs do, sharing `DriftWm::snap_move_location` / `snap_resize_edges` /
//! `ClusterResizeSnapshot`: a plain title-bar drag is single-window + snap
//! (like a client's), while a resize-border drag cascades its cluster when
//! `decoration_resize_snapped` is set (like a client's SSD-border resize).
//!
//! The grabs hold a [`SuspendedId`] (not the `Rc<SuspendedWindow>`, which isn't
//! `Send`) and look the element up each motion; if it's dismissed mid-drag the
//! grab simply no-ops.
//!
//! [`MoveSurfaceGrab`]: crate::grabs::MoveSurfaceGrab
//! [`ResizeSurfaceGrab`]: crate::grabs::ResizeSurfaceGrab

use smithay::{
    input::{
        SeatHandler,
        pointer::{ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle},
    },
    output::Output,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Size},
};

use std::collections::HashSet;

use crate::grabs::{has_bottom, has_left, has_right, has_top};
use crate::state::{
    ClusterMember, ClusterResizeSnapshot, DriftWm, StageWindow, SuspendedId, output_state,
};
use driftwm::layout::snap::{SnapState, snap_resize_edges};

/// Smallest a suspended window may be dragged to — keeps the chrome usable.
const MIN_SUSPENDED_SIZE: i32 = 120;

pub struct SuspendedMoveGrab {
    pub start_data: GrabStartData<DriftWm>,
    id: SuspendedId,
    /// Output whose camera/zoom scales the snap thresholds.
    output: Output,
    /// Content top-left at grab start; the drag delta is added to it.
    initial_loc: Point<i32, Logical>,
    /// Grab-start cursor position in canvas space — source of the drag delta.
    start_canvas: Point<f64, Logical>,
    snap: SnapState,
    /// Cluster members carried by a group move (`MoveSnappedWindows`), each with
    /// its canvas offset from the stand-in captured at grab start. Empty for a
    /// plain single-window drag. Members may be clients or other stand-ins;
    /// each is resolved live per tick (a member that leaves the stage drops out).
    cluster_members: Vec<(ClusterMember, Point<i32, Logical>)>,
}

impl SuspendedMoveGrab {
    pub fn new(
        start_data: GrabStartData<DriftWm>,
        id: SuspendedId,
        output: Output,
        origin: Point<i32, Logical>,
        grab_point: Point<f64, Logical>,
        cluster_members: Vec<(StageWindow, Point<i32, Logical>)>,
    ) -> Self {
        Self {
            start_data,
            id,
            output,
            initial_loc: origin,
            start_canvas: grab_point,
            snap: SnapState::default(),
            cluster_members: cluster_members
                .into_iter()
                .map(|(w, offset)| (ClusterMember::from_element(&w), offset))
                .collect(),
        }
    }

    /// Live `(StageWindow, offset)` pairs for members that still resolve —
    /// members closed mid-drag drop out.
    fn resolved_members(&self, data: &DriftWm) -> Vec<(StageWindow, Point<i32, Logical>)> {
        self.cluster_members
            .iter()
            .filter_map(|(m, off)| m.resolve(&data.stage).map(|sw| (sw, *off)))
            .collect()
    }

    fn apply(&mut self, data: &mut DriftWm, cursor: Point<f64, Logical>) {
        let Some(s) = data.find_suspended(self.id) else {
            return;
        };
        let element = StageWindow::Suspended(s);
        let delta = cursor - self.start_canvas;
        let natural = Point::from((
            self.initial_loc.x as f64 + delta.x,
            self.initial_loc.y as f64 + delta.y,
        ));
        // Resolve members once and exclude them from the primary's snap targets
        // so the stand-in doesn't snap onto its own cluster — like a client drag.
        let members = self.resolved_members(data);
        let snapped = if data.config.snap_enabled {
            let zoom = output_state(&self.output).zoom;
            #[allow(clippy::mutable_key_type)]
            let excludes: HashSet<StageWindow> = members.iter().map(|(w, _)| w.clone()).collect();
            data.snap_move_location(&element, zoom, natural, &mut self.snap, &excludes)
        } else {
            natural
        };
        // Truncate, not round, to match the client move grab exactly — the two
        // share `snap_move_location`, so their placement must land on the same
        // integer pixel.
        let new_loc = Point::from((snapped.x as i32, snapped.y as i32));
        // Map members first so the primary's `map_window` lands last and stays
        // on top of its own cluster (smithay re-inserts at the z-bucket end).
        for (member, offset) in members {
            data.map_window(member, new_loc + offset, false);
        }
        data.map_window(element, new_loc, false);
    }
}

impl PointerGrab<DriftWm> for SuspendedMoveGrab {
    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        _focus: Option<(<DriftWm as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        self.apply(data, event.location);
        // No client to focus; keep the pointer unfocused as we drag.
        handle.motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            // Members shifted by the group move get their stable rect refreshed
            // so a later close can still reconstruct the cluster.
            for (member, _) in self.resolved_members(data) {
                data.refresh_stable_snap_rect(&member);
            }
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn unset(&mut self, data: &mut DriftWm) {
        // The move settled: persist the new position on the debounce timer.
        data.session_store_mark_dirty();
        // A pick-mode promote is the only stand-in move that sets grab_cursor.
        // Defer the cursor restore to the next frame's flush rather than
        // calling into PointerHandle here, where the pointer mutex may be
        // held: clear grab_cursor and queue a resync — the flush's
        // `pick_mode() || decoration_cursor` gate then recomputes the cursor.
        if data.cursor.grab_cursor {
            data.cursor.grab_cursor = false;
            data.pending_pointer_resync = true;
        }
    }

    crate::grabs::forward_pointer_grab_methods!();
}

pub struct SuspendedResizeGrab {
    pub start_data: GrabStartData<DriftWm>,
    id: SuspendedId,
    edges: xdg_toplevel::ResizeEdge,
    initial_loc: Point<i32, Logical>,
    initial_size: Size<i32, Logical>,
    start_canvas: Point<f64, Logical>,
    output: Output,
    snap: SnapState,
    /// Frozen cluster for a snapped resize (empty for single-window), so the
    /// active edge cascades member shifts exactly like a client's SSD-border
    /// resize does.
    cluster_resize: ClusterResizeSnapshot,
}

impl SuspendedResizeGrab {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        start_data: GrabStartData<DriftWm>,
        id: SuspendedId,
        edges: xdg_toplevel::ResizeEdge,
        initial_loc: Point<i32, Logical>,
        initial_size: Size<i32, Logical>,
        start_canvas: Point<f64, Logical>,
        output: Output,
        cluster_resize: ClusterResizeSnapshot,
    ) -> Self {
        Self {
            start_data,
            id,
            edges,
            initial_loc,
            initial_size,
            start_canvas,
            output,
            snap: SnapState::default(),
            cluster_resize,
        }
    }

    fn apply(&mut self, data: &mut DriftWm, cursor: Point<f64, Logical>) {
        let Some(s) = data.find_suspended(self.id) else {
            return;
        };
        let element = StageWindow::Suspended(s.clone());
        let dx = (cursor.x - self.start_canvas.x).round() as i32;
        let dy = (cursor.y - self.start_canvas.y).round() as i32;

        // Raw size from the active edges, floored to the usable minimum before
        // snap — mirrors the client resize clamping min/max ahead of the snap.
        let mut new_w = self.initial_size.w;
        let mut new_h = self.initial_size.h;
        if has_right(self.edges) {
            new_w = self.initial_size.w + dx;
        } else if has_left(self.edges) {
            new_w = self.initial_size.w - dx;
        }
        if has_bottom(self.edges) {
            new_h = self.initial_size.h + dy;
        } else if has_top(self.edges) {
            new_h = self.initial_size.h - dy;
        }
        new_w = new_w.max(MIN_SUSPENDED_SIZE);
        new_h = new_h.max(MIN_SUSPENDED_SIZE);

        if data.config.snap_enabled {
            let zoom = output_state(&self.output).zoom;
            #[allow(clippy::mutable_key_type)]
            let excludes = self.cluster_resize.exclude_set(&data.stage);
            let (others, self_bar, self_bw) = data.snap_targets(&element, &excludes);
            snap_resize_edges(
                &mut self.snap,
                self.edges as u32,
                (self.initial_loc.x, self.initial_loc.y),
                (self.initial_size.w, self.initial_size.h),
                self_bar,
                self_bw,
                &mut new_w,
                &mut new_h,
                &others,
                zoom,
                data.config.snap_gap,
                data.config.snap_distance,
                data.config.snap_break_force,
                data.config.snap_corners,
            );
        }

        // Cascade the cluster (no-op for a single-window resize), then place the
        // primary: a left/top drag keeps the opposite edge fixed.
        self.cluster_resize.apply_member_shifts(
            &mut data.stage,
            &element,
            self.initial_size,
            new_w,
            new_h,
            data.config.snap_gap,
        );

        let mut loc = self.initial_loc;
        if has_left(self.edges) {
            loc.x = self.initial_loc.x + (self.initial_size.w - new_w);
        }
        if has_top(self.edges) {
            loc.y = self.initial_loc.y + (self.initial_size.h - new_h);
        }
        s.size.set(Size::from((new_w, new_h)));
        data.map_window(element, loc, false);
    }
}

impl PointerGrab<DriftWm> for SuspendedResizeGrab {
    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        _focus: Option<(<DriftWm as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        self.apply(data, event.location);
        handle.motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            data.cursor.grab_cursor = false;
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn unset(&mut self, data: &mut DriftWm) {
        data.cursor.grab_cursor = false;
        // Client members shifted by the cascade get their stable rect refreshed
        // so a later close can still reconstruct the cluster.
        for member in &self.cluster_resize.members {
            if let Some(element) = member.window.resolve(&data.stage) {
                data.refresh_stable_snap_rect(&element);
            }
        }
        // The resize settled: persist the new size on the debounce timer.
        data.session_store_mark_dirty();
    }

    crate::grabs::forward_pointer_grab_methods!();
}
