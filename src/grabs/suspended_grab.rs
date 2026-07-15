//! Lightweight pointer grabs for moving and resizing suspended windows.
//!
//! A suspended window has no client, so the surface-driven [`MoveSurfaceGrab`]
//! and [`ResizeSurfaceGrab`] (which hold a concrete `Window` and drive
//! client configures) don't apply. These grabs just update the stage position
//! and the size `Cell`; the render pass rebuilds the chrome/label from the new
//! size, with no configure/ack. Moves don't snap — suspended windows are out of
//! snap/cluster in pass 1.
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
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Size},
};

use crate::grabs::{has_bottom, has_left, has_right, has_top};
use crate::state::{DriftWm, StageWindow, SuspendedId};

/// Smallest a suspended window may be dragged to — keeps the chrome usable.
const MIN_SUSPENDED_SIZE: i32 = 120;

pub struct SuspendedMoveGrab {
    pub start_data: GrabStartData<DriftWm>,
    id: SuspendedId,
    /// Canvas offset from the window origin to the grab point, held constant so
    /// the grabbed spot stays under the cursor.
    grab_offset: Point<f64, Logical>,
}

impl SuspendedMoveGrab {
    pub fn new(
        start_data: GrabStartData<DriftWm>,
        id: SuspendedId,
        origin: Point<i32, Logical>,
        grab_point: Point<f64, Logical>,
    ) -> Self {
        Self {
            start_data,
            id,
            grab_offset: grab_point - origin.to_f64(),
        }
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
        if let Some(s) = data.find_suspended(self.id) {
            let new_loc = (event.location - self.grab_offset).to_i32_round();
            data.map_window(StageWindow::Suspended(s), new_loc, false);
        }
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
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn unset(&mut self, data: &mut DriftWm) {
        // The move settled: persist the new position on the debounce timer.
        data.session_store_mark_dirty();
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
}

impl SuspendedResizeGrab {
    pub fn new(
        start_data: GrabStartData<DriftWm>,
        id: SuspendedId,
        edges: xdg_toplevel::ResizeEdge,
        initial_loc: Point<i32, Logical>,
        initial_size: Size<i32, Logical>,
        start_canvas: Point<f64, Logical>,
    ) -> Self {
        Self {
            start_data,
            id,
            edges,
            initial_loc,
            initial_size,
            start_canvas,
        }
    }

    fn apply(&self, data: &mut DriftWm, cursor: Point<f64, Logical>) {
        let Some(s) = data.find_suspended(self.id) else {
            return;
        };
        let dx = (cursor.x - self.start_canvas.x).round() as i32;
        let dy = (cursor.y - self.start_canvas.y).round() as i32;

        let mut loc = self.initial_loc;
        let mut size = self.initial_size;

        if has_right(self.edges) {
            size.w = (self.initial_size.w + dx).max(MIN_SUSPENDED_SIZE);
        } else if has_left(self.edges) {
            // Dragging the left edge past the min pins it at the min width.
            let clamped_dx = dx.min(self.initial_size.w - MIN_SUSPENDED_SIZE);
            size.w = self.initial_size.w - clamped_dx;
            loc.x = self.initial_loc.x + clamped_dx;
        }
        if has_bottom(self.edges) {
            size.h = (self.initial_size.h + dy).max(MIN_SUSPENDED_SIZE);
        } else if has_top(self.edges) {
            let clamped_dy = dy.min(self.initial_size.h - MIN_SUSPENDED_SIZE);
            size.h = self.initial_size.h - clamped_dy;
            loc.y = self.initial_loc.y + clamped_dy;
        }

        s.size.set(size);
        data.map_window(StageWindow::Suspended(s), loc, false);
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
        // The resize settled: persist the new size on the debounce timer.
        data.session_store_mark_dirty();
    }

    crate::grabs::forward_pointer_grab_methods!();
}
