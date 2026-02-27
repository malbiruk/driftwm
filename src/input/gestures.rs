use std::cell::RefCell;

use smithay::{
    backend::input::{
        Event, GestureBeginEvent, GestureEndEvent,
        GesturePinchUpdateEvent as I_GesturePinchUpdateEvent,
        GestureSwipeUpdateEvent as I_GestureSwipeUpdateEvent, InputBackend,
    },
    desktop::Window,
    input::pointer::{
        CursorImageStatus, GestureHoldBeginEvent as WlHoldBegin,
        GestureHoldEndEvent as WlHoldEnd, GesturePinchBeginEvent as WlPinchBegin,
        GesturePinchEndEvent as WlPinchEnd, GesturePinchUpdateEvent as WlPinchUpdate,
        GestureSwipeBeginEvent as WlSwipeBegin, GestureSwipeEndEvent as WlSwipeEnd,
        GestureSwipeUpdateEvent as WlSwipeUpdate,
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Size, SERIAL_COUNTER},
    wayland::compositor::with_states,
};

use crate::grabs::{MoveSurfaceGrab, ResizeState, has_bottom, has_left, has_right, has_top};
use crate::state::{DriftWm, FocusTarget};
use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::config::{Action, Direction};
use super::pointer::{edges_from_position, resize_cursor};

/// Active gesture — decided at Begin, locked for the gesture's duration.
pub enum GestureState {
    /// 3-finger swipe → pan viewport (with momentum via drift_pan).
    Swipe3Pan,
    /// 3-finger double-tap+drag → move window
    Swipe3Move {
        window: Window,
        initial_location: Point<f64, Logical>,
        initial_screen_pos: Point<f64, Logical>,
        cumulative_screen: Point<f64, Logical>,
    },
    /// Mod+3-finger drag → resize window
    Swipe3Resize {
        window: Window,
        edges: xdg_toplevel::ResizeEdge,
        initial_location: Point<i32, Logical>,
        initial_size: Size<i32, Logical>,
        last_size: Size<i32, Logical>,
        cumulative: Point<f64, Logical>,
    },
    /// 4-finger swipe → navigate to nearest window after threshold
    Swipe4Navigate {
        cumulative: Point<f64, Logical>,
        fired: bool,
    },
    /// 2-finger pinch on empty canvas → cursor-anchored zoom
    Pinch2Desktop { initial_zoom: f64 },
    /// 2-finger pinch on window → forward to client app
    Pinch2Forward,
    /// 3-finger pinch → cursor-anchored zoom (ignores windows)
    Pinch3Zoom { initial_zoom: f64 },
    /// 4-finger pinch → pinch-in: HomeToggle, pinch-out: ZoomToFit
    Pinch4Nav { fired: bool },
}

const SWIPE4_THRESHOLD_SQ: f64 = 16.0 * 16.0;
const PINCH4_SCALE_LO: f64 = 0.8;
const PINCH4_SCALE_HI: f64 = 1.2;

pub(crate) const DOUBLE_TAP_WINDOW_MS: u64 = 300;

impl DriftWm {
    // ── Swipe ──────────────────────────────────────────────────────────

    pub fn on_gesture_swipe_begin<I: InputBackend>(&mut self, event: I::GestureSwipeBeginEvent) {
        let fingers = event.fingers();
        let time = Event::time_msec(&event);

        // During fullscreen: 3+ finger gestures exit fullscreen first
        if self.fullscreen.is_some() && fingers >= 3 {
            let pointer = self.seat.get_pointer().unwrap();
            let pos = pointer.current_location();
            self.exit_fullscreen_remap_pointer(pos);
        }

        let state = match fingers {
            3 => {
                self.cancel_animations();

                let keyboard = self.seat.get_keyboard().unwrap();
                let mods = keyboard.modifier_state();
                let mod_held = self.config.mod_key.is_pressed(&mods);
                let pointer = self.seat.get_pointer().unwrap();
                let pos = pointer.current_location();

                // Priority 1: Mod held + over window → resize
                if mod_held
                    && let Some((window, _)) =
                        self.space.element_under(pos).map(|(w, l)| (w.clone(), l))
                {
                    return self.start_gesture_resize(window, pos);
                }

                // Priority 2: Recent middle-click (3-finger tap) + over window → move
                if let Some(pending) = self.pending_middle_click.take() {
                    self.loop_handle.remove(pending.timer_token);
                    if let Some((window, _)) =
                        self.space.element_under(pos).map(|(w, l)| (w.clone(), l))
                    {
                        return self.start_gesture_move(window, pos);
                    }
                    // Not over a window — flush the click (paste) and fall through to pan
                    self.flush_middle_click(pending.press_time, pending.release_time);
                }

                // Priority 3: Default → pan
                GestureState::Swipe3Pan
            }
            4 => {
                self.cancel_animations();
                GestureState::Swipe4Navigate {
                    cumulative: Point::from((0.0, 0.0)),
                    fired: false,
                }
            }
            _ => {
                self.forward_swipe_begin(fingers, time);
                return;
            }
        };
        self.gesture_state = Some(state);
    }

