use smithay::{
    input::pointer::MotionEvent,
    utils::{Point, SERIAL_COUNTER},
};

use driftwm::canvas::{self};
use driftwm::config::Action;
use crate::state::{DriftWm, log_err};

impl DriftWm {
    pub fn execute_action(&mut self, action: &Action) {
        // Any action except ToggleFullscreen exits fullscreen first
        if self.fullscreen.is_some() && !matches!(action, Action::ToggleFullscreen) {
            self.exit_fullscreen();
        }

        self.momentum.stop();
        match action {
            Action::Exec(cmd) => {
                tracing::info!("Spawning: {cmd}");
                log_err(
                    "spawn command",
                    std::process::Command::new("sh").args(["-c", cmd]).spawn(),
                );
            }
            Action::CloseWindow => {
                let keyboard = self.seat.get_keyboard().unwrap();
                if let Some(focus) = keyboard.current_focus() {
                    let window = self
                        .space
                        .elements()
                        .find(|w| w.toplevel().unwrap().wl_surface() == &focus.0)
                        .cloned();
                    if let Some(window) = window {
                        window.toplevel().unwrap().send_close();
                    }
                }
            }
            Action::NudgeWindow(dir) => {
                let keyboard = self.seat.get_keyboard().unwrap();
                if let Some(focus) = keyboard.current_focus() {
                    if driftwm::config::applied_rule(&focus.0).is_some_and(|r| r.widget) {
                        return;
                    }
                    let window = self
                        .space
                        .elements()
                        .find(|w| w.toplevel().unwrap().wl_surface() == &focus.0)
                        .cloned();
                    if let Some(window) = window
                        && let Some(loc) = self.space.element_location(&window)
                    {
                        let step = self.config.nudge_step;
                        let (ux, uy) = dir.to_unit_vec();
                        let offset = (
                            (ux * step as f64).round() as i32,
                            (uy * step as f64).round() as i32,
                        );
                        let new_loc = loc + Point::from(offset);
                        self.space.map_element(window, new_loc, false);
                    }
                }
            }
            Action::PanViewport(dir) => {
                self.camera_target = None;
                self.zoom_target = None;
                self.overview_return = None;
                let step = self.config.pan_step / self.zoom;
                let (ux, uy) = dir.to_unit_vec();
                let delta: Point<f64, smithay::utils::Logical> =
                    Point::from((ux * step, uy * step));
                self.camera += delta;
                self.update_output_from_camera();

                // Shift pointer so cursor stays at the same screen position
                let pointer = self.seat.get_pointer().unwrap();
                let pos = pointer.current_location();
                let new_pos = pos + delta;
                let under = self.surface_under(new_pos);
                let serial = SERIAL_COUNTER.next_serial();
                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: new_pos,
                        serial,
                        time: self.start_time.elapsed().as_millis() as u32,
                    },
                );
                pointer.frame(self);
            }
            Action::CenterWindow => {
                let keyboard = self.seat.get_keyboard().unwrap();
                if let Some(focus) = keyboard.current_focus() {
                    let window = self
                        .space
                        .elements()
                        .find(|w| w.toplevel().unwrap().wl_surface() == &focus.0)
                        .cloned();
                    if let Some(window) = window {
                        self.navigate_to_window(&window);
                    }
                }
            }
            Action::CenterNearest(dir) => {
                let keyboard = self.seat.get_keyboard().unwrap();
                let focused = keyboard.current_focus().and_then(|focus| {
                    self.space
                        .elements()
                        .find(|w| w.toplevel().unwrap().wl_surface() == &focus.0)
                        .cloned()
                });

                let viewport_size = self.get_viewport_size();
                let viewport_center = Point::from((
                    self.camera.x + viewport_size.w as f64 / (2.0 * self.zoom),
                    self.camera.y + viewport_size.h as f64 / (2.0 * self.zoom),
                ));

                // If focused window is visible, search from its center and skip it.
                // If off-screen (or no focus), search from viewport center — the
                // focused window becomes a valid target so you can navigate back to it.
                let (origin, skip) = if let Some(ref w) = focused {
                    let loc = self.space.element_location(w).unwrap_or_default();
                    let size = w.geometry().size;
                    if canvas::is_rect_visible(loc, size, self.camera, viewport_size, self.zoom) {
                        let center = Point::from((
                            loc.x as f64 + size.w as f64 / 2.0,
                            loc.y as f64 + size.h as f64 / 2.0,
                        ));
                        (center, focused.clone())
                    } else {
                        (viewport_center, None)
                    }
                } else {
                    (viewport_center, None)
                };

                let windows = self.space.elements().filter(|w| {
                    !driftwm::config::applied_rule(w.toplevel().unwrap().wl_surface())
                        .is_some_and(|r| r.widget || r.no_focus)
                }).map(|w| {
                    let loc = self.space.element_location(w).unwrap_or_default();
                    let size = w.geometry().size;
                    let center = Point::from((
                        loc.x as f64 + size.w as f64 / 2.0,
                        loc.y as f64 + size.h as f64 / 2.0,
                    ));
                    (w.clone(), center)
                }).collect::<Vec<_>>();

                let nearest = canvas::find_nearest(
                    origin,
                    dir,
                    windows.into_iter(),
                    skip.as_ref(),
                );
                if let Some(window) = nearest {
                    self.navigate_to_window(&window);
                }
            }
            Action::CycleWindows { backward } => {
                if self.focus_history.is_empty() {
                    return;
                }

                let len = self.focus_history.len();
                if let Some(ref mut idx) = self.cycle_state {
                    if *backward {
                        *idx = (*idx + len - 1) % len;
                    } else {
                        *idx = (*idx + 1) % len;
                    }
                } else {
                    // First Tab press: jump to previous window (index 1)
                    self.cycle_state = Some(1 % len);
                }

                let idx = self.cycle_state.unwrap();
                if let Some(window) = self.focus_history.get(idx).cloned() {
                    self.navigate_to_window(&window);
                }
            }
            Action::HomeToggle => {
                let viewport_size = self.get_viewport_size();

                // "At home" means zoom ≈ 1.0 AND origin visible. At lower zoom
                // the origin is visible from afar, but you're not really home.
                let at_home = (self.zoom - 1.0).abs() < 0.01
                    && canvas::is_origin_visible(self.camera, viewport_size, self.zoom);

                if at_home {
                    // We're at home — return to saved position if we have one
                    if let Some((target_camera, target_zoom)) = self.home_return.take() {
                        self.camera_target = Some(target_camera);
                        self.zoom_target = Some(target_zoom);
                    }
                } else {
                    // Not at home — save current position+zoom and go home at zoom=1.0
                    self.home_return = Some((self.camera, self.zoom));
                    self.overview_return = None;
                    let home = Point::from((
                        -(viewport_size.w as f64) / 2.0,
                        -(viewport_size.h as f64) / 2.0,
                    ));
                    self.camera_target = Some(home);
                    self.zoom_target = Some(1.0);
                }
            }
            Action::ZoomIn => {
                let new_zoom = (self.zoom * self.config.zoom_step).min(canvas::MAX_ZOOM);
                let new_zoom = canvas::snap_zoom(new_zoom);
                self.zoom_to_anchored(new_zoom);
            }
            Action::ZoomOut => {
                let new_zoom = (self.zoom / self.config.zoom_step).max(self.min_zoom());
                let new_zoom = canvas::snap_zoom(new_zoom);
                self.zoom_to_anchored(new_zoom);
            }
            Action::ZoomReset => {
                self.zoom_to_anchored(1.0);
            }
            Action::ZoomToFit => {
                if let Some((saved_camera, saved_zoom)) = self.overview_return.take() {
                    // Toggle back from overview
                    self.camera_target = Some(saved_camera);
                    self.zoom_target = Some(saved_zoom);
                } else {
                    // Compute bounding box of all windows
                    let viewport = self.get_viewport_size();
                    let bbox = canvas::all_windows_bbox(
                        self.space.elements().map(|w| {
                            let loc = self.space.element_location(w).unwrap_or_default();
                            let size = w.geometry().size;
                            (loc, size)
                        }),
                    );
                    if let Some(bbox) = bbox {
                        let fit_zoom = canvas::zoom_to_fit(
                            bbox, viewport, self.config.zoom_fit_padding,
                        );
                        // Center camera on bbox center
                        let bbox_cx = bbox.loc.x as f64 + bbox.size.w as f64 / 2.0;
                        let bbox_cy = bbox.loc.y as f64 + bbox.size.h as f64 / 2.0;
                        let new_camera: Point<f64, smithay::utils::Logical> = Point::from((
                            bbox_cx - viewport.w as f64 / (2.0 * fit_zoom),
                            bbox_cy - viewport.h as f64 / (2.0 * fit_zoom),
                        ));
                        self.overview_return = Some((self.camera, self.zoom));
                        self.camera_target = Some(new_camera);
                        self.zoom_target = Some(fit_zoom);
                    }
                }
            }
            Action::ToggleFullscreen => {
                if self.fullscreen.is_some() {
                    self.exit_fullscreen();
                } else {
                    let keyboard = self.seat.get_keyboard().unwrap();
                    if let Some(focus) = keyboard.current_focus() {
                        let window = self
                            .space
                            .elements()
                            .find(|w| w.toplevel().unwrap().wl_surface() == &focus.0)
                            .cloned();
                        if let Some(window) = window {
                            self.enter_fullscreen(&window);
                        }
                    }
                }
            }
            Action::Quit => {
                tracing::info!("Quit action triggered — stopping compositor");
                self.loop_signal.stop();
            }
        }
    }

    /// Animate zoom to `target_zoom`, anchored on viewport center (for keyboard actions).
    fn zoom_to_anchored(&mut self, target_zoom: f64) {
        self.overview_return = None;
        let viewport = self.get_viewport_size();
        let vp_center_canvas = Point::from((
            self.camera.x + viewport.w as f64 / (2.0 * self.zoom),
            self.camera.y + viewport.h as f64 / (2.0 * self.zoom),
        ));
        let vp_center_screen = Point::from((
            viewport.w as f64 / 2.0,
            viewport.h as f64 / 2.0,
        ));
        let new_camera = canvas::zoom_anchor_camera(
            vp_center_canvas, vp_center_screen, target_zoom,
        );
        self.zoom_target = Some(target_zoom);
        self.camera_target = Some(new_camera);
    }
}
