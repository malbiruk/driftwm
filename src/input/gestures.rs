use std::cell::RefCell;

use smithay::{
    backend::input::{
        Event, GestureBeginEvent, GestureEndEvent,
        GesturePinchUpdateEvent as I_GesturePinchUpdateEvent,
        GestureSwipeUpdateEvent as I_GestureSwipeUpdateEvent, InputBackend,
    },
    desktop::Window,
    input::pointer::{

        CursorImageStatus, Focus, GestureHoldBeginEvent as WlHoldBegin,
        GestureHoldEndEvent as WlHoldEnd, GesturePinchBeginEvent as WlPinchBegin,
        GesturePinchEndEvent as WlPinchEnd, GesturePinchUpdateEvent as WlPinchUpdate,
        GestureSwipeBeginEvent as WlSwipeBegin, GestureSwipeEndEvent as WlSwipeEnd,
        GestureSwipeUpdateEvent as WlSwipeUpdate, GrabStartData,
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Size, SERIAL_COUNTER},
    wayland::{compositor::with_states, seat::WaylandFocus},
};

use crate::grabs::{MoveSurfaceGrab, ResizeState, has_bottom, has_left, has_right, has_top};
use crate::state::{DriftWm, FocusTarget, output_state, snap_targets_impl};
use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::snap::{SnapState, snap_resize_edges};
use driftwm::config::{
    Action, BindingContext, ContinuousAction, Direction, GestureConfigEntry, GestureTrigger,
    ThresholdAction,
};
use super::pointer::{edges_from_position, resize_cursor};

/// Active gesture — decided at Begin, locked for the gesture's duration.
pub enum GestureState {
    /// Continuous swipe → pan viewport (with momentum via drift_pan).
    SwipePan,
    /// Double-tap+drag → move window via MoveSurfaceGrab on the pointer.
    SwipeMove,
    /// Swipe → resize window (continuous).
    SwipeResize {
        window: Window,
        edges: xdg_toplevel::ResizeEdge,
        initial_location: Point<i32, Logical>,
        initial_size: Size<i32, Logical>,
        last_size: Size<i32, Logical>,
        cumulative: Point<f64, Logical>,
        last_x11_configure: Option<std::time::Instant>,
        snap: SnapState,
    },
    /// Threshold swipe — accumulate delta, detect direction, fire once.
    SwipeThreshold {
        cumulative: Point<f64, Logical>,
        fired: bool,
        /// Per-direction overrides (from SwipeUp/Down/Left/Right config entries).
        up: Option<ThresholdAction>,
        down: Option<ThresholdAction>,
        left: Option<ThresholdAction>,
        right: Option<ThresholdAction>,
        /// 8-direction fallback from the Swipe trigger's threshold action.
        directional: Option<ThresholdAction>,
    },
    /// Continuous pinch → cursor-anchored zoom.
    PinchZoom { initial_zoom: f64 },
    /// Pinch forwarded to client (unbound in this context).
    PinchForward,
    /// Threshold pinch — pinch-in/out fire discrete actions.
    PinchThreshold {
        fired_in: bool,
        fired_out: bool,
        action_in: Option<Action>,
        action_out: Option<Action>,
    },
    /// Hold gesture — fires action on release.
    HoldAction { action: Action },
}


pub(crate) const DOUBLE_TAP_WINDOW_MS: u64 = 300;

fn set_accel_profile(
    device: &mut smithay::reexports::input::Device,
    profile: driftwm::config::AccelProfile,
) {
    use driftwm::config::AccelProfile;
    let libinput_profile = match profile {
        AccelProfile::Flat => smithay::reexports::input::AccelProfile::Flat,
        AccelProfile::Adaptive => smithay::reexports::input::AccelProfile::Adaptive,
    };
    if let Err(e) = device.config_accel_set_profile(libinput_profile) {
        tracing::warn!("Failed to set accel_profile: {e:?}");
    }
}

impl DriftWm {
    // ── Swipe ──────────────────────────────────────────────────────────

