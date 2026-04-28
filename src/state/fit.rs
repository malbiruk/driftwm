use smithay::{
    desktop::Window,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{Logical, Point, Size},
    wayland::{compositor::with_states, seat::WaylandFocus},
};

use driftwm::config;
use driftwm::window_ext::WindowExt;
use super::{DriftWm, PendingRecenter};

/// Per-window fit state stored in the surface data_map via Mutex.
/// Some(size) = currently fit, holding the pre-fit size.
/// None = not fit.
pub struct FitState(pub Option<Size<i32, Logical>>);

/// Per-window "restore-to" size, used by fit/fullscreen to remember what to
/// resize back to. Tracked separately from `window.geometry().size` because
/// Chromium progressively shrinks its reported geometry after each
/// `Some(size)` configure (its CSD titlebar isn't suppressed by the Tiled
/// hint) — re-reading geometry on each fit would cause a spiral.
///
/// Updated on first map (client's preferred size) and at the end of a user
/// resize grab. Not touched on passive commits, so Chromium's shrunk
/// reported size never leaks in.
pub struct RestoreSize(pub Size<i32, Logical>);

/// Build a `SnapRect` from a hypothetical canvas position, size, and SSD
/// bar — used by `fit_window_snapped` / `unfit_window_snapped` to compute
/// exact per-edge deltas from the primary's pre-op and post-op geometry
/// without having to round-trip through `all_windows_with_snap_rects`.
fn snap_rect_at(loc: Point<i32, Logical>, size: Size<i32, Logical>, bar: i32) -> driftwm::snap::SnapRect {
    driftwm::snap::SnapRect {
        x_low: loc.x as f64,
        x_high: (loc.x + size.w) as f64,
        y_low: (loc.y - bar) as f64,
        y_high: (loc.y + size.h) as f64,
    }
}

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

pub fn restore_size(wl_surface: &WlSurface) -> Option<Size<i32, Logical>> {
    with_states(wl_surface, |states| {
        states
            .data_map
            .get::<std::sync::Mutex<RestoreSize>>()
            .and_then(|m| m.lock().ok())
            .map(|g| g.0)
    })
}

pub fn set_restore_size(wl_surface: &WlSurface, size: Size<i32, Logical>) {
    with_states(wl_surface, |states| {
        states
            .data_map
            .insert_if_missing_threadsafe(|| std::sync::Mutex::new(RestoreSize(size)));
        if let Some(m) = states.data_map.get::<std::sync::Mutex<RestoreSize>>()
            && let Ok(mut guard) = m.lock()
        {
            guard.0 = size;
        }
    });
}

pub fn set_restore_size_if_missing(wl_surface: &WlSurface, size: Size<i32, Logical>) {
    with_states(wl_surface, |states| {
        states
            .data_map
            .insert_if_missing_threadsafe(|| std::sync::Mutex::new(RestoreSize(size)));
    });
}

/// Fit geometry for a primary window: the canvas position, size, camera
/// target, and visual center the primary would have if fitted to the
/// viewport right now. Shared between `fit_window` (which applies it) and
/// `fit_window_snapped` (which feeds the exact post-fit rect into the
/// cluster-shift helper, so per-edge deltas account for the half-pixel
/// truncation in `target_camera.* as i32`).
struct FitGeometry {
    new_loc: Point<i32, Logical>,
    target_size: Size<i32, Logical>,
    target_camera: Point<f64, Logical>,
    visual_center: Point<f64, Logical>,
}

impl DriftWm {
    fn compute_fit_geometry(&self, window: &Window) -> FitGeometry {
        let usable = self.get_usable_area();
        let gap = self.config.snap_gap;
        let bar = self.window_ssd_bar(window);
        let target_size = Size::from((
            usable.size.w - (2.0 * gap) as i32,
            usable.size.h - (2.0 * gap) as i32 - bar,
        ));
        let usable_center_x = usable.loc.x as f64 + usable.size.w as f64 / 2.0;
        let usable_center_y = usable.loc.y as f64 + usable.size.h as f64 / 2.0;
        let visual_center = self.window_visual_center(window).unwrap_or_default();
        let target_camera = Point::from((
            visual_center.x - usable_center_x,
            visual_center.y - usable_center_y,
        ));
        let new_loc = Point::from((
            target_camera.x as i32 + usable.loc.x + gap as i32,
            target_camera.y as i32 + usable.loc.y + gap as i32 + bar,
        ));
        FitGeometry { new_loc, target_size, target_camera, visual_center }
    }

    pub fn fit_window(&mut self, window: &Window) {
        let Some(wl_surface) = window.wl_surface() else { return };
        if config::applied_rule(&wl_surface).is_some_and(|r| r.widget) {
            return;
        }

        // Use the tracked RestoreSize rather than window.geometry().size —
        // for Chromium the latter shrinks on each unfit round-trip.
        let current_size = restore_size(&wl_surface).unwrap_or_else(|| window.geometry().size);

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

        let FitGeometry { new_loc, target_size, target_camera, visual_center: center } =
            self.compute_fit_geometry(window);

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

        // Resize in-place: keep visual center, compute new loc from saved size
        let center = self.window_visual_center(window).unwrap_or_default();
        let bar = self.window_ssd_bar(window);
        let total_h = saved_size.h + bar;
        let new_loc = Point::from((
            (center.x - saved_size.w as f64 / 2.0) as i32,
            (center.y - total_h as f64 / 2.0) as i32 + bar,
        ));

        // Record the current (fit-era) geometry so the commit handler can
        // tell when the client has actually processed the exit configure,
        // then re-center using the real post-unfit size.
        let pre_exit_size = window.geometry().size;

        window.exit_fit_configure(saved_size);
        self.space.map_element(window.clone(), new_loc, false);

        self.pending_recenter.insert(
            wl_surface.id(),
            PendingRecenter {
                target_center: center,
                pre_exit_size,
            },
        );
    }

