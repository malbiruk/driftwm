//! Swipe gesture handlers — pan/move/resize/threshold swipes.
//!
//! Includes the swipe-specific setup helpers (`start_gesture_move`,
//! `start_gesture_resize`) and threshold-action execution, since they're
//! only reached through swipe and DoubletapSwipe begin paths.

use std::cell::RefCell;
use std::collections::HashSet;

use smithay::{
    backend::input::{
        Event, GestureBeginEvent, GestureEndEvent, GestureSwipeUpdateEvent, InputBackend,
    },
    desktop::Window,
    input::pointer::{
        CursorImageStatus, Focus, GestureSwipeBeginEvent as WlSwipeBegin,
        GestureSwipeEndEvent as WlSwipeEnd, GestureSwipeUpdateEvent as WlSwipeUpdate,
        GrabStartData,
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, SERIAL_COUNTER, Size},
    wayland::{compositor::with_states, seat::WaylandFocus},
};

use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::config::{
    Action, BindingContext, ContinuousAction, GestureConfigEntry, GestureTrigger, ThresholdAction,
};
use driftwm::snap::{SnapState, snap_resize_edges};

use crate::grabs::{MoveSurfaceGrab, ResizeState, has_bottom, has_left, has_right, has_top};
use crate::input::pointer::{edges_from_position, resize_cursor};
use crate::state::{DriftWm, FocusTarget, output_state, snap_targets_impl};

use super::{GestureState, direction_from_vector};

