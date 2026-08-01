use smithay::{
    desktop::Window,
    output::Output,
    utils::{Logical, Point, Rectangle, Size},
    wayland::seat::WaylandFocus,
};

use super::window_animation::{AnimSpace, ContentPolicy, GeometryRole};
use super::{DriftWm, FocusTarget};
use driftwm::window_ext::WindowExt;

impl DriftWm {
    pub fn is_fullscreen(&self) -> bool {
        self.active_output()
            .is_some_and(|o| self.is_output_fullscreen(&o))
    }

    pub fn is_output_fullscreen(&self, output: &Output) -> bool {
        self.stage.fullscreen_on(&output.name()).is_some()
    }

    /// Resolve which output a window should fullscreen onto. An already-fullscreen
    /// window re-asserting with no requested output stays on its current output;
    /// otherwise a window-rule `output` wins, then the client-requested output,
    /// then the window's pin site output, then the active output. Unknown output
    /// names fall through to the next choice.
    ///
    /// Fullscreen exit re-pins a pinned window to its pin output, so resolving
    /// there on entry keeps enter/exit symmetric.
    pub fn resolve_fullscreen_output(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        client_output: Option<smithay::output::Output>,
    ) -> Option<smithay::output::Output> {
        // Toolkits re-assert fullscreen (no requested output) on focus changes;
        // an already-fullscreen window must stay put. Falling through would
        // re-resolve down the chain and yank it to the pin or active (cursor)
        // output, undoing a send-to-output move.
        if client_output.is_none()
            && let Some(current) = self.find_fullscreen_output_for_surface(surface)
        {
            return Some(current);
        }

        driftwm::config::applied_rule(surface)
            .and_then(|r| r.output)
            .and_then(|name| self.space.outputs().find(|o| o.name() == name).cloned())
            .or(client_output)
            .or_else(|| {
                self.window_for_surface(surface)
                    .and_then(|w| self.stage.pin_of(&w).map(|site| site.output.clone()))
                    .and_then(|name| self.output_by_name(&name))
            })
            .or_else(|| self.active_output())
    }

