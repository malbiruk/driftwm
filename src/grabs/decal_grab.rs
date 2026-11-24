use smithay::{
    input::{
        SeatHandler,
        pointer::{ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle},
    },
    utils::{Logical, Point},
};

use crate::state::DriftWm;

/// Pointer grab that drags one decal. The decal keeps its offset from the
/// pointer; nothing snaps, and the registry is saved once when the button goes.
pub struct DecalGrab {
    pub start_data: GrabStartData<DriftWm>,
    id: u64,
    /// Where the pointer was when the drag began (internal canvas, Y-down).
    start_pointer: Point<f64, Logical>,
    /// The decal's center then (Y-up).
    start_position: [f64; 2],
}

impl DecalGrab {
    pub fn new(start_data: GrabStartData<DriftWm>, id: u64, start_position: [f64; 2]) -> Self {
        Self {
            start_pointer: start_data.location,
            start_data,
            id,
            start_position,
        }
    }
}

impl PointerGrab<DriftWm> for DecalGrab {
    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        _focus: Option<(<DriftWm as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        let delta = event.location - self.start_pointer;
        // The decal is Y-up, the pointer Y-down. A drag past the canvas bounds
        // leaves it where it was.
        let _ = data.place_decal(
            self.id,
            [
                self.start_position[0] + delta.x,
                self.start_position[1] - delta.y,
            ],
        );
        // Always forward — warp_pointer sends motion during camera animation
        // to keep the cursor at the same screen position.
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
        data.save_decals();
    }

    crate::grabs::forward_pointer_grab_methods!();
}
