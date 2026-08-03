//! Swipe gesture handlers — pan/move/resize/threshold swipes.
//!
//! Includes the swipe-specific setup helpers (`try_start_gesture_move`,
//! `try_start_gesture_resize`) and threshold-action execution, since they're
//! only reached through swipe and DoubletapSwipe begin paths.

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
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::seat::WaylandFocus,
};

use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::config::{
    Action, BindingContext, ContinuousAction, GestureConfigEntry, GestureTrigger, ThresholdAction,
};
use driftwm::layout::snap::SnapState;
use driftwm::window_ext::WindowExt;

use crate::grabs::{MoveGrab, ResizeGrab};
use crate::input::pointer::{edges_from_position, resize_cursor};
use crate::state::{ClusterMember, DriftWm, StageWindow};

use super::{GestureState, direction_from_vector};

/// What a move or resize gesture landed on. Pinned is always a live client —
/// see `MoveGrab::apply_pinned_move` / `ResizeGrab::apply_pinned_resize`.
enum GestureTarget {
    Pinned(Window),
    Canvas(StageWindow),
}

impl DriftWm {
    pub fn on_gesture_swipe_begin<I: InputBackend>(&mut self, event: I::GestureSwipeBeginEvent) {
        let fingers = event.fingers();
        let time = Event::time_msec(&event);

        let keyboard = self.seat.get_keyboard().unwrap();
        let mods = keyboard.modifier_state();
        let pointer = self.seat.get_pointer().unwrap();
        let mut pos = pointer.current_location();
        // The fullscreen window fills the screen; a continuous gesture exits it
        // eagerly below (the grab needs post-exit geometry).
        let context = if self.is_fullscreen() {
            BindingContext::OnWindow
        } else {
            self.pointer_context(pos)
        };

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
                    GestureConfigEntry::Continuous(
                        action @ (ContinuousAction::MoveWindow
                        | ContinuousAction::MoveSnappedWindows),
                    ) => {
                        if self.is_fullscreen() {
                            self.exit_fullscreen();
                            pos = pointer.current_location();
                        }
                        let cluster = matches!(action, ContinuousAction::MoveSnappedWindows);
                        if self.try_start_gesture_move(pos, cluster) {
                            return;
                        }
                        // Not over a moveable window — flush and fall through
                        self.flush_middle_click(pending.press_time, pending.release_time);
                    }
                    GestureConfigEntry::Continuous(
                        action @ (ContinuousAction::ResizeWindow
                        | ContinuousAction::ResizeWindowSnapped),
                    ) => {
                        if self.is_fullscreen() {
                            self.exit_fullscreen();
                            pos = pointer.current_location();
                        }
                        let want_cluster = matches!(action, ContinuousAction::ResizeWindowSnapped);
                        if self.try_start_gesture_resize(pos, want_cluster) {
                            return;
                        }
                        // Not over a resizable element — flush and fall through
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
                if self.is_fullscreen() {
                    self.exit_fullscreen();
                    pos = pointer.current_location();
                }
                self.cancel_animations();
                self.gesture_output = self.active_output();
                match action {
                    ContinuousAction::PanViewport => {
                        self.gesture_state = Some(GestureState::SwipePan);
                    }
                    ContinuousAction::MoveWindow | ContinuousAction::MoveSnappedWindows => {
                        let cluster = matches!(action, ContinuousAction::MoveSnappedWindows);
                        if !self.try_start_gesture_move(pos, cluster) {
                            // Not over a moveable element — fall back to pan
                            self.gesture_state = Some(GestureState::SwipePan);
                        }
                    }
                    ContinuousAction::ResizeWindow | ContinuousAction::ResizeWindowSnapped => {
                        let want_cluster = matches!(action, ContinuousAction::ResizeWindowSnapped);
                        if !self.try_start_gesture_resize(pos, want_cluster) {
                            // Not over a resizable element — fall back to pan
                            self.gesture_state = Some(GestureState::SwipePan);
                        }
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
                    self.drift_pan_on(canvas_delta, time, &output);
                } else {
                    self.drift_pan(canvas_delta, time);
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
            GestureState::SwipeResizeGrab => {
                // Warp the cursor (clamped to the grab's output); the grab does
                // the resize math. Unlike SwipeMove there's no cross-output
                // teleport — the grab forces the pointer back if input routing
                // crosses, so a resize stays on one output.
                let Some(output) = self.gesture_output.clone() else {
                    return;
                };
                let (camera, zoom) = {
                    let os = crate::state::output_state(&output);
                    (os.camera, os.zoom)
                };
                let output_size = crate::state::output_logical_size(&output);
                let pointer = self.seat.get_pointer().unwrap();
                let cur_screen =
                    canvas_to_screen(CanvasPos(pointer.current_location()), camera, zoom).0;
                drop(pointer);
                let new_screen: Point<f64, Logical> = (
                    (cur_screen.x + delta.x).clamp(0.0, output_size.w as f64 - 1.0),
                    (cur_screen.y + delta.y).clamp(0.0, output_size.h as f64 - 1.0),
                )
                    .into();
                let warp_target =
                    canvas::screen_to_canvas(canvas::ScreenPos(new_screen), camera, zoom).0;
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
                // Accumulate the physical finger vector: named per-direction
                // triggers (swipe-up/down/left/right) fire in physical direction.
                // CenterNearest negates it back to content/pan orientation below.
                *cumulative += Point::from((delta.x, delta.y));
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
            GestureState::SwipeResizeGrab => {
                // No button release on a gesture, so unset the grab here; its
                // `unset` finalizes the resize, same as the button-release path.
                let serial = SERIAL_COUNTER.next_serial();
                let pointer = self.seat.get_pointer().unwrap();
                pointer.unset_grab(self, serial, time);
            }
            GestureState::SwipeThreshold { fired: false, .. } if !cancelled => {
                // Short swipe that didn't reach threshold — no action
            }
            _ => {}
        }
        self.gesture_output = None;
    }

    /// Enter SwipeMove state: focus + raise whatever is under `pos` — a client
    /// window or a suspended stand-in — and set a MoveGrab on the pointer so
    /// gesture updates just warp the cursor and the grab handles positioning
    /// (identical to Alt+click drag). Pinned windows get the screen-space pinned
    /// grab. Returns `false` when nothing draggable is there, so the caller can
    /// fall back to pan.
    pub(crate) fn try_start_gesture_move(
        &mut self,
        pos: Point<f64, Logical>,
        cluster: bool,
    ) -> bool {
        let Some(target) = self.gesture_target_under(pos) else {
            return false;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let element = match target {
            // Screen-pinned windows move in screen space via the same grab as
            // Alt+drag; the SwipeMove warp drives it. The picker above already
            // resolved a pin site on the active output, so `start_pinned_move`'s
            // bails can't fire here — checking anyway is defensive, matching the
            // canvas arm's raise-after-grab-is-certain ordering below in case the
            // picker's guarantee ever loosens.
            GestureTarget::Pinned(window) => {
                let pointer = self.seat.get_pointer().unwrap();
                if !self.start_pinned_move(&pointer, &window, pos, 0, serial) {
                    return false;
                }
                self.raise_and_focus(&window, serial);
                self.gesture_state = Some(GestureState::SwipeMove);
                return true;
            }
            GestureTarget::Canvas(element) => element,
        };
        // Every bail comes before the raise + focus: a gesture that falls back
        // to pan must not leave a z-order and focus change behind.
        let Some(initial_window_location) = self.stage.position_of(&element) else {
            return false;
        };
        let Some(output) = self.active_output() else {
            return false;
        };
        self.raise_and_focus_element(&element, serial);

        let members = if cluster {
            self.cluster_snapshot_for_drag(&element, initial_window_location)
        } else {
            Vec::new()
        };
        // Moving re-anchors the element, invalidating any fill restore point —
        // for the primary and every member dragged along.
        self.stage.clear_fill(&element);
        for (member, _) in &members {
            self.stage.clear_fill(member);
        }
        let grab_target = ClusterMember::from_element(&element);
        self.arm_interactive_move(&grab_target);
        let grab = MoveGrab::new(
            GrabStartData {
                focus: None,
                button: 0, // no physical button — gesture-initiated
                location: pos,
            },
            grab_target,
            initial_window_location,
            output,
            members,
        );
        let pointer = self.seat.get_pointer().unwrap();
        pointer.set_grab(self, grab, serial, Focus::Clear);

        self.gesture_state = Some(GestureState::SwipeMove);
        true
    }

    /// Set up a ResizeGrab on the pointer so gesture updates just warp the cursor
    /// and the grab handles the resize (mirrors `try_start_gesture_move` /
    /// Alt+RMB drag). A client window and a suspended stand-in both resize here;
    /// pinned windows get the screen-space pointer path. Returns `false` when
    /// nothing resizable is there, so the caller can fall back to pan.
    ///
    /// `want_cluster = true` opts into snapped-neighbor propagation.
    pub(crate) fn try_start_gesture_resize(
        &mut self,
        pos: Point<f64, Logical>,
        want_cluster: bool,
    ) -> bool {
        let Some(target) = self.gesture_target_under(pos) else {
            return false;
        };
        let element = match target {
            // Pinned windows resize in screen space; reuse the pointer resize
            // path, which infers the edge against the screen rect and threads the
            // pinned anchor through to the grab and the commit-time reposition.
            // The picker above already resolved a pin site on the active output,
            // so that path's bails can't fire here — checking anyway is
            // defensive, matching the canvas arm's raise-after-grab-is-certain
            // ordering below in case the picker's guarantee ever loosens.
            GestureTarget::Pinned(window) => {
                let serial = SERIAL_COUNTER.next_serial();
                let pointer = self.seat.get_pointer().unwrap();
                if !self.start_compositor_resize_with_edge(
                    &pointer,
                    &window,
                    pos,
                    0,
                    serial,
                    None,
                    want_cluster,
                ) {
                    return false;
                }
                self.raise_and_focus(&window, serial);
                self.gesture_state = Some(GestureState::SwipeResizeGrab);
                return true;
            }
            GestureTarget::Canvas(element) => element,
        };
        // Every bail comes before the raise + focus: a gesture that falls back
        // to pan must not leave a z-order and focus change behind.
        let Some(initial_location) = self.stage.position_of(&element) else {
            return false;
        };
        let Some(output) = self.active_output() else {
            return false;
        };
        let initial_size = element.geometry().size;
        let edges = edges_from_position(pos, initial_location, initial_size);

        // Opt-in cluster propagation: only the `resize-snapped` gesture
        // variant snapshots the cluster. Plain gesture resize builds an
        // empty snapshot and behaves as single-window.
        let cluster_resize = if want_cluster {
            self.cluster_snapshot_for_resize(&element, edges)
        } else {
            crate::state::ClusterResizeSnapshot::empty()
        };

        let (grab_target, constraints, locked_ratio) = match &element {
            StageWindow::Client(window) => {
                let Some(wl_surface) = window.wl_surface().map(|s| s.into_owned()) else {
                    return false;
                };
                // No pinned anchor: the pinned arm above delegated to the
                // pointer path and returned, so this is a canvas window.
                self.begin_client_resize(window, &wl_surface, edges, initial_size, None);
                (
                    ClusterMember::Client(window.clone()),
                    crate::grabs::SizeConstraints::for_window(window),
                    crate::grabs::locked_ratio_for(window, initial_size),
                )
            }
            // A stand-in's size is the compositor's own — no configure to send
            // and no ack to wait for, so no surface state is written.
            StageWindow::Suspended(s) => {
                self.arm_interactive_move(&s.id);
                (
                    ClusterMember::Suspended(s.id),
                    crate::grabs::SizeConstraints::for_suspended(self.suspended_chrome()),
                    None,
                )
            }
        };

        self.cursor.grab_cursor = true;
        self.cursor.cursor_status = CursorImageStatus::Named(resize_cursor(edges));

        let serial = SERIAL_COUNTER.next_serial();
        self.raise_and_focus_element(&element, serial);

        let (start_screen, start_zoom) = crate::grabs::resize_screen_anchor(&output, pos);
        let grab = ResizeGrab {
            start_data: GrabStartData {
                focus: None,
                button: 0, // no physical button — gesture-initiated
                location: pos,
            },
            target: grab_target,
            edges,
            initial_window_location: initial_location,
            initial_window_size: initial_size,
            last_window_size: initial_size,
            output,
            start_screen,
            start_zoom,
            last_clamped_location: pos,
            snap: SnapState::default(),
            constraints,
            cluster_resize,
            pinned_initial_screen_pos: None,
            touch_start: None,
            touch_slots: 0,
            locked_ratio,
        };
        let pointer = self.seat.get_pointer().unwrap();
        pointer.set_grab(self, grab, serial, Focus::Clear);

        self.gesture_state = Some(GestureState::SwipeResizeGrab);
        true
    }

    /// Execute a threshold action, injecting direction from the swipe vector for
    /// CenterNearest. The vector is physical; CenterNearest keeps content/pan
    /// orientation (fingers left looks right), so it negates.
    fn execute_threshold_action(
        &mut self,
        action: &ThresholdAction,
        cumulative: Point<f64, Logical>,
    ) {
        match action {
            ThresholdAction::CenterNearest => {
                let dir = direction_from_vector(Point::from((-cumulative.x, -cumulative.y)));
                self.execute_action(&Action::CenterNearest(dir));
            }
            ThresholdAction::Fixed(a) => {
                self.execute_action(a);
            }
        }
    }

    /// What a move or resize gesture at `pos` landed on. Pinned windows render
    /// above the canvas and hit-test in screen space, so they take priority;
    /// everything else goes through the stand-in-aware `draggable_element_under`.
    /// Widgets are grab-proof on both channels.
    fn gesture_target_under(&self, pos: Point<f64, Logical>) -> Option<GestureTarget> {
        let screen_pos = canvas_to_screen(CanvasPos(pos), self.camera(), self.zoom()).0;
        if let Some(window) = self.pinned_element_under(screen_pos) {
            return (!window.is_widget()).then_some(GestureTarget::Pinned(window));
        }
        self.draggable_element_under(pos).map(GestureTarget::Canvas)
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