    /// Enter fullscreen for the given window on `target_output` (falling back to
    /// the active output): lock that output's viewport, expand window to fill it.
    pub fn enter_fullscreen(
        &mut self,
        window: &Window,
        target_output: Option<smithay::output::Output>,
    ) {
        // Widgets (immovable canvas layers) never fullscreen. Pinned windows
        // do: they temporarily unpin into a normal fullscreen and re-pin on
        // exit (saved_pinned), so a PiP video can fill the screen and snap back.
        if window
            .wl_surface()
            .as_ref()
            .and_then(|s| driftwm::config::applied_rule(s))
            .is_some_and(|r| r.widget)
        {
            return;
        }
        // A stale requested output (disconnected between request and now) falls
        // back to the active output.
        let Some(output) = target_output
            .filter(|o| self.space.outputs().any(|x| x == o))
            .or_else(|| self.active_output())
        else {
            return;
        };

        // Re-asserting fullscreen while already fullscreen (some toolkits do
        // this on focus changes) must be idempotent. Falling through to the
        // exit+re-enter path would recapture `saved_size` from the window's
        // current geometry — the fullscreen viewport size, since the windowed
        // buffer was never committed in between — so a later exit "restores" to
        // full size and toggling can never recover. Keep the existing saved_*.
        if self
            .stage
            .fullscreen_on(&output.name())
            .is_some_and(|fs| &fs.window == window)
        {
            window.enter_fullscreen_configure(super::output_logical_size(&output));
            return;
        }

        // This window is already fullscreen on a *different* output: tear that
        // down first, so `saved_size` below is captured from its windowed
        // geometry (preferring the stored restore size) rather than the
        // fullscreen viewport — same best-effort basis as the idempotent guard.
        if let Some(other) = window
            .wl_surface()
            .and_then(|s| self.find_fullscreen_output_for_surface(&s))
            && other != output
        {
            self.exit_fullscreen_on(&other);
        }

        // A different window is taking over this output's fullscreen: exit first.
        // Must target `output`, not the active output — they can differ when
        // fullscreen is requested on a specific monitor.
        if self.is_output_fullscreen(&output) {
            // Same reasoning: the displaced window's strip rides its exit
            // configure instead of arriving as a second back-to-back configure
            // once `activate_riding_batch` deactivates it below.
            if let Some(displaced) = self
                .stage
                .fullscreen_on(&output.name())
                .map(|fs| fs.window.clone())
            {
                displaced.set_activated(false);
            }
            self.exit_fullscreen_on(&output);
        }

        // The exit this supersedes can be the same-window cross-output one just
        // above, not only a prior fullscreen/fit/fill exit.
        self.drop_owed_recenter(window);

        let viewport_size = super::output_logical_size(&output);
        let saved_location = self.stage.position_of(window).unwrap_or_default();
        // The pre-fullscreen visual footprint, and the pin site if any, so the
        // entry animation can grow from where the window actually was.
        let windowed_size = window.geometry().size;
        let pre_pin_site = self.stage.pin_of(window).cloned();

        // Fit and fill membership both survive fullscreen, so the exit has to
        // hand the window back the rect the stage still believes it holds:
        // capture a size from the same era as the `saved_location` above, not
        // the pre-fit/pre-fill `restore_size`, which would pair a stale size
        // with a current position. Both read the size last *configured*, since
        // both map to their new position without waiting for the ack: a
        // fullscreen pressed into that gap still finds committed geometry at
        // the pre-fit/pre-fill size, at the fitted/filled position. Otherwise
        // prefer the restore size over geometry to dodge Chromium's CSD shrink
        // spiral.
        let saved_size = if self.stage.is_fill(window) || self.stage.is_fit(window) {
            super::configured_window_size(window)
        } else {
            self.stage
                .restore_size(window)
                .unwrap_or_else(|| window.geometry().size)
        };

        let (saved_camera, saved_zoom) = {
            let os = super::output_state(&output);
            (os.camera, os.zoom)
        };

        // A game that maps straight into fullscreen commits its first buffer at
        // a throwaway default before it learns it's fullscreen, and that size is
        // frozen into the restore size (X11 clients via xwayland-satellite often map
        // 1x1 first). Restoring it verbatim on exit would shrink the window to
        // nothing, so a captured size below the client's min — or a floor, since
        // many clients declare none — falls back to a half-viewport default.
        const MIN_RESTORE_FLOOR: i32 = 100;
        let cons = crate::grabs::SizeConstraints::for_window(window);
        let (saved_size, saved_location) = if saved_size.w < cons.min.w.max(MIN_RESTORE_FLOOR)
            || saved_size.h < cons.min.h.max(MIN_RESTORE_FLOOR)
        {
            let size = Size::from((
                (viewport_size.w / 2).max(cons.min.w),
                (viewport_size.h / 2).max(cons.min.h),
            ));
            let loc = Point::from((
                (saved_camera.x + viewport_size.w as f64 / 2.0 / saved_zoom) as i32 - size.w / 2,
                (saved_camera.y + viewport_size.h as f64 / 2.0 / saved_zoom) as i32 - size.h / 2,
            ));
            (size, loc)
        } else {
            (saved_size, saved_location)
        };

        // Unpin into the fullscreen viewport; exit_fullscreen_on re-pins.
        let saved_pinned = self.stage.take_pin(window);

        self.stage
            .set_fullscreen(&output.name(), window.clone(), saved_location, saved_size);
        super::output_state(&output).fullscreen_return = Some(super::FullscreenReturn {
            camera: saved_camera,
            zoom: saved_zoom,
            pinned: saved_pinned,
        });

        // Stage Activated before the fullscreen configure so it rides that send
        // — map/raise below don't carry activation, so a window that fullscreens
        // straight from a background placement (deferred fullscreen request)
        // would otherwise never receive the hint. Any displaced peer is
        // deactivated on the wire here.
        self.activate_riding_batch(window);
        window.enter_fullscreen_configure(viewport_size);

        // Lock the target output's viewport: stop all animations and momentum
        {
            let mut os = super::output_state(&output);
            os.zoom = 1.0;
            os.zoom_target = None;
            os.zoom_animation_anchor = None;
            os.camera_target = None;
            os.momentum.stop();
            os.overview_return = None;
        }
        // Top/Bottom layers are hidden during fullscreen — reset stale pointer state
        self.pointer_over_layer = false;

        // Snap camera to integer for pixel-perfect alignment. Write the
        // output's state directly: `set_camera` refuses to move a fullscreen
        // output (the window is pinned to its camera-origin), and this output's
        // stage fullscreen entry is already set above.
        let camera_i32 = super::output_state(&output).camera.to_i32_round();
        super::output_state(&output).camera =
            Point::from((camera_i32.x as f64, camera_i32.y as f64));

        // Place window at viewport origin and raise; activation already rode
        // the fullscreen configure staged above.
        self.map_window(window.clone(), camera_i32, false);

        // Grow from the pre-fullscreen visual rect, expressed in the now-locked
        // viewport (camera = camera_i32, zoom 1). Entering fullscreen unpins, so
        // the entry is Canvas and chases the camera-origin target.
        let (from_loc, from_size): (Point<i32, Logical>, Size<i32, Logical>) = if let Some(site) =
            pre_pin_site.as_ref()
        {
            (
                Point::from((
                    camera_i32.x + site.screen_pos.x,
                    camera_i32.y + site.screen_pos.y,
                )),
                Size::from((windowed_size.w.max(1), windowed_size.h.max(1))),
            )
        } else {
            (
                Point::from((
                    camera_i32.x
                        + ((saved_location.x as f64 - saved_camera.x) * saved_zoom).round() as i32,
                    camera_i32.y
                        + ((saved_location.y as f64 - saved_camera.y) * saved_zoom).round() as i32,
                )),
                Size::from((
                    (windowed_size.w as f64 * saved_zoom).round().max(1.0) as i32,
                    (windowed_size.h as f64 * saved_zoom).round().max(1.0) as i32,
                )),
            )
        };
        self.begin_geometry_animation_seeded(
            window,
            Rectangle::new(from_loc.to_f64(), from_size.to_f64()),
            AnimSpace::Canvas,
            Some(viewport_size),
            GeometryRole::FullscreenEntry {
                was_pinned: pre_pin_site.is_some(),
            },
            ContentPolicy::Cap,
            // Where this chase lands, for a window fullscreened in the same
            // commit that mapped it: it fades in already fullscreen rather than
            // showing the placement rect it was never meant to have.
            Some(camera_i32),
        );

        self.raise_window(window, false);
        self.enforce_below_windows();
        self.update_output_from_camera();

        // Make the fullscreen window the keyboard-focus intent (the recompute
        // still yields to an exclusive layer if one is mapped) and force
        // pointer focus below. Without pointer focus, pointer constraints (e.g.
        // game cursor lock) activate on whatever surface had focus before.
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let focus = window.wl_surface().map(|s| FocusTarget(s.into_owned()));
        self.set_window_focus(focus, serial);

        // Pointer focus + constraint (game cursor-lock) only apply when the
        // cursor is on the fullscreen output. For a fullscreen on a different
        // monitor, don't lock the pointer to a surface it isn't over — the
        // constraint activates naturally when the pointer arrives there.
        //
        // Never while locked: the locked input path sends `button` and `axis`
        // straight at `current_focus()`, so pointing it at the fullscreening app
        // here would hand that app every click and scroll until the next
        // physical motion, and activate its cursor lock under the lock screen.
        let on_active_output = self.active_output().as_ref() == Some(&output);
        if on_active_output
            && !self.session_lock.is_locked()
            && let Some(wl_surface) = window.wl_surface()
        {
            let pointer = self.seat.get_pointer().unwrap();
            // Deactivate any constraint on the old focused surface
            if let Some(old) = pointer.current_focus() {
                smithay::wayland::pointer_constraints::with_pointer_constraint(
                    &old.0,
                    &pointer,
                    |c| {
                        if let Some(c) = c
                            && c.is_active()
                        {
                            c.deactivate();
                        }
                    },
                );
            }
            // Keep the cursor at the same on-screen spot across the zoom park:
            // canvas position alone would land elsewhere (or off-output) once
            // zoom != 1. Same geometric-visibility check as
            // `restore_fullscreen_view` on the exit side.
            let canvas_pos = pointer.current_location();
            let in_saved_view = canvas_pos.x >= saved_camera.x
                && canvas_pos.x < saved_camera.x + viewport_size.w as f64 / saved_zoom
                && canvas_pos.y >= saved_camera.y
                && canvas_pos.y < saved_camera.y + viewport_size.h as f64 / saved_zoom;
            let new_pos = if in_saved_view {
                Point::from((
                    (canvas_pos.x - saved_camera.x) * saved_zoom + camera_i32.x as f64,
                    (canvas_pos.y - saved_camera.y) * saved_zoom + camera_i32.y as f64,
                ))
            } else {
                canvas_pos
            };
            let origin = self.stage.position_of(window).unwrap_or_default().to_f64();
            pointer.motion(
                self,
                Some((FocusTarget(wl_surface.into_owned()), origin)),
                &smithay::input::pointer::MotionEvent {
                    location: new_pos,
                    serial,
                    time: self.start_time.elapsed().as_millis() as u32,
                },
            );
            pointer.frame(self);
            self.maybe_activate_pointer_constraint();
        }
    }

