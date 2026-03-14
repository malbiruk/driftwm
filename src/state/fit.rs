use smithay::{
    desktop::Window,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Size},
    wayland::{compositor::with_states, seat::WaylandFocus},
};

use smithay::reexports::wayland_server::Resource;
use driftwm::config::{self, DecorationConfig};
use driftwm::window_ext::WindowExt;
use super::DriftWm;

/// Per-window fit state stored in the surface data_map via Mutex.
/// Some(size) = currently fit, holding the pre-fit size.
/// None = not fit.
pub struct FitState(pub Option<Size<i32, Logical>>);

pub fn is_fit(window: &Window) -> bool {
    let Some(wl_surface) = window.wl_surface() else { return false };
    with_states(&wl_surface, |states| {
        states
            .data_map
            .get::<std::sync::Mutex<FitState>>()
            .and_then(|m| m.lock().ok())
            .is_some_and(|guard| guard.0.is_some())
    })
}

pub fn clear_fit_state(wl_surface: &WlSurface) {
    with_states(wl_surface, |states| {
        if let Some(m) = states.data_map.get::<std::sync::Mutex<FitState>>()
            && let Ok(mut guard) = m.lock()
        {
            guard.0 = None;
        }
    });
}

impl DriftWm {
    pub fn fit_window(&mut self, window: &Window) {
        let Some(wl_surface) = window.wl_surface() else { return };
        if config::applied_rule(&wl_surface).is_some_and(|r| r.widget) {
            return;
        }

        let current_size = window.geometry().size;

        // Save current size into data_map
        with_states(&wl_surface, |states| {
            states
                .data_map
                .insert_if_missing_threadsafe(|| std::sync::Mutex::new(FitState(None)));
            if let Some(m) = states.data_map.get::<std::sync::Mutex<FitState>>()
                && let Ok(mut guard) = m.lock()
            {
                guard.0 = Some(current_size);
            }
        });

        let viewport = self.get_viewport_size();
        let zoom = self.zoom();
        let camera = self.camera();
        let gap = self.config.snap_gap;

        // SSD title bar sits above the content area — subtract from available height
        let has_ssd = self.decorations.contains_key(&wl_surface.id());
        let bar = if has_ssd { DecorationConfig::TITLE_BAR_HEIGHT } else { 0 };

        let target_w = (viewport.w as f64 / zoom - 2.0 * gap) as i32;
        let target_h = (viewport.h as f64 / zoom - 2.0 * gap) as i32 - bar;
        let target_size = Size::from((target_w, target_h));

        // Center visual whole (bar + content) so gaps to viewport edges are even
        let center_x = camera.x + viewport.w as f64 / (2.0 * zoom);
        let center_y = camera.y + viewport.h as f64 / (2.0 * zoom);
        let total_h = target_h + bar;
        let new_loc = Point::from((
            (center_x - target_w as f64 / 2.0) as i32,
            (center_y - total_h as f64 / 2.0) as i32 + bar,
        ));

        window.enter_fit_configure(target_size);
        self.space.map_element(window.clone(), new_loc, false);
    }

    pub fn unfit_window(&mut self, window: &Window) {
        let Some(wl_surface) = window.wl_surface() else { return };

        let saved_size = with_states(&wl_surface, |states| {
            let size = states
                .data_map
                .get::<std::sync::Mutex<FitState>>()
                .and_then(|m| m.lock().ok())
                .and_then(|guard| guard.0);
            // Clear fit state
            if let Some(m) = states.data_map.get::<std::sync::Mutex<FitState>>()
                && let Ok(mut guard) = m.lock()
            {
                guard.0 = None;
            }
            size
        });

        let Some(saved_size) = saved_size else { return };

        // Center content area the same way CenterWindow/navigate_to_window does
        let viewport = self.get_viewport_size();
        let zoom = self.zoom();
        let camera = self.camera();
        let center_x = camera.x + viewport.w as f64 / (2.0 * zoom);
        let center_y = camera.y + viewport.h as f64 / (2.0 * zoom);
        let new_loc = Point::from((
            (center_x - saved_size.w as f64 / 2.0) as i32,
            (center_y - saved_size.h as f64 / 2.0) as i32,
        ));

        window.exit_fit_configure(saved_size);
        self.space.map_element(window.clone(), new_loc, false);
    }

    pub fn toggle_fit_window(&mut self, window: &Window) {
        if is_fit(window) {
            self.unfit_window(window);
        } else {
            self.fit_window(window);
        }
    }
}
