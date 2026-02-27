use smithay::{
    input::{
        pointer::{
            ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
        },
        SeatHandler,
    },
    utils::{Logical, Point},
};

use driftwm::canvas::{CanvasPos, canvas_to_screen};
use driftwm::config::Action;
use crate::input::gestures::direction_from_vector;
use crate::state::DriftWm;

/// Squared pixel threshold before a direction is chosen (same as 4-finger swipe).
const THRESHOLD_SQ: f64 = 16.0 * 16.0;

/// Pointer grab that accumulates drag delta to navigate to the nearest window.
/// Uses "natural" direction: drag right → navigate right (negated screen delta,
/// matching 4-finger swipe convention).
pub struct NavigateGrab {
    pub start_data: GrabStartData<DriftWm>,
    last_screen_pos: Point<f64, Logical>,
    cumulative: Point<f64, Logical>,
    fired: bool,
}

impl NavigateGrab {
    pub fn new(
        start_data: GrabStartData<DriftWm>,
        screen_pos: Point<f64, Logical>,
    ) -> Self {
        Self {
            start_data,
            last_screen_pos: screen_pos,
            cumulative: Point::from((0.0, 0.0)),
            fired: false,
        }
    }
}

impl PointerGrab<DriftWm> for NavigateGrab {
    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        _focus: Option<(<DriftWm as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        let current_screen = canvas_to_screen(CanvasPos(event.location), data.camera, data.zoom).0;
        let screen_delta = current_screen - self.last_screen_pos;
        self.last_screen_pos = current_screen;

        if !self.fired {
            // Natural direction: negate delta (drag right → navigate right)
            self.cumulative -= screen_delta;

            let mag_sq = self.cumulative.x * self.cumulative.x
                + self.cumulative.y * self.cumulative.y;

            if mag_sq >= THRESHOLD_SQ {
                let dir = direction_from_vector(self.cumulative);
                data.execute_action(&Action::CenterNearest(dir));
                self.fired = true;
            }
        }

        // Always forward — warp_pointer sends motion during camera animation
        // to keep cursor at the same screen position
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

    fn unset(&mut self, _data: &mut DriftWm) {}

    crate::grabs::forward_pointer_grab_methods!();
}