    /// Exit fullscreen on the active output: restore window position, camera, and zoom.
    pub fn exit_fullscreen(&mut self) {
        let Some(output) = self.active_output() else {
            return;
        };
        self.exit_fullscreen_on(&output);
    }

    /// Exit fullscreen on a specific output.
    pub fn exit_fullscreen_on(&mut self, output: &smithay::output::Output) {
        // Take both halves unconditionally before bailing — a one-sided take
        // would strand the other half if they ever diverged.
        let ret = super::output_state(output).fullscreen_return.take();
        let entry = self.stage.take_fullscreen(&output.name());
        debug_assert_eq!(
            ret.is_some(),
            entry.is_some(),
            "fullscreen halves diverged for {}",
            output.name()
        );
        let (Some(ret), Some(entry)) = (ret, entry) else {
            return;
        };

        // Capture the currently presented rect before the stage/camera change,
        // so reversing a still-running entry starts from the visual rather than
        // the fullscreen target. The viewport is locked (zoom 1) here.
        let parked_camera = super::output_state(output).camera;
        let parked_zoom = super::output_state(output).zoom;
        let parked_loc = self
            .stage
            .position_of(&entry.window)
            .unwrap_or_else(|| parked_camera.to_i32_round());
        let cur_screen: Option<(Point<f64, Logical>, Size<f64, Logical>)> =
            self.stage.id_of(&entry.window).map(|id| {
                // The chase rect, never the drawn one: an open fade's shrink is
                // carried onto the exit's chase and re-applied when it is drawn,
                // so seeding from the drawn rect would scale the picture twice.
                let v = self.geometry_seed(id, parked_loc, entry.window.geometry().size);
                (
                    Point::from((
                        (v.loc.x - parked_camera.x) * parked_zoom,
                        (v.loc.y - parked_camera.y) * parked_zoom,
                    )),
                    Size::from((v.size.w * parked_zoom, v.size.h * parked_zoom)),
                )
            });

        entry.window.exit_fullscreen_configure(entry.saved_size);

        // Restore the window's position; the camera and zoom follow below.
        let bar = self.window_ssd_bar(&entry.window) as f64;
        let target_center = super::visual_frame_center(entry.saved_location, entry.saved_size, bar);
        self.establish_exit_placement(
            &entry.window,
            entry.saved_location,
            entry.saved_size,
            target_center,
            false,
        );

        // Re-pin if it was pinned before fullscreen, then snap its Space loc
        // back to screen_pos (update_output_from_camera's sync only fires on a
        // camera change, which restoring the saved camera may not be).
        let was_pinned = ret.pinned.is_some();
        if let Some(site) = ret.pinned {
            // The window may have entered the MRU history while fullscreen
            // (it wasn't pinned then); re-pinning takes it back out.
            self.stage.drop_from_focus_history(&entry.window);
            self.stage.set_pin(&entry.window, site);
        }
        self.restore_fullscreen_view(output, ret.camera, ret.zoom);
        if was_pinned {
            self.sync_pinned_locs();
        }

        // Shrink from the (locked) fullscreen visual back toward the restored
        // window. A re-pinned window renders in screen space, so its entry is
        // Screen; a normal one converts the screen rect into restored canvas.
        if let Some((screen_loc, screen_size)) = cur_screen {
            let (seed, space) = if let Some(site) = self.stage.pin_of(&entry.window) {
                (
                    Rectangle::new(screen_loc, screen_size),
                    AnimSpace::Screen(site.output.clone()),
                )
            } else {
                let seed_loc = Point::from((
                    ret.camera.x + screen_loc.x / ret.zoom,
                    ret.camera.y + screen_loc.y / ret.zoom,
                ));
                let seed_size = Size::from((screen_size.w / ret.zoom, screen_size.h / ret.zoom));
                (Rectangle::new(seed_loc, seed_size), AnimSpace::Canvas)
            };
            if let Some(client) = entry.window.client() {
                self.begin_geometry_animation_seeded(
                    client,
                    seed,
                    space,
                    Some(entry.saved_size),
                    GeometryRole::FullscreenExit {
                        output: output.name(),
                    },
                    ContentPolicy::Cap,
                    None,
                );
            }
        }
    }

