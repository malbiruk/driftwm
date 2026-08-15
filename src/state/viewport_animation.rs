//! Per-output camera motion: scroll momentum, edge pan, and the camera and
//! zoom animations, plus the [`DriftWm::tick_all_animations`] turn that drives
//! them from the event loop.
//!
//! Carries a second, unrelated subject that belongs in `src/input/`:
//! `apply_key_repeat`, the pointer group (`focus_under`,
//! `pointer_constraint_active`, `pointer_constraint_locked`, `locked_to`,
//! `cursor_over_surface`, `warp_pointer`, `flush_pointer_resync`), and
//! `check_exec_cursor_timeout`. They are here because they were already in
//! `animation.rs` when its window half was split off, and are pending
//! relocation — the module name does not describe them.

use std::time::{Duration, Instant};

use smithay::input::pointer::CursorImageStatus;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::utils::{Logical, Point};

use driftwm::canvas::{self, CanvasPos};
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use super::{DriftWm, FocusTarget, output_state};

/// How long after the last pan event momentum auto-launches, covering touchpads
/// that don't send AxisStop on finger lift.
const MOMENTUM_LAUNCH_DELAY: Duration = Duration::from_millis(50);

impl DriftWm {
    /// Stop `output`'s camera flight where it stands: both targets, the zoom
    /// anchor they lerp around, and any momentum still feeding them.
    /// `overview_return` deliberately survives — it is a place to go back to,
    /// not a motion.
    pub(crate) fn cancel_animations_on(&mut self, output: &Output) {
        {
            let mut os = output_state(output);
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_anchor = None;
            os.momentum.stop();
        }
        self.disarm_momentum_launch_on(&output.name());
    }

    /// Drop a pending momentum auto-launch, but only the one stored for
    /// `output_name`. The deadline is per-output, so an unconditional clear
    /// would let a cancel or a finger lift on one screen swallow a launch still
    /// pending on another.
    fn disarm_momentum_launch_on(&mut self, output_name: &str) {
        if self
            .momentum_deadline
            .as_ref()
            .is_some_and(|(_, name)| name == output_name)
        {
            self.momentum_deadline = None;
        }
    }

