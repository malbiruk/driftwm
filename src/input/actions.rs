use smithay::{
    input::{keyboard::Layout, pointer::MotionEvent},
    reexports::wayland_server::Resource,
    utils::{Logical, Point, SERIAL_COUNTER, Size},
    wayland::seat::WaylandFocus,
};

use crate::state::{DriftWm, HomeReturn};
use driftwm::canvas::{self};
use driftwm::config::{Action, LayoutSwitch};
use driftwm::window_ext::WindowExt;

/// Use the focused window as the cone-search origin only when it's fully
/// inside the viewport. Any clipping → search from viewport center instead
/// (cone can still return the focused window if it's the nearest in that
/// direction, which is useful for snapping back to a partially-visible
/// focused window).
const CENTER_NEAREST_ANCHOR_THRESHOLD: f64 = 1.0;

impl DriftWm {
    /// Spawn a command and reveal the cursor briefly so the launched app is
    /// easy to find.
    fn exec_command(&mut self, cmd: &str) {
        tracing::info!("Spawning: {cmd}");
        crate::state::spawn_command(cmd, &self.config.child_env);
        let now = std::time::Instant::now();
        self.cursor.exec_cursor_show_at = Some(now + std::time::Duration::from_millis(150));
        self.cursor.exec_cursor_deadline = Some(now + std::time::Duration::from_secs(5));
    }