    /// Restore an output's camera/zoom after fullscreen ends. Drops any
    /// animation targets set while the camera was locked (e.g. an activation
    /// aimed at this output) — the per-tick fullscreen clear stops once the
    /// stage entry is gone, and a stale target would animate a spurious jump.
    ///
    /// Keeps the cursor at the same on-screen spot when it was visible in the
    /// parked view: its canvas position alone lands on a different screen
    /// point whenever the restored zoom isn't the parked 1.0. Visibility is
    /// judged geometrically (cursor inside the parked viewport) rather than by
    /// pointer routing — touch flips `focused_output` while the mouse cursor
    /// may sit on another output. Must not run inside a pointer-grab callback
    /// (the warp's pointer calls would deadlock); every caller is plain
    /// dispatch (NavigateGrab defers its action for this).
    pub(crate) fn restore_fullscreen_view(
        &mut self,
        output: &smithay::output::Output,
        camera: Point<f64, Logical>,
        zoom: f64,
    ) {
        let (parked_camera, parked_zoom) = {
            let os = super::output_state(output);
            (os.camera, os.zoom)
        };
        {
            let mut os = super::output_state(output);
            os.camera = camera;
            os.zoom = zoom;
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_anchor = None;
        }
        self.update_output_from_camera();

        let pointer = self.seat.get_pointer().unwrap();
        let canvas_pos = pointer.current_location();
        let size = super::output_logical_size(output);
        let in_parked_view = canvas_pos.x >= parked_camera.x
            && canvas_pos.x < parked_camera.x + size.w as f64 / parked_zoom
            && canvas_pos.y >= parked_camera.y
            && canvas_pos.y < parked_camera.y + size.h as f64 / parked_zoom;
        if in_parked_view {
            let new_pos = Point::from((
                (canvas_pos.x - parked_camera.x) * parked_zoom / zoom + camera.x,
                (canvas_pos.y - parked_camera.y) * parked_zoom / zoom + camera.y,
            ));
            if new_pos != canvas_pos {
                self.warp_pointer(new_pos);
            }
        }

        // Exiting fullscreen can restore a hidden bar beneath a stationary
        // cursor; `pointer_over_layer` and smithay's focus are otherwise only
        // refreshed by pointer motion, and a stale flag would route the next
        // press/scroll over the bar to the canvas. Same plain-dispatch
        // constraint as the warp above: this makes pointer calls, so no caller
        // may sit inside a pointer-grab callback.
        self.refresh_pointer_focus();
    }