    fn exit_fullscreen_for_gesture(&mut self) {
        self.gesture_exited_fullscreen = self.active_fullscreen().map(|fs| fs.window.clone());
        let pointer = self.seat.get_pointer().unwrap();
        let pos = pointer.current_location();
        self.exit_fullscreen_remap_pointer(pos);
    }

    pub fn on_gesture_swipe_begin<I: InputBackend>(&mut self, event: I::GestureSwipeBeginEvent) {
        let fingers = event.fingers();
        let time = Event::time_msec(&event);

        // During fullscreen: 3+ finger gestures exit fullscreen first
        if self.is_fullscreen() && fingers >= 3 {
            self.exit_fullscreen_for_gesture();
        }

        let keyboard = self.seat.get_keyboard().unwrap();
        let mods = keyboard.modifier_state();
        let pointer = self.seat.get_pointer().unwrap();
        let pos = pointer.current_location();
        let context = self.pointer_context(pos);

        // Priority 1: Pending middle-click (3-finger tap) → check DoubletapSwipe
        if let Some(pending) = self.pending_middle_click.take() {
            self.loop_handle.remove(pending.timer_token);
            let dt_trigger = GestureTrigger::DoubletapSwipe { fingers };
            let dt_entry = self.config.gesture_lookup(&mods, &dt_trigger, context).cloned();
            if let Some(entry) = dt_entry {
                self.cancel_animations();
                self.gesture_output = self.active_output();
                match entry {
                    GestureConfigEntry::Continuous(ContinuousAction::MoveWindow) => {
                        if let Some((window, _)) = self.window_under(pos) {
                            return self.start_gesture_move(window, pos);
                        }
                        // Not over a moveable window — flush and fall through
                        self.flush_middle_click(pending.press_time, pending.release_time);
                    }
                    GestureConfigEntry::Continuous(ContinuousAction::ResizeWindow) => {
                        if let Some((window, _)) = self.window_under(pos)
                            .filter(|(w, _)| {
                                !w.wl_surface().as_ref().and_then(|s| driftwm::config::applied_rule(s))
                                    .is_some_and(|r| r.widget)
                            })
                        {
                            return self.start_gesture_resize(window, pos);
                        }
                        self.flush_middle_click(pending.press_time, pending.release_time);
                    }
                    _ => {
                        // Non-window continuous/threshold: flush middle click, fall through to Swipe lookup
                        self.flush_middle_click(pending.press_time, pending.release_time);
                    }
                }
            } else {
                // No DoubletapSwipe binding — flush middle click
                self.flush_middle_click(pending.press_time, pending.release_time);
            }
        }

        // Priority 2: Look up Swipe { fingers } in config
        let swipe_trigger = GestureTrigger::Swipe { fingers };
        let entry = self.config.gesture_lookup(&mods, &swipe_trigger, context).cloned();

        match entry {
            Some(GestureConfigEntry::Continuous(action)) => {
                self.cancel_animations();
                self.gesture_output = self.active_output();
                match action {
                    ContinuousAction::PanViewport => {
                        self.gesture_state = Some(GestureState::SwipePan);
                    }
                    ContinuousAction::MoveWindow => {
                        if let Some((window, _)) = self.window_under(pos) {
                            return self.start_gesture_move(window, pos);
                        }
                        // Not over window — fall back to pan
                        self.gesture_state = Some(GestureState::SwipePan);
                    }
                    ContinuousAction::ResizeWindow => {
                        if let Some((window, _)) = self.window_under(pos)
                            .filter(|(w, _)| {
                                !w.wl_surface().as_ref().and_then(|s| driftwm::config::applied_rule(s))
                                    .is_some_and(|r| r.widget)
                            })
                        {
                            return self.start_gesture_resize(window, pos);
                        }
                        self.gesture_state = Some(GestureState::SwipePan);
                    }
                    ContinuousAction::Zoom => {
                        // Swipe doesn't produce scale — treat as pan
                        self.gesture_state = Some(GestureState::SwipePan);
                    }
                }
            }
            Some(GestureConfigEntry::Threshold(action)) => {
                self.cancel_animations();
                self.gesture_output = self.active_output();
                self.gesture_state = Some(self.build_swipe_threshold(fingers, &mods, context, Some(action)));
            }
            None => {
                // Check if per-direction overrides exist even without a Swipe fallback
                let has_dirs = self.has_swipe_direction_bindings(fingers, &mods, context);
                if has_dirs {
                    self.cancel_animations();
                    self.gesture_output = self.active_output();
                    self.gesture_state = Some(self.build_swipe_threshold(fingers, &mods, context, None));
                } else {
                    self.forward_swipe_begin(fingers, time);
                }
            }
        }
    }