    pub fn execute_action(&mut self, action: &Action) {
        // Snapshot fullscreen window before the guard exits it.
        // Also check gesture_exited_fullscreen (set by exit_fullscreen_for_gesture
        // which runs before execute_action in the gesture path).
        let was_fullscreen = self
            .active_fullscreen()
            .map(|fs| fs.window.clone())
            .or_else(|| self.gesture_exited_fullscreen.take());

        // Any action except ToggleFullscreen/Spawn/ReloadConfig/ToggleCursorPan exits fullscreen first
        if self.is_fullscreen()
            && !matches!(
                action,
                Action::ToggleFullscreen
                    | Action::Spawn(_)
                    | Action::ReloadConfig
                    | Action::SwitchLayout(_)
                    | Action::ToggleCursorPan
            )
        {
            self.exit_fullscreen();
        }

        self.with_output_state(|os| os.momentum.stop());
        match action {
            Action::Exec(cmd) => self.exec_command(cmd),
            Action::ExecTerminal => self.exec_command(&detect_terminal()),
            Action::ExecLauncher => self.exec_command(&detect_launcher()),
            Action::Spawn(cmd) => {
                tracing::info!("Spawning (no cursor): {cmd}");
                crate::state::spawn_command(cmd, &self.config.child_env);
            }
            Action::CloseWindow => {
                if let Some(window) = self.focused_window().filter(|w| !w.is_widget()) {
                    window.send_close();
                }
            }
            Action::NudgeWindow(dir) => {
                if let Some(window) = self.focused_window().filter(|w| self.is_canvas_window(w))
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
                }
            }
            Action::PanViewport(dir) => {
                let Some(zoom) = self.with_output_state(|os| {
                    os.camera_target = None;
                    os.zoom_target = None;
                    os.zoom_animation_center = None;
                    os.overview_return = None;
                    os.zoom
                }) else {
                    return;
                };
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
                if let Some(window) = self.focused_window().filter(|w| self.is_canvas_window(w)) {
                    self.navigate_to_window(&window, true);
                } else {
                    let center = self.viewport_center_canvas();
                    let closest = self
                        .space
                        .elements()
                        .filter(|w| self.is_canvas_window(w))
                        .min_by(|a, b| {
                            let dist = |w: &smithay::desktop::Window| {
                                let c = self.window_visual_center(w).unwrap_or_default();
                                let dx = c.x - center.x;
                                let dy = c.y - center.y;
                                dx * dx + dy * dy
                            };
                            dist(a).total_cmp(&dist(b))
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
                // Pinned windows live in screen space (no canvas position to
                // center the camera on) — skip them here.
                if let Some((window, _)) = self.element_under(pos) {
                    let window = window.clone();
                    if !self.is_pinned(&window) {
                        self.navigate_to_window(&window, true);
                    }
                }
            }
            Action::CenterNearest(dir) => {
                #[derive(Clone, PartialEq)]
                enum NavTarget {
                    Window(smithay::desktop::Window),
                    Anchor(Point<f64, smithay::utils::Logical>),
                }

                let focused = self.focused_window().filter(|w| !self.is_pinned(w));

                // Anchor the directional search to the just-exited fullscreen
                // window (wherever the restored view placed it) — otherwise the
                // anchor falls back to a corner/offscreen spot and the swipe
                // finds nothing.
                let anchor = was_fullscreen.clone().or_else(|| {
                    focused.filter(|w| {
                        self.window_visible_at_least(w, CENTER_NEAREST_ANCHOR_THRESHOLD)
                    })
                });

                let (origin, skip) = if let Some(ref w) = anchor {
                    let center = self.window_visual_center(w).unwrap_or_else(|| {
                        let loc = self.space.element_location(w).unwrap_or_default();
                        let size = w.geometry().size;
                        Point::from((
                            loc.x as f64 + size.w as f64 / 2.0,
                            loc.y as f64 + size.h as f64 / 2.0,
                        ))
                    });
                    (center, Some(NavTarget::Window(w.clone())))
                } else {
                    (self.viewport_center_canvas(), None)
                };

                let windows = self
                    .space
                    .elements()
                    .filter(|w| self.is_canvas_window(w))
                    .map(|w| {
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

                let anchors = self
                    .config
                    .nav_anchors
                    .iter()
                    .map(|&p| (NavTarget::Anchor(p), p));

                let nearest =
                    canvas::find_nearest(origin, dir, windows.chain(anchors), skip.as_ref());
                match nearest {
                    Some(NavTarget::Window(w)) => {
                        self.navigate_to_window(&w, false);
                    }
                    Some(NavTarget::Anchor(p)) => {
                        // Unfocus so next CenterNearest searches from viewport center (= this anchor)
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        self.set_window_focus(None, serial);
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
                let step = |i: usize| {
                    if *backward {
                        (i + len - 1) % len
                    } else {
                        (i + 1) % len
                    }
                };
                // First Tab press jumps to the previous window (index 1).
                let mut idx = match self.cycle_state {
                    Some(cur) => step(cur),
                    None => 1 % len,
                };
                // The active output's fullscreen was already exited above (not
                // allowlisted), so any fullscreen entry left here is on another
                // output — shown only on its own monitor, never a target here.
                // Bounded by `len` so an all-fullscreen history can't loop.
                let mut steps = 0;
                while steps < len
                    && self
                        .focus_history
                        .get(idx)
                        .is_some_and(|w| self.is_window_fullscreen(w))
                {
                    idx = step(idx);
                    steps += 1;
                }
                let Some(window) = self
                    .focus_history
                    .get(idx)
                    .filter(|w| !self.is_window_fullscreen(w))
                    .cloned()
                else {
                    return;
                };
                self.cycle_state = Some(idx);
                self.navigate_to_window(&window, false);
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
                    let ret = self.with_output_state(|os| os.home_return.take()).flatten();
                    if let Some(ret) = ret {
                        let can_fullscreen = ret
                            .fullscreen_window
                            .as_ref()
                            .is_some_and(|w| self.space.elements().any(|e| e == w));
                        if can_fullscreen {
                            // Set camera/zoom directly — enter_fullscreen locks the viewport
                            self.set_camera(ret.camera);
                            self.set_zoom(ret.zoom);
                            self.enter_fullscreen(ret.fullscreen_window.as_ref().unwrap(), None);
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
                let target_camera = Point::from((x - vc.x / zoom, -y - vc.y / zoom));
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
                    let windows = self
                        .space
                        .elements()
                        .filter(|w| self.is_canvas_window(w))
                        .map(|w| {
                            let loc = self.space.element_location(w).unwrap_or_default();
                            let size = w.geometry().size;
                            (loc, size)
                        });
                    let anchors = self
                        .config
                        .nav_anchors
                        .iter()
                        .map(|p| (Point::from((p.x as i32, p.y as i32)), Size::from((0, 0))));
                    if let Some(bbox) = canvas::all_windows_bbox(windows.chain(anchors)) {
                        self.fit_to_bbox(bbox);
                    }
                }
            }
            Action::ZoomToFitSnapped => {
                if self.try_restore_overview() {
                    // toggled back
                } else if let Some(focused) =
                    self.focused_window().filter(|w| self.is_canvas_window(w))
                {
                    let rects = self.all_windows_with_snap_rects();
                    // Window's Hash/Eq are Arc pointer identity — stable despite
                    // interior mutability. Same allow as cluster_snapshot.rs.
                    #[allow(clippy::mutable_key_type)]
                    let cluster = driftwm::layout::cluster::cluster_of(
                        &focused,
                        &rects,
                        self.config.snap_gap,
                    );
                    let members = self
                        .space
                        .elements()
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
                    let target = window
                        .wl_surface()
                        .and_then(|s| self.resolve_fullscreen_output(&s, None));
                    self.enter_fullscreen(&window, target);
                }
            }
            Action::FitWindow => {
                if let Some(window) = self.focused_window().filter(|w| self.is_canvas_window(w)) {
                    self.toggle_fit_window(&window);
                }
            }
            Action::FitWindowSnapped => {
                if let Some(window) = self.focused_window().filter(|w| self.is_canvas_window(w)) {
                    self.toggle_fit_window_snapped(&window);
                }
            }
            Action::SendToOutput(dir) => {
                if let Some(window) = self.focused_window().filter(|w| self.is_canvas_window(w))
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
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    self.raise_and_focus(&window, serial);
                }
            }
            Action::SendCursorToOutput(dir) => {
                // The cursor's output (active_output), not keyboard focus or
                // focus_follows_mouse: absolute devices stay pinned to it.
                let Some(from_output) = self.active_output() else {
                    return;
                };
                let Some(target_output) = self.output_in_direction(&from_output, dir) else {
                    return;
                };

                // Center of the target output's usable area in canvas coords.
                // Excludes layer-shell exclusive zones (panels) — same anchor
                // send-to-output and window_placement = "center" use, so a
                // panned cursor lands where new windows would.
                let (target_cam, target_zoom) = {
                    let os = crate::state::output_state(&target_output);
                    (os.camera, os.zoom)
                };
                let target_vc = crate::state::usable_center_for_output(&target_output);
                let target_canvas = Point::<f64, Logical>::from((
                    target_cam.x + target_vc.x / target_zoom,
                    target_cam.y + target_vc.y / target_zoom,
                ));

                self.warp_pointer(target_canvas);
                // Match what the pointer-motion handler does on real cursor
                // moves so the next action (center-nearest, focus, etc.) targets
                // the new output even though no real motion event happened.
                self.focused_output = Some(target_output);
            }
            Action::SwitchLayout(target) => {
                // with_xkb_state broadcasts the layout/modifier change to the
                // focused client on exit, so the app sees the new layout too.
                let keyboard = self.seat.get_keyboard().unwrap();
                let name = keyboard.with_xkb_state(self, |mut ctx| {
                    match target {
                        LayoutSwitch::Next => ctx.cycle_next_layout(),
                        LayoutSwitch::Prev => ctx.cycle_prev_layout(),
                        LayoutSwitch::Index(i) => {
                            let count = ctx.xkb().lock().unwrap().layouts().count();
                            if *i < count {
                                ctx.set_layout(Layout(*i as u32));
                            } else {
                                tracing::warn!(
                                    "switch-layout: index {i} out of range ({count} layouts)"
                                );
                            }
                        }
                    }
                    let xkb = ctx.xkb().lock().unwrap();
                    xkb.layout_name(xkb.active_layout()).to_owned()
                });
                self.active_layout = name;
            }
            Action::TogglePinToScreen => {
                self.toggle_pin_to_screen();
            }
            Action::ReloadConfig => {
                self.reload_config();
            }
            Action::ToggleCursorPan => {
                self.cursor_edge_pan = !self.cursor_edge_pan;
                if !self.cursor_edge_pan {
                    // Stop any in-progress pan immediately.
                    let outputs: Vec<_> = self.space.outputs().cloned().collect();
                    for o in outputs {
                        crate::state::output_state(&o).edge_pan_velocity = None;
                    }
                }
            }
            Action::Quit => {
                tracing::info!("Quit action triggered — stopping compositor");
                self.loop_signal.stop();
            }
        }
    }

    /// Toggle screen-pinning of the focused window. Pin/unpin keeps the window
    /// in the same on-screen position (no visual jump) and survives reload
    /// (state lives in `self.pinned`, not the rules).
    fn toggle_pin_to_screen(&mut self) {
        let Some(window) = self.focused_window() else {
            return;
        };
        let Some(id) = window.wl_surface().map(|s| s.id()) else {
            return;
        };
        if self.is_pinned(&window) {
            // Unpin: convert the fixed screen position back to a canvas
            // location at the current camera/zoom — no visual jump.
            if let Some((output, screen_pos)) = self
                .pinned
                .get(&id)
                .map(|p| (p.output.clone(), p.screen_pos))
            {
                let (camera, zoom) = {
                    let os = crate::state::output_state(&output);
                    (os.camera, os.zoom)
                };
                let canvas = driftwm::canvas::screen_to_canvas(
                    driftwm::canvas::ScreenPos(screen_pos.to_f64()),
                    camera,
                    zoom,
                )
                .0
                .to_i32_round();
                self.space.map_element(window.clone(), canvas, true);
            }
            self.pinned.remove(&id);
        } else {
            // Pin at the window's current on-screen position on its output.
            let Some(output) = self.output_for_window(&window) else {
                return;
            };
            let Some(loc) = self.space.element_location(&window) else {
                return;
            };
            let (camera, zoom) = {
                let os = crate::state::output_state(&output);
                (os.camera, os.zoom)
            };
            let screen = driftwm::canvas::canvas_to_screen(
                driftwm::canvas::CanvasPos(loc.to_f64()),
                camera,
                zoom,
            )
            .0;
            let screen_pos = Point::from((screen.x.round() as i32, screen.y.round() as i32));
            // Pinned windows are out of the focus cycle.
            self.focus_history.retain(|w| w != &window);
            self.pinned
                .insert(id, crate::state::PinnedState { output, screen_pos });
        }
        // The hit-test path changed (pinned vs canvas); recompute pointer focus.
        self.refresh_pointer_focus();
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
        let fit_zoom = canvas::zoom_to_fit(bbox, usable.size, self.config.zoom_fit_padding);
        let bbox_cx = bbox.loc.x as f64 + bbox.size.w as f64 / 2.0;
        let bbox_cy = bbox.loc.y as f64 + bbox.size.h as f64 / 2.0;
        let new_camera: Point<f64, smithay::utils::Logical> =
            Point::from((bbox_cx - vc.x / fit_zoom, bbox_cy - vc.y / fit_zoom));
        self.set_overview_return(Some((self.camera(), self.zoom())));
        self.set_zoom_animation_center(Some(Point::from((bbox_cx, bbox_cy))));
        self.set_camera_target(Some(new_camera));
        self.set_zoom_target(Some(fit_zoom));
    }

    /// Animate zoom to `target_zoom`, anchored on viewport center (for keyboard actions).
    pub(crate) fn zoom_to_anchored(&mut self, target_zoom: f64) {
        self.set_overview_return(None);
        let vc = self.usable_center_screen();
        let camera = self.camera();
        let zoom = self.zoom();
        let vc_canvas = Point::from((camera.x + vc.x / zoom, camera.y + vc.y / zoom));
        let new_camera = canvas::zoom_anchor_camera(vc_canvas, vc, target_zoom);
        self.set_zoom_animation_center(Some(vc_canvas));
        self.set_zoom_target(Some(target_zoom));
        self.set_camera_target(Some(new_camera));
    }
}

fn which(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `$TERMINAL`, else the first installed of a preference list, else `foot`.
fn detect_terminal() -> String {
    if let Ok(term) = std::env::var("TERMINAL")
        && !term.is_empty()
    {
        return term;
    }
    for cmd in [
        "foot",
        "alacritty",
        "ptyxis",
        "kitty",
        "wezterm",
        "gnome-terminal",
        "konsole",
    ] {
        if which(cmd) {
            return cmd.to_string();
        }
    }
    "foot".to_string()
}

/// `$LAUNCHER`, else the first installed of a preference list, else `fuzzel`.
/// Detection probes only the binary; the returned string carries drun-mode
/// flags so bare menus actually launch apps.
fn detect_launcher() -> String {
    if let Ok(launcher) = std::env::var("LAUNCHER")
        && !launcher.is_empty()
    {
        return launcher;
    }
    for cmd in [
        "fuzzel",
        "wofi --show drun",
        "rofi -show drun",
        "bemenu-run",
        "wmenu-run",
        "tofi-drun",
        "mew-run",
    ] {
        let bin = cmd.split_whitespace().next().unwrap();
        if which(bin) {
            return cmd.to_string();
        }
    }
    "fuzzel".to_string()
}
