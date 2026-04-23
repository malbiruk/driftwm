use smithay::{
    input::pointer::MotionEvent,
    utils::{Point, Size, SERIAL_COUNTER},
};

use driftwm::canvas::{self};
use driftwm::config::Action;
use driftwm::window_ext::WindowExt;
use crate::state::{DriftWm, FocusTarget, HomeReturn};

impl DriftWm {
    pub fn execute_action(&mut self, action: &Action) {
        // Snapshot fullscreen window before the guard exits it.
        // Also check gesture_exited_fullscreen (set by exit_fullscreen_for_gesture
        // which runs before execute_action in the gesture path).
        let was_fullscreen = self.active_fullscreen().map(|fs| fs.window.clone())
            .or_else(|| self.gesture_exited_fullscreen.take());

        // Any action except ToggleFullscreen/Spawn/ReloadConfig exits fullscreen first
        if self.is_fullscreen()
            && !matches!(action, Action::ToggleFullscreen | Action::Spawn(_) | Action::ReloadConfig)
        {
            self.exit_fullscreen();
        }

        self.with_output_state(|os| os.momentum.stop());
        match action {
            Action::Exec(cmd) => {
                tracing::info!("Spawning: {cmd}");
                crate::state::spawn_command(cmd);
                let now = std::time::Instant::now();
                self.cursor.exec_cursor_show_at =
                    Some(now + std::time::Duration::from_millis(150));
                self.cursor.exec_cursor_deadline =
                    Some(now + std::time::Duration::from_secs(5));
            }
            Action::Spawn(cmd) => {
                tracing::info!("Spawning (no cursor): {cmd}");
                crate::state::spawn_command(cmd);
            }
            Action::CloseWindow => {
                if let Some(window) = self.focused_window().filter(|w| !w.is_widget()) {
                    window.send_close();
                }
            }
            Action::NudgeWindow(dir) => {
                if let Some(window) = self.focused_window().filter(|w| !w.is_widget())
                    && let Some(loc) = self.space.element_location(&window)
                {
                    let step = self.config.nudge_step;
                    let (ux, uy) = dir.to_unit_vec();
                    let offset = (
                        (ux * step as f64).round() as i32,
                        (uy * step as f64).round() as i32,
                    );
                    let new_loc = loc + Point::from(offset);
                    self.space.map_element(window.clone(), new_loc, false);
                    self.sync_x11_position(&window);
                }
            }
            Action::PanViewport(dir) => {
                let zoom = self.with_output_state(|os| {
                    os.camera_target = None;
                    os.zoom_target = None;
                    os.zoom_animation_center = None;
                    os.overview_return = None;
                    os.zoom
                });
                let step = self.config.pan_step / zoom;
                let (ux, uy) = dir.to_unit_vec();
                let delta: Point<f64, smithay::utils::Logical> =
                    Point::from((ux * step, uy * step));
                self.set_camera(self.camera() + delta);
                self.update_output_from_camera();

                // Shift pointer so cursor stays at the same screen position
                let pointer = self.seat.get_pointer().unwrap();
                let pos = pointer.current_location();
                let new_pos = pos + delta;
                let under = self.surface_under(new_pos, None);
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
                if let Some(window) = self.focused_window().filter(|w| !w.is_widget()) {
                    self.navigate_to_window(&window, true);
                } else {
                    let center = self.viewport_center_canvas();
                    let closest = self
                        .space
                        .elements()
                        .filter(|w| !w.is_widget())
                        .min_by(|a, b| {
                            let dist = |w: &smithay::desktop::Window| {
                                let c = self.window_visual_center(w).unwrap_or_default();
                                let dx = c.x - center.x;
                                let dy = c.y - center.y;
                                dx * dx + dy * dy
                            };
                            dist(a).partial_cmp(&dist(b)).unwrap()
                        })
                        .cloned();
                    if let Some(window) = closest {
                        self.navigate_to_window(&window, true);
                    }
                }
            }
            Action::FocusCenter => {
                let pointer = self.seat.get_pointer().unwrap();
                let pos = pointer.current_location();
                if let Some((window, _)) = self.space.element_under(pos) {
                    let window = window.clone();
                    self.navigate_to_window(&window, true);
                }
            }
            Action::CenterNearest(dir) => {
                #[derive(Clone, PartialEq)]
                enum NavTarget {
                    Window(smithay::desktop::Window),
                    Anchor(Point<f64, smithay::utils::Logical>),
                }

                let focused = self.focused_window();
                let viewport_size = self.get_viewport_size();
                let camera = self.camera();
                let zoom = self.zoom();
                let viewport_center = self.viewport_center_canvas();

                let (origin, skip) = if let Some(ref w) = focused {
                    let loc = self.space.element_location(w).unwrap_or_default();
                    let size = w.geometry().size;
                    if canvas::visible_fraction(loc, size, camera, viewport_size, zoom)
                        >= 0.5
                    {
                        let center = self.window_visual_center(w).unwrap_or_else(|| {
                            Point::from((
                                loc.x as f64 + size.w as f64 / 2.0,
                                loc.y as f64 + size.h as f64 / 2.0,
                            ))
                        });
                        (center, Some(NavTarget::Window(w.clone())))
                    } else {
                        (viewport_center, None)
                    }
                } else {
                    (viewport_center, None)
                };

                let windows = self.space.elements().filter(|w| !w.is_widget()).map(|w| {
                    let loc = self.space.element_location(w).unwrap_or_default();
                    let size = w.geometry().size;
                    let closest = canvas::closest_point_on_rect(origin, loc, size);
                    let point = if closest == origin {
                        self.window_visual_center(w).unwrap_or_else(|| {
                            Point::from((
                                loc.x as f64 + size.w as f64 / 2.0,
                                loc.y as f64 + size.h as f64 / 2.0,
                            ))
                        })
                    } else {
                        closest
                    };
                    (NavTarget::Window(w.clone()), point)
                });

                let anchors = self.config.nav_anchors.iter()
                    .map(|&p| (NavTarget::Anchor(p), p));

                let nearest = canvas::find_nearest(
                    origin,
                    dir,
                    windows.chain(anchors),
                    skip.as_ref(),
                );
                match nearest {
                    Some(NavTarget::Window(w)) => {
                        self.navigate_to_window(&w, false);
                    }
                    Some(NavTarget::Anchor(p)) => {
                        // Unfocus so next CenterNearest searches from viewport center (= this anchor)
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        let keyboard = self.seat.get_keyboard().unwrap();
                        keyboard.set_focus(self, None::<FocusTarget>, serial);
                        self.with_output_state(|os| os.momentum.stop());
                        let vc = self.usable_center_screen();
                        let zoom = self.zoom();
                        self.set_camera_target(Some(Point::from((
                            p.x - vc.x / zoom,
                            p.y - vc.y / zoom,
                        ))));
                    }
                    None => {}
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
                    self.navigate_to_window(&window, false);
                }
            }
            Action::HomeToggle => {
                let viewport_size = self.get_viewport_size();
                let zoom = self.zoom();
                let camera = self.camera();

                // At home means zoom ≈ 1.0 AND origin visible
                let at_home = (zoom - 1.0).abs() < 0.01
                    && canvas::is_origin_visible(camera, viewport_size, zoom);

                if at_home {
                    // We're at home — return to saved position
                    let ret = self.with_output_state(|os| os.home_return.take());
                    if let Some(ret) = ret {
                        let can_fullscreen = ret.fullscreen_window.as_ref()
                            .is_some_and(|w| self.space.elements().any(|e| e == w));
                        if can_fullscreen {
                            // Set camera/zoom directly — enter_fullscreen locks the viewport
                            self.set_camera(ret.camera);
                            self.set_zoom(ret.zoom);
                            self.enter_fullscreen(ret.fullscreen_window.as_ref().unwrap());
                        } else {
                            let vc = self.usable_center_screen();
                            self.set_zoom_animation_center(Some(Point::from((
                                ret.camera.x + vc.x / ret.zoom,
                                ret.camera.y + vc.y / ret.zoom,
                            ))));
                            self.set_camera_target(Some(ret.camera));
                            self.set_zoom_target(Some(ret.zoom));
                        }
                    }
                } else {
                    // Not at home — save current position+zoom and go home at zoom=1.0
                    self.with_output_state(|os| {
                        os.home_return = Some(HomeReturn {
                            camera,
                            zoom,
                            fullscreen_window: was_fullscreen.clone(),
                        });
                    });
                    self.set_overview_return(None);
                    let vc = self.usable_center_screen();
                    let home = Point::from((-vc.x, -vc.y));
                    if was_fullscreen.is_some() {
                        // Snap instantly — matches the instant return path and
                        // avoids animation warps that misplace the cursor.
                        self.set_camera(home);
                        self.set_zoom(1.0);
                        self.update_output_from_camera();
                        self.warp_pointer(Point::from((0.0, 0.0)));
                    } else {
                        self.set_zoom_animation_center(Some(Point::from((0.0, 0.0))));
                        self.set_camera_target(Some(home));
                        self.set_zoom_target(Some(1.0));
                    }
                }
            }
            Action::GoToPosition(x, y) => {
                let vc = self.usable_center_screen();
                let zoom = self.zoom();
                let target_camera = Point::from((
                    x - vc.x / zoom,
                    -y - vc.y / zoom,
                ));
                self.set_overview_return(None);
                self.set_camera_target(Some(target_camera));
            }
            Action::ZoomIn => {
                let new_zoom = (self.zoom() * self.config.zoom_step).min(canvas::MAX_ZOOM);
                let new_zoom = canvas::snap_zoom(new_zoom);
                self.zoom_to_anchored(new_zoom);
            }
            Action::ZoomOut => {
                let new_zoom = (self.zoom() / self.config.zoom_step).max(self.min_zoom());
                let new_zoom = canvas::snap_zoom(new_zoom);
                self.zoom_to_anchored(new_zoom);
            }
            Action::ZoomReset => {
                self.zoom_to_anchored(1.0);
            }
            Action::ZoomToFit => {
                if self.try_restore_overview() {
                    // toggled back
                } else {
                    let windows = self.space.elements().filter(|w| !w.is_widget()).map(|w| {
                        let loc = self.space.element_location(w).unwrap_or_default();
                        let size = w.geometry().size;
                        (loc, size)
                    });
                    let anchors = self.config.nav_anchors.iter().map(|p| {
                        (Point::from((p.x as i32, p.y as i32)), Size::from((0, 0)))
                    });
                    if let Some(bbox) = canvas::all_windows_bbox(windows.chain(anchors)) {
                        self.fit_to_bbox(bbox);
                    }
                }
            }
            Action::ZoomToFitSnapped => {
                if self.try_restore_overview() {
                    // toggled back
                } else if let Some(focused) = self.focused_window().filter(|w| !w.is_widget()) {
                    let rects = self.all_windows_with_snap_rects();
                    // Window's Hash/Eq are Arc pointer identity — stable despite
                    // interior mutability. Same allow as cluster_snapshot.rs.
                    #[allow(clippy::mutable_key_type)]
                    let cluster = crate::cluster::cluster_of(
                        &focused, &rects, self.config.snap_gap,
                    );
                    let members = self.space.elements()
                        .filter(|w| cluster.contains(w))
                        .map(|w| {
                            let loc = self.space.element_location(w).unwrap_or_default();
                            let size = w.geometry().size;
                            (loc, size)
                        });
                    if let Some(bbox) = canvas::all_windows_bbox(members) {
                        self.fit_to_bbox(bbox);
                    }
                }
            }
            Action::ToggleFullscreen => {
                if self.is_fullscreen() {
                    self.exit_fullscreen();
                } else if was_fullscreen.is_some() {
                    // Gesture already exited fullscreen — don't re-enter
                } else if let Some(window) = self.focused_window().filter(|w| !w.is_widget()) {
                    self.enter_fullscreen(&window);
                }
            }
            Action::FitWindow => {
                if let Some(window) = self.focused_window().filter(|w| !w.is_widget()) {
                    self.toggle_fit_window(&window);
                }
            }
            Action::FitWindowSnapped => {
                if let Some(window) = self.focused_window().filter(|w| !w.is_widget()) {
                    self.toggle_fit_window_snapped(&window);
                }
            }
            Action::SendToOutput(dir) => {
                if let Some(window) = self.focused_window().filter(|w| !w.is_widget())
                    && let Some(from_output) = self.output_for_window(&window)
                    && let Some(target_output) = self.output_in_direction(&from_output, dir)
                {
                    // Compute target output's usable area center in canvas coords
                    let (target_cam, target_zoom) = {
                        let os = crate::state::output_state(&target_output);
                        (os.camera, os.zoom)
                    };
                    let target_vc = crate::state::usable_center_for_output(&target_output);
                    let center_x = target_cam.x + target_vc.x / target_zoom;
                    let center_y = target_cam.y + target_vc.y / target_zoom;
                    let geo = window.geometry();
                    let new_loc = Point::from((
                        (center_x - geo.size.w as f64 / 2.0) as i32,
                        (center_y - geo.size.h as f64 / 2.0) as i32,
                    ));
                    self.space.map_element(window.clone(), new_loc, true);
                    self.sync_x11_position(&window);
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    self.raise_and_focus(&window, serial);
                }
            }
            Action::ReloadConfig => {
                self.reload_config();
            }
            Action::Quit => {
                tracing::info!("Quit action triggered — stopping compositor");
                self.loop_signal.stop();
            }
        }
    }