    /// Build a SwipeThreshold state by resolving per-direction overrides from config.
    fn build_swipe_threshold(
        &self,
        fingers: u32,
        mods: &smithay::input::keyboard::ModifiersState,
        context: BindingContext,
        directional: Option<ThresholdAction>,
    ) -> GestureState {
        let resolve_dir = |trigger: GestureTrigger| -> Option<ThresholdAction> {
            self.config.gesture_lookup(mods, &trigger, context).and_then(|entry| {
                match entry {
                    GestureConfigEntry::Threshold(a) => Some(a.clone()),
                    _ => None, // continuous on a directional trigger was rejected at parse time
                }
            })
        };
        GestureState::SwipeThreshold {
            cumulative: Point::from((0.0, 0.0)),
            fired: false,
            up: resolve_dir(GestureTrigger::SwipeUp { fingers }),
            down: resolve_dir(GestureTrigger::SwipeDown { fingers }),
            left: resolve_dir(GestureTrigger::SwipeLeft { fingers }),
            right: resolve_dir(GestureTrigger::SwipeRight { fingers }),
            directional: directional.clone(),
        }
    }

    /// Check if any SwipeUp/Down/Left/Right bindings exist for this finger count.
    fn has_swipe_direction_bindings(
        &self,
        fingers: u32,
        mods: &smithay::input::keyboard::ModifiersState,
        context: BindingContext,
    ) -> bool {
        [
            GestureTrigger::SwipeUp { fingers },
            GestureTrigger::SwipeDown { fingers },
            GestureTrigger::SwipeLeft { fingers },
            GestureTrigger::SwipeRight { fingers },
        ]
        .iter()
        .any(|t| self.config.gesture_lookup(mods, t, context).is_some())
    }

