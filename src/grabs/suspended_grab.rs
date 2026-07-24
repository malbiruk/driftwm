//! Lightweight pointer grab for resizing a suspended window.
//!
//! A suspended window has no client, so the surface-driven [`ResizeSurfaceGrab`]
//! (which holds a concrete `Window` and drives client configures) doesn't apply:
//! this grab updates the stage position and the size `Cell`, and the render pass
//! rebuilds the chrome/label from the new size with no configure/ack. It snaps
//! and cascades its cluster the same way the client resize does, sharing
//! `DriftWm::snap_targets` / `snap_resize_edges` / `ClusterResizeSnapshot`.
//! Moving a stand-in goes through the unified [`MoveGrab`], not a separate grab.
//!
//! The grab holds a [`SuspendedId`] (not the `Rc<SuspendedWindow>`, which isn't
//! `Send`) and looks the element up each motion; if it's dismissed mid-drag the
//! grab simply no-ops.
//!
//! [`ResizeSurfaceGrab`]: crate::grabs::ResizeSurfaceGrab
//! [`MoveGrab`]: crate::grabs::MoveGrab

use smithay::{
    input::{
        SeatHandler,
        pointer::{ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle},
    },
    output::Output,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Size},
};

use crate::grabs::{has_bottom, has_left, has_right, has_top};
use crate::state::{ClusterResizeSnapshot, DriftWm, StageWindow, SuspendedId, output_state};
use driftwm::layout::snap::{SnapState, snap_resize_edges};

/// Smallest a suspended window may be resized to — keeps the chrome usable.
const MIN_SUSPENDED_SIZE: i32 = 120;

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
