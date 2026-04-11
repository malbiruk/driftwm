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
                &[WlrLayer::Overlay, WlrLayer::Top, WlrLayer::Bottom, WlrLayer::Background],
            )
        } else {
            self.override_redirect_under(canvas_pos)
                .or_else(|| self.surface_under(canvas_pos, Some(false)))
                .or_else(|| self.canvas_layer_under(canvas_pos))
                .or_else(|| self.surface_under(canvas_pos, Some(true)))
        }
    }

    /// Send a synthetic pointer motion to keep the cursor at the same screen
    /// position after a camera or zoom change.
    pub(crate) fn warp_pointer(&mut self, new_pos: Point<f64, Logical>) {
        let under = self.focus_under(new_pos);
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer().unwrap();
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
    }

    /// Apply scroll momentum each frame. Suppressed during active
    /// PanGrab to avoid interfering with grab tracking.
    pub fn apply_scroll_momentum(&mut self, dt: Duration) {
        if self.panning() {
            return;
        }
        let delta = self.with_output_state(|os| os.momentum.tick(dt));
        let Some(delta) = delta else {
            return;
        };

        self.set_camera(self.camera() + delta);
        self.update_output_from_camera();

        // Shift pointer canvas position so screen position stays fixed
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.warp_pointer(pos + delta);
    }

    /// Apply edge auto-pan each frame during a window drag near viewport edges.
    /// Synthetic pointer motion keeps cursor at the same screen position and
    /// lets the active MoveSurfaceGrab reposition the window automatically.
    pub fn apply_edge_pan(&mut self) {
        let Some(velocity) = self.edge_pan_velocity() else { return; };
        // velocity is screen-space speed; convert to canvas delta
        let zoom = self.zoom();
        let canvas_delta = Point::from((velocity.x / zoom, velocity.y / zoom));
        self.set_camera(self.camera() + canvas_delta);
        self.update_output_from_camera();

        let pos = self.seat.get_pointer().unwrap().current_location();
        self.warp_pointer(pos + canvas_delta);
    }

    /// Apply a viewport pan delta with momentum accumulation.
    /// Call this from any input path that should drift (scroll, click-drag, future gestures).
    /// Targets the active output (where the pointer is).
    pub fn drift_pan(&mut self, delta: Point<f64, Logical>) {
        let now = Instant::now();
        self.with_output_state(|os| {
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_center = None;
            os.overview_return = None;
            os.momentum.accumulate(delta, now);
            os.camera.x += delta.x;
            os.camera.y += delta.y;
        });
        self.update_output_from_camera();
        self.schedule_momentum_timer();
    }

    /// Apply a viewport pan delta on a specific output (for grabs pinned to an output).
    pub fn drift_pan_on(&mut self, delta: Point<f64, Logical>, output: &smithay::output::Output) {
        let now = Instant::now();
        {
            let mut os = super::output_state(output);
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_center = None;
            os.overview_return = None;
            os.momentum.accumulate(delta, now);
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
        let token = self.loop_handle.insert_source(
            smithay::reexports::calloop::timer::Timer::from_duration(Duration::from_millis(50)),
            |_, _, data: &mut DriftWm| {
                data.launch_momentum();
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            },
        ).ok();
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

    /// Advance the camera animation toward `camera_target`.
    /// Uses spring dynamics if enabled in config, otherwise falls back to lerp.
    pub fn apply_camera_animation(&mut self, dt: Duration) {
        let Some(target) = self.camera_target() else {
            return;
        };

        let old_camera = self.camera();
        let dt_secs = dt.as_secs_f64();

        if self.config.animations.enabled {
            let stiffness = self.config.animations.spring_stiffness;
            let damping = self.config.animations.spring_damping;

            self.with_output_state(|os| {
                let force_x = stiffness * (target.x - os.camera.x) - damping * os.camera_velocity.x;
                let force_y = stiffness * (target.y - os.camera.y) - damping * os.camera_velocity.y;

                os.camera_velocity.x += force_x * dt_secs;
                os.camera_velocity.y += force_y * dt_secs;

                os.camera.x += os.camera_velocity.x * dt_secs;
                os.camera.y += os.camera_velocity.y * dt_secs;

                // Stop condition: close enough and slow enough
                let dist_sq = (target.x - os.camera.x).powi(2) + (target.y - os.camera.y).powi(2);
                let vel_sq = os.camera_velocity.x.powi(2) + os.camera_velocity.y.powi(2);
                if dist_sq < 0.25 && vel_sq < 10.0 {
                    os.camera = target;
                    os.camera_target = None;
                    os.camera_velocity = (0.0, 0.0).into();
                }
            });
        } else {
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
        } else if let Some(show_at) = self.cursor.exec_cursor_show_at
            && now >= show_at
        {
            self.cursor.exec_cursor_show_at = None;
            self.cursor.cursor_status = CursorImageStatus::Named(
                smithay::input::pointer::CursorIcon::Wait,
            );
        }
    }

    /// Advance zoom animation toward `zoom_target`.
    /// Uses spring dynamics if enabled in config, otherwise falls back to lerp.
    pub fn apply_zoom_animation(&mut self, dt: Duration) {
        let Some(target) = self.zoom_target() else {
            return;
        };

        let old_zoom = self.zoom();
        let old_camera = self.camera();
        let dt_secs = dt.as_secs_f64();
        let factor = self.animation_factor(dt);

        if self.config.animations.enabled {
            let stiffness = self.config.animations.spring_stiffness;
            let damping = self.config.animations.spring_damping;

            let blur_dirty = self.with_output_state(|os| {
                let force = stiffness * (target - os.zoom) - damping * os.zoom_velocity;
                os.zoom_velocity += force * dt_secs;
                os.zoom += os.zoom_velocity * dt_secs;

                // Stop condition
                let dist = (target - os.zoom).abs();
                let vel = os.zoom_velocity.abs();
                if dist < 0.001 && vel < 0.1 {
                    os.zoom = target;
                    os.zoom_target = None;
                    os.zoom_velocity = 0.0;
                    true
                } else {
                    false
                }
            });
            if blur_dirty {
                self.render.blur_scene_generation += 1;
            }
        } else {
            let dz = target - old_zoom;
            if dz.abs() < 0.001 {
                self.set_zoom(target);
                self.set_zoom_target(None);
                self.render.blur_scene_generation += 1;
            } else {
                self.set_zoom(old_zoom + dz * factor);
            }
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
            self.set_camera(Point::from((
                cx - vc.x / cur_zoom,
                cy - vc.y / cur_zoom,
            )));
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

        {
            let mut os = output_state(output);
            os.camera.x += delta.x;
            os.camera.y += delta.y;
        }

        if is_active {
            let pos = self.seat.get_pointer().unwrap().current_location();
            self.warp_pointer(pos + delta);
        }
    }

    fn tick_edge_pan_on(&mut self, output: &Output, is_active: bool) {
        let canvas_delta = {
            let os = output_state(output);
            let Some(velocity) = os.edge_pan_velocity else { return };
            Point::from((velocity.x / os.zoom, velocity.y / os.zoom))
        };

        {
            let mut os = output_state(output);
            os.camera.x += canvas_delta.x;
            os.camera.y += canvas_delta.y;
        }

        if is_active {
            let pos = self.seat.get_pointer().unwrap().current_location();
            self.warp_pointer(pos + canvas_delta);
        }
    }

    fn tick_camera_animation_on(&mut self, output: &Output, is_active: bool, dt: Duration) {
        let (target, old_camera) = {
            let os = output_state(output);
            let Some(target) = os.camera_target else { return };
            (target, os.camera)
        };

        let dt_secs = dt.as_secs_f64();

        if self.config.animations.enabled {
            let stiffness = self.config.animations.spring_stiffness;
            let damping = self.config.animations.spring_damping;

            {
                let mut os = output_state(output);
                let force_x = stiffness * (target.x - os.camera.x) - damping * os.camera_velocity.x;
                let force_y = stiffness * (target.y - os.camera.y) - damping * os.camera_velocity.y;

                os.camera_velocity.x += force_x * dt_secs;
                os.camera_velocity.y += force_y * dt_secs;

                os.camera.x += os.camera_velocity.x * dt_secs;
                os.camera.y += os.camera_velocity.y * dt_secs;

                // Stop condition
                let dist_sq = (target.x - os.camera.x).powi(2) + (target.y - os.camera.y).powi(2);
                let vel_sq = os.camera_velocity.x.powi(2) + os.camera_velocity.y.powi(2);
                if dist_sq < 0.25 && vel_sq < 10.0 {
                    os.camera = target;
                    os.camera_target = None;
                    os.camera_velocity = (0.0, 0.0).into();
                }
            }
        } else {
            let factor = self.animation_factor(dt);
            let dx = target.x - old_camera.x;
            let dy = target.y - old_camera.y;

            {
                let mut os = output_state(output);
                if dx * dx + dy * dy < 0.25 {
                    os.camera = target;
                    os.camera_target = None;
                } else {
                    os.camera = Point::from((
                        old_camera.x + dx * factor,
                        old_camera.y + dy * factor,
                    ));
                }
            }
        }

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

        let dt_secs = dt.as_secs_f64();
        let factor = self.animation_factor(dt);

        if self.config.animations.enabled {
            let stiffness = self.config.animations.spring_stiffness;
            let damping = self.config.animations.spring_damping;

            let blur_dirty = {
                let mut os = output_state(output);
                let force = stiffness * (target - os.zoom) - damping * os.zoom_velocity;
                os.zoom_velocity += force * dt_secs;
                os.zoom += os.zoom_velocity * dt_secs;

                // Stop condition
                let dist = (target - os.zoom).abs();
                let vel = os.zoom_velocity.abs();
                if dist < 0.001 && vel < 0.1 {
                    os.zoom = target;
                    os.zoom_target = None;
                    os.zoom_velocity = 0.0;
                    true
                } else {
                    false
                }
            };
            if blur_dirty {
                self.render.blur_scene_generation += 1;
            }
        } else {
            let dz = target - old_zoom;
            {
                let mut os = output_state(output);
                if dz.abs() < 0.001 {
                    os.zoom = target;
                    os.zoom_target = None;
                    drop(os);
                    self.render.blur_scene_generation += 1;
                } else {
                    os.zoom = old_zoom + dz * factor;
                }
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

            {
                let mut os = output_state(output);
                let cur_zoom = os.zoom;
                os.camera = Point::from((
                    cx - vc.x / cur_zoom,
                    cy - vc.y / cur_zoom,
                ));
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
