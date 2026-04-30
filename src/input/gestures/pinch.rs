//! Pinch gesture handlers — continuous zoom, threshold pinch-in/out, and
//! client forwarding when unbound.

use smithay::{
    backend::input::{
        Event, GestureBeginEvent, GestureEndEvent, GesturePinchUpdateEvent, InputBackend,
    },
    input::pointer::{
        GesturePinchBeginEvent as WlPinchBegin, GesturePinchEndEvent as WlPinchEnd,
        GesturePinchUpdateEvent as WlPinchUpdate,
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};

use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::config::{ContinuousAction, GestureConfigEntry, GestureTrigger, ThresholdAction};

use crate::state::DriftWm;

use super::GestureState;

impl DriftWm {
    pub fn on_gesture_pinch_begin<I: InputBackend>(&mut self, event: I::GesturePinchBeginEvent) {
        let fingers = event.fingers();
        let time = Event::time_msec(&event);

        // During fullscreen: 3+ finger pinch exits fullscreen first;
        // 2-finger pinch and hold forward to the fullscreen app.
        if self.is_fullscreen() {
            if fingers >= 3 {
                self.exit_fullscreen_for_gesture();
            } else {
                self.forward_pinch_begin(fingers, time);
                return;
            }
        }

        let keyboard = self.seat.get_keyboard().unwrap();
        let mods = keyboard.modifier_state();
        let pointer = self.seat.get_pointer().unwrap();
        let pos = pointer.current_location();
        let context = self.pointer_context(pos);

        // Check continuous Pinch trigger first
        let pinch_trigger = GestureTrigger::Pinch { fingers };
        if let Some(entry) = self.config.gesture_lookup(&mods, &pinch_trigger, context)
            && matches!(
                entry,
                GestureConfigEntry::Continuous(ContinuousAction::Zoom)
            )
        {
            self.cancel_animations();
            self.gesture_output = self.active_output();
            self.gesture_state = Some(GestureState::PinchZoom {
                initial_zoom: self.zoom(),
            });
            return;
        }

        // Check threshold PinchIn/PinchOut triggers
        let pin_in =
            self.config
                .gesture_lookup(&mods, &GestureTrigger::PinchIn { fingers }, context);
        let pin_out =
            self.config
                .gesture_lookup(&mods, &GestureTrigger::PinchOut { fingers }, context);

        let action_in = pin_in.and_then(|e| match e {
            GestureConfigEntry::Threshold(ThresholdAction::Fixed(a)) => Some(a.clone()),
            _ => None,
        });
        let action_out = pin_out.and_then(|e| match e {
            GestureConfigEntry::Threshold(ThresholdAction::Fixed(a)) => Some(a.clone()),
            _ => None,
        });

        if action_in.is_some() || action_out.is_some() {
            self.cancel_animations();
            self.gesture_state = Some(GestureState::PinchThreshold {
                fired_in: false,
                fired_out: false,
                action_in,
                action_out,
            });
            return;
        }

        // No binding — forward to client
        self.forward_pinch_begin(fingers, time);
        self.gesture_state = Some(GestureState::PinchForward);
    }

    pub fn on_gesture_pinch_update<I: InputBackend>(&mut self, event: I::GesturePinchUpdateEvent) {
        let scale = event.scale();
        let delta = event.delta();
        let rotation = event.rotation();
        let time = Event::time_msec(&event);

        let Some(ref mut state) = self.gesture_state else {
            self.forward_pinch_update(delta, scale, rotation, time);
            return;
        };

        match state {
            GestureState::PinchZoom { initial_zoom } => {
                let new_zoom = (*initial_zoom * scale).clamp(self.min_zoom(), canvas::MAX_ZOOM);

                let (cur_zoom, cur_camera) = self.gesture_camera_zoom();
                if new_zoom != cur_zoom {
                    let pointer = self.seat.get_pointer().unwrap();
                    let pos = pointer.current_location();
                    let screen_pos = canvas_to_screen(CanvasPos(pos), cur_camera, cur_zoom).0;

                    if let Some(ref output) = self.gesture_output {
                        let mut os = crate::state::output_state(output);
                        os.overview_return = None;
                        os.camera = canvas::zoom_anchor_camera(pos, screen_pos, new_zoom);
                        os.zoom = new_zoom;
                        drop(os);
                    } else {
                        self.set_overview_return(None);
                        self.set_camera(canvas::zoom_anchor_camera(pos, screen_pos, new_zoom));
                        self.set_zoom(new_zoom);
                    }
                    self.update_output_from_camera();

                    self.warp_pointer(pos);
                }
            }
            GestureState::PinchForward => {
                self.forward_pinch_update(delta, scale, rotation, time);
            }
            GestureState::PinchThreshold {
                fired_in,
                fired_out,
                action_in,
                action_out,
            } => {
                let to_exec = if !*fired_in && scale < self.config.gesture_thresholds.pinch_in_scale
                {
                    *fired_in = true;
                    action_in.clone()
                } else if !*fired_out && scale > self.config.gesture_thresholds.pinch_out_scale {
                    *fired_out = true;
                    action_out.clone()
                } else {
                    None
                };
                if let Some(action) = to_exec {
                    self.execute_action(&action);
                }
            }
            _ => {
                self.forward_pinch_update(delta, scale, rotation, time);
            }
        }
    }

    pub fn on_gesture_pinch_end<I: InputBackend>(&mut self, event: I::GesturePinchEndEvent) {
        let cancelled = event.cancelled();
        let time = Event::time_msec(&event);

        let Some(state) = self.gesture_state.take() else {
            self.gesture_output = None;
            self.forward_pinch_end(cancelled, time);
            return;
        };

        match state {
            GestureState::PinchZoom { .. } => {
                let (cur_zoom, cur_camera) = self.gesture_camera_zoom();
                let snapped = canvas::snap_zoom(cur_zoom);
                if snapped != cur_zoom {
                    let pointer = self.seat.get_pointer().unwrap();
                    let pos = pointer.current_location();
                    let screen_pos = canvas_to_screen(CanvasPos(pos), cur_camera, cur_zoom).0;
                    if let Some(ref output) = self.gesture_output {
                        let mut os = crate::state::output_state(output);
                        os.camera = canvas::zoom_anchor_camera(pos, screen_pos, snapped);
                        os.zoom = snapped;
                        drop(os);
                    } else {
                        self.set_camera(canvas::zoom_anchor_camera(pos, screen_pos, snapped));
                        self.set_zoom(snapped);
                    }
                    self.update_output_from_camera();
                    self.warp_pointer(pos);
                }
            }
            GestureState::PinchForward => {
                self.forward_pinch_end(cancelled, time);
            }
            GestureState::PinchThreshold {
                fired_in: false,
                fired_out: false,
                ..
            } if !cancelled => {
                // Pinch that didn't reach threshold — no action
            }
            _ => {}
        }
        self.gesture_output = None;
    }

    fn forward_pinch_begin(&mut self, fingers: u32, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_pinch_begin(
            self,
            &WlPinchBegin {
                serial,
                time,
                fingers,
            },
        );
    }

    fn forward_pinch_update(
        &mut self,
        delta: Point<f64, Logical>,
        scale: f64,
        rotation: f64,
        time: u32,
    ) {
        let pointer = self.seat.get_pointer().unwrap();
        pointer.gesture_pinch_update(
            self,
            &WlPinchUpdate {
                time,
                delta,
                scale,
                rotation,
            },
        );
    }

    fn forward_pinch_end(&mut self, cancelled: bool, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_pinch_end(
            self,
            &WlPinchEnd {
                serial,
                time,
                cancelled,
            },
        );
    }
}