    /// If an overview-return is pending, animate back to it and return true.
    fn try_restore_overview(&mut self) -> bool {
        let Some((saved_camera, saved_zoom)) = self.overview_return() else {
            return false;
        };
        self.set_overview_return(None);
        let vc = self.usable_center_screen();
        self.set_zoom_animation_center(Some(Point::from((
            saved_camera.x + vc.x / saved_zoom,
            saved_camera.y + vc.y / saved_zoom,
        ))));
        self.set_camera_target(Some(saved_camera));
        self.set_zoom_target(Some(saved_zoom));
        true
    }

    /// Animate zoom + camera to fit `bbox` inside the viewport. Saves the
    /// current camera/zoom into `overview_return` so the next zoom-to-fit
    /// press toggles back.
    fn fit_to_bbox(&mut self, bbox: smithay::utils::Rectangle<i32, smithay::utils::Logical>) {
        let usable = self.get_usable_area();
        let vc = self.usable_center_screen();
        let fit_zoom = canvas::zoom_to_fit(
            bbox, usable.size, self.config.zoom_fit_padding,
        );
        let bbox_cx = bbox.loc.x as f64 + bbox.size.w as f64 / 2.0;
        let bbox_cy = bbox.loc.y as f64 + bbox.size.h as f64 / 2.0;
        let new_camera: Point<f64, smithay::utils::Logical> = Point::from((
            bbox_cx - vc.x / fit_zoom,
            bbox_cy - vc.y / fit_zoom,
        ));
        self.set_overview_return(Some((self.camera(), self.zoom())));
        self.set_zoom_animation_center(Some(Point::from((bbox_cx, bbox_cy))));
        self.set_camera_target(Some(new_camera));
        self.set_zoom_target(Some(fit_zoom));
    }

    /// Animate zoom to `target_zoom`, anchored on viewport center (for keyboard actions).
    fn zoom_to_anchored(&mut self, target_zoom: f64) {
        self.set_overview_return(None);
        let vc = self.usable_center_screen();
        let camera = self.camera();
        let zoom = self.zoom();
        let vc_canvas = Point::from((
            camera.x + vc.x / zoom,
            camera.y + vc.y / zoom,
        ));
        let new_camera = canvas::zoom_anchor_camera(
            vc_canvas, vc, target_zoom,
        );
        self.set_zoom_animation_center(Some(vc_canvas));
        self.set_zoom_target(Some(target_zoom));
        self.set_camera_target(Some(new_camera));
    }
}