impl DriftWm {
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
            let dt_entry = self
                .config
                .gesture_lookup(&mods, &dt_trigger, context)
                .cloned();
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
                    GestureConfigEntry::Continuous(
                        action @ (ContinuousAction::ResizeWindow
                        | ContinuousAction::ResizeWindowSnapped),
                    ) => {
                        if let Some((window, _)) = self.window_under(pos).filter(|(w, _)| {
                            !w.wl_surface()
                                .as_ref()
                                .and_then(|s| driftwm::config::applied_rule(s))
                                .is_some_and(|r| r.widget)
                        }) {
                            let want_cluster =
                                matches!(action, ContinuousAction::ResizeWindowSnapped);
                            return self.start_gesture_resize(window, pos, want_cluster);
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
        let entry = self
            .config
            .gesture_lookup(&mods, &swipe_trigger, context)
            .cloned();

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
                    ContinuousAction::ResizeWindow | ContinuousAction::ResizeWindowSnapped => {
                        if let Some((window, _)) = self.window_under(pos).filter(|(w, _)| {
                            !w.wl_surface()
                                .as_ref()
                                .and_then(|s| driftwm::config::applied_rule(s))
                                .is_some_and(|r| r.widget)
                        }) {
                            let want_cluster =
                                matches!(action, ContinuousAction::ResizeWindowSnapped);
                            return self.start_gesture_resize(window, pos, want_cluster);
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
                self.gesture_state =
                    Some(self.build_swipe_threshold(fingers, &mods, context, Some(action)));
            }
            None => {
                // Check if per-direction overrides exist even without a Swipe fallback
                let has_dirs = self.has_swipe_direction_bindings(fingers, &mods, context);
                if has_dirs {
                    self.cancel_animations();
                    self.gesture_output = self.active_output();
                    self.gesture_state =
                        Some(self.build_swipe_threshold(fingers, &mods, context, None));
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
            self.config
                .gesture_lookup(mods, &trigger, context)
                .and_then(|entry| {
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
                let new_screen: Point<f64, Logical> =
                    (old_screen.x + delta.x, old_screen.y + delta.y).into();
                let new_layout: Point<f64, Logical> = (
                    new_screen.x + cur_layout_pos.x as f64,
                    new_screen.y + cur_layout_pos.y as f64,
                )
                    .into();

                let (target_output, target_screen) =
                    if let Some(target) = self.output_at_layout_pos(new_layout) {
                        if target != gesture_output {
                            let target_lp = crate::state::output_state(&target).layout_position;
                            let ts: Point<f64, Logical> = (
                                new_layout.x - target_lp.x as f64,
                                new_layout.y - target_lp.y as f64,
                            )
                                .into();
                            (target, ts)
                        } else {
                            (gesture_output.clone(), new_screen)
                        }
                    } else {
                        // No adjacent output — clamp to gesture output bounds
                        let clamped: Point<f64, Logical> = (
                            new_screen.x.clamp(0.0, output_size.w as f64 - 1.0),
                            new_screen.y.clamp(0.0, output_size.h as f64 - 1.0),
                        )
                            .into();
                        (gesture_output.clone(), clamped)
                    };

                let (target_camera, target_zoom) = {
                    let os = crate::state::output_state(&target_output);
                    (os.camera, os.zoom)
                };
                let new_canvas = canvas::screen_to_canvas(
                    canvas::ScreenPos(target_screen),
                    target_camera,
                    target_zoom,
                )
                .0;

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
                snap,
                constraints,
                cluster_resize,
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
                    let cur_screen =
                        canvas_to_screen(CanvasPos(pointer.current_location()), cam, zm).0;
                    drop(pointer);
                    let new_screen: Point<f64, Logical> = (
                        (cur_screen.x + delta.x).clamp(0.0, output_size.w as f64 - 1.0),
                        (cur_screen.y + delta.y).clamp(0.0, output_size.h as f64 - 1.0),
                    )
                        .into();
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
                // Same client-hints clamp as `ResizeSurfaceGrab::motion`:
                // applied before snap + cluster propagation so both see
                // the real clamped size.
                let (nw, nh) = constraints.clamp(new_w, new_h);
                new_w = nw;
                new_h = nh;

                if self.config.snap_enabled
                    && let Some(ref output) = self.gesture_output
                    && let Some(self_surface) = window.wl_surface().map(|s| s.into_owned())
                {
                    let zoom = output_state(output).zoom;
                    // Can't call self.snap_targets() here — gesture_state is
                    // mutably borrowed by the match arm, so go through the
                    // free function on disjoint field borrows instead.
                    let (others, self_bar) = snap_targets_impl(
                        &self.space,
                        &self.decorations,
                        &self_surface,
                        &cluster_resize.exclude,
                    );

                    snap_resize_edges(
                        snap,
                        *edges as u32,
                        (initial_location.x, initial_location.y),
                        (initial_size.w, initial_size.h),
                        self_bar,
                        &mut new_w,
                        &mut new_h,
                        &others,
                        zoom,
                        self.config.snap_gap,
                        self.config.snap_distance,
                        self.config.snap_break_force,
                        self.config.snap_same_edge,
                    );
                }

                cluster_resize.apply_member_shifts(
                    &mut self.space,
                    window,
                    *initial_size,
                    new_w,
                    new_h,
                    self.config.snap_gap,
                );

                let new_size = Size::from((new_w, new_h));
                if new_size != *last_size {
                    *last_size = new_size;
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.with_pending_state(|state| {
                            state.size = Some(new_size);
                            state.states.set(xdg_toplevel::State::Resizing);
                        });
                        toplevel.send_pending_configure();
                    }
                }

                self.warp_pointer(warp_target);
            }
            GestureState::SwipeThreshold {
                cumulative,
                fired,
                up,
                down,
                left,
                right,
                directional,
            } => {
                if *fired {
                    return;
                }
                *cumulative += Point::from((-delta.x, -delta.y));
                let mag_sq = cumulative.x.powi(2) + cumulative.y.powi(2);
                if mag_sq >= self.config.gesture_thresholds.swipe_distance.powi(2) {
                    *fired = true;
                    let action = if cumulative.y.abs() > cumulative.x.abs() {
                        if cumulative.y < 0.0 {
                            up.clone()
                        } else {
                            down.clone()
                        }
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

    /// Enter Swipe3Move state: focus + raise the window, set a MoveSurfaceGrab
    /// on the pointer so gesture updates just warp the cursor and the grab
    /// handles window positioning (identical to Alt+click drag).
    /// If the window is pinned, falls through to Swipe3Pan instead.
    fn start_gesture_move(&mut self, window: Window, pos: Point<f64, Logical>) {
        if window
            .wl_surface()
            .as_ref()
            .and_then(|s| driftwm::config::applied_rule(s))
            .is_some_and(|r| r.widget)
        {
            self.gesture_state = Some(GestureState::SwipePan);
            return;
        }
        let serial = SERIAL_COUNTER.next_serial();
        self.space.raise_element(&window, true);
        let keyboard = self.seat.get_keyboard().unwrap();
        let Some(surface) = window.wl_surface().map(|s| s.into_owned()) else {
            return;
        };
        keyboard.set_focus(self, Some(FocusTarget(surface)), serial);
        self.enforce_below_windows();

        // 3-finger double-tap+drag is the trackpad-first way to move a
        // single window. Cluster drag is a mouse action (Alt+Shift+Left);
        // there's no modifier on a gesture to distinguish single-vs-cluster,
        // so gestures always move the focused window alone.
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
            Vec::new(),
            HashSet::new(),
        );
        pointer.set_grab(self, grab, serial, Focus::Clear);

        self.gesture_state = Some(GestureState::SwipeMove);
    }

    /// Enter Swipe3Resize state: store initial geometry, set resize state + cursor.
    ///
    /// `want_cluster = true` opts into snapped-neighbor propagation (the
    /// gesture caller picked `ContinuousAction::ResizeWindowSnapped`); `false`
    /// keeps resize strictly single-window by handing the grab an empty
    /// cluster snapshot.
    fn start_gesture_resize(
        &mut self,
        window: Window,
        pos: Point<f64, Logical>,
        want_cluster: bool,
    ) {
        let serial = SERIAL_COUNTER.next_serial();
        let Some(wl_surface) = window.wl_surface().map(|s| s.into_owned()) else {
            return;
        };
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

        // Opt-in cluster propagation: only the `resize-snapped` gesture
        // variant snapshots the cluster. Plain gesture resize builds an
        // empty snapshot and behaves as single-window.
        let cluster_resize = if want_cluster {
            self.cluster_snapshot_for_resize(&window, edges)
        } else {
            crate::state::ClusterResizeSnapshot::empty()
        };
        let constraints = crate::grabs::SizeConstraints::for_window(&window);
        self.gesture_state = Some(GestureState::SwipeResize {
            window,
            edges,
            initial_location,
            initial_size,
            last_size: initial_size,
            cumulative: Point::from((0.0, 0.0)),
            snap: SnapState::default(),
            constraints,
            cluster_resize,
        });
    }

    /// Execute a threshold action, injecting direction from the swipe vector for CenterNearest.
    fn execute_threshold_action(
        &mut self,
        action: &ThresholdAction,
        cumulative: Point<f64, Logical>,
    ) {
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
    fn window_under(&self, pos: Point<f64, Logical>) -> Option<(Window, Point<i32, Logical>)> {
        self.space.element_under(pos).map(|(w, l)| (w.clone(), l))
    }

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
    }

    fn forward_swipe_update(&mut self, delta: Point<f64, Logical>, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        pointer.gesture_swipe_update(self, &WlSwipeUpdate { time, delta });
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
    }
}
