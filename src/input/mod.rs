mod actions;
pub(crate) mod gestures;
pub(crate) mod keyboard;
mod pointer;
pub(crate) mod touch;

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, Event, InputBackend, InputEvent, PointerAxisEvent,
        PointerButtonEvent, PointerMotionEvent,
    },
    desktop::{WindowSurfaceType, layer_map_for_output},
    input::pointer::{MotionEvent, RelativeMotionEvent},
    utils::{Point, SERIAL_COUNTER},
    wayland::shell::wlr_layer::Layer as WlrLayer,
};

use smithay::desktop::Window;
use smithay::desktop::space::SpaceElement;
use smithay::reexports::wayland_server::Resource;
use smithay::wayland::pointer_constraints::{PointerConstraint, with_pointer_constraint};
use smithay::wayland::seat::WaylandFocus;

use smithay::utils::Logical;
use smithay::wayland::compositor::RegionAttributes;

use crate::decorations::DecorationHit;
use crate::state::{DriftWm, FocusTarget};
use driftwm::canvas::{ScreenPos, screen_to_canvas};
use driftwm::protocols::output_power::OutputPowerHandler;

/// Constant-speed edge-pan velocity for the bare cursor: a steady glide
/// whenever the cursor sits within `zone` px of an edge of the *usable* area
/// (output minus layer-shell exclusive zones), directed away from the edge(s)
/// it's near. Measuring from the usable area rather than the raw output keeps
/// the pan zone reachable below a bar that reserves an exclusive zone — against
/// the raw output, a bar taller than `zone` swallows that edge's zone entirely
/// and panning toward it becomes impossible. Unlike the window-drag joystick
/// curve, the magnitude does not ramp with depth — so the speed stays the same
/// no matter how hard the cursor is pushed into the edge. Diagonals are
/// normalized so a corner doesn't pan √2 faster. Returns `None` outside the
/// zone.
fn cursor_edge_pan_velocity(
    screen_pos: Point<f64, Logical>,
    usable: smithay::utils::Rectangle<i32, Logical>,
    zone: f64,
    speed: f64,
) -> Option<Point<f64, Logical>> {
    let usable_x = usable.loc.x as f64;
    let usable_y = usable.loc.y as f64;
    let usable_right = usable_x + usable.size.w as f64;
    let usable_bottom = usable_y + usable.size.h as f64;

    // Outside the usable area (e.g. over a bar's reserved space) the distances
    // below go negative, which would read as "even deeper in the zone" and pan.
    if screen_pos.x < usable_x
        || screen_pos.x > usable_right
        || screen_pos.y < usable_y
        || screen_pos.y > usable_bottom
    {
        return None;
    }

    let dist_left = screen_pos.x - usable_x;
    let dist_right = usable_right - screen_pos.x;
    let dist_top = screen_pos.y - usable_y;
    let dist_bottom = usable_bottom - screen_pos.y;

    let mut vx: f64 = 0.0;
    let mut vy: f64 = 0.0;
    if dist_left < zone {
        vx -= 1.0;
    }
    if dist_right < zone {
        vx += 1.0;
    }
    if dist_top < zone {
        vy -= 1.0;
    }
    if dist_bottom < zone {
        vy += 1.0;
    }

    let len = (vx * vx + vy * vy).sqrt();
    if len == 0.0 {
        return None;
    }
    Some(Point::from((vx / len * speed, vy / len * speed)))
}

