use crate::DriftWm;
use driftwm::canvas::{CanvasPos, canvas_to_screen};
use smithay::{
    input::pointer::{ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle},
    utils::{Logical, Point},
};

pub struct LayerClickGrab {
    pub start_data: GrabStartData<DriftWm>,
    pub screen_loc: Point<f64, Logical>,
}

impl PointerGrab<DriftWm> for LayerClickGrab {
    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        _focus: Option<(
            <DriftWm as smithay::input::SeatHandler>::PointerFocus,
            Point<f64, Logical>,
        )>,
        event: &MotionEvent,
    ) {
        let canvas_pos = event.location;
        let camera = data.camera();
        let zoom = data.zoom();
        let screen_pos = canvas_to_screen(CanvasPos(canvas_pos), camera, zoom).0;

        let new_loc = self.screen_loc + (canvas_pos - screen_pos);
        let focus = self
            .start_data
            .focus
            .as_ref()
            .map(|(f, _)| (f.clone(), new_loc));

        handle.motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            handle.unset_grab(self, data, event.serial, event.time, false);
        }
    }

    fn unset(&mut self, _data: &mut DriftWm) {}

    forward_pointer_grab_methods!();
}