    pub fn on_gesture_swipe_update<I: InputBackend>(&mut self, event: I::GestureSwipeUpdateEvent) {
        let delta = event.delta();
        let time = Event::time_msec(&event);

        let Some(ref mut state) = self.gesture_state else {
            self.forward_swipe_update(delta, time);
            return;
        };

        match state {
            GestureState::Swipe3Pan => {
                // Negate: swipe right → camera moves left → content follows fingers
                let canvas_delta: Point<f64, Logical> =
                    (-delta.x / self.zoom, -delta.y / self.zoom).into();
                self.drift_pan(canvas_delta);

                let pointer = self.seat.get_pointer().unwrap();
                let pos = pointer.current_location();
                self.warp_pointer(pos + canvas_delta);
            }
            GestureState::Swipe3Move {
                window,
                initial_location,
                initial_screen_pos,
                cumulative_screen,
            } => {
                *cumulative_screen += Point::from((delta.x, delta.y));
                let new_loc = Point::from((
                    (initial_location.x + cumulative_screen.x / self.zoom) as i32,
                    (initial_location.y + cumulative_screen.y / self.zoom) as i32,
                ));
                self.space.map_element(window.clone(), new_loc, false);

                // Edge pan detection using virtual screen position
                let virtual_screen = *initial_screen_pos + *cumulative_screen;
                let output_size = self
                    .space
                    .outputs()
                    .next()
                    .and_then(|o| o.current_mode())
                    .map(|m| m.size.to_logical(1));
                if let Some(size) = output_size {
                    let cfg = &self.config;
                    self.edge_pan_velocity = MoveSurfaceGrab::edge_pan_velocity(
                        virtual_screen,
                        size.w as f64,
                        size.h as f64,
                        cfg.edge_zone,
                        cfg.edge_pan_min,
                        cfg.edge_pan_max,
                    );
                }
            }
            GestureState::Swipe3Resize {
                window,
                edges,
                initial_size,
                last_size,
                cumulative,
                ..
            } => {
                *cumulative += Point::from((delta.x / self.zoom, delta.y / self.zoom));

                let mut new_w = initial_size.w;
                let mut new_h = initial_size.h;
                if has_left(*edges) {
                    new_w -= cumulative.x as i32;
                } else if has_right(*edges) {
                    new_w += cumulative.x as i32;
                }
                if has_top(*edges) {
                    new_h -= cumulative.y as i32;
                } else if has_bottom(*edges) {
                    new_h += cumulative.y as i32;
                }
                new_w = new_w.max(1);
                new_h = new_h.max(1);

                let new_size = Size::from((new_w, new_h));
                if new_size != *last_size {
                    *last_size = new_size;
                    let toplevel = window.toplevel().unwrap();
                    toplevel.with_pending_state(|state| {
                        state.size = Some(new_size);
                        state.states.set(xdg_toplevel::State::Resizing);
                    });
                    toplevel.send_pending_configure();
                }
            }
            GestureState::Swipe4Navigate { cumulative, fired } => {
                if *fired {
                    return;
                }
                // Negate: swipe left → navigate left (delta points left,
                // but we want the direction the fingers moved toward)
                *cumulative += Point::from((-delta.x, -delta.y));
                let mag_sq = cumulative.x.powi(2) + cumulative.y.powi(2);
                if mag_sq >= SWIPE4_THRESHOLD_SQ {
                    *fired = true;
                    let dir = direction_from_vector(*cumulative);
                    self.execute_action(&Action::CenterNearest(dir));
                }
            }
            _ => {
                self.forward_swipe_update(delta, time);
            }
        }
    }

