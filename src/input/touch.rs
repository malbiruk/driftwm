use std::collections::HashMap;
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Event, InputBackend, TouchEvent, TouchSlot,
    },
    input::touch::{DownEvent, MotionEvent, UpEvent},
    utils::{Logical, Point, SERIAL_COUNTER},
};
use crate::state::{DriftWm, FocusTarget};
use driftwm::canvas::{self, ScreenPos, screen_to_canvas};
use driftwm::config::Action;
use crate::input::gestures::direction_from_vector;

#[derive(Debug, Clone)]
pub struct ActiveTouchPoint {
    pub slot: TouchSlot,
    pub start_screen_pos: Point<f64, Logical>,
    pub last_screen_pos: Point<f64, Logical>,
    pub start_canvas_pos: Point<f64, Logical>,
    pub last_canvas_pos: Point<f64, Logical>,
    pub focus: Option<(FocusTarget, Point<f64, Logical>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchGestureMode {
    None,
    CanvasPan,
    CanvasPinch,
    CanvasSwipe,
}

#[derive(Debug, Clone)]
pub struct TouchState {
    pub active_touches: HashMap<TouchSlot, ActiveTouchPoint>,
    pub gesture_active: TouchGestureMode,
    pub initial_zoom: f64,
    pub swipe_start_pos: Point<f64, Logical>,
    pub swipe_triggered: bool,
}

impl TouchState {
    pub fn new() -> Self {
        Self {
            active_touches: HashMap::new(),
            gesture_active: TouchGestureMode::None,
            initial_zoom: 1.0,
            swipe_start_pos: Point::from((0.0, 0.0)),
            swipe_triggered: false,
        }
    }
}

fn distance(p1: Point<f64, Logical>, p2: Point<f64, Logical>) -> f64 {
    let dx = p1.x - p2.x;
    let dy = p1.y - p2.y;
    (dx * dx + dy * dy).sqrt()
}

impl DriftWm {
    pub fn on_touch_down<I: InputBackend>(&mut self, event: I::TouchDownEvent) {
        if !self.config.touch.enable {
            return;
        }
        let output = match self.active_output() {
            Some(o) => o,
            None => return,
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        let screen_pos = event.position_transformed(output_geo.size);
        let canvas_pos = screen_to_canvas(ScreenPos(screen_pos), self.camera(), self.zoom()).0;
        let slot = event.slot();
        let time = Event::time_msec(&event);
        let serial = SERIAL_COUNTER.next_serial();

        // 1. Locked session handling
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            let Some(ls) = self.lock_surfaces.get(&output) else {
                return;
            };
            let focus = FocusTarget(ls.wl_surface().clone());
            let touch_handle = self.seat.get_touch().unwrap();
            let raw_event = DownEvent {
                slot,
                location: screen_pos,
                serial,
                time,
            };
            touch_handle.down(
                self,
                Some((focus.clone(), Point::from((0.0, 0.0)))),
                &raw_event,
            );
            touch_handle.frame(self);

            // Record active touch point
            self.touch_state.active_touches.insert(
                slot,
                ActiveTouchPoint {
                    slot,
                    start_screen_pos: screen_pos,
                    last_screen_pos: screen_pos,
                    start_canvas_pos: screen_pos,
                    last_canvas_pos: screen_pos,
                    focus: Some((focus, Point::from((0.0, 0.0)))),
                },
            );
            return;
        }

        // 2. Unlocked session handling
        let under = self.pointer_focus_under(screen_pos, canvas_pos);

        if let Some((ref target, ref origin)) = under {
            // Touch landed on a window or layer surface
            if self.config.touch.touch_to_focus {
                if let Some(window) = self.window_for_surface(&target.0) {
                    self.raise_and_focus(&window, serial);
                } else {
                    self.set_window_focus(Some(target.clone()), serial);
                }
            }

            // Forward to Wayland client
            if let Some(touch_handle) = self.seat.get_touch() {
                let raw_event = DownEvent {
                    slot,
                    location: canvas_pos,
                    serial,
                    time,
                };
                touch_handle.down(self, Some((target.clone(), *origin)), &raw_event);
            }
        } else {
            // Touch landed on empty background. Check gestures.
            if self.config.touch.enable_canvas_gestures {
                // Cancel existing animations (stop slide)
                self.cancel_animations();
            }
        }

        // Record touch state
        self.touch_state.active_touches.insert(
            slot,
            ActiveTouchPoint {
                slot,
                start_screen_pos: screen_pos,
                last_screen_pos: screen_pos,
                start_canvas_pos: canvas_pos,
                last_canvas_pos: canvas_pos,
                focus: under.clone(),
            },
        );

        // Update/Transition gesture states based on active count
        if self.config.touch.enable_canvas_gestures {
            let active_count = self.touch_state.active_touches.len();
            if active_count == 3 {
                // Determine starting average of the 3 touches
                let sum_screen = self.touch_state.active_touches.values()
                    .map(|tp| tp.last_screen_pos)
                    .fold(Point::from((0.0, 0.0)), |acc, p| acc + p);
                self.touch_state.swipe_start_pos = Point::from((sum_screen.x / 3.0, sum_screen.y / 3.0));
                self.touch_state.swipe_triggered = false;
                self.touch_state.gesture_active = TouchGestureMode::CanvasSwipe;

                // Cancel client touches
                let mut needs_cancel = false;
                for tp in self.touch_state.active_touches.values() {
                    if tp.focus.is_some() {
                        needs_cancel = true;
                        break;
                    }
                }
                if needs_cancel {
                    if let Some(touch_handle) = self.seat.get_touch() {
                        touch_handle.cancel(self);
                    }
                    for tp in self.touch_state.active_touches.values_mut() {
                        tp.focus = None;
                    }
                }
            } else if active_count == 2 {
                self.touch_state.gesture_active = TouchGestureMode::CanvasPinch;
                self.touch_state.initial_zoom = self.zoom();

                // Cancel client touches
                let mut needs_cancel = false;
                for tp in self.touch_state.active_touches.values() {
                    if tp.focus.is_some() {
                        needs_cancel = true;
                        break;
                    }
                }
                if needs_cancel {
                    if let Some(touch_handle) = self.seat.get_touch() {
                        touch_handle.cancel(self);
                    }
                    for tp in self.touch_state.active_touches.values_mut() {
                        tp.focus = None;
                    }
                }
            } else if active_count == 1 && under.is_none() {
                self.touch_state.gesture_active = TouchGestureMode::CanvasPan;
            }
        }
    }

    pub fn on_touch_motion<I: InputBackend>(&mut self, event: I::TouchMotionEvent) {
        if !self.config.touch.enable {
            return;
        }
        let output = match self.active_output() {
            Some(o) => o,
            None => return,
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        let screen_pos = event.position_transformed(output_geo.size);
        let canvas_pos = screen_to_canvas(ScreenPos(screen_pos), self.camera(), self.zoom()).0;
        let slot = event.slot();
        let time = Event::time_msec(&event);

        // Retrieve recorded touch point
        let Some(touch_point) = self.touch_state.active_touches.get(&slot).cloned() else {
            return;
        };

        // 1. Locked session handling
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            if let Some(ref focus) = touch_point.focus {
                let touch_handle = self.seat.get_touch().unwrap();
                let raw_event = MotionEvent {
                    slot,
                    location: screen_pos,
                    time,
                };
                touch_handle.motion(self, Some((focus.0.clone(), focus.1)), &raw_event);
                touch_handle.frame(self);
            }

            // Update touch point coordinates
            if let Some(tp) = self.touch_state.active_touches.get_mut(&slot) {
                tp.last_screen_pos = screen_pos;
                tp.last_canvas_pos = screen_pos;
            }
            return;
        }

        // 2. Unlocked session handling
        match self.touch_state.gesture_active {
            TouchGestureMode::CanvasPan => {
                let delta = screen_pos - touch_point.last_screen_pos;
                let pan_speed = self.config.touch.pan_speed;
                let zoom = self.zoom();
                let mut camera = self.camera();
                camera.x -= (delta.x * pan_speed) / zoom;
                camera.y -= (delta.y * pan_speed) / zoom;
                self.set_camera(camera);
                self.mark_all_dirty();
            }
            TouchGestureMode::CanvasPinch => {
                let slots: Vec<TouchSlot> = self.touch_state.active_touches.keys().copied().collect();
                if slots.len() >= 2 {
                    let s1 = slots[0];
                    let s2 = slots[1];
                    let p1_start = self.touch_state.active_touches[&s1].start_screen_pos;
                    let p2_start = self.touch_state.active_touches[&s2].start_screen_pos;
                    let p1_curr = if s1 == slot { screen_pos } else { self.touch_state.active_touches[&s1].last_screen_pos };
                    let p2_curr = if s2 == slot { screen_pos } else { self.touch_state.active_touches[&s2].last_screen_pos };

                    let start_dist = distance(p1_start, p2_start);
                    let curr_dist = distance(p1_curr, p2_curr);

                    if start_dist > 0.0 {
                        let scale = curr_dist / start_dist;
                        let initial_zoom = self.touch_state.initial_zoom;
                        let zoom_speed = self.config.touch.zoom_speed;
                        let min_zoom = self.min_zoom();
                        
                        let mut new_zoom = initial_zoom * (1.0 + (scale - 1.0) * zoom_speed);
                        new_zoom = new_zoom.clamp(min_zoom, canvas::MAX_ZOOM);

                        let sum_screen = p1_curr + p2_curr;
                        let midpoint_screen = Point::from((sum_screen.x / 2.0, sum_screen.y / 2.0));

                        let sum_canvas = self.touch_state.active_touches[&s1].start_canvas_pos + self.touch_state.active_touches[&s2].start_canvas_pos;
                        let midpoint_canvas_start = Point::from((sum_canvas.x / 2.0, sum_canvas.y / 2.0));

                        let new_camera = canvas::zoom_anchor_camera(midpoint_canvas_start, midpoint_screen, new_zoom);
                        self.set_zoom(new_zoom);
                        self.set_camera(new_camera);
                        self.mark_all_dirty();
                    }
                }
            }
            TouchGestureMode::CanvasSwipe => {
                if !self.touch_state.swipe_triggered {
                    let sum_screen = self.touch_state.active_touches.values()
                        .map(|tp| if tp.slot == slot { screen_pos } else { tp.last_screen_pos })
                        .fold(Point::from((0.0, 0.0)), |acc, p| acc + p);
                    let count = self.touch_state.active_touches.len() as f64;
                    if count > 0.0 {
                        let current_avg = Point::from((sum_screen.x / count, sum_screen.y / count));
                        let dist = distance(current_avg, self.touch_state.swipe_start_pos);
                        if dist >= self.config.touch.swipe_threshold {
                            let dir = direction_from_vector(current_avg - self.touch_state.swipe_start_pos);
                            self.execute_action(&Action::CenterNearest(dir));
                            self.touch_state.swipe_triggered = true;
                        }
                    }
                }
            }
            TouchGestureMode::None => {
                if let Some(ref focus) = touch_point.focus {
                    if let Some(touch_handle) = self.seat.get_touch() {
                        let raw_event = MotionEvent {
                            slot,
                            location: canvas_pos,
                            time,
                        };
                        touch_handle.motion(self, Some((focus.0.clone(), focus.1)), &raw_event);
                    }
                }
            }
        }

        // Update recorded touch point coordinates
        if let Some(tp) = self.touch_state.active_touches.get_mut(&slot) {
            tp.last_screen_pos = screen_pos;
            tp.last_canvas_pos = canvas_pos;
        }
    }

    pub fn on_touch_up<I: InputBackend>(&mut self, event: I::TouchUpEvent) {
        if !self.config.touch.enable {
            return;
        }
        let slot = event.slot();
        let time = Event::time_msec(&event);
        let serial = SERIAL_COUNTER.next_serial();

        let Some(touch_point) = self.touch_state.active_touches.remove(&slot) else {
            return;
        };

        // 1. Locked session handling
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            let touch_handle = self.seat.get_touch().unwrap();
            let raw_event = UpEvent {
                slot,
                serial,
                time,
            };
            touch_handle.up(self, &raw_event);
            touch_handle.frame(self);
            return;
        }

        // 2. Unlocked session handling
        if self.touch_state.gesture_active != TouchGestureMode::None {
            if self.touch_state.active_touches.is_empty() {
                self.touch_state.gesture_active = TouchGestureMode::None;
            }
        } else if let Some(ref _focus) = touch_point.focus {
            if let Some(touch_handle) = self.seat.get_touch() {
                let raw_event = UpEvent {
                    slot,
                    serial,
                    time,
                };
                touch_handle.up(self, &raw_event);
            }
        }
    }

    pub fn on_touch_cancel<I: InputBackend>(&mut self, _event: I::TouchCancelEvent) {
        if let Some(touch_handle) = self.seat.get_touch() {
            touch_handle.cancel(self);
        }
        self.touch_state.active_touches.clear();
        self.touch_state.gesture_active = TouchGestureMode::None;
    }

    pub fn on_touch_frame<I: InputBackend>(&mut self, _event: I::TouchFrameEvent) {
        if let Some(touch_handle) = self.seat.get_touch() {
            touch_handle.frame(self);
        }
    }
}
