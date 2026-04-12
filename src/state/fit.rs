use smithay::{
    desktop::Window,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Size},
    wayland::{compositor::with_states, seat::WaylandFocus},
};

use super::DriftWm;
use driftwm::config;
use driftwm::window_ext::WindowExt;

/// Per-window fit state stored in the surface data_map via Mutex.
/// Some(size) = currently fit, holding the pre-fit size.
/// None = not fit.
pub struct FitState(pub Option<Size<i32, Logical>>);

pub fn is_fit(window: &Window) -> bool {
    let Some(wl_surface) = window.wl_surface() else {
        return false;
    };
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
        let Some(wl_surface) = window.wl_surface() else {
            return;
        };
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

        // Compute fit size at zoom=1.0 — navigate_to_window will animate there
        let usable = self.get_usable_area();
        let gap = self.config.snap_gap;
        let bar = self.window_ssd_bar(window);

        let target_w = usable.size.w - (2.0 * gap) as i32;
        let target_h = usable.size.h - (2.0 * gap) as i32 - bar;
        let target_size = Size::from((target_w, target_h));

        // Camera: center the fitted window within the usable screen area
        let usable_center_x = usable.loc.x as f64 + usable.size.w as f64 / 2.0;
        let usable_center_y = usable.loc.y as f64 + usable.size.h as f64 / 2.0;
        let center = self.window_visual_center(window).unwrap_or_default();
        let target_camera = Point::from((center.x - usable_center_x, center.y - usable_center_y));

        // Window location: usable area offset + gap from the target camera edges
        let new_loc = Point::from((
            target_camera.x as i32 + usable.loc.x + gap as i32,
            target_camera.y as i32 + usable.loc.y + gap as i32 + bar,
        ));

        window.enter_fit_configure(target_size);
        self.space.map_element(window.clone(), new_loc, false);

        // Raise, focus, animate camera + zoom to 1.0
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        self.raise_and_focus(window, serial);
        self.set_overview_return(None);
        self.with_output_state(|os| {
            os.momentum.stop();
            os.zoom_animation_center = Some(center);
            os.camera_target = Some(target_camera);
            os.zoom_target = Some(1.0);
        });
    }

    pub fn unfit_window(&mut self, window: &Window) {
        let Some(wl_surface) = window.wl_surface() else {
            return;
        };

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

        // Resize in-place: keep visual center, compute new loc from saved size
        let center = self.window_visual_center(window).unwrap_or_default();
        let bar = self.window_ssd_bar(window);
        let total_h = saved_size.h + bar;
        let new_loc = Point::from((
            (center.x - saved_size.w as f64 / 2.0) as i32,
            (center.y - total_h as f64 / 2.0) as i32 + bar,
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
