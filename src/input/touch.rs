use crate::decorations::DecorationHit;
use crate::grabs::{MoveSurfaceGrab, TouchGestureGrab};
use crate::state::{DriftWm, FocusTarget, SessionLock, output_state};
use driftwm::window_ext::WindowExt;
use driftwm::canvas::{ScreenPos, screen_to_canvas};
use smithay::{
    backend::input::{AbsolutePositionEvent, Event, InputBackend, TouchEvent, TouchSlot},
    desktop::Window,
    input::touch::{DownEvent, GrabStartData as TouchGrabStartData, MotionEvent, UpEvent},
    output::Output,
    utils::{Logical, Point, SERIAL_COUNTER},
};

/// A close-button press awaiting release. Fires only if the finger lifts while
/// still inside the button — touch's analogue of the pointer close path.
pub struct PendingClose {
    slot: TouchSlot,
    window: Window,
    last_canvas: Point<f64, Logical>,
}

/// Coordinator-side touch state. Per-gesture state lives in `TouchGestureGrab`;
/// this only holds what must survive across grab lifetimes.
pub struct TouchState {
    /// Timestamp of the last clean 3-finger tap, for double-tap detection.
    pub last_three_finger_tap: Option<u32>,
    pub pending_close: Option<PendingClose>,
}

impl TouchState {
    pub fn new() -> Self {
        Self {
            last_three_finger_tap: None,
            pending_close: None,
        }
    }
}

impl DriftWm {
    /// Output a touch maps to. No per-device mapping yet (future
    /// `[input.touch] map_to_output`); use the first output — the touch panel —
    /// rather than the keyboard-focused one, which may be a non-touch monitor.
    fn touch_output(&self) -> Option<Output> {
        self.space.outputs().next().cloned()
    }

    pub fn on_touch_down<I: InputBackend>(&mut self, event: I::TouchDownEvent) {
        if !self.config.touch.enable {
            return;
        }
        let Some(output) = self.touch_output() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        // Touch acts on its own output and hides the pointer.
        self.focused_output = Some(output.clone());
        self.cursor.hidden_by_touch = true;

        let screen_pos = event.position_transformed(output_geo.size);
        let (camera, zoom) = {
            let os = output_state(&output);
            (os.camera, os.zoom)
        };
        let canvas_pos = screen_to_canvas(ScreenPos(screen_pos), camera, zoom).0;
        let slot = event.slot();
        let time = Event::time_msec(&event);
        let serial = SERIAL_COUNTER.next_serial();

        // Locked session: forward straight to the lock surface, no gestures.
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            let Some(ls) = self.lock_surfaces.get(&output) else {
                return;
            };
            let focus = FocusTarget(ls.wl_surface().clone());
            let touch = self.seat.get_touch().unwrap();
            touch.down(
                self,
                Some((focus, Point::from((0.0, 0.0)))),
                &DownEvent {
                    slot,
                    location: screen_pos,
                    serial,
                    time,
                },
            );
            touch.frame(self);
            return;
        }

        // An active grab (canvas-gesture or move) owns routing — forward the
        // new finger into it and let it decide.
        let touch = self.seat.get_touch().unwrap();
        if touch.is_grabbed() {
            let under = self.pointer_focus_under(screen_pos, canvas_pos);
            self.seat.get_touch().unwrap().down(
                self,
                under,
                &DownEvent {
                    slot,
                    location: canvas_pos,
                    serial,
                    time,
                },
            );
            return;
        }

        // Fresh interaction. The first finger hit-tests SSD decorations.
        match self.decoration_under(canvas_pos) {
            Some((window, DecorationHit::TitleBar)) => {
                self.start_touch_move(&window, slot, canvas_pos, serial);
                return;
            }
            Some((window, DecorationHit::CloseButton)) => {
                self.touch_state.pending_close = Some(PendingClose {
                    slot,
                    window,
                    last_canvas: canvas_pos,
                });
                return;
            }
            // Resize borders aren't touch-draggable (8px ≪ a fingertip); fall
            // through to the canvas-gesture grab.
            _ => {}
        }

        // Otherwise start the canvas-gesture grab. A content touch focuses +
        // raises (same as click-to-focus); empty canvas stops any coast.
        let under = self.pointer_focus_under(screen_pos, canvas_pos);
        if let Some((ref target, _)) = under {
            if let Some(window) = self.window_for_surface(&target.0) {
                self.raise_and_focus(&window, serial);
            } else {
                self.set_window_focus(Some(target.clone()), serial);
            }
        } else {
            self.cancel_animations();
        }