    /// [`Self::cancel_animations_on`] for every output. What a grab install
    /// needs: the cancel runs once, but `focused_output` keeps moving after it,
    /// so a flight left running on another output becomes the active one mid-grab.
    pub(crate) fn cancel_animations_everywhere(&mut self) {
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            self.cancel_animations_on(&output);
        }
    }

    /// Frame-rate independent lerp factor for smooth animations.
    /// Returns how much of the remaining distance to cover this frame.
    fn animation_factor(&self, dt: Duration) -> f64 {
        let base = self.config.camera_speed;
        let dt_secs = dt.as_secs_f64();
        1.0 - (1.0 - base).powf(dt_secs * 60.0)
    }

    /// Fire held compositor action if repeat delay/rate has elapsed.
    pub fn apply_key_repeat(&mut self) {
        let Some((_, ref action, next_fire)) = self.held_action else {
            return;
        };
        let now = std::time::Instant::now();
        if now < next_fire {
            return;
        }
        let action = action.clone();
        let rate_interval = Duration::from_millis(1000 / self.config.repeat_rate.max(1) as u64);
        self.held_action.as_mut().unwrap().2 = now + rate_interval;
        self.execute_action(&action);
    }

    /// Compute focus target at the given canvas position, respecting whether
    /// the pointer is currently over a layer surface or a canvas window.
    fn focus_under(
        &self,
        canvas_pos: Point<f64, Logical>,
    ) -> Option<(FocusTarget, Point<f64, Logical>)> {
        if self.pointer_over_layer {
            let screen_pos =
                canvas::canvas_to_screen(CanvasPos(canvas_pos), self.camera(), self.zoom()).0;
            self.layer_surface_under(
                screen_pos,
                canvas_pos,
                &[
                    WlrLayer::Overlay,
                    WlrLayer::Top,
                    WlrLayer::Bottom,
                    WlrLayer::Background,
                ],
            )
        } else {
            // A resync landing on a stand-in must yield no focus, matching
            // pointer_focus_under — otherwise the hidden client gets a stray enter.
            if self.suspended_occludes(canvas_pos) {
                return None;
            }
            let window_hit = self.surface_under(canvas_pos, Some(false));
            // Pick mode: a canvas window under the pointer holds no pointer
            // focus, mirroring focus_cascade's pick guard, so every per-frame
            // resync agrees and can't hand the client its enter back. Widgets /
            // canvas layers / Bottom layers keep focus.
            if window_hit.is_some() && self.pick_mode() {
                return None;
            }
            window_hit
                .or_else(|| self.canvas_layer_under(canvas_pos))
                .or_else(|| self.surface_under(canvas_pos, Some(true)))
        }
    }

    /// Whether the focused surface holds an active pointer constraint. Motion
    /// to a locked surface reads as a phantom absolute move (snap-back).
    pub(crate) fn pointer_constraint_active(&self) -> bool {
        let pointer = self.seat.get_pointer().unwrap();
        pointer.current_focus().is_some_and(|focus| {
            smithay::wayland::pointer_constraints::with_pointer_constraint(
                &focus.0,
                &pointer,
                |c| c.is_some_and(|c| c.is_active()),
            )
        })
    }

    /// Whether `surface` is the focused surface and holds an active lock.
    pub(crate) fn locked_to(&self, surface: &WlSurface) -> bool {
        let pointer = self.seat.get_pointer().unwrap();
        pointer
            .current_focus()
            .is_some_and(|focus| focus.0 == *surface)
            && self.pointer_constraint_locked()
    }

    /// Whether the focused surface holds an active *lock*, as opposed to a
    /// confine. Only a lock freezes the cursor, so only a lock makes an
    /// absolute motion a position the client never moved to.
    pub(crate) fn pointer_constraint_locked(&self) -> bool {
        use smithay::wayland::pointer_constraints::PointerConstraint;
        let pointer = self.seat.get_pointer().unwrap();
        pointer.current_focus().is_some_and(|focus| {
            smithay::wayland::pointer_constraints::with_pointer_constraint(
                &focus.0,
                &pointer,
                |c| c.is_some_and(|c| c.is_active() && matches!(&*c, PointerConstraint::Locked(_))),
            )
        })
    }

    /// Whether the cursor currently sits over `surface`. Pointer focus alone
    /// can't answer that: [`Self::warp_pointer`] deactivates a constraint
    /// without re-seating focus (that waits for the next frame), so between the
    /// two the focused surface is one the cursor has already left.
    pub(crate) fn cursor_over_surface(&self, surface: &WlSurface) -> bool {
        let pointer = self.seat.get_pointer().unwrap();
        self.focus_under(pointer.current_location())
            .is_some_and(|(under, _)| under.0 == *surface)
    }

    /// Keep the cursor at the same screen position after a camera or zoom
    /// change. When a constraint is active, silently update the internal
    /// location (see [`Self::pointer_constraint_active`]).
    ///
    /// A pointer grab (window move/resize, edge-pan) drives its repositioning
    /// off this motion and needs every event, so send synchronously. Otherwise
    /// the cursor is free over a sliding canvas: update the internal location
    /// now (hit-testing stays correct) but defer the client-facing motion to
    /// [`Self::flush_pointer_resync`], coalescing to one motion per frame.
    pub(crate) fn warp_pointer(&mut self, new_pos: Point<f64, Logical>) {
        // `new_pos` is canvas-space, but a locked session keeps screen coords in
        // `current_location` (the invariant `SessionLockHandler::lock` sets up),
        // so writing one here would strand the pointer at unlock. An output
        // hotplug is the reachable caller — the input and animation ones are all
        // gated already — and it has nothing to hold still: the lock surface is
        // screen-fixed, which is what a warp exists to preserve.
        if self.session_lock.is_locked() {
            return;
        }
        let pointer = self.seat.get_pointer().unwrap();

        if self.pointer_constraint_active() {
            // A camera warp can slide another surface under a screen-fixed
            // cursor, stranding input on a stale lock. Reactivates itself once
            // the cursor returns.
            let same_surface_under_cursor = pointer.current_focus().is_some_and(|current| {
                self.focus_under(new_pos)
                    .is_some_and(|(under, _)| under == current)
            });
            if same_surface_under_cursor {
                pointer.set_location(new_pos);
                return;
            }
            if let Some(focus) = pointer.current_focus() {
                smithay::wayland::pointer_constraints::with_pointer_constraint(
                    &focus.0,
                    &pointer,
                    |c| {
                        if let Some(c) = c
                            && c.is_active()
                        {
                            c.deactivate();
                        }
                    },
                );
            }
        }

        if pointer.is_grabbed() {
            let under = self.focus_under(new_pos);
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            pointer.motion(
                self,
                under,
                &smithay::input::pointer::MotionEvent {
                    location: new_pos,
                    serial,
                    time: self.start_time.elapsed().as_millis() as u32,
                },
            );
            pointer.frame(self);
            return;
        }

        pointer.set_location(new_pos);
        self.pending_pointer_resync = true;
    }

    /// Flush a pointer resync deferred by [`Self::warp_pointer`]. Sends a single
    /// `wl_pointer.motion` to the surface under the (already-updated) cursor,
    /// refreshing focus/hover and enter/leave. Called once per rendered frame.
    pub(crate) fn flush_pointer_resync(&mut self) {
        if !std::mem::take(&mut self.pending_pointer_resync) {
            return;
        }
        // `focus_under` is lock-unaware, so a flush while locked would re-target
        // pointer focus at the app behind the lock surface. Swallowed after the
        // take rather than deferred past it: a flag left standing keeps udev's
        // render loop out of its idle path for the whole lock, and `unlock`
        // re-seats pointer focus anyway.
        if self.session_lock.is_locked() {
            return;
        }
        // A constraint may have activated since the deferred warp.
        if self.pointer_constraint_active() {
            return;
        }
        let pointer = self.seat.get_pointer().unwrap();
        let pos = pointer.current_location();
        let under = self.focus_under(pos);
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location: pos,
                serial,
                time: self.start_time.elapsed().as_millis() as u32,
            },
        );
        pointer.frame(self);
        // Pick-mode transitions are zoom-driven, so the pick affordance won't
        // refresh on the pinch into/out of pick mode or the zoom-to-1.0
        // animation after a pick — this per-frame resync is the only pointer
        // path on every zoom writer. Gate on decoration_cursor too, not
        // pick_mode() alone: the frame that steps above the threshold must still
        // run once to clear a latched affordance, and it already reads
        // pick_mode() == false. The second disjunct is a bare bool (no hit-test)
        // and self-clears once the clear arm sets decoration_cursor = false.
        if self.pick_mode() || self.cursor.decoration_cursor {
            self.update_decoration_cursor(pos);
        }
    }

    /// Apply scroll momentum each frame. Suppressed during active
    /// PanGrab to avoid interfering with grab tracking.
    pub fn apply_scroll_momentum(&mut self, dt: Duration) {
        if self.panning() {
            return;
        }
        let delta = self.with_output_state(|os| os.momentum.tick(dt)).flatten();
        let Some(delta) = delta else {
            return;
        };

        self.set_camera(self.camera() + delta);
        self.update_output_from_camera();

        // Warps by the requested delta, not the applied one, which is only safe
        // because `set_camera` cannot refuse here: `enter_fullscreen` stops the
        // momentum and clears its tracker, and `drift_pan_on` — the sole
        // `accumulate` caller — refuses to refill it while fullscreen, so a
        // fullscreen output can never have a coasting momentum to tick.
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.warp_pointer(pos + delta);
    }

    /// During a touch window-move that has reached a screen edge, re-drive the
    /// move grab from the finger's fixed screen position after the camera has
    /// edge-panned, so the window keeps following the finger. Returns true if a
    /// touch move consumed the edge-pan for `output`.
    fn redrive_touch_edge_pan(&mut self, output: &Output) -> bool {
        let Some(tep) = self.touch_state.edge_pan.clone() else {
            return false;
        };
        if &tep.output != output {
            return false;
        }
        let (camera, zoom) = {
            let os = output_state(output);
            (os.camera, os.zoom)
        };
        let location = canvas::screen_to_canvas(canvas::ScreenPos(tep.screen_pos), camera, zoom).0;
        let Some(touch) = self.seat.get_touch() else {
            return false;
        };
        let time = self.start_time.elapsed().as_millis() as u32;
        touch.motion(
            self,
            None,
            &smithay::input::touch::MotionEvent {
                slot: tep.slot,
                location,
                time,
            },
        );
        touch.frame(self);
        true
    }

    /// Apply edge auto-pan each frame during a window drag near viewport edges.
    /// Synthetic pointer motion keeps cursor at the same screen position and
    /// lets the active MoveGrab reposition the window automatically.
    pub fn apply_edge_pan(&mut self) {
        let Some(output) = self.active_output() else {
            return;
        };
        let Some(velocity) = self.effective_edge_pan_velocity(&output, Instant::now()) else {
            return;
        };
        // velocity is screen-space speed; convert to canvas delta
        let zoom = self.zoom();
        let canvas_delta = Point::from((velocity.x / zoom, velocity.y / zoom));
        self.set_camera(self.camera() + canvas_delta);
        self.update_output_from_camera();

        // Touch move: re-drive the grab instead of warping the (hidden) pointer.
        if let Some(output) = self.focused_output.clone()
            && self.redrive_touch_edge_pan(&output)
        {
            return;
        }

        let pos = self.seat.get_pointer().unwrap().current_location();
        self.warp_pointer(pos + canvas_delta);
    }

    /// Apply a viewport pan delta with momentum accumulation.
    /// Call this from any input path that should drift (scroll, click-drag, future gestures).
    /// Targets the active output (where the pointer is).
    /// `time_ms` is the libinput event timestamp (see [`canvas::VelocityTracker`]).
    ///
    /// Returns the delta the camera actually took, as [`Self::drift_pan_on`] does
    /// — zero when there is no output to pan.
    pub fn drift_pan(&mut self, delta: Point<f64, Logical>, time_ms: u32) -> Point<f64, Logical> {
        match self.active_output() {
            Some(output) => self.drift_pan_on(delta, time_ms, &output),
            None => Point::from((0.0, 0.0)),
        }
    }

    /// Apply a viewport pan delta on a specific output (for grabs pinned to an output).
    /// `time_ms` is the libinput event timestamp (see [`canvas::VelocityTracker`]).
    ///
    /// Returns the delta the camera actually took — zero on a fullscreen
    /// output, which refuses the pan. Callers that warp the pointer to hold
    /// the cursor against the camera must compensate by the returned delta,
    /// not the requested one, or the cursor drifts off the parked window.
    pub fn drift_pan_on(
        &mut self,
        delta: Point<f64, Logical>,
        time_ms: u32,
        output: &smithay::output::Output,
    ) -> Point<f64, Logical> {
        // Bypasses `set_camera_on`'s guard (this writes output_state directly),
        // so it needs its own: a pan in flight when fullscreen lands goes inert
        // rather than race that lock. `enter_fullscreen` already cleared the
        // velocity tracker, so release banks no fling either.
        if self.is_output_fullscreen(output) {
            return Point::from((0.0, 0.0));
        }
        {
            let mut os = super::output_state(output);
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_anchor = None;
            os.overview_return = None;
            os.momentum.accumulate(delta, time_ms);
            os.camera.x += delta.x;
            os.camera.y += delta.y;
        }
        self.update_output_from_camera();
        self.schedule_momentum_timer(output);
        delta
    }

    /// Push the momentum auto-launch out to [`MOMENTUM_LAUNCH_DELAY`] from now,
    /// arming the timer that serves it if the burst hasn't already.
    ///
    /// Pan events arrive at touchpad rates, so re-registering the timer per
    /// event would pay a source-list scan and a timer-wheel rebuild each time.
    /// Instead one timer per burst reschedules itself onto whatever deadline it
    /// finds, and drops once that deadline is met or taken away — an idle
    /// compositor keeps no armed timer waking the loop.
    fn schedule_momentum_timer(&mut self, output: &Output) {
        self.momentum_deadline = Some((Instant::now() + MOMENTUM_LAUNCH_DELAY, output.name()));
        if self.momentum_timer.is_some() {
            return;
        }
        self.momentum_timer = self
            .loop_handle
            .insert_source(
                Timer::from_duration(MOMENTUM_LAUNCH_DELAY),
                |_, _, data: &mut DriftWm| {
                    let Some((deadline, name)) = data.momentum_deadline.clone() else {
                        // Clearing before the drop is what lets the next pan
                        // re-arm; leaving the token set wedges `is_some()` true
                        // against a source the loop has already unregistered.
                        data.momentum_timer = None;
                        return TimeoutAction::Drop;
                    };
                    if Instant::now() < deadline {
                        return TimeoutAction::ToInstant(deadline);
                    }
                    data.momentum_timer = None;
                    // Resolved by name at fire time, so a burst on a
                    // non-active output launches there, and one whose output
                    // has since disconnected launches nowhere.
                    match data.output_by_name(&name) {
                        Some(output) => data.launch_momentum_on(&output),
                        None => data.momentum_deadline = None,
                    }
                    TimeoutAction::Drop
                },
            )
            .ok();
    }

    /// Launch momentum on the active output — called when input ends (finger lift, gesture end).
    pub fn launch_momentum(&mut self) {
        if let Some(output) = self.active_output() {
            self.launch_momentum_on(&output);
        }
    }

    /// Launch momentum on a specific output.
    pub fn launch_momentum_on(&mut self, output: &smithay::output::Output) {
        self.disarm_momentum_launch_on(&output.name());
        super::output_state(output).momentum.launch();
    }

    /// Advance the camera animation toward `camera_target` using frame-rate independent lerp.
    /// Shifts the pointer by the camera delta so the cursor stays at the same screen position.
    pub fn apply_camera_animation(&mut self, dt: Duration) {
        let Some(target) = self.camera_target() else {
            return;
        };

        let old_camera = self.camera();

        let factor = self.animation_factor(dt);

        let dx = target.x - old_camera.x;
        let dy = target.y - old_camera.y;

        if dx * dx + dy * dy < 0.25 {
            self.set_camera(target);
            self.set_camera_target(None);
        } else {
            self.set_camera(Point::from((
                old_camera.x + dx * factor,
                old_camera.y + dy * factor,
            )));
        }

        self.update_output_from_camera();

        let delta = self.camera() - old_camera;
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.warp_pointer(pos + delta);
    }

    /// Manage the loading cursor: activate after grace period, clear after deadline.
    pub fn check_exec_cursor_timeout(&mut self) {
        let Some(deadline) = self.cursor.exec_cursor_deadline else {
            return;
        };
        let now = Instant::now();
        if now >= deadline {
            self.cursor.exec_cursor_show_at = None;
            self.cursor.exec_cursor_deadline = None;
            self.cursor.cursor_status = CursorImageStatus::default_named();
            // The Wait cursor was what kept the loop spinning; without a dirty mark
            // the last animated frame would stay on screen until another wake.
            self.mark_all_dirty();
        } else if let Some(show_at) = self.cursor.exec_cursor_show_at
            && now >= show_at
        {
            self.cursor.exec_cursor_show_at = None;
            self.cursor.cursor_status =
                CursorImageStatus::Named(smithay::input::pointer::CursorIcon::Wait);
        }
    }

    /// Advance zoom animation toward `zoom_target` using frame-rate independent lerp.
    /// When `zoom_animation_anchor` is set (combined zoom+camera animation), keeps
    /// its screen-space anchor stable while deriving camera, preventing drift.
    /// Otherwise just adjusts pointer so cursor stays at the same screen position.
    pub fn apply_zoom_animation(&mut self, dt: Duration) {
        let Some(target) = self.zoom_target() else {
            return;
        };

        let old_zoom = self.zoom();
        let old_camera = self.camera();

        let factor = self.animation_factor(dt);

        let dz = target - old_zoom;
        let zoom_close = dz.abs() < 0.001;
        if zoom_close {
            self.set_zoom(target);
            if self.zoom_animation_anchor().is_none() {
                self.set_zoom_target(None);
            }
        } else {
            self.set_zoom(old_zoom + dz * factor);
        }

        if let Some(anchor) = self.zoom_animation_anchor() {
            // Combined zoom+camera: lerp the canvas point at the fixed screen
            // anchor, then derive camera. The anchor can be the viewport center
            // (keyboard/fit) or the pointer position (wheel zoom).
            let current_anchor: Point<f64, Logical> = Point::from((
                old_camera.x + anchor.screen.x / old_zoom,
                old_camera.y + anchor.screen.y / old_zoom,
            ));
            let cx = current_anchor.x + (anchor.canvas.x - current_anchor.x) * factor;
            let cy = current_anchor.y + (anchor.canvas.y - current_anchor.y) * factor;

            let cur_zoom = self.zoom();
            self.set_camera(Point::from((
                cx - anchor.screen.x / cur_zoom,
                cy - anchor.screen.y / cur_zoom,
            )));
            self.update_output_from_camera();

            // Suppress camera_animation — we set camera directly
            self.set_camera_target(None);

            let center_dx = anchor.canvas.x - current_anchor.x;
            let center_dy = anchor.canvas.y - current_anchor.y;
            if zoom_close && center_dx * center_dx + center_dy * center_dy < 0.25 {
                // Finish both coordinates together to avoid a camera-only tail.
                let cur_zoom = self.zoom();
                let final_camera = Point::from((
                    anchor.canvas.x - anchor.screen.x / cur_zoom,
                    anchor.canvas.y - anchor.screen.y / cur_zoom,
                ));
                self.set_zoom_target(None);
                self.clear_zoom_animation_anchor();
                self.set_camera(final_camera);
                self.update_output_from_camera();
            }

            // Warp pointer: compensate for both camera and zoom change
            let pos = self.seat.get_pointer().unwrap().current_location();
            let screen_x = (pos.x - old_camera.x) * old_zoom;
            let screen_y = (pos.y - old_camera.y) * old_zoom;
            let cur_zoom = self.zoom();
            let cur_camera = self.camera();
            let new_pos = Point::from((
                screen_x / cur_zoom + cur_camera.x,
                screen_y / cur_zoom + cur_camera.y,
            ));
            self.warp_pointer(new_pos);
        } else if self.zoom() != old_zoom {
            // Standalone zoom: just compensate pointer for zoom change
            let pos = self.seat.get_pointer().unwrap().current_location();
            let cur_camera = self.camera();
            let screen_x = (pos.x - cur_camera.x) * old_zoom;
            let screen_y = (pos.y - cur_camera.y) * old_zoom;
            let cur_zoom = self.zoom();
            let new_pos = Point::from((
                screen_x / cur_zoom + cur_camera.x,
                screen_y / cur_zoom + cur_camera.y,
            ));
            self.warp_pointer(new_pos);
        }
    }

    /// Drop `output`'s pending camera/zoom flight while it is fullscreen. The
    /// camera write is already guarded by `set_camera_on`, but a zoom target
    /// left armed is never reached by the arrival check that would clear it,
    /// so on winit the lerp would run for the whole fullscreen session.
    ///
    /// Call position is load-bearing, not stylistic: it must precede the zoom
    /// tick. udev's `tick_zoom_animation_on` writes `os.zoom` directly rather
    /// than through the guarded setter, so a disarm sequenced after it would let
    /// the zoom leave the park and draw the parked window scaled.
    pub(crate) fn disarm_view_flight_on_fullscreen(&mut self, output: &Output) {
        if !self.is_output_fullscreen(output) {
            return;
        }
        let mut os = output_state(output);
        os.camera_target = None;
        os.zoom_target = None;
        os.zoom_animation_anchor = None;
    }

    // -- Multi-output animation ticking (udev backend) --
    // The existing apply_* methods above operate on active_output() and are used
    // by the winit backend (single output, timer-based). Winit gets away with
    // tick-in-render because it's always single-output with a fixed timer.

    /// Tick all per-output animations once per iteration.
    /// Called from udev render_if_needed() before any render_frame() calls.
    pub fn tick_all_animations(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_animation_tick).min(Duration::from_millis(33));
        self.last_animation_tick = now;

        // Global (not per-output) ticks
        self.apply_key_repeat();
        self.check_exec_cursor_timeout();
        self.tick_window_animations(dt);
        // Re-arm cursor edge-pan from the current cursor position before the
        // per-output velocities are applied below (disarms outputs the cursor
        // has left; keeps the active output's speed stable frame-to-frame).
        self.refresh_cursor_edge_pan();

        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let active = self.active_output();

        for output in &outputs {
            let is_active = active.as_ref().is_some_and(|a| a == output);

            {
                let mut os = output_state(output);
                os.last_frame_instant = now;
            }

            self.tick_scroll_momentum_on(output, is_active, dt);
            self.tick_edge_pan_on(output, is_active);
            self.disarm_view_flight_on_fullscreen(output);
            self.tick_zoom_animation_on(output, is_active, dt);
            self.tick_camera_animation_on(output, is_active, dt);
        }

        // Single camera sync after all outputs are ticked (avoids N×M redundancy)
        self.update_output_from_camera();
    }

    fn tick_scroll_momentum_on(&mut self, output: &Output, is_active: bool, dt: Duration) {
        {
            let os = output_state(output);
            if os.panning {
                return;
            }
        }

        let delta = {
            let mut os = output_state(output);
            os.momentum.tick(dt)
        };
        let Some(delta) = delta else { return };

        let cam = output_state(output).camera;
        self.set_camera_on(output, Point::from((cam.x + delta.x, cam.y + delta.y)));

        // Requested delta rather than applied, safe for the same reason as
        // `apply_scroll_momentum`: a fullscreen output's momentum is stopped at
        // entry and cannot be re-accumulated, so `tick` never yields one here.
        if is_active {
            let pos = self.seat.get_pointer().unwrap().current_location();
            self.warp_pointer(pos + delta);
        }
    }

    fn tick_edge_pan_on(&mut self, output: &Output, is_active: bool) {
        let Some(velocity) = self.effective_edge_pan_velocity(output, Instant::now()) else {
            return;
        };
        let canvas_delta = {
            let os = output_state(output);
            Point::from((velocity.x / os.zoom, velocity.y / os.zoom))
        };

        let cam = output_state(output).camera;
        self.set_camera_on(
            output,
            Point::from((cam.x + canvas_delta.x, cam.y + canvas_delta.y)),
        );

        // Touch move: re-drive the grab instead of warping the (hidden) pointer.
        if self.redrive_touch_edge_pan(output) {
            return;
        }

        if is_active {
            let pos = self.seat.get_pointer().unwrap().current_location();
            self.warp_pointer(pos + canvas_delta);
        }
    }

    fn tick_camera_animation_on(&mut self, output: &Output, is_active: bool, dt: Duration) {
        let (target, old_camera) = {
            let os = output_state(output);
            let Some(target) = os.camera_target else {
                return;
            };
            (target, os.camera)
        };

        let factor = self.animation_factor(dt);

        let dx = target.x - old_camera.x;
        let dy = target.y - old_camera.y;

        let new_camera = if dx * dx + dy * dy < 0.25 {
            output_state(output).camera_target = None;
            target
        } else {
            Point::from((old_camera.x + dx * factor, old_camera.y + dy * factor))
        };
        self.set_camera_on(output, new_camera);

        if is_active {
            let new_camera = output_state(output).camera;
            let delta = new_camera - old_camera;
            let pos = self.seat.get_pointer().unwrap().current_location();
            self.warp_pointer(pos + delta);
        }
    }

    fn tick_zoom_animation_on(&mut self, output: &Output, is_active: bool, dt: Duration) {
        let (target, old_zoom, old_camera, anim_anchor) = {
            let os = output_state(output);
            let Some(target) = os.zoom_target else { return };
            (target, os.zoom, os.camera, os.zoom_animation_anchor)
        };

        let factor = self.animation_factor(dt);

        let dz = target - old_zoom;
        let zoom_close = dz.abs() < 0.001;
        {
            let mut os = output_state(output);
            if zoom_close {
                os.zoom = target;
                if anim_anchor.is_none() {
                    os.zoom_target = None;
                }
                drop(os);
            } else {
                os.zoom = old_zoom + dz * factor;
            }
        }

        if let Some(anchor) = anim_anchor {
            let current_anchor: Point<f64, Logical> = Point::from((
                old_camera.x + anchor.screen.x / old_zoom,
                old_camera.y + anchor.screen.y / old_zoom,
            ));
            let cx = current_anchor.x + (anchor.canvas.x - current_anchor.x) * factor;
            let cy = current_anchor.y + (anchor.canvas.y - current_anchor.y) * factor;

            let cur_zoom = output_state(output).zoom;
            self.set_camera_on(
                output,
                Point::from((
                    cx - anchor.screen.x / cur_zoom,
                    cy - anchor.screen.y / cur_zoom,
                )),
            );
            {
                let mut os = output_state(output);
                // Suppress camera_animation — we set camera directly
                os.camera_target = None;

                let center_dx = anchor.canvas.x - current_anchor.x;
                let center_dy = anchor.canvas.y - current_anchor.y;
                if zoom_close && center_dx * center_dx + center_dy * center_dy < 0.25 {
                    let final_camera = Point::from((
                        anchor.canvas.x - anchor.screen.x / cur_zoom,
                        anchor.canvas.y - anchor.screen.y / cur_zoom,
                    ));
                    os.zoom_target = None;
                    os.zoom_animation_anchor = None;
                    drop(os);
                    self.set_camera_on(output, final_camera);
                }
            }

            if is_active {
                let (cur_zoom, cur_camera) = {
                    let os = output_state(output);
                    (os.zoom, os.camera)
                };
                let pos = self.seat.get_pointer().unwrap().current_location();
                let screen_x = (pos.x - old_camera.x) * old_zoom;
                let screen_y = (pos.y - old_camera.y) * old_zoom;
                let new_pos = Point::from((
                    screen_x / cur_zoom + cur_camera.x,
                    screen_y / cur_zoom + cur_camera.y,
                ));
                self.warp_pointer(new_pos);
            }
        } else {
            let cur_zoom = output_state(output).zoom;
            if cur_zoom != old_zoom && is_active {
                let cur_camera = output_state(output).camera;
                let pos = self.seat.get_pointer().unwrap().current_location();
                let screen_x = (pos.x - cur_camera.x) * old_zoom;
                let screen_y = (pos.y - cur_camera.y) * old_zoom;
                let new_pos = Point::from((
                    screen_x / cur_zoom + cur_camera.x,
                    screen_y / cur_zoom + cur_camera.y,
                ));
                self.warp_pointer(new_pos);
            }
        }
    }
}
