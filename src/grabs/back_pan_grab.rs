//! Buffer a Back click until pointer motion distinguishes it from a canvas drag.
use super::PanGrab;
use crate::state::{DriftWm, output_state};
use driftwm::canvas::{CanvasPos, canvas_to_screen};
use smithay::{
    backend::input::ButtonState,
    input::{
        SeatHandler,
        pointer::{ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle},
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};

pub struct BackPanGrab {
    pub start_data: GrabStartData<DriftWm>,
    pub pan: PanGrab,
    pub dragging: bool,
    pub forwarded: bool,
}

impl BackPanGrab {
    fn forward_press(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        time: u32,
    ) {
        handle.motion(
            data,
            self.start_data.focus.clone(),
            &MotionEvent {
                location: handle.current_location(),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        handle.button(
            data,
            &ButtonEvent {
                button: self.start_data.button,
                state: ButtonState::Pressed,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        self.forwarded = true;
    }
}

impl PointerGrab<DriftWm> for BackPanGrab {
    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        focus: Option<(<DriftWm as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        if !self.dragging && !self.forwarded {
            let (camera, zoom) = {
                let os = output_state(&self.pan.output);
                (os.camera, os.zoom)
            };
            let screen = canvas_to_screen(CanvasPos(event.location), camera, zoom).0;
            let delta = screen - self.pan.start_screen_pos;
            if delta.x * delta.x + delta.y * delta.y >= 36.0 {
                self.dragging = true;
                data.set_panning(true);
            }
        }
        if self.dragging && data.held_buttons.contains(&self.start_data.button) {
            self.pan.motion(data, handle, focus, event);
        } else if self.dragging {
            handle.motion(data, None, event);
        } else {
            handle.motion(data, self.start_data.focus.clone(), event);
        }
    }

    fn button(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        event: &ButtonEvent,
    ) {
        if event.button != self.start_data.button {
            if !self.dragging && !self.forwarded {
                self.forward_press(data, handle, event.time);
            }
            if !self.dragging {
                let mut forwarded = *event;
                forwarded.serial = SERIAL_COUNTER.next_serial();
                handle.button(data, &forwarded);
            }
        } else if event.state == ButtonState::Released {
            if !self.dragging {
                if !self.forwarded {
                    self.forward_press(data, handle, event.time);
                }
                let mut release = *event;
                release.serial = SERIAL_COUNTER.next_serial();
                handle.button(data, &release);
            } else {
                data.set_panning(false);
                data.launch_momentum_on(&self.pan.output);
            }
        }
        if data.held_buttons.is_empty() {
            handle.unset_grab(self, data, SERIAL_COUNTER.next_serial(), event.time, true);
        }
    }

    fn unset(&mut self, data: &mut DriftWm) {
        data.set_panning(false);
    }
    forward_pointer_grab_methods!();
}
