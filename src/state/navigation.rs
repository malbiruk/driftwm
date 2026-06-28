use crate::surface_tree::focus_belongs_to_window;
use driftwm::layout::snap::SnapRect;
use driftwm::window_ext::WindowExt;
use smithay::{
    desktop::Window,
    output::Output,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{Logical, Point},
    wayland::seat::WaylandFocus,
};

use super::{DriftWm, output_state};

fn rects_overlap(a: &SnapRect, b: &SnapRect) -> bool {
    a.x_low < b.x_high && b.x_low < a.x_high && a.y_low < b.y_high && b.y_low < a.y_high
}

impl DriftWm {
    /// Navigate the viewport to center on a window: raise, focus, animate camera.
    /// When `reset_zoom` is true, zoom animates to 1.0 (intentional navigation).
    /// Otherwise preserves current zoom, or restores saved zoom if leaving overview.
    pub fn navigate_to_window(&mut self, window: &Window, reset_zoom: bool) {
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        self.raise_and_focus(window, serial);

        let target_zoom = if reset_zoom {
            self.set_overview_return(None);
            1.0
        } else {
            let overview_ret = self.overview_return();
            self.set_overview_return(None);
            if let Some((_, saved_zoom)) = overview_ret {
                saved_zoom
            } else {
                self.zoom()
            }
        };

        let window_loc = self.space.element_location(window).unwrap_or_default();
        let window_size = window.geometry().size;
        let bar = self.window_ssd_bar(window);
        let vc = self.usable_center_screen();
        let target =
            driftwm::canvas::camera_to_center_window(window_loc, window_size, vc, target_zoom, bar);

        let window_center = self.window_visual_center(window).unwrap_or_else(|| {
            Point::from((
                window_loc.x as f64 + window_size.w as f64 / 2.0,
                window_loc.y as f64 + window_size.h as f64 / 2.0,
            ))
        });
        self.with_output_state(|os| {
            os.momentum.stop();
            os.zoom_animation_center = Some(window_center);
            os.camera_target = Some(target);
            os.zoom_target = Some(target_zoom);
        });
    }

