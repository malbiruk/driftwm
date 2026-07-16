use std::time::{Duration, Instant};

use smithay::input::pointer::CursorImageStatus;
use smithay::utils::{Logical, Point};

use driftwm::canvas::{self, CanvasPos};
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use smithay::output::Output;

use super::{DriftWm, FocusTarget, output_state};

impl DriftWm {
    /// Frame-rate independent lerp factor for smooth animations.
    /// Returns how much of the remaining distance to cover this frame.
    fn animation_factor(&self, dt: Duration) -> f64 {
        let base = self.config.animation_speed;
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
            self.surface_under(canvas_pos, Some(false))
                .or_else(|| self.canvas_layer_under(canvas_pos))
                .or_else(|| self.surface_under(canvas_pos, Some(true)))
        }
    }

    /// Whether the focused surface holds an active pointer constraint. Motion
    /// to a locked surface reads as a phantom absolute move (snap-back).
    fn pointer_constraint_active(&self) -> bool {
        let pointer = self.seat.get_pointer().unwrap();
        pointer.current_focus().is_some_and(|focus| {
            smithay::wayland::pointer_constraints::with_pointer_constraint(
                &focus.0,
                &pointer,
                |c| c.is_some_and(|c| c.is_active()),
            )
        })
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

        // Shift pointer canvas position so screen position stays fixed
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
    /// lets the active MoveSurfaceGrab reposition the window automatically.
    pub fn apply_edge_pan(&mut self) {
        let Some(velocity) = self.edge_pan_velocity() else {
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
    pub fn drift_pan(&mut self, delta: Point<f64, Logical>, time_ms: u32) {
        self.with_output_state(|os| {
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_center = None;
            os.overview_return = None;
            os.momentum.accumulate(delta, time_ms);
            os.camera.x += delta.x;
            os.camera.y += delta.y;
        });
        self.update_output_from_camera();
        self.schedule_momentum_timer();
    }

    /// Apply a viewport pan delta on a specific output (for grabs pinned to an output).
    /// `time_ms` is the libinput event timestamp (see [`canvas::VelocityTracker`]).
    pub fn drift_pan_on(
        &mut self,
        delta: Point<f64, Logical>,
        time_ms: u32,
        output: &smithay::output::Output,
    ) {
        {
            let mut os = super::output_state(output);
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_center = None;
            os.overview_return = None;
            os.momentum.accumulate(delta, time_ms);
            os.camera.x += delta.x;
            os.camera.y += delta.y;
        }
        self.update_output_from_camera();
        self.schedule_momentum_timer();
    }

    /// Schedule a 50ms one-shot timer that auto-launches momentum.
    /// Covers touchpads that don't send AxisStop on finger lift.
    /// Each call resets the timer — only the last one fires.
    fn schedule_momentum_timer(&mut self) {
        if let Some(token) = self.momentum_timer.take() {
            self.loop_handle.remove(token);
        }
        let token = self
            .loop_handle
            .insert_source(
                smithay::reexports::calloop::timer::Timer::from_duration(Duration::from_millis(50)),
                |_, _, data: &mut DriftWm| {
                    data.launch_momentum();
                    smithay::reexports::calloop::timer::TimeoutAction::Drop
                },
            )
            .ok();
        self.momentum_timer = token;
    }

    fn cancel_momentum_timer(&mut self) {
        if let Some(token) = self.momentum_timer.take() {
            self.loop_handle.remove(token);
        }
    }

    /// Launch momentum on the active output — called when input ends (finger lift, gesture end).
    pub fn launch_momentum(&mut self) {
        self.cancel_momentum_timer();
        self.with_output_state(|os| os.momentum.launch());
    }

    /// Launch momentum on a specific output.
    pub fn launch_momentum_on(&mut self, output: &smithay::output::Output) {
        self.cancel_momentum_timer();
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
    /// When `zoom_animation_center` is set (combined zoom+camera animation), lerps
    /// the on-screen center directly and derives camera, preventing lateral drift.
    /// Otherwise just adjusts pointer so cursor stays at the same screen position.
    pub fn apply_zoom_animation(&mut self, dt: Duration) {
        let Some(target) = self.zoom_target() else {
            return;
        };

        let old_zoom = self.zoom();
        let old_camera = self.camera();

        let factor = self.animation_factor(dt);

        let dz = target - old_zoom;
        if dz.abs() < 0.001 {
            self.set_zoom(target);
            self.set_zoom_target(None);
        } else {
            self.set_zoom(old_zoom + dz * factor);
        }

        if let Some(target_center) = self.zoom_animation_center() {
            // Combined zoom+camera: lerp the on-screen center, derive camera
            let vc = self.usable_center_screen();
            let current_center: Point<f64, Logical> = Point::from((
                old_camera.x + vc.x / old_zoom,
                old_camera.y + vc.y / old_zoom,
            ));
            let cx = current_center.x + (target_center.x - current_center.x) * factor;
            let cy = current_center.y + (target_center.y - current_center.y) * factor;

            let cur_zoom = self.zoom();
            self.set_camera(Point::from((cx - vc.x / cur_zoom, cy - vc.y / cur_zoom)));
            self.update_output_from_camera();

            // Suppress camera_animation — we set camera directly
            self.set_camera_target(None);

            if self.zoom_target().is_none() {
                // Zoom snapped — hand off final convergence to camera_animation
                let cur_zoom = self.zoom();
                let final_camera = Point::from((
                    target_center.x - vc.x / cur_zoom,
                    target_center.y - vc.y / cur_zoom,
                ));
                self.set_zoom_animation_center(None);
                self.set_camera_target(Some(final_camera));
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
            // A fullscreen output's camera is locked (set_camera_on refuses to
            // move it). Drop any pending pan/zoom target so it can't fire the
            // moment fullscreen exits; the ticks then no-op on the None targets.
            if self.is_output_fullscreen(output) {
                let mut os = output_state(output);
                os.camera_target = None;
                os.zoom_target = None;
                os.zoom_animation_center = None;
            }
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

        if is_active {
            let pos = self.seat.get_pointer().unwrap().current_location();
            self.warp_pointer(pos + delta);
        }
    }

    fn tick_edge_pan_on(&mut self, output: &Output, is_active: bool) {
        let canvas_delta = {
            let os = output_state(output);
            let Some(velocity) = os.edge_pan_velocity else {
                return;
            };
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
        let (target, old_zoom, old_camera, anim_center) = {
            let os = output_state(output);
            let Some(target) = os.zoom_target else { return };
            (target, os.zoom, os.camera, os.zoom_animation_center)
        };

        let factor = self.animation_factor(dt);

        let dz = target - old_zoom;
        {
            let mut os = output_state(output);
            if dz.abs() < 0.001 {
                os.zoom = target;
                os.zoom_target = None;
                drop(os);
            } else {
                os.zoom = old_zoom + dz * factor;
            }
        }

        if let Some(target_center) = anim_center {
            let vc = super::usable_center_for_output(output);

            let current_center: Point<f64, Logical> = Point::from((
                old_camera.x + vc.x / old_zoom,
                old_camera.y + vc.y / old_zoom,
            ));
            let cx = current_center.x + (target_center.x - current_center.x) * factor;
            let cy = current_center.y + (target_center.y - current_center.y) * factor;

            let cur_zoom = output_state(output).zoom;
            self.set_camera_on(
                output,
                Point::from((cx - vc.x / cur_zoom, cy - vc.y / cur_zoom)),
            );
            {
                let mut os = output_state(output);
                // Suppress camera_animation — we set camera directly
                os.camera_target = None;

                if os.zoom_target.is_none() {
                    // Zoom snapped — hand off final convergence to camera_animation
                    let final_camera = Point::from((
                        target_center.x - vc.x / cur_zoom,
                        target_center.y - vc.y / cur_zoom,
                    ));
                    os.zoom_animation_center = None;
                    os.camera_target = Some(final_camera);
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