    pub fn toggle_fit_window(&mut self, window: &Window) {
        if is_fit(window) {
            self.unfit_window(window);
        } else {
            self.fit_window(window);
        }
    }

    /// Translate snapped cluster members to follow the primary's resize
    /// from `old_rect` to `new_rect`. Works for arbitrary asymmetric edge
    /// movements — per-side deltas are derived directly from the rects, so
    /// off-by-one from integer truncation in fit/unfit position math can't
    /// desync a neighbor from the primary's actual edge.
    ///
    /// Two passes over the existing resize-grab cluster infrastructure:
    /// `BottomRight` shifts right+bottom neighbors by `(dx_right, dy_bottom)`,
    /// `TopLeft` shifts left+top by `(dx_left, dy_top)`.
    fn shift_cluster_around_primary(
        &mut self,
        primary: &Window,
        old_rect: driftwm::snap::SnapRect,
        new_rect: driftwm::snap::SnapRect,
    ) {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

        let dx_right = (new_rect.x_high - old_rect.x_high) as i32;
        let dx_left = (new_rect.x_low - old_rect.x_low) as i32;
        let dy_bottom = (new_rect.y_high - old_rect.y_high) as i32;
        let dy_top = (new_rect.y_low - old_rect.y_low) as i32;

        if dx_right == 0 && dx_left == 0 && dy_bottom == 0 && dy_top == 0 {
            return;
        }

        // `initial_size` here is a delta carrier for `apply_member_shifts`,
        // which uses it only to compute `width_delta = new_w - initial_size.w`.
        // Rect width/height (which include the SSD bar) are fine — bars cancel.
        let old_size = Size::from((
            (old_rect.x_high - old_rect.x_low) as i32,
            (old_rect.y_high - old_rect.y_low) as i32,
        ));
        let gap = self.config.snap_gap;

        // BR pass: right members shift by +dx_right (= width_delta),
        // bottom members shift by +dy_bottom (= height_delta).
        let mut br =
            self.cluster_snapshot_for_resize(primary, xdg_toplevel::ResizeEdge::BottomRight);
        br.apply_member_shifts(
            &mut self.space,
            primary,
            old_size,
            old_size.w + dx_right,
            old_size.h + dy_bottom,
            gap,
        );

        // TL pass: left members shift by `-width_delta`, so width_delta = -dx_left
        // (left edge moves left → dx_left is negative → width_delta positive →
        // left members shift by dx_left). Same reasoning for top.
        let mut tl =
            self.cluster_snapshot_for_resize(primary, xdg_toplevel::ResizeEdge::TopLeft);
        tl.apply_member_shifts(
            &mut self.space,
            primary,
            old_size,
            old_size.w - dx_left,
            old_size.h - dy_top,
            gap,
        );
    }

    pub fn fit_window_snapped(&mut self, window: &Window) {
        let Some(wl_surface) = window.wl_surface() else { return };
        if config::applied_rule(&wl_surface).is_some_and(|r| r.widget) {
            return;
        }
        let Some(old_loc) = self.space.element_location(window) else { return };
        let old_size = restore_size(&wl_surface).unwrap_or_else(|| window.geometry().size);
        let bar = self.window_ssd_bar(window);
        let fit = self.compute_fit_geometry(window);
        let old_rect = snap_rect_at(old_loc, old_size, bar);
        let new_rect = snap_rect_at(fit.new_loc, fit.target_size, bar);
        self.shift_cluster_around_primary(window, old_rect, new_rect);
        self.fit_window(window);
    }

    pub fn unfit_window_snapped(&mut self, window: &Window) {
        let Some(wl_surface) = window.wl_surface() else { return };
        let saved_size = with_states(&wl_surface, |states| {
            states
                .data_map
                .get::<std::sync::Mutex<FitState>>()
                .and_then(|m| m.lock().ok())
                .and_then(|g| g.0)
        });
        let Some(saved_size) = saved_size else { return };
        let Some(old_loc) = self.space.element_location(window) else { return };
        let old_size = window.geometry().size;
        let bar = self.window_ssd_bar(window);
        // Mirror unfit_window's new_loc computation so per-edge deltas match.
        let center = self.window_visual_center(window).unwrap_or_default();
        let total_h = saved_size.h + bar;
        let new_loc = Point::from((
            (center.x - saved_size.w as f64 / 2.0) as i32,
            (center.y - total_h as f64 / 2.0) as i32 + bar,
        ));
        let old_rect = snap_rect_at(old_loc, old_size, bar);
        let new_rect = snap_rect_at(new_loc, saved_size, bar);
        self.shift_cluster_around_primary(window, old_rect, new_rect);
        self.unfit_window(window);
    }

    pub fn toggle_fit_window_snapped(&mut self, window: &Window) {
        if is_fit(window) {
            self.unfit_window_snapped(window);
        } else {
            self.fit_window_snapped(window);
        }
    }

    pub fn decoration_toggle_fit(&mut self, window: &Window) {
        if self.config.decoration_fit_snapped {
            self.toggle_fit_window_snapped(window);
        } else {
            self.toggle_fit_window(window);
        }
    }

    pub fn decoration_unfit(&mut self, window: &Window) {
        if self.config.decoration_fit_snapped {
            self.unfit_window_snapped(window);
        } else {
            self.unfit_window(window);
        }
    }
}