    pub fn on_gesture_swipe_end<I: InputBackend>(&mut self, event: I::GestureSwipeEndEvent) {
        let cancelled = event.cancelled();
        let time = Event::time_msec(&event);

        let Some(state) = self.gesture_state.take() else {
            self.forward_swipe_end(cancelled, time);
            return;
        };

        match state {
            GestureState::Swipe3Pan => {
                // Momentum from drift_pan() carries the camera
            }
            GestureState::Swipe3Move { .. } => {
                self.edge_pan_velocity = None;
            }
            GestureState::Swipe3Resize {
                window,
                edges,
                initial_location,
                initial_size,
                ..
            } => {
                let toplevel = window.toplevel().unwrap();
                toplevel.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Resizing);
                });
                toplevel.send_pending_configure();

                let surface = toplevel.wl_surface().clone();
                with_states(&surface, |states| {
                    states
                        .data_map
                        .get_or_insert(|| RefCell::new(ResizeState::Idle))
                        .replace(ResizeState::WaitingForLastCommit {
                            edges,
                            initial_window_location: initial_location,
                            initial_window_size: initial_size,
                        });
                });

                self.grab_cursor = false;
                self.cursor_status = CursorImageStatus::default_named();
            }
            _ => {}
        }
    }

    // ── Pinch ──────────────────────────────────────────────────────────

    pub fn on_gesture_pinch_begin<I: InputBackend>(&mut self, event: I::GesturePinchBeginEvent) {
        let fingers = event.fingers();
        let time = Event::time_msec(&event);

        // During fullscreen: 3+ finger pinch exits fullscreen first;
        // 2-finger pinch and hold forward to the fullscreen app.
        if self.fullscreen.is_some() {
            if fingers >= 3 {
                let pointer = self.seat.get_pointer().unwrap();
                let pos = pointer.current_location();
                self.exit_fullscreen_remap_pointer(pos);
            } else {
                self.forward_pinch_begin(fingers, time);
                return;
            }
        }

        let state = match fingers {
            2 => {
                // Mod held → zoom anywhere (same as 3-finger pinch)
                let keyboard = self.seat.get_keyboard().unwrap();
                let mods = keyboard.modifier_state();
                let mod_held = self.config.mod_key.is_pressed(&mods);

                let pointer = self.seat.get_pointer().unwrap();
                let pos = pointer.current_location();
                if mod_held
                    || self.pointer_over_layer
                    || self.space.element_under(pos).is_none()
                {
                    self.cancel_animations();
                    GestureState::Pinch2Desktop {
                        initial_zoom: self.zoom,
                    }
                } else {
                    self.forward_pinch_begin(fingers, time);
                    GestureState::Pinch2Forward
                }
            }
            3 => {
                self.cancel_animations();
                GestureState::Pinch3Zoom {
                    initial_zoom: self.zoom,
                }
            }
            4 => {
                self.cancel_animations();
                GestureState::Pinch4Nav { fired: false }
            }
            _ => {
                self.forward_pinch_begin(fingers, time);
                return;
            }
        };
        self.gesture_state = Some(state);
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
            GestureState::Pinch2Desktop { initial_zoom }
            | GestureState::Pinch3Zoom { initial_zoom } => {
                let new_zoom = (*initial_zoom * scale).clamp(self.min_zoom(), canvas::MAX_ZOOM);

                if new_zoom != self.zoom {
                    let pointer = self.seat.get_pointer().unwrap();
                    let pos = pointer.current_location();
                    let screen_pos = canvas_to_screen(CanvasPos(pos), self.camera, self.zoom).0;

                    self.overview_return = None;
                    self.camera = canvas::zoom_anchor_camera(pos, screen_pos, new_zoom);
                    self.zoom = new_zoom;
                    self.update_output_from_camera();

                    self.warp_pointer(pos);
                }
            }
            GestureState::Pinch2Forward => {
                self.forward_pinch_update(delta, scale, rotation, time);
            }
            GestureState::Pinch4Nav { fired } => {
                if !*fired {
                    if scale < PINCH4_SCALE_LO {
                        *fired = true;
                        self.execute_action(&Action::ZoomToFit);
                    } else if scale > PINCH4_SCALE_HI {
                        *fired = true;
                        self.execute_action(&Action::HomeToggle);
                    }
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
            self.forward_pinch_end(cancelled, time);
            return;
        };

        match state {
            GestureState::Pinch2Desktop { .. } | GestureState::Pinch3Zoom { .. } => {
                // Snap zoom to 1.0 if close
                let snapped = canvas::snap_zoom(self.zoom);
                if snapped != self.zoom {
                    let pointer = self.seat.get_pointer().unwrap();
                    let pos = pointer.current_location();
                    let screen_pos = canvas_to_screen(CanvasPos(pos), self.camera, self.zoom).0;
                    self.camera = canvas::zoom_anchor_camera(pos, screen_pos, snapped);
                    self.zoom = snapped;
                    self.update_output_from_camera();
                    self.warp_pointer(pos);
                }
            }
            GestureState::Pinch2Forward => {
                self.forward_pinch_end(cancelled, time);
            }
            _ => {}
        }
    }

    // ── Hold ───────────────────────────────────────────────────────────

    pub fn on_gesture_hold_begin<I: InputBackend>(&mut self, event: I::GestureHoldBeginEvent) {
        let fingers = event.fingers();
        let time = Event::time_msec(&event);
        self.forward_hold_begin(fingers, time);
    }

    pub fn on_gesture_hold_end<I: InputBackend>(&mut self, event: I::GestureHoldEndEvent) {
        let cancelled = event.cancelled();
        let time = Event::time_msec(&event);
        self.forward_hold_end(cancelled, time);
    }

    // ── DeviceAdded ────────────────────────────────────────────────────

    /// Configure a libinput device using trackpad settings from config.
    /// Called from the udev backend where we know the concrete device type.
    pub fn configure_libinput_device(&self, device: &mut smithay::reexports::input::Device) {
        // Only configure touchpads (identified by tap support)
        if device.config_tap_finger_count() == 0 {
            return;
        }

        let cfg = &self.config.trackpad;
        tracing::info!(
            "Configuring trackpad: {} (tap={}, natural_scroll={}, accel={})",
            device.name(),
            cfg.tap_to_click,
            cfg.natural_scroll,
            cfg.accel_speed,
        );

        if let Err(e) = device.config_tap_set_enabled(cfg.tap_to_click) {
            tracing::warn!("Failed to set tap_to_click: {e:?}");
        }
        if let Err(e) = device.config_tap_set_drag_enabled(cfg.tap_and_drag) {
            tracing::warn!("Failed to set tap_and_drag: {e:?}");
        }
        if let Err(e) = device.config_scroll_set_natural_scroll_enabled(cfg.natural_scroll) {
            tracing::warn!("Failed to set natural_scroll: {e:?}");
        }
        // LRM: 1-finger=left, 2-finger=right, 3-finger=middle.
        // Hardcoded — the compositor uses BTN_MIDDLE from 3-finger tap
        // for double-tap+drag window move detection.
        if let Err(e) = device.config_tap_set_button_map(
            smithay::reexports::input::TapButtonMap::LeftRightMiddle,
        ) {
            tracing::warn!("Failed to set button_map: {e:?}");
        }
        if let Err(e) = device.config_accel_set_speed(cfg.accel_speed) {
            tracing::warn!("Failed to set accel_speed: {e:?}");
        }
    }

    // ── Gesture setup helpers ─────────────────────────────────────────

    /// Enter Swipe3Move state: focus + raise the window, set initial tracking state.
    fn start_gesture_move(&mut self, window: Window, pos: Point<f64, Logical>) {
        self.space.raise_element(&window, true);
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().unwrap();
        let surface = window.toplevel().unwrap().wl_surface().clone();
        keyboard.set_focus(self, Some(FocusTarget(surface)), serial);

        let initial_location = self.space.element_location(&window).unwrap_or_default();
        let screen_pos = canvas_to_screen(CanvasPos(pos), self.camera, self.zoom).0;

        self.gesture_state = Some(GestureState::Swipe3Move {
            window,
            initial_location: Point::from((initial_location.x as f64, initial_location.y as f64)),
            initial_screen_pos: screen_pos,
            cumulative_screen: Point::from((0.0, 0.0)),
        });
    }

    /// Enter Swipe3Resize state: store initial geometry, set resize state + cursor.
    fn start_gesture_resize(&mut self, window: Window, pos: Point<f64, Logical>) {
        let initial_location = self.space.element_location(&window).unwrap();
        let initial_size = window.geometry().size;
        let edges = edges_from_position(pos, initial_location, initial_size);

        // Store resize state on surface data map for commit() repositioning
        let wl_surface = window.toplevel().unwrap().wl_surface().clone();
        with_states(&wl_surface, |states| {
            states
                .data_map
                .get_or_insert(|| RefCell::new(ResizeState::Idle))
                .replace(ResizeState::Resizing {
                    edges,
                    initial_window_location: initial_location,
                    initial_window_size: initial_size,
                });
        });

        window.toplevel().unwrap().with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
        });

        self.grab_cursor = true;
        self.cursor_status = CursorImageStatus::Named(resize_cursor(edges));

        self.gesture_state = Some(GestureState::Swipe3Resize {
            window,
            edges,
            initial_location,
            initial_size,
            last_size: initial_size,
            cumulative: Point::from((0.0, 0.0)),
        });
    }

    fn cancel_animations(&mut self) {
        self.camera_target = None;
        self.zoom_target = None;
        self.momentum.stop();
    }

    // ── Client forwarding ──────────────────────────────────────────────

    fn forward_swipe_begin(&mut self, fingers: u32, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_swipe_begin(
            self,
            &WlSwipeBegin {
                serial,
                time,
                fingers,
            },
        );
        pointer.frame(self);
    }

    fn forward_swipe_update(&mut self, delta: Point<f64, Logical>, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        pointer.gesture_swipe_update(self, &WlSwipeUpdate { time, delta });
        pointer.frame(self);
    }

    fn forward_swipe_end(&mut self, cancelled: bool, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_swipe_end(
            self,
            &WlSwipeEnd {
                serial,
                time,
                cancelled,
            },
        );
        pointer.frame(self);
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
        pointer.frame(self);
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
        pointer.frame(self);
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
        pointer.frame(self);
    }

    fn forward_hold_begin(&mut self, fingers: u32, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_hold_begin(
            self,
            &WlHoldBegin {
                serial,
                time,
                fingers,
            },
        );
        pointer.frame(self);
    }

    fn forward_hold_end(&mut self, cancelled: bool, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_hold_end(
            self,
            &WlHoldEnd {
                serial,
                time,
                cancelled,
            },
        );
        pointer.frame(self);
    }
}

/// Map a 2D vector to the nearest of 8 directions (4 cardinal + 4 diagonal).
/// Uses 45° octants: tan(22.5°) ≈ 0.4142 as the minor/major axis ratio threshold.
pub(crate) fn direction_from_vector(v: Point<f64, Logical>) -> Direction {
    let ax = v.x.abs();
    let ay = v.y.abs();
    let minor = ax.min(ay);
    let major = ax.max(ay);

    // If the minor axis is > 41.4% of the major axis, the vector is diagonal
    if major > 0.0 && minor > major * 0.4142 {
        match (v.x > 0.0, v.y > 0.0) {
            (true, true) => Direction::DownRight,
            (true, false) => Direction::UpRight,
            (false, true) => Direction::DownLeft,
            (false, false) => Direction::UpLeft,
        }
    } else if ax > ay {
        if v.x > 0.0 { Direction::Right } else { Direction::Left }
    } else if v.y > 0.0 {
        Direction::Down
    } else {
        Direction::Up
    }
}
