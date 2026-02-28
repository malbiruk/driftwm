use smithay::{
    desktop::Window,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
};

use super::{DriftWm, FocusTarget};

impl DriftWm {
    /// Navigate the viewport to center on a window: raise, focus, animate camera.
    /// If returning from overview (ZoomToFit), also restores the saved zoom level.
    pub fn navigate_to_window(&mut self, window: &Window) {
        self.space.raise_element(window, true);
        self.enforce_below_windows();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().unwrap();
        let surface = window.toplevel().unwrap().wl_surface().clone();
        keyboard.set_focus(self, Some(FocusTarget(surface)), serial);

        // If in overview, restore saved zoom; otherwise keep current zoom
        let target_zoom = if let Some((_, saved_zoom)) = self.overview_return.take() {
            saved_zoom
        } else {
            self.zoom
        };

        let window_loc = self.space.element_location(window).unwrap_or_default();
        let window_size = window.geometry().size;
        let viewport_size = self.get_viewport_size();
        let target = driftwm::canvas::camera_to_center_window(
            window_loc, window_size, viewport_size, target_zoom,
        );

        self.momentum.stop();
        self.camera_target = Some(target);
        self.zoom_target = Some(target_zoom);
    }

    /// Dynamic minimum zoom based on the current window layout.
    /// Allows zooming out far enough to see all windows.
    pub fn min_zoom(&self) -> f64 {
        let viewport = self.get_viewport_size();
        driftwm::canvas::dynamic_min_zoom(
            self.space.elements().map(|w| {
                let loc = self.space.element_location(w).unwrap_or_default();
                let size = w.geometry().size;
                (loc, size)
            }),
            viewport,
            self.config.zoom_fit_padding,
        )
    }

    /// Update focus history with the given surface (push to front / move to front).
    /// Should NOT be called during Alt-Tab cycling (history is frozen).
    /// Skips windows with `skip_taskbar` rule.
    pub fn update_focus_history(&mut self, surface: &WlSurface) {
        if driftwm::config::applied_rule(surface).is_some_and(|r| r.widget || r.no_focus) {
            return;
        }
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel().unwrap().wl_surface() == surface)
            .cloned();
        if let Some(window) = window {
            self.focus_history.retain(|w| w != &window);
            self.focus_history.insert(0, window);
        }
    }

    /// End Alt-Tab cycling: commit the selected window to focus history.
    pub fn end_cycle(&mut self) {
        let idx = self.cycle_state.take();
        if let Some(idx) = idx
            && let Some(window) = self.focus_history.get(idx).cloned()
        {
            self.focus_history.retain(|w| w != &window);
            self.focus_history.insert(0, window);
        }
    }
}