/// Find the canvas-space element location of the window that owns the given surface.
fn window_origin_for_surface(
    state: &DriftWm,
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> Option<Point<f64, smithay::utils::Logical>> {
    let window = state
        .space
        .elements()
        .find(|w| w.wl_surface().as_deref() == Some(surface))?;
    Some(state.stage.position_of(window)?.to_f64())
}

impl DriftWm {
    fn wake_dpms_off_outputs(&mut self) {
        if self.dpms_off_outputs.is_empty() {
            return;
        }
        let outputs: Vec<_> = self.dpms_off_outputs.iter().cloned().collect();
        for output in outputs {
            OutputPowerHandler::set_dpms(self, &output, true);
        }
    }

    /// True when the event is relative motion under a locked pointer (typically a
    /// fullscreen game). The pointer position is frozen (and the cursor usually
    /// hidden) and the client redraws via its own surface commits, so a blanket
    /// mark would only compete with its frames at mouse-poll rate.
    fn is_relative_motion_to_locked_pointer<I: InputBackend>(&self, event: &InputEvent<I>) -> bool {
        if !matches!(event, InputEvent::PointerMotion { .. }) {
            return false;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return false;
        };
        let Some(focus) = pointer.current_focus() else {
            return false;
        };
        with_pointer_constraint(&focus.0, &pointer, |c| {
            c.is_some_and(|c| c.is_active() && matches!(&*c, PointerConstraint::Locked(_)))
        })
    }

    /// Process a single input event from any backend (winit, libinput, etc).
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>)
    where
        I::Device: 'static,
    {
        if !self.is_relative_motion_to_locked_pointer(&event) {
            self.mark_all_dirty();
        }

        // Notify idle tracker of user activity (skip device add/remove metadata events).
        // Also wake any DPMS-off outputs — without this, recovering from
        // `wlopm --off` requires a daemon round-trip (swayidle resume command)
        // and the user perceives a dead-screen frame.
        if !matches!(
            &event,
            InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }
        ) {
            self.idle_notifier_state.notify_activity(&self.seat);
            self.wake_dpms_off_outputs();
        }

        // When locked, forward keyboard (VT switch + lock surface input) and
        // pointer events directly to smithay — no compositor grabs or gestures.
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            match event {
                InputEvent::Keyboard { event } => self.on_keyboard::<I>(event),
                InputEvent::PointerMotion { event } => self.on_pointer_motion_relative::<I>(event),
                InputEvent::PointerMotionAbsolute { event } => {
                    self.on_pointer_motion_absolute::<I>(event)
                }
                InputEvent::PointerButton { event } => {
                    let pointer = self.seat.get_pointer().unwrap();
                    pointer.button(
                        self,
                        &smithay::input::pointer::ButtonEvent {
                            button: PointerButtonEvent::button_code(&event),
                            state: PointerButtonEvent::state(&event),
                            serial: SERIAL_COUNTER.next_serial(),
                            time: Event::time_msec(&event),
                        },
                    );
                    pointer.frame(self);
                }
                InputEvent::PointerAxis { event } => {
                    let pointer = self.seat.get_pointer().unwrap();
                    let mut frame =
                        smithay::input::pointer::AxisFrame::new(Event::time_msec(&event))
                            .source(event.source());
                    for axis in [Axis::Horizontal, Axis::Vertical] {
                        if let Some(amount) = event.amount(axis) {
                            frame = frame
                                .value(axis, amount)
                                .relative_direction(axis, event.relative_direction(axis));
                        }
                        if let Some(v120) = event.amount_v120(axis) {
                            frame = frame.v120(axis, v120 as i32);
                        }
                    }
                    pointer.axis(self, frame);
                    pointer.frame(self);
                }
                InputEvent::TouchDown { event } => self.on_touch_down::<I>(event),
                InputEvent::TouchMotion { event } => self.on_touch_motion::<I>(event),
                InputEvent::TouchUp { event } => self.on_touch_up::<I>(event),
                InputEvent::TouchCancel { event } => self.on_touch_cancel::<I>(event),
                InputEvent::TouchFrame { event } => self.on_touch_frame::<I>(event),
                _ => {}
            }
            return;
        }

        // Active pointer/gesture input on top of a held modifier chord makes it
        // a binding prefix, not a tap — cancel any pending tap binding. Motion is
        // passive (the cursor can drift mid-chord), so it's deliberately excluded.
        if matches!(
            event,
            InputEvent::PointerButton { .. }
                | InputEvent::PointerAxis { .. }
                | InputEvent::GestureSwipeBegin { .. }
                | InputEvent::GesturePinchBegin { .. }
                | InputEvent::GestureHoldBegin { .. }
                | InputEvent::TouchDown { .. }
                | InputEvent::TouchMotion { .. }
        ) {
            self.tap.taint();
        }

        match event {
            InputEvent::Keyboard { event } => self.on_keyboard::<I>(event),
            InputEvent::PointerMotion { event } => self.on_pointer_motion_relative::<I>(event),
            InputEvent::PointerMotionAbsolute { event } => {
                self.on_pointer_motion_absolute::<I>(event)
            }
            InputEvent::PointerButton { event } => self.on_pointer_button::<I>(event),
            InputEvent::PointerAxis { event } => self.on_pointer_axis::<I>(event),
            InputEvent::GestureSwipeBegin { event } => self.on_gesture_swipe_begin::<I>(event),
            InputEvent::GestureSwipeUpdate { event } => self.on_gesture_swipe_update::<I>(event),
            InputEvent::GestureSwipeEnd { event } => self.on_gesture_swipe_end::<I>(event),
            InputEvent::GesturePinchBegin { event } => self.on_gesture_pinch_begin::<I>(event),
            InputEvent::GesturePinchUpdate { event } => self.on_gesture_pinch_update::<I>(event),
            InputEvent::GesturePinchEnd { event } => self.on_gesture_pinch_end::<I>(event),
            InputEvent::GestureHoldBegin { event } => self.on_gesture_hold_begin::<I>(event),
            InputEvent::GestureHoldEnd { event } => self.on_gesture_hold_end::<I>(event),
            InputEvent::TouchDown { event } => self.on_touch_down::<I>(event),
            InputEvent::TouchMotion { event } => self.on_touch_motion::<I>(event),
            InputEvent::TouchUp { event } => self.on_touch_up::<I>(event),
            InputEvent::TouchCancel { event } => self.on_touch_cancel::<I>(event),
            InputEvent::TouchFrame { event } => self.on_touch_frame::<I>(event),
            _ => {}
        }
    }

    /// Hit-test the pointer against all surface layers in z-order. Sets
    /// `self.pointer_over_layer` as a side effect. The caller is responsible
    /// for issuing `pointer.motion()` / `pointer.relative_motion()` /
    /// `pointer.frame()` and calling `update_decoration_cursor()` so that
    /// absolute and relative motion events agree on the same target surface.
    fn pointer_focus_under(
        &mut self,
        screen_pos: Point<f64, smithay::utils::Logical>,
        canvas_pos: Point<f64, smithay::utils::Logical>,
    ) -> Option<(FocusTarget, Point<f64, smithay::utils::Logical>)> {
        // A fullscreen window occludes the Top/Bottom/Background layers on its
        // output — only Overlay renders above it (mirror compose_frame's layer
        // culling). Hit-testing the hidden layers here would route clicks to a
        // bar covered by the fullscreen window instead of the window itself.
        let output_fullscreen = self
            .active_output()
            .is_some_and(|o| self.is_output_fullscreen(&o));

        // Overlay and Top layers
        let above: &[WlrLayer] = if output_fullscreen {
            &[WlrLayer::Overlay]
        } else {
            &[WlrLayer::Overlay, WlrLayer::Top]
        };
        if let Some(hit) = self.layer_surface_under(screen_pos, canvas_pos, above) {
            self.pointer_over_layer = true;
            return Some(hit);
        }

        // Screen-pinned windows: above normal canvas windows, below Top/Overlay.
        if let Some(hit) = self.pinned_window_under(screen_pos, canvas_pos) {
            self.pointer_over_layer = false;
            return Some(hit);
        }

        // Non-widget canvas windows (visually above canvas layers)
        if let Some(hit) = self.surface_under(canvas_pos, Some(false)) {
            self.pointer_over_layer = false;
            return Some(hit);
        }

        // Canvas-positioned layer surfaces
        if let Some(hit) = self.canvas_layer_under(canvas_pos) {
            self.pointer_over_layer = false;
            return Some(hit);
        }

        // Widget canvas windows (visually below canvas layers)
        if let Some(hit) = self.surface_under(canvas_pos, Some(true)) {
            self.pointer_over_layer = false;
            return Some(hit);
        }

        // Bottom and Background layers (also occluded by a fullscreen window)
        if !output_fullscreen
            && let Some(hit) = self.layer_surface_under(
                screen_pos,
                canvas_pos,
                &[WlrLayer::Bottom, WlrLayer::Background],
            )
        {
            self.pointer_over_layer = true;
            return Some(hit);
        }

        self.pointer_over_layer = false;
        None
    }

    /// Sloppy focus: when enabled, focus the non-widget window under the pointer
    /// without raising it. Skips layers, widgets, and empty canvas.
    fn maybe_hover_focus(&mut self, canvas_pos: Point<f64, smithay::utils::Logical>) {
        if !self.config.focus_follows_mouse || self.pointer_over_layer {
            return;
        }
        // A pointer grab (popup menu, window move/resize) owns input. Letting
        // hover change focus under it would tear down a live popup grab.
        if self.seat.get_pointer().unwrap().is_grabbed() {
            return;
        }
        // On a fullscreen output the window owns focus; re-assert it rather than
        // hit-testing for a hover target. This reclaims focus that hover moved to
        // another output's window, which nothing else here would restore.
        if let Some(window) = self.active_fullscreen_window() {
            let focus_surface = window.wl_surface().map(|s| FocusTarget(s.into_owned()));
            let already_focused = focus_surface
                .as_ref()
                .is_some_and(|t| self.window_focus.as_ref().is_some_and(|f| f.0 == t.0));
            if !already_focused {
                let serial = SERIAL_COUNTER.next_serial();
                self.set_window_focus(focus_surface, serial);
            }
            return;
        }
        // Pinned windows render above the canvas and hit-test in screen space,
        // so they take focus priority — mirror the pointer-focus ordering
        // (pinned_window_under sits above the canvas in pointer_focus_under).
        let screen_pos = driftwm::canvas::canvas_to_screen(
            driftwm::canvas::CanvasPos(canvas_pos),
            self.camera(),
            self.zoom(),
        )
        .0;
        let window = match self.pinned_window_under(screen_pos, canvas_pos) {
            Some((focus, _)) => self.window_for_surface(&focus.0),
            None => self.element_under(canvas_pos).map(|(w, _)| w.clone()),
        };
        let Some(window) = window else { return };
        let is_widget = window
            .wl_surface()
            .and_then(|s| driftwm::config::applied_rule(&s))
            .is_some_and(|r| r.widget);
        if is_widget {
            return;
        }

        let focus_surface = self
            .topmost_modal_child(&window)
            .or(Some(window))
            .and_then(|w| w.wl_surface().map(|s| FocusTarget(s.into_owned())));

        // Compare against the window-focus intent, not the live keyboard focus:
        // while a layer surface owns focus the latter never matches, which would
        // re-run the focus recompute on every motion event.
        let already_focused = focus_surface
            .as_ref()
            .is_some_and(|target| self.window_focus.as_ref().is_some_and(|f| f.0 == target.0));
        if already_focused {
            return;
        }

        let serial = SERIAL_COUNTER.next_serial();
        self.set_window_focus(focus_surface, serial);
    }

    /// Deactivate the constraint on the previous focus if focus changed,
    /// then try to activate one on the new focus.
    fn update_pointer_constraint(&mut self, old_focus: Option<FocusTarget>) {
        let pointer = self.seat.get_pointer().unwrap();
        let new_focus = pointer.current_focus();
        let focus_changed = old_focus.as_ref().map(|f| &f.0) != new_focus.as_ref().map(|f| &f.0);

        if focus_changed && let Some(old) = &old_focus {
            with_pointer_constraint(&old.0, &pointer, |c| {
                if let Some(c) = c
                    && c.is_active()
                {
                    c.deactivate();
                }
            });
        }

        self.maybe_activate_pointer_constraint();
    }

    /// Activate a pointer constraint if the pointer is over the constraining surface
    /// and within the constraint region.
    pub(crate) fn maybe_activate_pointer_constraint(&self) {
        let pointer = self.seat.get_pointer().unwrap();
        let Some(focus) = pointer.current_focus() else {
            return;
        };

        with_pointer_constraint(&focus.0, &pointer, |constraint| {
            let Some(constraint) = constraint else { return };
            if constraint.is_active() {
                return;
            }

            if let Some(region) = constraint.region() {
                let pointer_canvas = pointer.current_location();
                let Some(surface_origin) = window_origin_for_surface(self, &focus.0) else {
                    return;
                };
                let local = pointer_canvas - surface_origin;
                if !region.contains(local.to_i32_round()) {
                    return;
                }
            }

            constraint.activate();
        });
    }

    /// Recompute pointer focus at the current cursor location and dispatch a
    /// synthetic motion. Call after the scene under the cursor changes without a
    /// real pointer event (e.g. the window under the cursor closes): smithay's
    /// `PointerHandle` keeps its last focus until the next `motion()`, so without
    /// this, button/axis events keep routing to the destroyed surface until the
    /// user physically moves the pointer.
    pub(crate) fn refresh_pointer_focus(&mut self) {
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            return;
        }
        let pointer = self.seat.get_pointer().unwrap();
        let canvas_pos = pointer.current_location();
        let screen_pos = driftwm::canvas::canvas_to_screen(
            driftwm::canvas::CanvasPos(canvas_pos),
            self.camera(),
            self.zoom(),
        )
        .0;
        let old_focus = pointer.current_focus();
        let under = self.pointer_focus_under(screen_pos, canvas_pos);
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.start_time.elapsed().as_millis() as u32;
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: canvas_pos,
                serial,
                time,
            },
        );
        pointer.frame(self);
        self.update_decoration_cursor(canvas_pos);
        self.update_pointer_constraint(old_focus);
    }

    fn on_pointer_motion_absolute<I: InputBackend>(
        &mut self,
        event: I::PointerMotionAbsoluteEvent,
    ) {
        // Real pointer motion restores the cursor that touch input hid.
        self.cursor.hidden_by_touch = false;
        let output = match self.active_output() {
            Some(o) => o,
            None => return,
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        // position_transformed gives screen-local coords (0..width, 0..height)
        let screen_pos = event.position_transformed(output_geo.size);
        let canvas_pos = screen_to_canvas(ScreenPos(screen_pos), self.camera(), self.zoom()).0;

        // When locked, pointer only targets the lock surface
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            let serial = SERIAL_COUNTER.next_serial();
            let time = Event::time_msec(&event);
            let pointer = self.seat.get_pointer().unwrap();
            let focus = self
                .active_output()
                .and_then(|o| self.lock_surfaces.get(&o))
                .map(|ls| {
                    (
                        FocusTarget(ls.wl_surface().clone()),
                        Point::<f64, smithay::utils::Logical>::from((0.0, 0.0)),
                    )
                });
            pointer.motion(
                self,
                focus,
                &MotionEvent {
                    location: screen_pos,
                    serial,
                    time,
                },
            );
            pointer.frame(self);
            return;
        }
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let pointer = self.seat.get_pointer().unwrap();
        let old_focus = pointer.current_focus();
        let under = self.pointer_focus_under(screen_pos, canvas_pos);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: canvas_pos,
                serial,
                time,
            },
        );
        pointer.frame(self);
        self.update_decoration_cursor(canvas_pos);
        self.update_pointer_constraint(old_focus);
        self.maybe_hover_focus(canvas_pos);
        self.refresh_cursor_edge_pan();
    }

    /// Handle relative pointer motion (libinput mice/trackpads).
    /// Multi-monitor aware: converts to layout space for output crossing,
    /// then to target output's canvas coords.
    fn on_pointer_motion_relative<I: InputBackend>(&mut self, event: I::PointerMotionEvent) {
        // Real pointer motion restores the cursor that touch input hid.
        self.cursor.hidden_by_touch = false;
        // When locked, pointer only targets the lock surface
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            let pointer = self.seat.get_pointer().unwrap();
            let old_pos = pointer.current_location();
            let delta = event.delta();
            let new_pos: Point<f64, smithay::utils::Logical> =
                (old_pos.x + delta.x, old_pos.y + delta.y).into();
            let serial = SERIAL_COUNTER.next_serial();
            let time = Event::time_msec(&event);
            let focus = self
                .active_output()
                .and_then(|o| self.lock_surfaces.get(&o))
                .map(|ls| {
                    (
                        FocusTarget(ls.wl_surface().clone()),
                        Point::<f64, smithay::utils::Logical>::from((0.0, 0.0)),
                    )
                });
            pointer.motion(
                self,
                focus,
                &MotionEvent {
                    location: new_pos,
                    serial,
                    time,
                },
            );
            pointer.frame(self);
            return;
        }

        let pointer = self.seat.get_pointer().unwrap();
        let old_canvas = pointer.current_location();
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let delta = event.delta();

        // Pointer lock: freeze position, only send relative motion
        if let Some(focus) = pointer.current_focus() {
            let locked = with_pointer_constraint(&focus.0, &pointer, |c| {
                c.is_some_and(|c| c.is_active() && matches!(&*c, PointerConstraint::Locked(_)))
            });
            if locked {
                let origin = window_origin_for_surface(self, &focus.0).unwrap_or(old_canvas);
                pointer.relative_motion(
                    self,
                    Some((focus, origin)),
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: event.delta_unaccel(),
                        utime: Event::time(&event),
                    },
                );
                pointer.frame(self);
                return;
            }
        }

        // A confined pointer (e.g. a fullscreen game in its menu/inventory) must
        // not leave its surface or region. Capture the active confine now; the
        // prevent check after the new position is computed rejects an offending
        // move rather than clamping it — clamping to a region's bounding box
        // would let the cursor slip onto another output, after which the
        // constraint can never re-establish.
        let confined: Option<(FocusTarget, Option<RegionAttributes>)> =
            pointer.current_focus().and_then(|focus| {
                let region = with_pointer_constraint(&focus.0, &pointer, |c| {
                    let c = c?;
                    if !c.is_active() {
                        return None;
                    }
                    match &*c {
                        PointerConstraint::Confined(confine) => Some(confine.region().cloned()),
                        _ => None,
                    }
                })?;
                // A confine only restricts motion while the pointer is inside its
                // region; if it's currently outside, leave this motion free so it
                // can move back in — the same gate activation uses.
                if let Some(region) = &region
                    && let Some(origin) = window_origin_for_surface(self, &focus.0)
                    && !region.contains((old_canvas - origin).to_i32_round())
                {
                    return None;
                }
                Some((focus, region))
            });

        let cur_output = match self.active_output() {
            Some(o) => o,
            None => return,
        };

        // Read current output's state
        let (cur_camera, cur_zoom, cur_layout_pos) = {
            let os = crate::state::output_state(&cur_output);
            (os.camera, os.zoom, os.layout_position)
        };

        let output_size = crate::state::output_logical_size(&cur_output);

        // Convert old canvas pos to screen pos, add layout_position → old layout pos
        let old_screen = driftwm::canvas::canvas_to_screen(
            driftwm::canvas::CanvasPos(old_canvas),
            cur_camera,
            cur_zoom,
        )
        .0;
        let old_layout: Point<f64, smithay::utils::Logical> = Point::from((
            old_screen.x + cur_layout_pos.x as f64,
            old_screen.y + cur_layout_pos.y as f64,
        ));

        // Add delta to get new layout pos (libinput deltas are logical pixels = layout space)
        let new_layout: Point<f64, smithay::utils::Logical> =
            (old_layout.x + delta.x, old_layout.y + delta.y).into();

        // Find target output at new layout pos. A confined pointer is pinned to
        // its current output — it must never cross to one whose camera views a
        // different canvas region, after which the confine could never
        // re-establish. The reject below keeps it inside its surface in this
        // output's coordinate space.
        let (target_output, screen_pos) = if confined.is_none()
            && let Some(target) = self.output_at_layout_pos(new_layout)
        {
            if target != cur_output {
                // Cross to target output
                let target_lp = crate::state::output_state(&target).layout_position;
                let target_screen: Point<f64, smithay::utils::Logical> = (
                    new_layout.x - target_lp.x as f64,
                    new_layout.y - target_lp.y as f64,
                )
                    .into();
                (target, target_screen)
            } else {
                // Same output — compute screen pos within it
                let screen: Point<f64, smithay::utils::Logical> = (
                    new_layout.x - cur_layout_pos.x as f64,
                    new_layout.y - cur_layout_pos.y as f64,
                )
                    .into();
                (cur_output.clone(), screen)
            }
        } else {
            // No output at new pos, or a confined pointer staying put →
            // clamp to the current output.
            let clamped: Point<f64, smithay::utils::Logical> = (
                (old_screen.x + delta.x).clamp(0.0, output_size.w as f64 - 1.0),
                (old_screen.y + delta.y).clamp(0.0, output_size.h as f64 - 1.0),
            )
                .into();
            (cur_output.clone(), clamped)
        };

        // Convert target-output-local screen pos to canvas via target's camera/zoom
        let (target_camera, target_zoom) = {
            let os = crate::state::output_state(&target_output);
            (os.camera, os.zoom)
        };
        let canvas_pos =
            driftwm::canvas::screen_to_canvas(ScreenPos(screen_pos), target_camera, target_zoom).0;

        let prev_focused_output = self.focused_output.clone();
        let prev_pointer_over_layer = self.pointer_over_layer;
        self.focused_output = Some(target_output);

        let old_focus = pointer.current_focus();
        // Compute the focus once and use it for both motion and relative_motion,
        // so zwp_relative_pointer clients agree with wl_pointer about the target
        // surface — otherwise relative motion lands on a window underneath a
        // layer surface while wl_pointer.motion lands on the layer.
        let under = self.pointer_focus_under(screen_pos, canvas_pos);

        // Reject a confined move that would leave the surface or its region:
        // forward only the relative delta (the app still tracks motion) and hold
        // the absolute cursor in place, so it can't cross to another output and
        // strand the constraint.
        if let Some((focus, region)) = &confined {
            let origin = window_origin_for_surface(self, &focus.0);
            let leaves_surface = under.as_ref().map(|(f, _)| &f.0) != Some(&focus.0);
            let leaves_region = match (region, origin) {
                (Some(region), Some(origin)) => {
                    !region.contains((canvas_pos - origin).to_i32_round())
                }
                // No region, or the confined surface's origin can't be located
                // (a confine not owned by a space window) — fall back to the
                // surface check rather than freeze the cursor.
                _ => false,
            };
            if leaves_surface || leaves_region {
                self.focused_output = prev_focused_output;
                self.pointer_over_layer = prev_pointer_over_layer;
                pointer.relative_motion(
                    self,
                    Some((focus.clone(), origin.unwrap_or(old_canvas))),
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: event.delta_unaccel(),
                        utime: Event::time(&event),
                    },
                );
                pointer.frame(self);
                return;
            }
        }

        pointer.motion(
            self,
            under.clone(),
            &MotionEvent {
                location: canvas_pos,
                serial,
                time,
            },
        );
        pointer.relative_motion(
            self,
            under,
            &RelativeMotionEvent {
                delta,
                delta_unaccel: event.delta_unaccel(),
                utime: Event::time(&event),
            },
        );
        pointer.frame(self);
        self.update_decoration_cursor(canvas_pos);
        self.update_pointer_constraint(old_focus);
        self.maybe_hover_focus(canvas_pos);
        self.refresh_cursor_edge_pan();
    }

    /// Cursor edge-pan: recompute the velocity from the cursor's *current*
    /// position every frame, rather than latching it on pointer-motion events.
    ///
    /// Re-evaluating from position each frame makes the pan speed stable — the
    /// same whether the cursor rests against the edge or is actively shoved into
    /// it. (A per-motion latch goes stale the instant the cursor stops, so a
    /// resting cursor would keep whatever speed the last motion event sampled,
    /// while a continuously-pushed one stays at full speed: pushing felt
    /// faster.) The speed is constant within the zone, not ramped by depth, so
    /// pushing deeper never speeds it up either — a steady glide, like a game's
    /// screen-edge scroll.
    ///
    /// Only the output the cursor is on is ever armed; every other output is
    /// disarmed, so a monitor the cursor leaves stops panning immediately
    /// instead of drifting on its own.
    pub(super) fn refresh_cursor_edge_pan(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        // During a grab (e.g. window move) the grab owns edge_pan_velocity.
        if pointer.is_grabbed() {
            return;
        }
        // A touch window-move owns edge_pan_velocity too; don't let the resting
        // (hidden) cursor's position overwrite it.
        if self.seat.get_touch().is_some_and(|t| t.is_grabbed()) {
            return;
        }
        if !self.cursor_edge_pan {
            return;
        }

        let active = self.active_output();
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        for o in &outputs {
            if active.as_ref() != Some(o) {
                crate::state::output_state(o).edge_pan_velocity = None;
            }
        }

        let Some(output) = active else {
            return;
        };
        // A fullscreen window owns the whole viewport — edge-panning the camera
        // out from under it just breaks the fullscreen surface.
        if self.is_output_fullscreen(&output) {
            crate::state::output_state(&output).edge_pan_velocity = None;
            return;
        }

        let (camera, zoom) = {
            let os = crate::state::output_state(&output);
            (os.camera, os.zoom)
        };
        let canvas_pos = pointer.current_location();
        let screen_pos =
            driftwm::canvas::canvas_to_screen(driftwm::canvas::CanvasPos(canvas_pos), camera, zoom)
                .0;

        let map = layer_map_for_output(&output);
        // Floating bars/docks may reserve no exclusive zone, so there's nothing
        // to measure against -- hit-test directly instead, or hovering the bar
        // to click it also pans and fights the click with pointer warps.
        // Only Top/Overlay: Background/Bottom usually hold a full-output
        // wallpaper surface, which would match everywhere and kill cursor-pan.
        //
        // surface_under() rather than a bare layer_under(): a bar surface often
        // spans the full width while only drawing a few clusters, so the bbox
        // alone would block panning across the transparent gaps. Checking the
        // input region makes this exactly "would a click here hit the bar?".
        let over_layer_surface = [WlrLayer::Top, WlrLayer::Overlay].iter().any(|&layer| {
            let Some(surface) = map.layer_under(layer, screen_pos) else {
                return false;
            };
            let geo = map.layer_geometry(surface).unwrap_or_default();
            let surface_local = screen_pos - geo.loc.to_f64();
            surface
                .surface_under(surface_local, WindowSurfaceType::ALL)
                .is_some()
        });
        if over_layer_surface {
            crate::state::output_state(&output).edge_pan_velocity = None;
            return;
        }
        let usable = map.non_exclusive_zone();
        let velocity = cursor_edge_pan_velocity(
            screen_pos,
            usable,
            self.config.edge_pan_cursor_zone,
            self.config.edge_pan_max,
        );
        crate::state::output_state(&output).edge_pan_velocity = velocity;
    }

    /// True when `surface`'s window is fullscreen on an output *other* than the
    /// active one. Cameras overlap on the canvas, so the active output's
    /// canvas-space hit-tests must ignore such a window — it is visible only on
    /// its own output (mirrors the render isolation in `window_render_transform`).
    fn fullscreen_on_other_output(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        active: &Option<smithay::output::Output>,
    ) -> bool {
        self.find_fullscreen_output_for_surface(surface)
            .is_some_and(|fs| active.as_ref() != Some(&fs))
    }

    /// Stage-side `Space::element_under` (bbox filter, render_location, input
    /// region), minus the windows `skip` rejects.
    fn element_under_where(
        &self,
        point: Point<f64, Logical>,
        mut skip: impl FnMut(&Window) -> bool,
    ) -> Option<(&Window, Point<i32, Logical>)> {
        self.stage
            .windows()
            .rev()
            .filter(|w| !skip(w))
            .filter(|w| {
                self.window_bbox(w)
                    .is_some_and(|bbox| bbox.to_f64().contains(point))
            })
            .find_map(|w| {
                let render_location = self.stage.position_of(w)? - w.geometry().loc;
                w.is_in_input_region(&(point - render_location.to_f64()))
                    .then_some((w, render_location))
            })
    }

    /// Isolation-aware hit-test: skips a window fullscreen on an output other
    /// than the pointer's (see `fullscreen_on_other_output`). Every canvas-space
    /// pointer path must use this, or an off-output fullscreen window leaks into
    /// focus / grab / binding-context lookups on the other monitor.
    pub(crate) fn element_under(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<(&Window, Point<i32, Logical>)> {
        let active = self.active_output();
        self.element_under_where(point, |w| {
            w.wl_surface()
                .is_some_and(|s| self.fullscreen_on_other_output(&s, &active))
        })
    }

    /// Hit-test without the off-output-fullscreen skip, for paths whose point
    /// is not anchored to the pointer's active output (touch: the finger may be
    /// on the very output the skip would key against).
    pub(crate) fn element_under_raw(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<(&Window, Point<i32, Logical>)> {
        self.element_under_where(point, |_| false)
    }

    /// Find the Wayland surface and local coordinates under the given canvas position.
    /// This is the foundation for all hit-testing — focus, gestures, resize grabs.
    /// Also checks SSD decoration areas (title bar, resize borders), interleaved
    /// with window content in z-order so a higher window's content takes priority
    /// over a lower window's decorations.
    pub fn surface_under(
        &self,
        pos: Point<f64, smithay::utils::Logical>,
        widget_filter: Option<bool>,
    ) -> Option<(FocusTarget, Point<f64, smithay::utils::Logical>)> {
        let bar_height = self.config.decorations.title_bar_height;
        let border_width = driftwm::config::DecorationConfig::RESIZE_BORDER_WIDTH;
        let active_output = self.active_output();

        for window in self.stage.windows().rev() {
            let Some(wl_surface) = window.wl_surface() else {
                continue;
            };
            // Pinned windows live in screen space — hit-tested by
            // `pinned_window_under`, never by the canvas-space path.
            if self.is_pinned(window) {
                continue;
            }
            // A window fullscreen on a different output isn't visible here; on
            // its own output the path below still hit-tests it.
            if self.fullscreen_on_other_output(&wl_surface, &active_output) {
                continue;
            }
            let rule = driftwm::config::applied_rule(&wl_surface);
            if let Some(want_widget) = widget_filter {
                let is_widget = rule.as_ref().is_some_and(|r| r.widget);
                if is_widget != want_widget {
                    continue;
                }
            }

            let Some(loc) = self.stage.position_of(window) else {
                continue;
            };

            // element_location returns the geometry origin, but surface_under
            // expects coords relative to the surface origin (which includes
            // client-side shadows/margins). The offset is geometry().loc.
            let geom_offset = window.geometry().loc;
            let surface_origin = loc - geom_offset;

            // Check window content first (higher priority than decorations)
            if let Some((surface, surface_loc)) =
                window.surface_under(pos - surface_origin.to_f64(), WindowSurfaceType::ALL)
            {
                return Some((
                    FocusTarget(surface),
                    (surface_loc + surface_origin).to_f64(),
                ));
            }

            // Then check decoration areas for this window
            let size = window.geometry().size;
            if self.decorations.contains_key(&wl_surface.id()) {
                if crate::decorations::close_button_contains(pos, loc, size.w, bar_height)
                    || crate::decorations::title_bar_contains(pos, loc, size.w, bar_height)
                    || crate::decorations::resize_edge_at(pos, loc, size, bar_height, border_width)
                        .is_some()
                {
                    return Some((FocusTarget((*wl_surface).clone()), loc.to_f64()));
                }
            } else {
                // CSD: compositor-side resize margin strictly outside the client
                // rect. Catches Zed-class clients that drop their own edge handles
                // on seeing our Tiled hint. Clients that kept their handles
                // (Brave, Nautilus) own the inside; we own the outside — no overlap.
                let is_widget = rule.as_ref().is_some_and(|r| r.widget);
                let is_fullscreen = self.is_window_fullscreen(window);
                if !is_widget
                    && !is_fullscreen
                    && crate::decorations::resize_edge_at(pos, loc, size, 0, border_width).is_some()
                {
                    return Some((FocusTarget((*wl_surface).clone()), loc.to_f64()));
                }
            }
        }
        None
    }

    /// Find the pinned window (content or SSD decoration) under a screen-space
    /// pointer position. Pinned windows render at scale 1.0 at their fixed
    /// `screen_pos`, so hit-testing is done entirely in output-relative screen
    /// coords. The returned focus location is canvas-adjusted exactly like
    /// `layer_surface_under` so smithay's `pointer_canvas − focus_loc` yields
    /// correct surface-local coordinates. Only windows on the active output are
    /// considered — the pointer is always on the active output, and `screen_pos`
    /// is relative to it.
    pub(crate) fn pinned_window_under(
        &self,
        screen_pos: Point<f64, smithay::utils::Logical>,
        canvas_pos: Point<f64, smithay::utils::Logical>,
    ) -> Option<(FocusTarget, Point<f64, smithay::utils::Logical>)> {
        if !self.stage.has_pinned() {
            return None;
        }
        let output = self.active_output()?;
        // Fullscreen covers pinned windows on that output (like the top layer).
        if self.is_output_fullscreen(&output) {
            return None;
        }
        let output_name = output.name();
        let bar_height = self.config.decorations.title_bar_height;
        let border_width = driftwm::config::DecorationConfig::RESIZE_BORDER_WIDTH;

        for window in self.stage.windows().rev() {
            let Some(wl_surface) = window.wl_surface() else {
                continue;
            };
            let Some(p) = self.stage.pin_of(window) else {
                continue;
            };
            if p.output != output_name {
                continue;
            }
            // Surface-tree (buffer) origin in output-relative screen coords.
            let surface_origin = p.screen_pos - window.geometry().loc;

            if let Some((surface, surface_loc)) =
                window.surface_under(screen_pos - surface_origin.to_f64(), WindowSurfaceType::ALL)
            {
                let screen_loc = (surface_loc + surface_origin).to_f64();
                let adjusted = screen_loc + (canvas_pos - screen_pos);
                return Some((FocusTarget(surface), adjusted));
            }

            let size = window.geometry().size;
            if self.decorations.contains_key(&wl_surface.id()) {
                if crate::decorations::close_button_contains(
                    screen_pos,
                    p.screen_pos,
                    size.w,
                    bar_height,
                ) || crate::decorations::title_bar_contains(
                    screen_pos,
                    p.screen_pos,
                    size.w,
                    bar_height,
                ) || crate::decorations::resize_edge_at(
                    screen_pos,
                    p.screen_pos,
                    size,
                    bar_height,
                    border_width,
                )
                .is_some()
                {
                    let adjusted = p.screen_pos.to_f64() + (canvas_pos - screen_pos);
                    return Some((FocusTarget((*wl_surface).clone()), adjusted));
                }
            } else {
                let is_widget =
                    driftwm::config::applied_rule(&wl_surface).is_some_and(|r| r.widget);
                if !is_widget
                    && crate::decorations::resize_edge_at(
                        screen_pos,
                        p.screen_pos,
                        size,
                        0,
                        border_width,
                    )
                    .is_some()
                {
                    let adjusted = p.screen_pos.to_f64() + (canvas_pos - screen_pos);
                    return Some((FocusTarget((*wl_surface).clone()), adjusted));
                }
            }
        }
        None
    }

    /// Screen-space SSD-decoration hit-test for pinned windows (mirror of
    /// `decoration_under`). `screen_pos` is output-relative. Used by the button
    /// dispatch and the cursor update so pinned windows' title bar / close
    /// button / resize borders behave like canvas windows'.
    pub(crate) fn pinned_decoration_under(
        &self,
        screen_pos: Point<f64, smithay::utils::Logical>,
    ) -> Option<(Window, crate::decorations::DecorationHit)> {
        use crate::decorations::DecorationHit;
        if !self.stage.has_pinned() {
            return None;
        }
        let output = self.active_output()?;
        // Fullscreen covers pinned windows on that output (like the top layer).
        if self.is_output_fullscreen(&output) {
            return None;
        }
        let output_name = output.name();
        let bar_height = self.config.decorations.title_bar_height;
        let border_width = driftwm::config::DecorationConfig::RESIZE_BORDER_WIDTH;

        for window in self.stage.windows().rev() {
            let Some(wl_surface) = window.wl_surface() else {
                continue;
            };
            let Some(p) = self.stage.pin_of(window) else {
                continue;
            };
            if p.output != output_name {
                continue;
            }
            let loc = p.screen_pos;
            let size = window.geometry().size;

            if self.decorations.contains_key(&wl_surface.id()) {
                if crate::decorations::close_button_contains(screen_pos, loc, size.w, bar_height) {
                    return Some((window.clone(), DecorationHit::CloseButton));
                }
                if crate::decorations::title_bar_contains(screen_pos, loc, size.w, bar_height) {
                    return Some((window.clone(), DecorationHit::TitleBar));
                }
                if self.config.resize_on_border
                    && let Some(edge) = crate::decorations::resize_edge_at(
                        screen_pos,
                        loc,
                        size,
                        bar_height,
                        border_width,
                    )
                {
                    return Some((window.clone(), DecorationHit::ResizeBorder(edge)));
                }
            } else {
                let is_widget =
                    driftwm::config::applied_rule(&wl_surface).is_some_and(|r| r.widget);
                if self.config.resize_on_border
                    && !is_widget
                    && let Some(edge) =
                        crate::decorations::resize_edge_at(screen_pos, loc, size, 0, border_width)
                {
                    return Some((window.clone(), DecorationHit::ResizeBorder(edge)));
                }
            }

            // Content occludes a lower window's decoration margin.
            let surface_origin = loc - window.geometry().loc;
            if window
                .surface_under(screen_pos - surface_origin.to_f64(), WindowSurfaceType::ALL)
                .is_some()
            {
                return None;
            }
        }
        None
    }

    /// Update cursor icon based on what decoration area the pointer is over.
    /// Called after pointer motion to set resize/pointer cursors for SSD areas.
    fn update_decoration_cursor(&mut self, canvas_pos: Point<f64, smithay::utils::Logical>) {
        if self.cursor.grab_cursor || self.pointer_over_layer {
            return;
        }
        // Pinned windows are screen-space; check them first (they're above
        // normal windows), then fall back to the canvas decoration hit-test.
        let screen_pos = driftwm::canvas::canvas_to_screen(
            driftwm::canvas::CanvasPos(canvas_pos),
            self.camera(),
            self.zoom(),
        )
        .0;
        let hit = self
            .pinned_decoration_under(screen_pos)
            .or_else(|| self.decoration_under(canvas_pos));
        match hit {
            Some((ref window, DecorationHit::CloseButton)) => {
                self.cursor.decoration_cursor = true;
                self.cursor.cursor_status = smithay::input::pointer::CursorImageStatus::Named(
                    smithay::input::pointer::CursorIcon::Pointer,
                );
                self.set_close_hovered(window, true);
            }
            Some((ref window, DecorationHit::ResizeBorder(edge))) => {
                self.cursor.decoration_cursor = true;
                self.cursor.cursor_status = smithay::input::pointer::CursorImageStatus::Named(
                    crate::input::pointer::resize_cursor(edge),
                );
                self.set_close_hovered(window, false);
            }
            Some((ref window, DecorationHit::TitleBar)) => {
                self.cursor.decoration_cursor = true;
                self.cursor.cursor_status =
                    smithay::input::pointer::CursorImageStatus::default_named();
                self.set_close_hovered(window, false);
            }
            None => {
                if self.cursor.decoration_cursor {
                    self.cursor.decoration_cursor = false;
                    self.cursor.cursor_status =
                        smithay::input::pointer::CursorImageStatus::default_named();
                    self.clear_all_close_hovered();
                }
            }
        }
    }

    /// Set the close button hover state for a specific window's decoration.
    fn set_close_hovered(&mut self, window: &Window, hovered: bool) {
        let Some(wl_surface) = window.wl_surface() else {
            return;
        };
        if let Some(deco) = self.decorations.get_mut(&wl_surface.id())
            && deco.close_hovered != hovered
        {
            deco.close_hovered = hovered;
            deco.title_bar = crate::decorations::render_title_bar(
                deco.width,
                deco.focused,
                hovered,
                deco.scale,
                &deco.title,
                deco.pinned,
                &self.config.decorations,
            );
        }
    }

    /// Clear close button hover on all decorations (when leaving decoration areas).
    fn clear_all_close_hovered(&mut self) {
        for deco in self.decorations.values_mut() {
            if deco.close_hovered {
                deco.close_hovered = false;
                deco.title_bar = crate::decorations::render_title_bar(
                    deco.width,
                    deco.focused,
                    false,
                    deco.scale,
                    &deco.title,
                    deco.pinned,
                    &self.config.decorations,
                );
            }
        }
    }

    /// Check if a canvas position hits a decoration area (SSD chrome, or the
    /// compositor-side CSD resize margin).
    pub fn decoration_under(
        &self,
        pos: Point<f64, smithay::utils::Logical>,
    ) -> Option<(Window, DecorationHit)> {
        let bar_height = self.config.decorations.title_bar_height;
        let border_width = driftwm::config::DecorationConfig::RESIZE_BORDER_WIDTH;
        let active = self.active_output();

        // Iterate in z-order (topmost first, matching space.elements().rev())
        for window in self.stage.windows().rev() {
            let Some(wl_surface) = window.wl_surface() else {
                continue;
            };
            // Pinned windows are screen-space; canvas-space decoration hit-test
            // doesn't apply (their SSD is handled via pinned_window_under).
            if self.is_pinned(window) {
                continue;
            }
            // An off-output fullscreen window isn't visible here — and skipping
            // it also prevents its surface from short-circuiting the loop below
            // (the occlusion `return None`) over a window beneath it on this output.
            if self.fullscreen_on_other_output(&wl_surface, &active) {
                continue;
            }
            let Some(loc) = self.stage.position_of(window) else {
                continue;
            };
            let size = window.geometry().size;

            if self.decorations.contains_key(&wl_surface.id()) {
                if crate::decorations::close_button_contains(pos, loc, size.w, bar_height) {
                    return Some((window.clone(), DecorationHit::CloseButton));
                }
                if crate::decorations::title_bar_contains(pos, loc, size.w, bar_height) {
                    return Some((window.clone(), DecorationHit::TitleBar));
                }
                if self.config.resize_on_border
                    && let Some(edge) =
                        crate::decorations::resize_edge_at(pos, loc, size, bar_height, border_width)
                {
                    return Some((window.clone(), DecorationHit::ResizeBorder(edge)));
                }
            } else {
                // CSD: only the outer resize margin (see surface_under).
                let is_widget =
                    driftwm::config::applied_rule(&wl_surface).is_some_and(|r| r.widget);
                let is_fullscreen = self.is_window_fullscreen(window);
                if self.config.resize_on_border
                    && !is_widget
                    && !is_fullscreen
                    && let Some(edge) =
                        crate::decorations::resize_edge_at(pos, loc, size, 0, border_width)
                {
                    return Some((window.clone(), DecorationHit::ResizeBorder(edge)));
                }
            }

            // If this window's client surface covers pos, stop: a higher window's
            // content occludes any lower window's decoration margin (mirrors
            // surface_under's z-order semantics so cursor and click agree).
            let surface_origin = loc - window.geometry().loc;
            if window
                .surface_under(pos - surface_origin.to_f64(), WindowSurfaceType::ALL)
                .is_some()
            {
                return None;
            }
        }
        None
    }

    /// Find a canvas-positioned layer surface under the given canvas position.
    /// These live in canvas coords (like xdg windows), so no coordinate tricks needed.
    pub(crate) fn canvas_layer_under(
        &self,
        canvas_pos: Point<f64, smithay::utils::Logical>,
    ) -> Option<(FocusTarget, Point<f64, smithay::utils::Logical>)> {
        for cl in &self.canvas_layers {
            let Some(pos) = cl.position else {
                continue;
            };
            let surface_local = canvas_pos - pos.to_f64();
            if let Some((wl_surface, sub_loc)) = cl
                .surface
                .surface_under(surface_local, WindowSurfaceType::ALL)
            {
                let loc = (sub_loc + pos).to_f64();
                return Some((FocusTarget(wl_surface), loc));
            }
        }
        None
    }

    /// Find a layer surface under the given screen-space position.
    /// Checks the given layers in order.
    ///
    /// Returns a focus target with a *canvas-adjusted* location: smithay computes
    /// surface-local coords as `pointer_pos - focus_loc`, and the pointer is always
    /// in canvas coords, so we offset the screen-space location by `canvas_pos - screen_pos`
    /// to keep the surface-local math correct.
    pub(crate) fn layer_surface_under(
        &self,
        screen_pos: Point<f64, smithay::utils::Logical>,
        canvas_pos: Point<f64, smithay::utils::Logical>,
        layers: &[WlrLayer],
    ) -> Option<(FocusTarget, Point<f64, smithay::utils::Logical>)> {
        let output = self.active_output()?;
        let output = &output;
        let map = layer_map_for_output(output);
        for &layer in layers {
            if let Some(surface) = map.layer_under(layer, screen_pos) {
                let geo = map.layer_geometry(surface).unwrap_or_default();
                let surface_local = screen_pos - geo.loc.to_f64();
                if let Some((wl_surface, sub_loc)) =
                    surface.surface_under(surface_local, WindowSurfaceType::ALL)
                {
                    let screen_loc = (sub_loc + geo.loc).to_f64();
                    // Adjust so: canvas_pos - adjusted = screen_pos - screen_loc
                    let adjusted = screen_loc + (canvas_pos - screen_pos);
                    return Some((FocusTarget(wl_surface), adjusted));
                }
            }
        }
        None
    }
}
