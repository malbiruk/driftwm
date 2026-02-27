use std::time::Duration;

use smithay::utils::{Logical, Point};

use driftwm::canvas::{self, CanvasPos};
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use crate::input::gestures::GestureState;
use super::{DriftWm, FocusTarget};

impl DriftWm {
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
                canvas::canvas_to_screen(CanvasPos(canvas_pos), self.camera, self.zoom).0;
            self.layer_surface_under(
                screen_pos,
                canvas_pos,
                &[WlrLayer::Overlay, WlrLayer::Top, WlrLayer::Bottom, WlrLayer::Background],
            )
        } else {
            self.surface_under(canvas_pos)
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

    /// Apply scroll momentum each frame. Skips frames where a scroll event
    /// already moved the camera (via frame counter). Suppressed during active
    /// PanGrab to avoid interfering with grab tracking.
    pub fn apply_scroll_momentum(&mut self) {
        if self.panning {
            return;
        }
        let Some(delta) = self.momentum.tick(self.frame_counter) else {
            return;
        };

        self.camera += delta;
        self.update_output_from_camera();

        // Shift pointer canvas position so screen position stays fixed
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.warp_pointer(pos + delta);
    }

    /// Apply edge auto-pan each frame during a window drag near viewport edges.
    /// Synthetic pointer motion keeps cursor at the same screen position and
    /// lets the active MoveSurfaceGrab reposition the window automatically.
    pub fn apply_edge_pan(&mut self) {
        let Some(velocity) = self.edge_pan_velocity else { return; };
        // velocity is screen-space speed; convert to canvas delta
        let canvas_delta = Point::from((velocity.x / self.zoom, velocity.y / self.zoom));
        self.camera += canvas_delta;
        self.update_output_from_camera();

        // During gesture move, also reposition the window and adjust initial_location
        // so the window stays under the finger as the viewport auto-pans.
        let gesture_move_window = match &self.gesture_state {
            Some(GestureState::Swipe3Move { window, .. }) => Some(window.clone()),
            _ => None,
        };
        if let Some(ref window) = gesture_move_window
            && let Some(loc) = self.space.element_location(window)
        {
            let new_loc = loc + Point::from((canvas_delta.x as i32, canvas_delta.y as i32));
            self.space.map_element(window.clone(), new_loc, false);
        }
        if gesture_move_window.is_some()
            && let Some(GestureState::Swipe3Move { initial_location, .. }) = &mut self.gesture_state
        {
            initial_location.x += canvas_delta.x;
            initial_location.y += canvas_delta.y;
        }

        // Shift pointer canvas position so screen position stays fixed
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.warp_pointer(pos + canvas_delta);
    }

    /// Apply a viewport pan delta with momentum accumulation.
    /// Call this from any input path that should drift (scroll, click-drag, future gestures).
    pub fn drift_pan(&mut self, delta: Point<f64, Logical>) {
        self.camera_target = None; // Cancel animation on manual input
        self.zoom_target = None;
        self.overview_return = None;
        self.momentum.accumulate(delta, self.frame_counter);
        self.camera += delta;
        self.update_output_from_camera();
    }

    /// Advance the camera animation toward `camera_target` using frame-rate independent lerp.
    /// Shifts the pointer by the camera delta so the cursor stays at the same screen position.
    pub fn apply_camera_animation(&mut self, dt: Duration) {
        let Some(target) = self.camera_target else {
            return;
        };

        let old_camera = self.camera;

        let base = self.config.animation_speed;
        let reference_dt = 1.0 / 60.0;
        let dt_secs = dt.as_secs_f64();
        let factor = 1.0 - (1.0 - base).powf(dt_secs / reference_dt);

        let dx = target.x - self.camera.x;
        let dy = target.y - self.camera.y;

        // Snap when sub-pixel close
        if dx * dx + dy * dy < 0.25 {
            self.camera = target;
            self.camera_target = None;
        } else {
            self.camera = Point::from((
                self.camera.x + dx * factor,
                self.camera.y + dy * factor,
            ));
        }

        self.update_output_from_camera();

        // Shift pointer so cursor stays at the same screen position
        let delta = self.camera - old_camera;
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.warp_pointer(pos + delta);
    }

    /// Advance zoom animation toward `zoom_target` using frame-rate independent lerp.
    /// Adjusts pointer canvas position so the cursor stays at the same screen position.
    pub fn apply_zoom_animation(&mut self, dt: Duration) {
        let Some(target) = self.zoom_target else {
            return;
        };

        let old_zoom = self.zoom;

        let base = self.config.animation_speed;
        let reference_dt = 1.0 / 60.0;
        let dt_secs = dt.as_secs_f64();
        let factor = 1.0 - (1.0 - base).powf(dt_secs / reference_dt);

        let dz = target - self.zoom;
        if dz.abs() < 0.001 {
            self.zoom = target;
            self.zoom_target = None;
        } else {
            self.zoom += dz * factor;
        }

        // Adjust pointer so cursor stays at the same screen position.
        // screen = (canvas - camera) * zoom  ⟹  new_canvas = screen / new_zoom + camera
        if self.zoom != old_zoom {
            let pos = self.seat.get_pointer().unwrap().current_location();
            let screen_x = (pos.x - self.camera.x) * old_zoom;
            let screen_y = (pos.y - self.camera.y) * old_zoom;
            let new_pos = Point::from((
                screen_x / self.zoom + self.camera.x,
                screen_y / self.zoom + self.camera.y,
            ));
            self.warp_pointer(new_pos);
        }
    }
}