        let start_data = TouchGrabStartData {
            focus: under.clone(),
            slot,
            location: canvas_pos,
        };
        let grab = TouchGestureGrab::new(start_data, output);
        let touch = self.seat.get_touch().unwrap();
        touch.set_grab(self, grab, serial);
        self.seat.get_touch().unwrap().down(
            self,
            under,
            &DownEvent {
                slot,
                location: canvas_pos,
                serial,
                time,
            },
        );
    }

    fn start_touch_move(
        &mut self,
        window: &Window,
        slot: TouchSlot,
        location: Point<f64, Logical>,
        serial: smithay::utils::Serial,
    ) {
        let Some(output) = self.touch_output() else {
            return;
        };
        let Some(initial) = self.space.element_location(window) else {
            return;
        };
        self.raise_and_focus(window, serial);
        let start = TouchGrabStartData {
            focus: None,
            slot,
            location,
        };
        // One finger down (the titlebar press); the grab intercepts its motion
        // and up directly, so no `down` forward is needed.
        let grab = MoveSurfaceGrab::new_touch(start, window.clone(), initial, output, 1);
        self.seat.get_touch().unwrap().set_grab(self, grab, serial);
    }

    pub fn on_touch_motion<I: InputBackend>(&mut self, event: I::TouchMotionEvent) {
        if !self.config.touch.enable {
            return;
        }
        let Some(output) = self.touch_output() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        self.cursor.hidden_by_touch = true;
        let screen_pos = event.position_transformed(output_geo.size);
        let (camera, zoom) = {
            let os = output_state(&output);
            (os.camera, os.zoom)
        };
        let canvas_pos = screen_to_canvas(ScreenPos(screen_pos), camera, zoom).0;
        let slot = event.slot();
        let time = Event::time_msec(&event);

        if !matches!(self.session_lock, SessionLock::Unlocked) {
            let touch = self.seat.get_touch().unwrap();
            touch.motion(
                self,
                None,
                &MotionEvent {
                    slot,
                    location: screen_pos,
                    time,
                },
            );
            touch.frame(self);
            return;
        }

        // A close-button press just tracks its finger so the up event knows
        // whether it's still inside.
        if let Some(pc) = self.touch_state.pending_close.as_mut()
            && pc.slot == slot
        {
            pc.last_canvas = canvas_pos;
            return;
        }

        let touch = self.seat.get_touch().unwrap();
        if touch.is_grabbed() {
            touch.motion(
                self,
                None,
                &MotionEvent {
                    slot,
                    location: canvas_pos,
                    time,
                },
            );
        }
    }

    pub fn on_touch_up<I: InputBackend>(&mut self, event: I::TouchUpEvent) {
        if !self.config.touch.enable {
            return;
        }
        let slot = event.slot();
        let time = Event::time_msec(&event);
        let serial = SERIAL_COUNTER.next_serial();

        if !matches!(self.session_lock, SessionLock::Unlocked) {
            let touch = self.seat.get_touch().unwrap();
            touch.up(self, &UpEvent { slot, serial, time });
            touch.frame(self);
            return;
        }

        if let Some(pc) = self.touch_state.pending_close.take() {
            if pc.slot == slot {
                let still_inside = matches!(
                    self.decoration_under(pc.last_canvas),
                    Some((ref w, DecorationHit::CloseButton)) if *w == pc.window
                );
                if still_inside {
                    pc.window.send_close();
                }
                return;
            }
            // Different slot — leave the pending close in place.
            self.touch_state.pending_close = Some(pc);
        }

        let touch = self.seat.get_touch().unwrap();
        if touch.is_grabbed() {
            touch.up(self, &UpEvent { slot, serial, time });
        }
    }

    pub fn on_touch_cancel<I: InputBackend>(&mut self, _event: I::TouchCancelEvent) {
        if let Some(touch) = self.seat.get_touch() {
            touch.cancel(self);
        }
        self.touch_state.pending_close = None;
    }

    pub fn on_touch_frame<I: InputBackend>(&mut self, _event: I::TouchFrameEvent) {
        if let Some(touch) = self.seat.get_touch() {
            touch.frame(self);
        }
    }
}
