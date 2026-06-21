use smithay::{
    input::{
        SeatHandler,
        pointer::{ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle},
    },
    output::Output,
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::state::{DriftWm, output_logical_size, output_state};
use driftwm::canvas::{CanvasPos, ScreenPos, canvas_to_screen, screen_to_canvas};

/// Max squared screen-pixel distance for a press-release to count as a
/// "click" (deselect) rather than a "drag" (pan). 5px → 25.
const CLICK_THRESHOLD_SQ: f64 = 25.0;

/// Pointer grab that pans the viewport camera with momentum. Triggered by
/// Super+left-click or left-click on empty canvas; accumulates momentum so
/// the viewport coasts on release.
pub struct PanGrab {
    pub start_data: GrabStartData<DriftWm>,
    pub last_screen_pos: Point<f64, Logical>,
    /// Position at grab start — compared on release to decide click vs drag.
    pub start_screen_pos: Point<f64, Logical>,
    pub from_empty_canvas: bool,
    /// True once pointer has moved past CLICK_THRESHOLD from start.
    pub dragged: bool,
    /// Output this grab is pinned to; uses its camera/zoom throughout.
    pub output: Output,
    /// Last cursor canvas position clamped to `output`'s bounds. Re-dispatched
    /// when input routing momentarily crosses to another output mid-pan.
    pub last_clamped_location: Point<f64, Logical>,
}

impl PointerGrab<DriftWm> for PanGrab {
    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        _focus: Option<(<DriftWm as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // If input routing crossed to another output mid-pan, event.location is
        // expressed in that output's canvas frame; running it through this
        // output's camera yields a bogus delta that runs the camera away. Snap
        // the pointer back to the last in-bounds position and drop the event.
        if data
            .focused_output
            .as_ref()
            .is_some_and(|fo| *fo != self.output)
        {
            data.focused_output = Some(self.output.clone());
            let clamped_event = MotionEvent {
                location: self.last_clamped_location,
                serial: event.serial,
                time: event.time,
            };
            handle.motion(data, None, &clamped_event);
            return;
        }

        let (camera, zoom) = {
            let os = output_state(&self.output);
            (os.camera, os.zoom)
        };

        // Clamp the cursor to this output's bounds so a drag-pan stays anchored
        // to the output it began on and can't wander onto a neighbour.
        let output_size = output_logical_size(&self.output);
        let screen = canvas_to_screen(CanvasPos(event.location), camera, zoom).0;
        let current_screen_pos: Point<f64, Logical> = (
            screen.x.clamp(0.0, output_size.w as f64 - 1.0),
            screen.y.clamp(0.0, output_size.h as f64 - 1.0),
        )
            .into();
        let screen_delta = current_screen_pos - self.last_screen_pos;

        let mouse_speed = data.config.mouse_speed;
        let camera_delta = Point::from((
            -screen_delta.x * mouse_speed / zoom,
            -screen_delta.y * mouse_speed / zoom,
        ));
        data.drift_pan_on(camera_delta, event.time, &self.output);
        self.last_screen_pos = current_screen_pos;

        if !self.dragged {
            let dx = current_screen_pos.x - self.start_screen_pos.x;
            let dy = current_screen_pos.y - self.start_screen_pos.y;
            if dx * dx + dy * dy >= CLICK_THRESHOLD_SQ {
                self.dragged = true;
            }
        }

        // Shift pointer canvas position so the cursor stays at the same screen spot.
        let location =
            screen_to_canvas(ScreenPos(current_screen_pos), camera, zoom).0 + camera_delta;
        self.last_clamped_location = location;
        let adjusted = MotionEvent {
            location,
            serial: event.serial,
            time: event.time,
        };
        handle.motion(data, None, &adjusted);
    }

    fn button(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            // Click-without-drag on empty canvas → unfocus. Must run BEFORE
            // unset_grab — unset() holds the pointer mutex and the seat
            // access would deadlock there.
            if self.from_empty_canvas && !self.dragged {
                let serial = SERIAL_COUNTER.next_serial();
                data.set_window_focus(None, serial);
            }
            data.set_panning(false);
            data.launch_momentum_on(&self.output);
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn unset(&mut self, _data: &mut DriftWm) {}

    crate::grabs::forward_pointer_grab_methods!();
}