    pub fn on_gesture_swipe_update<I: InputBackend>(&mut self, event: I::GestureSwipeUpdateEvent) {
        let delta = event.delta();
        let time = Event::time_msec(&event);
        let (zoom, _) = self.gesture_camera_zoom();

        let Some(ref mut state) = self.gesture_state else {
            self.forward_swipe_update(delta, time);
            return;
        };

        match state {
            GestureState::SwipePan => {
                let s = self.config.trackpad_speed;
                let canvas_delta: Point<f64, Logical> =
                    (-delta.x * s / zoom, -delta.y * s / zoom).into();
                if let Some(output) = self.gesture_output.clone() {
                    self.drift_pan_on(canvas_delta, &output);
                } else {
                    self.drift_pan(canvas_delta);
                }

                let pointer = self.seat.get_pointer().unwrap();
                let pos = pointer.current_location();
                self.warp_pointer(pos + canvas_delta);
            }
            GestureState::SwipeMove => {
                let pointer = self.seat.get_pointer().unwrap();
                let cursor_pos = pointer.current_location();
                drop(pointer);

                let gesture_output = match self.gesture_output.clone() {
                    Some(o) => o,
                    None => return,
                };
                let (cur_camera, cur_zoom, cur_layout_pos) = {
                    let os = crate::state::output_state(&gesture_output);
                    (os.camera, os.zoom, os.layout_position)
                };
                let output_size = crate::state::output_logical_size(&gesture_output);

                // Current canvas → screen on gesture output, then to layout space
                let old_screen = canvas_to_screen(CanvasPos(cursor_pos), cur_camera, cur_zoom).0;
                let new_screen: Point<f64, Logical> = (
                    old_screen.x + delta.x,
                    old_screen.y + delta.y,
                ).into();
                let new_layout: Point<f64, Logical> = (
                    new_screen.x + cur_layout_pos.x as f64,
                    new_screen.y + cur_layout_pos.y as f64,
                ).into();

                let (target_output, target_screen) =
                    if let Some(target) = self.output_at_layout_pos(new_layout) {
                        if target != gesture_output {
                            let target_lp = crate::state::output_state(&target).layout_position;
                            let ts: Point<f64, Logical> = (
                                new_layout.x - target_lp.x as f64,
                                new_layout.y - target_lp.y as f64,
                            ).into();
                            (target, ts)
                        } else {
                            (gesture_output.clone(), new_screen)
                        }
                    } else {
                        // No adjacent output — clamp to gesture output bounds
                        let clamped: Point<f64, Logical> = (
                            new_screen.x.clamp(0.0, output_size.w as f64 - 1.0),
                            new_screen.y.clamp(0.0, output_size.h as f64 - 1.0),
                        ).into();
                        (gesture_output.clone(), clamped)
                    };

                let (target_camera, target_zoom) = {
                    let os = crate::state::output_state(&target_output);
                    (os.camera, os.zoom)
                };
                let new_canvas = canvas::screen_to_canvas(
                    canvas::ScreenPos(target_screen), target_camera, target_zoom,
                ).0;

                if target_output != gesture_output {
                    self.focused_output = Some(target_output.clone());
                    self.gesture_output = Some(target_output);
                }
                self.warp_pointer(new_canvas);
            }
            GestureState::SwipeResize {
                window,
                edges,
                initial_location,
                initial_size,
                last_size,
                cumulative,
                last_x11_configure,
                snap,
            } => {
                // Force focused_output back if it drifted during resize
                if let Some(ref output) = self.gesture_output
                    && self.focused_output.as_ref().is_some_and(|fo| fo != output)
                {
                    self.focused_output = Some(output.clone());
                }

                // Clamp gesture delta so the virtual pointer stays within the
                // gesture output's bounds (screen space).
                let clamped_delta = if let Some(ref output) = self.gesture_output {
                    let (cam, zm) = {
                        let os = crate::state::output_state(output);
                        (os.camera, os.zoom)
                    };
                    let output_size = crate::state::output_logical_size(output);
                    let pointer = self.seat.get_pointer().unwrap();
                    let cur_screen = canvas_to_screen(CanvasPos(pointer.current_location()), cam, zm).0;
                    drop(pointer);
                    let new_screen: Point<f64, Logical> = (
                        (cur_screen.x + delta.x).clamp(0.0, output_size.w as f64 - 1.0),
                        (cur_screen.y + delta.y).clamp(0.0, output_size.h as f64 - 1.0),
                    ).into();
                    let clamped_dx = new_screen.x - cur_screen.x;
                    let clamped_dy = new_screen.y - cur_screen.y;
                    Point::from((clamped_dx / zm, clamped_dy / zm))
                } else {
                    Point::from((delta.x / zoom, delta.y / zoom))
                };
                // Compute cursor warp target (applied after match to avoid borrow conflict)
                let pointer = self.seat.get_pointer().unwrap();
                let warp_target = pointer.current_location() + clamped_delta;
                drop(pointer);

                *cumulative += clamped_delta;

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

                if self.config.snap_enabled
                    && let Some(ref output) = self.gesture_output
                    && let Some(self_surface) = window.wl_surface().map(|s| s.into_owned())
                {
                    let zoom = output_state(output).zoom;
                    // Can't call self.snap_targets() here — gesture_state is
                    // mutably borrowed by the match arm, so go through the
                    // free function on disjoint field borrows instead.
                    let (others, self_bar) =
                        snap_targets_impl(&self.space, &self.decorations, &self_surface);

                    snap_resize_edges(
                        snap,
                        *edges as u32,
                        (initial_location.x, initial_location.y),
                        (initial_size.w, initial_size.h),
                        self_bar,
                        &mut new_w, &mut new_h,
                        &others, zoom,
                        self.config.snap_gap,
                        self.config.snap_distance,
                        self.config.snap_break_force,
                        self.config.snap_same_edge,
                    );
                }

                let new_size = Size::from((new_w, new_h));
                if new_size != *last_size {
                    *last_size = new_size;
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.with_pending_state(|state| {
                            state.size = Some(new_size);
                            state.states.set(xdg_toplevel::State::Resizing);
                        });
                        toplevel.send_pending_configure();
                    } else if let Some(x11) = window.x11_surface() {
                        let now = std::time::Instant::now();
                        let throttle_ok = last_x11_configure.as_ref().is_none_or(|t| {
                            now.duration_since(*t) >= std::time::Duration::from_millis(16)
                        });
                        if throttle_ok {
                            *last_x11_configure = Some(now);
                            let mut geo = x11.geometry();
                            geo.size = new_size;
                            x11.configure(geo).ok();
                        }
                    }
                }

                self.warp_pointer(warp_target);
            }
            GestureState::SwipeThreshold { cumulative, fired, up, down, left, right, directional } => {
                if *fired {
                    return;
                }
                *cumulative += Point::from((-delta.x, -delta.y));
                let mag_sq = cumulative.x.powi(2) + cumulative.y.powi(2);
                if mag_sq >= self.config.gesture_thresholds.swipe_distance.powi(2) {
                    *fired = true;
                    let action = if cumulative.y.abs() > cumulative.x.abs() {
                        if cumulative.y < 0.0 { up.clone() } else { down.clone() }
                    } else if cumulative.x < 0.0 {
                        left.clone()
                    } else {
                        right.clone()
                    };
                    let action = action.or(directional.clone());
                    let cum = *cumulative;
                    if let Some(action) = action {
                        self.execute_threshold_action(&action, cum);
                    }
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
            self.gesture_output = None;
            self.forward_swipe_end(cancelled, time);
            return;
        };

        match state {
            GestureState::SwipePan => {
                if let Some(output) = self.gesture_output.clone() {
                    self.launch_momentum_on(&output);
                } else {
                    self.launch_momentum();
                }
            }
            GestureState::SwipeMove => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let pointer = self.seat.get_pointer().unwrap();
                pointer.unset_grab(self, serial, time);
            }
            GestureState::SwipeResize {
                window,
                edges,
                initial_location,
                initial_size,
                ..
            } => {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.states.unset(xdg_toplevel::State::Resizing);
                    });
                    toplevel.send_pending_configure();
                } else if let Some(x11) = window.x11_surface() {
                    x11.configure(window.geometry()).ok();
                }

