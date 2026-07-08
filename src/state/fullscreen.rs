use smithay::{
    desktop::Window,
    utils::{Logical, Point, Size},
    wayland::seat::WaylandFocus,
};

use super::{DriftWm, FocusTarget};
use driftwm::window_ext::WindowExt;

impl DriftWm {
    /// Resolve which output a window should fullscreen onto: a window-rule
    /// `output` wins, then the client-requested output, then the active output.
    /// Unknown output names fall through to the next choice.
    pub fn resolve_fullscreen_output(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        client_output: Option<smithay::output::Output>,
    ) -> Option<smithay::output::Output> {
        driftwm::config::applied_rule(surface)
            .and_then(|r| r.output)
            .and_then(|name| self.space.outputs().find(|o| o.name() == name).cloned())
            .or(client_output)
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
            self.exit_fullscreen_on(&output);
        }

        let viewport_size = super::output_logical_size(&output);
        let saved_location = self.stage.position_of(window).unwrap_or_default();

        // If the window is fit, capture the fit-era geometry so exit_fullscreen
        // restores it back to fit size with the fit state still intact. Otherwise
        // prefer the restore size over geometry to dodge Chromium's CSD shrink spiral.
        let saved_size = if self.stage.is_fit(window) {
            window.geometry().size
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

        window.enter_fullscreen_configure(viewport_size);

        // Lock the target output's viewport: stop all animations and momentum
        {
            let mut os = super::output_state(&output);
            os.zoom = 1.0;
            os.zoom_target = None;
            os.zoom_animation_center = None;
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

        // Place window at viewport origin and raise
        self.map_window(window.clone(), camera_i32, true);
        self.raise_window(window, true);
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
        let on_active_output = self.active_output().as_ref() == Some(&output);
        if on_active_output && let Some(wl_surface) = window.wl_surface() {
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
            let canvas_pos = pointer.current_location();
            let origin = self
                .space
                .element_location(window)
                .unwrap_or_default()
                .to_f64();
            pointer.motion(
                self,
                Some((FocusTarget(wl_surface.into_owned()), origin)),
                &smithay::input::pointer::MotionEvent {
                    location: canvas_pos,
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

        entry.window.exit_fullscreen_configure(entry.saved_size);

        // Restore window position, camera, zoom on the specific output
        self.map_window(entry.window.clone(), entry.saved_location, false);
        {
            let mut os = super::output_state(output);
            os.camera = ret.camera;
            os.zoom = ret.zoom;
            // Match the enter path's clean slate: drop any animation targets
            // that were set while the camera was locked (e.g. an activation
            // aimed at this output). The per-tick fullscreen clear handles the
            // steady state, but an exit in the same dispatch cycle — before any
            // tick — would otherwise animate a spurious jump to the stale target.
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_center = None;
        }
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
        self.update_output_from_camera();
        if was_pinned {
            self.sync_pinned_locs();
        }
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
                let mut os = super::output_state(&output);
                os.camera = ret.camera;
                os.zoom = ret.zoom;
            }
        }
        if !dead.is_empty() {
            self.update_output_from_camera();
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

    /// Exit fullscreen and remap the pointer to maintain its screen position
    /// under the restored camera/zoom. Returns the new canvas position.
    pub fn exit_fullscreen_remap_pointer(
        &mut self,
        canvas_pos: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        let old_camera = self.camera();
        let old_zoom = self.zoom();
        self.exit_fullscreen();
        let screen: Point<f64, Logical> = Point::from((
            (canvas_pos.x - old_camera.x) * old_zoom,
            (canvas_pos.y - old_camera.y) * old_zoom,
        ));
        let cur_zoom = self.zoom();
        let cur_camera = self.camera();
        let new_pos = Point::from((
            screen.x / cur_zoom + cur_camera.x,
            screen.y / cur_zoom + cur_camera.y,
        ));
        self.warp_pointer(new_pos);
        new_pos
    }
}
