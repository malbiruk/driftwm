use smithay::{
    desktop::Window,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point},
};

use super::{DriftWm, FocusTarget, FullscreenState};

impl DriftWm {
    /// Enter fullscreen for the given window: lock viewport, expand window to fill screen.
    pub fn enter_fullscreen(&mut self, window: &Window) {
        // If already fullscreen (same or different window), exit first
        if self.fullscreen.is_some() {
            self.exit_fullscreen();
        }

        let viewport_size = self.get_viewport_size();
        let saved_location = self.space.element_location(window).unwrap_or_default();

        self.fullscreen = Some(FullscreenState {
            window: window.clone(),
            saved_location,
            saved_camera: self.camera,
            saved_zoom: self.zoom,
        });

        // Tell the client to go fullscreen at output size
        window.toplevel().unwrap().with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.size = Some(viewport_size);
        });
        window.toplevel().unwrap().send_configure();

        // Lock viewport: stop all animations and momentum
        self.zoom = 1.0;
        self.zoom_target = None;
        self.camera_target = None;
        self.momentum.stop();
        self.overview_return = None;
        self.home_return = None;
        // Top/Bottom layers are hidden during fullscreen — reset stale pointer state
        self.pointer_over_layer = false;

        // Snap camera to integer for pixel-perfect alignment
        let camera_i32 = self.camera.to_i32_round();
        self.camera = Point::from((camera_i32.x as f64, camera_i32.y as f64));

        // Place window at viewport origin and raise
        self.space.map_element(window.clone(), camera_i32, true);
        self.space.raise_element(window, true);
        self.enforce_below_windows();
        self.update_output_from_camera();

        // Ensure keyboard focus is on the fullscreen window
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().unwrap();
        let surface = window.toplevel().unwrap().wl_surface().clone();
        keyboard.set_focus(self, Some(FocusTarget(surface)), serial);
    }

    /// Exit fullscreen: restore window position, camera, and zoom.
    pub fn exit_fullscreen(&mut self) {
        let Some(fs) = self.fullscreen.take() else {
            return;
        };

        // Tell client to leave fullscreen
        fs.window.toplevel().unwrap().with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Fullscreen);
            state.size = None;
        });
        fs.window.toplevel().unwrap().send_configure();

        // Restore window position, camera, zoom
        self.space.map_element(fs.window, fs.saved_location, false);
        self.camera = fs.saved_camera;
        self.zoom = fs.saved_zoom;
        self.update_output_from_camera();
    }

    /// Exit fullscreen and remap the pointer to maintain its screen position
    /// under the restored camera/zoom. Returns the new canvas position.
    pub fn exit_fullscreen_remap_pointer(
        &mut self,
        canvas_pos: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        let old_camera = self.camera;
        let old_zoom = self.zoom;
        self.exit_fullscreen();
        let screen: Point<f64, Logical> = Point::from((
            (canvas_pos.x - old_camera.x) * old_zoom,
            (canvas_pos.y - old_camera.y) * old_zoom,
        ));
        let new_pos = Point::from((
            screen.x / self.zoom + self.camera.x,
            screen.y / self.zoom + self.camera.y,
        ));
        self.warp_pointer(new_pos);
        new_pos
    }
}