                if let Some(surface) = window.wl_surface().map(|s| s.into_owned()) {
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
                }

                self.cursor.grab_cursor = false;
                self.cursor.cursor_status = CursorImageStatus::default_named();
            }
            GestureState::SwipeThreshold { fired: false, .. } if !cancelled => {
                // Short swipe that didn't reach threshold — no action
            }
            _ => {}
        }
        self.gesture_output = None;
    }

    // ── Pinch ──────────────────────────────────────────────────────────

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
            && matches!(entry, GestureConfigEntry::Continuous(ContinuousAction::Zoom))
        {
            self.cancel_animations();
            self.gesture_output = self.active_output();
            self.gesture_state = Some(GestureState::PinchZoom {
                initial_zoom: self.zoom(),
            });
            return;
        }

        // Check threshold PinchIn/PinchOut triggers
        let pin_in = self.config.gesture_lookup(&mods, &GestureTrigger::PinchIn { fingers }, context);
        let pin_out = self.config.gesture_lookup(&mods, &GestureTrigger::PinchOut { fingers }, context);

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
            GestureState::PinchThreshold { fired_in, fired_out, action_in, action_out } => {
                let to_exec = if !*fired_in && scale < self.config.gesture_thresholds.pinch_in_scale {
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
            GestureState::PinchThreshold { fired_in: false, fired_out: false, .. } if !cancelled => {
                // Pinch that didn't reach threshold — no action
            }
            _ => {}
        }
        self.gesture_output = None;
    }

    pub fn on_gesture_hold_begin<I: InputBackend>(&mut self, event: I::GestureHoldBeginEvent) {
        let fingers = event.fingers();
        let time = Event::time_msec(&event);

        let keyboard = self.seat.get_keyboard().unwrap();
        let mods = keyboard.modifier_state();
        let pointer = self.seat.get_pointer().unwrap();
        let pos = pointer.current_location();
        let context = self.pointer_context(pos);

        let hold_trigger = GestureTrigger::Hold { fingers };
        if let Some(entry) = self.config.gesture_lookup(&mods, &hold_trigger, context) {
            let action = match entry {
                GestureConfigEntry::Threshold(ThresholdAction::Fixed(a)) => Some(a.clone()),
                _ => None,
            };
            if let Some(action) = action {
                self.gesture_state = Some(GestureState::HoldAction { action });
                return;
            }
        }
        self.forward_hold_begin(fingers, time);
    }

    pub fn on_gesture_hold_end<I: InputBackend>(&mut self, event: I::GestureHoldEndEvent) {
        let cancelled = event.cancelled();
        let time = Event::time_msec(&event);
        if let Some(GestureState::HoldAction { action }) = self.gesture_state.take() {
            if !cancelled {
                self.execute_action(&action);
            }
            return;
        }
        self.forward_hold_end(cancelled, time);
    }

    // ── DeviceAdded ────────────────────────────────────────────────────

    /// Configure a libinput device using settings from config.
    /// Trackpads get trackpad settings, mice get mouse settings.
    pub fn configure_libinput_device(&self, device: &mut smithay::reexports::input::Device) {
        if device.config_tap_finger_count() > 0 {
            self.configure_trackpad(device);
        } else if device.has_capability(smithay::reexports::input::DeviceCapability::Pointer) {
            self.configure_mouse(device);
        }
    }

    fn configure_trackpad(&self, device: &mut smithay::reexports::input::Device) {
        let cfg = &self.config.trackpad;
        tracing::info!(
            "Configuring trackpad: {} (tap={}, natural_scroll={}, accel={}, profile={:?}, click_method={:?})",
            device.name(),
            cfg.tap_to_click,
            cfg.natural_scroll,
            cfg.accel_speed,
            cfg.accel_profile,
            cfg.click_method,
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
        set_accel_profile(device, cfg.accel_profile);
        if let Some(ref method) = cfg.click_method {
            let click = match method.as_str() {
                "none" => None,
                "button_areas" => Some(smithay::reexports::input::ClickMethod::ButtonAreas),
                "clickfinger" => Some(smithay::reexports::input::ClickMethod::Clickfinger),
                other => {
                    tracing::warn!("Unknown click_method '{other}', ignoring");
                    None
                }
            };
            if let Some(click) = click
                && let Err(e) = device.config_click_set_method(click)
            {
                tracing::warn!("Failed to set click_method: {e:?}");
            }
        }
    }

    fn configure_mouse(&self, device: &mut smithay::reexports::input::Device) {
        let cfg = &self.config.mouse_device;
        tracing::info!(
            "Configuring mouse: {} (accel={}, profile={:?}, natural_scroll={})",
            device.name(),
            cfg.accel_speed,
            cfg.accel_profile,
            cfg.natural_scroll,
        );

        if let Err(e) = device.config_accel_set_speed(cfg.accel_speed) {
            tracing::warn!("Failed to set mouse accel_speed: {e:?}");
        }
        set_accel_profile(device, cfg.accel_profile);
        if let Err(e) = device.config_scroll_set_natural_scroll_enabled(cfg.natural_scroll) {
            tracing::warn!("Failed to set mouse natural_scroll: {e:?}");
        }
    }

    // ── Gesture setup helpers ─────────────────────────────────────────

    /// Enter Swipe3Move state: focus + raise the window, set a MoveSurfaceGrab
    /// on the pointer so gesture updates just warp the cursor and the grab
    /// handles window positioning (identical to Alt+click drag).
    /// If the window is pinned, falls through to Swipe3Pan instead.
    fn start_gesture_move(&mut self, window: Window, pos: Point<f64, Logical>) {
        if window.wl_surface().as_ref()
            .and_then(|s| driftwm::config::applied_rule(s))
            .is_some_and(|r| r.widget)
        {
            self.gesture_state = Some(GestureState::SwipePan);
            return;
        }
        let serial = SERIAL_COUNTER.next_serial();
        self.space.raise_element(&window, true);
        let keyboard = self.seat.get_keyboard().unwrap();
        let Some(surface) = window.wl_surface().map(|s| s.into_owned()) else { return; };
        keyboard.set_focus(self, Some(FocusTarget(surface)), serial);
        self.enforce_below_windows();

        let initial_window_location = self.space.element_location(&window).unwrap_or_default();
        let pointer = self.seat.get_pointer().unwrap();
        let grab = MoveSurfaceGrab::new(
            GrabStartData {
                focus: None,
                button: 0, // no physical button — gesture-initiated
                location: pos,
            },
            window,
            initial_window_location,
            self.active_output().unwrap(),
        );
        pointer.set_grab(self, grab, serial, Focus::Clear);

        self.gesture_state = Some(GestureState::SwipeMove);
    }

    /// Enter Swipe3Resize state: store initial geometry, set resize state + cursor.
    fn start_gesture_resize(&mut self, window: Window, pos: Point<f64, Logical>) {
        let serial = SERIAL_COUNTER.next_serial();
        let Some(wl_surface) = window.wl_surface().map(|s| s.into_owned()) else { return; };
        self.space.raise_element(&window, true);
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, Some(FocusTarget(wl_surface.clone())), serial);
        self.enforce_below_windows();

        let initial_location = self.space.element_location(&window).unwrap();
        let initial_size = window.geometry().size;
        let edges = edges_from_position(pos, initial_location, initial_size);

        // Clear fit state — user took manual control
        crate::state::fit::clear_fit_state(&wl_surface);

        // Store resize state on surface data map for commit() repositioning
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

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
            });
        }

        self.cursor.grab_cursor = true;
        self.cursor.cursor_status = CursorImageStatus::Named(resize_cursor(edges));

        self.gesture_state = Some(GestureState::SwipeResize {
            window,
            edges,
            initial_location,
            initial_size,
            last_size: initial_size,
            cumulative: Point::from((0.0, 0.0)),
            last_x11_configure: None,
            snap: SnapState::default(),
        });
    }

    /// Execute a threshold action, injecting direction from the swipe vector for CenterNearest.
    fn execute_threshold_action(&mut self, action: &ThresholdAction, cumulative: Point<f64, Logical>) {
        match action {
            ThresholdAction::CenterNearest => {
                let dir = direction_from_vector(cumulative);
                self.execute_action(&Action::CenterNearest(dir));
            }
            ThresholdAction::Fixed(a) => {
                self.execute_action(a);
            }
        }
    }

    /// Return the window under `pos` for move/resize gestures.
    fn window_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(Window, Point<i32, Logical>)> {
        self.space
            .element_under(pos)
            .map(|(w, l)| (w.clone(), l))
    }

    /// Read camera/zoom from the pinned gesture output, falling back to active output.
    fn gesture_camera_zoom(&self) -> (f64, Point<f64, Logical>) {
        match self.gesture_output {
            Some(ref o) => {
                let os = crate::state::output_state(o);
                (os.zoom, os.camera)
            }
            None => (self.zoom(), self.camera()),
        }
    }

    fn cancel_animations(&mut self) {
        self.with_output_state(|os| {
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_center = None;
            os.momentum.stop();
        });
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