    /// Dynamic minimum zoom based on the current window layout.
    /// Allows zooming out far enough to see all windows.
    pub fn min_zoom(&self) -> f64 {
        let viewport = self.get_usable_area().size;
        driftwm::canvas::dynamic_min_zoom(
            self.space
                .elements()
                .filter(|w| {
                    !w.wl_surface()
                        .and_then(|s| driftwm::config::applied_rule(&s))
                        .is_some_and(|r| r.widget)
                        && !self.is_pinned(w)
                        && !self.is_window_fullscreen(w)
                })
                .map(|w| {
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
        let window = self
            .space
            .elements()
            .find(|w| focus_belongs_to_window(surface, w))
            .cloned();
        if let Some(window) = window {
            // Widgets and pinned (PiP-style) windows stay out of the focus
            // cycle / alt-tab history.
            if window
                .wl_surface()
                .and_then(|s| driftwm::config::applied_rule(&s))
                .is_some_and(|r| r.widget)
                || self.is_pinned(&window)
            {
                return;
            }
            // Modal dialogs don't enter focus history — Alt-Tab navigates to
            // the parent instead, and focus redirect handles the rest.
            if window.is_modal() {
                return;
            }
            self.focus_history.retain(|w| w != &window);
            self.focus_history.insert(0, window);
        }
    }

    /// Is the window's full snap rect (borders + title bar) inside the active
    /// output's usable area at the current camera and zoom? Returns `false`
    /// for widgets and unmapped windows — they have no meaningful viewport
    /// relation, so callers treat them as "needs movement" and skip them.
    pub fn window_fully_in_viewport(&self, w: &Window) -> bool {
        let Some(rect) = self.snap_rect_for(w) else {
            return false;
        };
        let camera = self.camera();
        let zoom = self.zoom();
        let usable = self.get_usable_area();

        let screen_x_low = (rect.x_low - camera.x) * zoom;
        let screen_y_low = (rect.y_low - camera.y) * zoom;
        let screen_x_high = (rect.x_high - camera.x) * zoom;
        let screen_y_high = (rect.y_high - camera.y) * zoom;

        let u_x_low = usable.loc.x as f64;
        let u_y_low = usable.loc.y as f64;
        let u_x_high = (usable.loc.x + usable.size.w) as f64;
        let u_y_high = (usable.loc.y + usable.size.h) as f64;

        screen_x_low >= u_x_low
            && screen_y_low >= u_y_low
            && screen_x_high <= u_x_high
            && screen_y_high <= u_y_high
    }

    /// Does the window's snap rect intersect `output`'s usable area at that
    /// output's camera and zoom? Partial overlap counts as visible. Returns
    /// `false` for unmapped windows and widgets (no snap rect).
    pub fn window_intersects_viewport_on(&self, w: &Window, output: &Output) -> bool {
        let Some(rect) = self.snap_rect_for(w) else {
            return false;
        };
        let (camera, zoom) = {
            let os = output_state(output);
            (os.camera, os.zoom)
        };
        let usable = smithay::desktop::layer_map_for_output(output).non_exclusive_zone();

        let screen_x_low = (rect.x_low - camera.x) * zoom;
        let screen_y_low = (rect.y_low - camera.y) * zoom;
        let screen_x_high = (rect.x_high - camera.x) * zoom;
        let screen_y_high = (rect.y_high - camera.y) * zoom;

        let u_x_low = usable.loc.x as f64;
        let u_y_low = usable.loc.y as f64;
        let u_x_high = (usable.loc.x + usable.size.w) as f64;
        let u_y_high = (usable.loc.y + usable.size.h) as f64;

        screen_x_low < u_x_high
            && u_x_low < screen_x_high
            && screen_y_low < u_y_high
            && u_y_low < screen_y_high
    }

    /// Nearest window (by canvas distance from `from_center`) that is at least
    /// partially visible on `output`. Excludes `exclude` and widgets.
    pub fn nearest_visible_window_on(
        &self,
        from_center: Point<f64, Logical>,
        output: &Output,
        exclude: &Window,
    ) -> Option<Window> {
        self.space
            .elements()
            .filter(|w| {
                *w != exclude
                    && !w
                        .wl_surface()
                        .and_then(|s| driftwm::config::applied_rule(&s))
                        .is_some_and(|r| r.widget)
                    && !self.is_window_fullscreen(w)
                    && self.window_intersects_viewport_on(w, output)
            })
            .min_by(|a, b| {
                let dist = |w: &Window| {
                    self.window_visual_center(w)
                        .map(|c| {
                            let dx = c.x - from_center.x;
                            let dy = c.y - from_center.y;
                            dx * dx + dy * dy
                        })
                        .unwrap_or(f64::INFINITY)
                };
                dist(a)
                    .partial_cmp(&dist(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned()
    }

    /// Most-recent focus-history entry that is spatially related to `destroyed`:
    /// either a snap-cluster member (auto-placement snaps transients here) or
    /// a geometric overlap. Used to pick a "follow" target when no explicit
    /// `parent_surface()` link exists.
    ///
    /// Uses `stable_snap_rects` for `destroyed`'s rect when available — some
    /// clients (foot) shrink or reposition their surface during the destroy
    /// sequence, so the live geometry at this point may not reflect what the
    /// user last saw as the cluster.
    #[allow(clippy::mutable_key_type)]
    pub fn first_spatially_related_in_history(&self, destroyed: &Window) -> Option<Window> {
        let cached_destroyed_rect = destroyed
            .wl_surface()
            .and_then(|s| self.stable_snap_rects.get(&s.id()).copied());
        let destroyed_rect = cached_destroyed_rect.or_else(|| self.snap_rect_for(destroyed))?;

        let mut rects = self.all_windows_with_snap_rects();
        if cached_destroyed_rect.is_some() {
            for (w, r) in &mut rects {
                if w == destroyed {
                    *r = destroyed_rect;
                }
            }
        }
        let cluster = driftwm::layout::cluster::cluster_of(destroyed, &rects, self.config.snap_gap);

        self.focus_history
            .iter()
            .filter(|w| *w != destroyed)
            .find(|w| {
                cluster.contains(*w)
                    || self
                        .snap_rect_for(w)
                        .is_some_and(|r| rects_overlap(&destroyed_rect, &r))
            })
            .cloned()
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
