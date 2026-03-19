use smithay::{
    input::pointer::MotionEvent,
    utils::{Point, Size, SERIAL_COUNTER},
    wayland::seat::WaylandFocus,
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
                self.exec_cursor_show_at =
                    Some(now + std::time::Duration::from_millis(150));
                self.exec_cursor_deadline =
                    Some(now + std::time::Duration::from_secs(5));
            }
            Action::Spawn(cmd) => {
                tracing::info!("Spawning (no cursor): {cmd}");
                crate::state::spawn_command(cmd);
            }
            Action::CloseWindow => {
                let keyboard = self.seat.get_keyboard().unwrap();
                if let Some(focus) = keyboard.current_focus() {
                    let window = self
                        .space
                        .elements()
                        .find(|w| w.wl_surface().as_deref() == Some(&focus.0))
                        .cloned();
                    if let Some(window) = window {
                        window.send_close();
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
                        .find(|w| w.wl_surface().as_deref() == Some(&focus.0))
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
                let keyboard = self.seat.get_keyboard().unwrap();
                let focused_non_widget = keyboard.current_focus().and_then(|focus| {
                    if driftwm::config::applied_rule(&focus.0).is_some_and(|r| r.widget) {
                        return None;
                    }
                    self.space
                        .elements()
                        .find(|w| w.wl_surface().as_deref() == Some(&focus.0))
                        .cloned()
                });
                if let Some(window) = focused_non_widget {
                    self.navigate_to_window(&window, true);
                } else {
                    // No focused non-widget window — find and focus the closest to viewport center
                    let viewport = self.get_viewport_size();
                    let camera = self.camera();
                    let zoom = self.zoom();
                    let center_x = camera.x + viewport.w as f64 / (2.0 * zoom);
                    let center_y = camera.y + viewport.h as f64 / (2.0 * zoom);
                    let closest = self
                        .space
                        .elements()
                        .filter(|w| {
                            !w.wl_surface().as_ref().and_then(|s| driftwm::config::applied_rule(s))
                                .is_some_and(|r| r.widget)
                        })
                        .min_by(|a, b| {
                            let dist = |w: &smithay::desktop::Window| {
                                let c = self.window_visual_center(w).unwrap_or_default();
                                let dx = c.x - center_x;
                                let dy = c.y - center_y;
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

                let keyboard = self.seat.get_keyboard().unwrap();
                let focused = keyboard.current_focus().and_then(|focus| {
                    self.space
                        .elements()
                        .find(|w| w.wl_surface().as_deref() == Some(&focus.0))
                        .cloned()
                });

                let viewport_size = self.get_viewport_size();
                let camera = self.camera();
                let zoom = self.zoom();
                let viewport_center = Point::from((
                    camera.x + viewport_size.w as f64 / (2.0 * zoom),
                    camera.y + viewport_size.h as f64 / (2.0 * zoom),
                ));

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

                let windows = self.space.elements().filter(|w| {
                    !w.wl_surface().as_ref().and_then(|s| driftwm::config::applied_rule(s))
                        .is_some_and(|r| r.widget)
                }).map(|w| {
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
                        let vp = self.get_viewport_size();
                        let zoom = self.zoom();
                        self.set_camera_target(Some(Point::from((
                            p.x - vp.w as f64 / (2.0 * zoom),
                            p.y - vp.h as f64 / (2.0 * zoom),
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
                            let vp = self.get_viewport_size();
                            self.set_zoom_animation_center(Some(Point::from((
                                ret.camera.x + vp.w as f64 / (2.0 * ret.zoom),
                                ret.camera.y + vp.h as f64 / (2.0 * ret.zoom),
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
                    let home = Point::from((
                        -(viewport_size.w as f64) / 2.0,
                        -(viewport_size.h as f64) / 2.0,
                    ));
                    self.set_zoom_animation_center(Some(Point::from((0.0, 0.0))));
                    self.set_camera_target(Some(home));
                    self.set_zoom_target(Some(1.0));
                }
            }
            Action::GoToPosition(x, y) => {
                let viewport = self.get_viewport_size();
                let zoom = self.zoom();
                let target_camera = Point::from((
                    x - viewport.w as f64 / (2.0 * zoom),
                    -y - viewport.h as f64 / (2.0 * zoom),
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
                let overview_ret = self.overview_return();
                self.set_overview_return(None);
                if let Some((saved_camera, saved_zoom)) = overview_ret {
                    // Toggle back from overview
                    let vp = self.get_viewport_size();
                    self.set_zoom_animation_center(Some(Point::from((
                        saved_camera.x + vp.w as f64 / (2.0 * saved_zoom),
                        saved_camera.y + vp.h as f64 / (2.0 * saved_zoom),
                    ))));
                    self.set_camera_target(Some(saved_camera));
                    self.set_zoom_target(Some(saved_zoom));
                } else {
                    // Compute bounding box of all windows
                    let viewport = self.get_viewport_size();
                    let windows = self.space.elements().filter(|w| {
                        !w.wl_surface().as_ref().and_then(|s| driftwm::config::applied_rule(s))
                            .is_some_and(|r| r.widget)
                    }).map(|w| {
                        let loc = self.space.element_location(w).unwrap_or_default();
                        let size = w.geometry().size;
                        (loc, size)
                    });
                    let anchors = self.config.nav_anchors.iter().map(|p| {
                        (Point::from((p.x as i32, p.y as i32)), Size::from((0, 0)))
                    });
                    let bbox = canvas::all_windows_bbox(windows.chain(anchors));
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
                        self.set_overview_return(Some((self.camera(), self.zoom())));
                        self.set_zoom_animation_center(Some(Point::from((bbox_cx, bbox_cy))));
                        self.set_camera_target(Some(new_camera));
                        self.set_zoom_target(Some(fit_zoom));
                    }
                }
            }
            Action::ToggleFullscreen => {
                if self.is_fullscreen() {
                    self.exit_fullscreen();
                } else if was_fullscreen.is_some() {
                    // Gesture already exited fullscreen — don't re-enter
                } else {
                    let keyboard = self.seat.get_keyboard().unwrap();
                    if let Some(focus) = keyboard.current_focus() {
                        let window = self
                            .space
                            .elements()
                            .find(|w| w.wl_surface().as_deref() == Some(&focus.0))
                            .cloned();
                        if let Some(window) = window {
                            self.enter_fullscreen(&window);
                        }
                    }
                }
            }
            Action::FitWindow => {
                let keyboard = self.seat.get_keyboard().unwrap();
                if let Some(focus) = keyboard.current_focus() {
                    let window = self
                        .space
                        .elements()
                        .find(|w| w.wl_surface().as_deref() == Some(&focus.0))
                        .cloned();
                    if let Some(window) = window {
                        self.toggle_fit_window(&window);
                    }
                }
            }
            Action::SendToOutput(dir) => {
                let keyboard = self.seat.get_keyboard().unwrap();
                if let Some(focus) = keyboard.current_focus() {
                    if driftwm::config::applied_rule(&focus.0).is_some_and(|r| r.widget) {
                        return;
                    }
                    let window = self
                        .space
                        .elements()
                        .find(|w| w.wl_surface().as_deref() == Some(&focus.0))
                        .cloned();
                    if let Some(window) = window
                        && let Some(from_output) = self.output_for_window(&window)
                        && let Some(target_output) = self.output_in_direction(&from_output, dir)
                    {
                        // Compute target output's viewport center in canvas coords
                        let (target_cam, target_zoom, target_size) = {
                            let os = crate::state::output_state(&target_output);
                            let sz = crate::state::output_logical_size(&target_output);
                            (os.camera, os.zoom, sz)
                        };
                        let center_x = target_cam.x + target_size.w as f64 / (2.0 * target_zoom);
                        let center_y = target_cam.y + target_size.h as f64 / (2.0 * target_zoom);
                        let geo = window.geometry();
                        let new_loc = Point::from((
                            (center_x - geo.size.w as f64 / 2.0) as i32,
                            (center_y - geo.size.h as f64 / 2.0) as i32,
                        ));
                        self.space.map_element(window.clone(), new_loc, true);
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        self.raise_and_focus(&window, serial);
                    }
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

    /// Animate zoom to `target_zoom`, anchored on viewport center (for keyboard actions).
    fn zoom_to_anchored(&mut self, target_zoom: f64) {
        self.set_overview_return(None);
        let viewport = self.get_viewport_size();
        let camera = self.camera();
        let zoom = self.zoom();
        let vp_center_canvas = Point::from((
            camera.x + viewport.w as f64 / (2.0 * zoom),
            camera.y + viewport.h as f64 / (2.0 * zoom),
        ));
        let vp_center_screen = Point::from((
            viewport.w as f64 / 2.0,
            viewport.h as f64 / 2.0,
        ));
        let new_camera = canvas::zoom_anchor_camera(
            vp_center_canvas, vp_center_screen, target_zoom,
        );
        self.set_zoom_animation_center(Some(vp_center_canvas));
        self.set_zoom_target(Some(target_zoom));
        self.set_camera_target(Some(new_camera));
    }
}