    /// Tear down any fullscreen entry whose window is dead, restoring that
    /// output's camera/zoom. The exit paths handle live windows; this covers
    /// a client that crashed while fullscreen, whose entry would otherwise
    /// keep the camera parked forever.
    pub fn reap_dead_fullscreen(&mut self) {
        use smithay::utils::IsAlive;
        let dead: Vec<String> = self
            .stage
            .fullscreen_entries()
            .filter(|(_, fs)| !fs.window.alive())
            .map(|(name, _)| name.clone())
            .collect();
        for name in &dead {
            self.stage.take_fullscreen(name);
            let Some(output) = self.output_by_name(name) else {
                continue;
            };
            // Two statements, not a let-chain: a chain scrutinee's MutexGuard
            // lives to the end of the whole `if`, deadlocking the re-lock.
            let ret = super::output_state(&output).fullscreen_return.take();
            if let Some(ret) = ret {
                self.restore_fullscreen_view(&output, ret.camera, ret.zoom);
            }
        }
    }

    /// Re-configure the fullscreen window (if any) on this output to the new
    /// viewport size after a mode change. Without this, a fullscreen game
    /// keeps rendering at the old resolution and leaves a stale strip until
    /// the client redraws on its own.
    pub fn resize_fullscreen_for_output(
        &mut self,
        output: &smithay::output::Output,
        new_size: smithay::utils::Size<i32, smithay::utils::Logical>,
    ) {
        let Some(fs) = self.stage.fullscreen_on(&output.name()) else {
            return;
        };
        fs.window.enter_fullscreen_configure(new_size);
    }

    /// Find which output holds a fullscreen window by its surface.
    pub fn find_fullscreen_output_for_surface(
        &self,
        wl_surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Option<smithay::output::Output> {
        let name = self
            .stage
            .fullscreen_entries()
            .find(|(_, fs)| fs.window.wl_surface().as_deref() == Some(wl_surface))
            .map(|(name, _)| name.clone())?;
        self.output_by_name(&name)
    }
}
