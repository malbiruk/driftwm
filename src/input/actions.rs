use smithay::{
    input::keyboard::Layout,
    utils::{Logical, Point, Size},
    wayland::seat::WaylandFocus,
};

use crate::state::{DriftWm, HomeReturn};
use driftwm::canvas::{self};
use driftwm::config::{Action, LayoutSwitch, Modifiers};
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
        // A non-cycle action from any input source ends an in-progress Alt-Tab
        // cycle here, committing the selection before it runs. CycleWindows
        // itself is exempt so stepping deeper keeps working.
        if !matches!(action, Action::CycleWindows { .. }) && self.stage.cycle_state().is_some() {
            self.end_cycle();
        }

        // Snapshot fullscreen window before the guard exits it. Also check
        // pre_exited_fullscreen (set by input-layer code that exited fullscreen
        // ahead of dispatching this action).
        let was_fullscreen = self
            .active_fullscreen_window()
            .or_else(|| self.pre_exited_fullscreen.take());

        // A suspend of a fullscreen window must record the windowed rect, not
        // the fullscreen buffer the client still reports until it acks the exit
        // configure the prelude below sends. Capture it before the exit.
        let suspend_restore_rect = matches!(action, Action::SuspendWindow)
            .then(|| self.focused_fullscreen_restore_rect())
            .flatten();

        if self.is_fullscreen() && !action.runs_during_fullscreen() {
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
                if let Some(id) = self.gated_suspended_focus() {
                    // A focused suspended window has no client to close — the
                    // close binding dismisses it instead (ratified semantics).
                    self.dismiss_suspended(id);
                } else if let Some(window) = self.focused_window().filter(|w| !w.is_widget()) {
                    self.mark_real_close(&window);
                    window.send_close();
                }
            }
            Action::SuspendWindow => {
                if let Some(id) = self.gated_suspended_focus() {
                    // Repeating the put-away gesture escalates: the second
                    // press dismisses the stand-in, mirroring a second click
                    // on its close button.
                    self.dismiss_suspended(id);
                } else {
                    self.suspend_focused_window(suspend_restore_rect);
                }
            }
            Action::NudgeWindow(dir) => {
                if let Some(window) = self.focused_window().filter(|w| self.is_canvas_window(w))
                    && let Some(loc) = self.stage.position_of(&window)
                {
                    // Nudging re-anchors the window, invalidating any fill restore point.
                    self.stage.clear_fill(&window);
                    let step = self.config.nudge_step;
                    let (ux, uy) = dir.to_unit_vec();
                    let offset = (
                        (ux * step as f64).round() as i32,
                        (uy * step as f64).round() as i32,
                    );
                    let new_loc = loc + Point::from(offset);
                    self.map_window(window.clone(), new_loc, false);
                }
            }
            Action::PanViewport(dir) => {
                let Some(zoom) = self.with_output_state(|os| {
                    os.zoom_target = None;
                    os.zoom_animation_anchor = None;
                    os.overview_return = None;
                    os.zoom
                }) else {
                    return;
                };
                let step = self.config.pan_step / zoom;
                let (ux, uy) = dir.to_unit_vec();
                let delta: Point<f64, smithay::utils::Logical> =
                    Point::from((ux * step, uy * step));
                // Repeated key actions extend the destination instead of
                // restarting from the partially animated camera position.
                let target = self.camera_target().unwrap_or_else(|| self.camera()) + delta;
                self.set_camera_target(Some(target));
            }
            Action::CenterWindow => {
                if let Some(window) = self.focused_window().filter(|w| self.is_canvas_window(w)) {
                    self.navigate_to_window(&window, true);
                } else {
                    let center = self.viewport_center_canvas();
                    let closest = self
                        .stage
                        .windows()
                        .filter_map(|w| w.client())
                        .filter(|w| self.is_canvas_window(*w))
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
                } else if let Some((crate::input::DecoTarget::Suspended(s), _)) =
                    self.decoration_under(pos)
                {
                    // A stand-in occludes any client beneath it — center the
                    // stand-in by its visual frame, not the hidden client.
                    self.navigate_to_suspended(s.id);
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
                        let loc = self.stage.position_of(w).unwrap_or_default();
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
                    .stage
                    .windows()
                    .filter_map(|w| w.client())
                    .filter(|w| self.is_canvas_window(*w))
                    .map(|w| {
                        let loc = self.stage.position_of(w).unwrap_or_default();
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
                // The active output's fullscreen was already exited above (not
                // allowlisted), so any fullscreen entry the stage skips is on
                // another output — shown only on its own monitor, never a
                // target here.
                let anchor = self.cycle_anchor();
                let Some(window) = self
                    .stage
                    .cycle_step(*backward, anchor.as_ref())
                    .as_ref()
                    .and_then(|w| w.client())
                    .cloned()
                else {
                    return;
                };
                // Mark the focus change this navigate causes as cycle-initiated so
                // `focus_changed` freezes the history instead of committing.
                self.cycle_navigating = true;
                self.navigate_to_window(&window, false);
                self.cycle_navigating = false;
                // A truly modifier-less fire (gesture, bare wheel, IPC) has no key
                // release to end the session, so each fire is a single step with a
                // fresh anchor and commits now. If any real modifier is physically
                // held (alt+tab, mod+wheel with any mod_key), keep the session: the
                // modifier's release is a keyboard event and the keyboard check
                // commits then, so a session still can't dangle. Lock states
                // (num_lock, caps_lock) are ignored — `from_state` reads only the
                // real modifiers, so a num_lock-on keyboard still commits.
                let mods = self.seat.get_keyboard().unwrap().modifier_state();
                if Modifiers::from_state(&mods).is_empty() {
                    self.end_cycle();
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
                    let ret = self.with_output_state(|os| os.home_return.take()).flatten();
                    if let Some(ret) = ret {
                        let can_fullscreen = ret
                            .fullscreen_window
                            .as_ref()
                            .is_some_and(|w| self.stage.contains(w));
                        if can_fullscreen {
                            // Set camera/zoom directly — enter_fullscreen locks the viewport
                            self.set_camera(ret.camera);
                            self.set_zoom(ret.zoom);
                            self.enter_fullscreen(ret.fullscreen_window.as_ref().unwrap(), None);
                        } else {
                            let vc = self.usable_center_screen();
                            self.set_zoom_animation_anchor(
                                Point::from((
                                    ret.camera.x + vc.x / ret.zoom,
                                    ret.camera.y + vc.y / ret.zoom,
                                )),
                                vc,
                            );
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
                        self.set_zoom_animation_anchor(Point::from((0.0, 0.0)), vc);
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
                        .stage
                        .windows()
                        .filter(|w| self.is_canvas_window(*w))
                        .map(|w| {
                            let loc = self.stage.position_of(w).unwrap_or_default();
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
                        .stage
                        .windows()
                        .filter_map(|w| w.client())
                        .filter(|w| cluster.contains(*w))
                        .map(|w| {
                            let loc = self.stage.position_of(w).unwrap_or_default();
                            let size = w.geometry().size;
                            (loc, size)
                        });
                    if let Some(bbox) = canvas::all_windows_bbox(members) {
                        self.fit_to_bbox(bbox);
                    }
                }
            }
            Action::ToggleFullscreen => {
                let focused = self.focused_window().filter(|w| !w.is_widget());
                if was_fullscreen.is_some() && !self.is_fullscreen() {
                    // Input-layer code exited the active output's fullscreen
                    // before this ran — the toggle is done; don't re-enter or
                    // reach into another output's fullscreen.
                } else if let Some(output) = focused
                    .as_ref()
                    .and_then(|w| w.wl_surface())
                    .and_then(|s| self.find_fullscreen_output_for_surface(&s))
                {
                    // Toggle the focused window, not the active output: Mod+F
                    // exits a fullscreen window wherever it lives — keyboard focus
                    // can be on it while the pointer is on another monitor, where
                    // `is_fullscreen()` (active output) reads false.
                    self.exit_fullscreen_on(&output);
                } else if self.is_fullscreen() {
                    // The focused window isn't fullscreen (focus on a layer, a
                    // windowed window, or nothing) but the active output is.
                    self.exit_fullscreen();
                } else if let Some(window) = focused {
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
            Action::FillWindow => {
                if let Some(window) = self.focused_window().filter(|w| self.is_canvas_window(w)) {
                    self.toggle_fill_window(&window);
                }
            }
            Action::SendToOutput(dir) => {
                let Some(window) = self.focused_window().filter(|w| !w.is_widget()) else {
                    return;
                };
                let fullscreen = self.is_window_fullscreen(&window);
                // A fullscreen window is parked at its output's camera origin, so
                // the geometric output_for_window can mis-resolve it to another
                // monitor whose independent camera shows the same canvas region —
                // resolve it from the fullscreen entry instead. output_for_window
                // already short-circuits to the pin site's output for a pin.
                let from_output = if fullscreen {
                    window
                        .wl_surface()
                        .and_then(|s| self.find_fullscreen_output_for_surface(&s))
                } else {
                    self.output_for_window(&window)
                };
                let Some(from_output) = from_output else {
                    return;
                };
                let Some(target_output) = self.output_in_direction(&from_output, dir) else {
                    return;
                };

                if fullscreen {
                    // enter_fullscreen tears down the old output's fullscreen
                    // (restoring its camera/zoom and any suspended pin) and
                    // sets focus itself.
                    self.enter_fullscreen(&window, Some(target_output));
                } else if self.is_pinned(&window) {
                    // Pinned windows live outside the MRU history and are
                    // already focused.
                    self.send_pinned_to_output(&window, &target_output);
                } else {
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
                    // Relocating to another output re-anchors the window,
                    // invalidating any fill restore point.
                    self.stage.clear_fill(&window);
                    self.map_window(window.clone(), new_loc, true);
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
                        self.clear_edge_pan(&o);
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
    /// (state lives on the stage, not the rules).
    fn toggle_pin_to_screen(&mut self) {
        // Focus can linger on another output's fullscreen window (the guard in
        // execute_action only exits the active output's); pinning it would mix
        // two screen-space modes on one window.
        let Some(window) = self
            .focused_window()
            .filter(|w| !self.is_window_fullscreen(w))
        else {
            return;
        };
        if let Some(site) = self.stage.take_pin(&window) {
            // Unpin: convert the fixed screen position back to a canvas
            // location at the current camera/zoom — no visual jump.
            if let Some(output) = self.output_by_name(&site.output) {
                let (camera, zoom) = {
                    let os = crate::state::output_state(&output);
                    (os.camera, os.zoom)
                };
                let canvas = driftwm::canvas::screen_to_canvas(
                    driftwm::canvas::ScreenPos(site.screen_pos.to_f64()),
                    camera,
                    zoom,
                )
                .0
                .to_i32_round();
                self.map_window(window.clone(), canvas, true);
            }
        } else {
            // Pin at the window's current on-screen position on its output.
            let Some(output) = self.output_for_window(&window) else {
                return;
            };
            let Some(loc) = self.stage.position_of(&window) else {
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
            self.stage.drop_from_focus_history(&window);
            self.stage.set_pin(
                &window,
                driftwm::stage::PinnedSite {
                    output: output.name(),
                    screen_pos,
                },
            );
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
        self.set_zoom_animation_anchor(
            Point::from((
                saved_camera.x + vc.x / saved_zoom,
                saved_camera.y + vc.y / saved_zoom,
            )),
            vc,
        );
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
        self.set_zoom_animation_anchor(Point::from((bbox_cx, bbox_cy)), vc);
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
        self.set_zoom_animation_anchor(vc_canvas, vc);
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
