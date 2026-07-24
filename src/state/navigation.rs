use std::time::Duration;

use crate::surface_tree::focus_belongs_to_window;
use driftwm::canvas::{CanvasPos, canvas_to_screen};
use driftwm::window_ext::WindowExt;
use smithay::{
    desktop::Window,
    output::Output,
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{IsAlive, Logical, Point},
    wayland::seat::WaylandFocus,
};

use super::{
    DriftWm, PendingClickNavigate, PendingPick, PickTarget, StageWindow, ZoomAnimationAnchor,
    output_state,
};

/// Max pointer travel (screen px) between press and release for a click to
/// still count as a click rather than a drag. Beyond it, no auto-navigate — a
/// text selection or slow drag inside a client never slides the canvas.
pub(crate) const CLICK_NAVIGATE_SLOP: f64 = 5.0;

/// Skip the activation pan only when the window is already fully inside its
/// home output's viewport. Any clipping → pan that output to bring it fully
/// into view; activation is a request to look at the window.
const ACTIVATION_VISIBLE_THRESHOLD: f64 = 1.0;

/// `visible_fraction` returns exactly 0.0 with no overlap, so an epsilon
/// separates "clipped to a hair" from "off screen entirely" without a
/// size-dependent cutoff. See `window_already_active`.
const ACTIVATION_ONSCREEN_THRESHOLD: f64 = f64::EPSILON;

impl DriftWm {
    /// Navigate the active output's viewport to center on a window: raise,
    /// focus, animate camera. When `reset_zoom` is true, zoom animates to 1.0
    /// (intentional navigation); otherwise the current zoom is preserved.
    pub fn navigate_to_window(&mut self, window: &Window, reset_zoom: bool) {
        if let Some(output) = self.active_output() {
            self.navigate_to_window_on(window, &output, reset_zoom);
        }
    }

    /// Navigate the active output's viewport to center on a stage element — the
    /// element-generic form of `navigate_to_window` / `center_on_suspended`.
    pub fn navigate_to_element(&mut self, element: &StageWindow, reset_zoom: bool) {
        match element {
            StageWindow::Client(w) => self.navigate_to_window(w, reset_zoom),
            StageWindow::Suspended(s) => self.center_on_suspended(s.id, reset_zoom),
        }
    }

    /// As `navigate_to_window`, but pans `output`'s camera instead of the
    /// active one. Lets xdg-activation reveal a window on the monitor it
    /// already lives on rather than dragging the active monitor's camera
    /// across to it.
    pub fn navigate_to_window_on(&mut self, window: &Window, output: &Output, reset_zoom: bool) {
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();

        // A fullscreen window lives in screen space, shown only on its own
        // output. From any other output it's invisible and parked at that
        // output's camera origin, so it isn't a navigation target there at all —
        // don't focus or pan to it (mirrors the hit-test isolation). On its own
        // output, focus it but leave the locked camera put.
        if let Some(fs_output) = window
            .wl_surface()
            .and_then(|s| self.find_fullscreen_output_for_surface(&s))
        {
            if &fs_output == output {
                self.raise_and_focus(window, serial);
            }
            return;
        }

        // A pinned window also lives in screen space: its canvas position is
        // re-derived from the pin's screen_pos on every camera update, so panning
        // can never bring more of it into view — it would only slide the rest of
        // the canvas underneath it.
        if self.stage.is_pinned(window) {
            self.raise_and_focus(window, serial);
            return;
        }

        self.raise_and_focus(window, serial);

        let target_zoom = self.navigation_target_zoom(output, reset_zoom);

        let window_loc = self.stage.position_of(window).unwrap_or_default();
        let window_size = window.geometry().size;
        let bar = self.window_ssd_bar(window);
        let vc = self.usable_center_screen_on(output);
        let target =
            driftwm::canvas::camera_to_center_window(window_loc, window_size, vc, target_zoom, bar);

        let window_center = self.window_visual_center(window).unwrap_or_else(|| {
            Point::from((
                window_loc.x as f64 + window_size.w as f64 / 2.0,
                window_loc.y as f64 + window_size.h as f64 / 2.0,
            ))
        });
        let mut os = output_state(output);
        // Disarms a pending zoom-to-fit return, same as a pan: the fit view is
        // a camera position, not a mode that navigation exits.
        os.overview_return = None;
        os.momentum.stop();
        os.zoom_animation_anchor = Some(ZoomAnimationAnchor {
            canvas: window_center,
            screen: vc,
        });
        os.camera_target = Some(target);
        os.zoom_target = Some(target_zoom);
    }

    /// The zoom a navigation animates to on `output`: 1.0 when `reset_zoom`
    /// (intentional navigation), else the zoom already in effect.
    pub(crate) fn navigation_target_zoom(&self, output: &Output, reset_zoom: bool) -> f64 {
        if reset_zoom {
            1.0
        } else {
            output_state(output).zoom
        }
    }

    /// Arm a completed-click auto-navigate for `window` at press time. No-op
    /// unless `auto_navigate_on_click` is enabled; the decision runs on release
    /// (see `resolve_click_navigate`). `press_pos` is in canvas coords. `defer`
    /// waits out the double-click window before panning (see
    /// `PendingClickNavigate::defer`).
    pub fn arm_click_navigate(
        &mut self,
        window: &Window,
        press_pos: Point<f64, Logical>,
        button: u32,
        defer: bool,
    ) {
        if !self.config.auto_navigate_on_click {
            return;
        }
        let Some(output) = self.active_output() else {
            return;
        };
        let press_screen_pos = canvas_to_screen(CanvasPos(press_pos), self.camera(), self.zoom()).0;
        self.pending_click_navigate = Some(PendingClickNavigate {
            window: window.clone(),
            press_screen_pos,
            button,
            output,
            defer,
        });
    }

    /// Resolve a click armed by `arm_click_navigate` at button release. When the
    /// armed button lifts within the click slop on the same output, pan to the
    /// window. A `defer` pending waits out the double-click window first (see
    /// `PendingClickNavigate::defer` and `fire_click_navigate`); otherwise the
    /// pan runs immediately. `release_pos` is in canvas coords.
    pub fn resolve_click_navigate(&mut self, button: u32, release_pos: Point<f64, Logical>) {
        let Some(pending) = self.pending_click_navigate.take() else {
            return;
        };
        // A different button lifted before the armed one — keep waiting for it.
        if pending.button != button {
            self.pending_click_navigate = Some(pending);
            return;
        }
        // A different output's screen coords aren't comparable to the press
        // (see `PendingClickNavigate::output`).
        if self.active_output().as_ref() != Some(&pending.output) {
            return;
        }
        if !pending.window.alive() {
            return;
        }
        let release_screen_pos =
            canvas_to_screen(CanvasPos(release_pos), self.camera(), self.zoom()).0;
        let dx = release_screen_pos.x - pending.press_screen_pos.x;
        let dy = release_screen_pos.y - pending.press_screen_pos.y;
        if dx * dx + dy * dy > CLICK_NAVIGATE_SLOP * CLICK_NAVIGATE_SLOP {
            return;
        }
        // Content clicks pan on release; only the SSD title bar defers, because
        // that's the sole target where the compositor owns a competing
        // double-click (fit). Deferring content clicks would slow every one to
        // protect a gesture the compositor doesn't own.
        if !pending.defer {
            self.fire_click_navigate(&pending.window);
            return;
        }
        // Defer the pan past the double-click window rather than panning now: a
        // pan on release #1 would slide the window out from under the title
        // bar's double-click-fit at click #2. Press #2 lands inside the delay
        // and cancels this. Visibility is deliberately re-checked at fire time —
        // it can change during the delay.
        if let Some(token) = self.click_navigate_timer.take() {
            self.loop_handle.remove(token);
        }
        let window = pending.window;
        let timer = Timer::from_duration(Duration::from_millis(
            crate::input::gestures::DOUBLE_TAP_WINDOW_MS,
        ));
        self.click_navigate_timer = self
            .loop_handle
            .insert_source(timer, move |_, _, data: &mut DriftWm| {
                data.click_navigate_timer = None;
                data.fire_click_navigate(&window);
                TimeoutAction::Drop
            })
            .ok();
    }

    /// Fire a deferred click-navigate scheduled by `resolve_click_navigate`. The
    /// double-click window has elapsed, so re-check the world before panning: the
    /// window must still be alive, still hold keyboard focus (a focus move during
    /// the delay — Alt-Tab, say — means the user went elsewhere; don't yank them
    /// back), and still be clipped.
    pub fn fire_click_navigate(&mut self, window: &Window) {
        if !window.alive() {
            return;
        }
        let focused = self
            .seat
            .get_keyboard()
            .and_then(|kb| kb.current_focus())
            .is_some_and(|focus| focus_belongs_to_window(&focus.0, window));
        if !focused {
            return;
        }
        if !self.window_fully_in_viewport(window) {
            self.navigate_to_window(window, false);
        }
    }

    /// Cancel any armed or deferred click-navigate. Clearing the deferred timer
    /// here is what makes double-click work — the second press lands inside the
    /// deferral and stops the pan before it starts.
    pub fn cancel_click_navigate(&mut self) {
        self.pending_click_navigate = None;
        if let Some(token) = self.click_navigate_timer.take() {
            self.loop_handle.remove(token);
        }
    }

    /// Whether canvas content is too small to interact with directly, so clicks
    /// pick windows instead of reaching them. A pure function of the active
    /// output's current zoom — no stored flag. Strict `<`, so the `0.0` sentinel
    /// (feature off) is handled by the `t > 0.0` guard.
    pub(crate) fn pick_mode(&self) -> bool {
        let t = self.config.zoom_interact_min;
        t > 0.0 && self.zoom() < t
    }

    /// Arm a pick-mode left click for `target` at press time (canvas coords).
    /// Fired on release by `resolve_pick` if the pointer barely moved and we're
    /// still below the threshold; cancelled by `maybe_promote_pick` on a drag.
    pub(crate) fn arm_pick(
        &mut self,
        target: PickTarget,
        press_pos: Point<f64, Logical>,
        button: u32,
    ) {
        let Some(output) = self.active_output() else {
            return;
        };
        let press_screen_pos = canvas_to_screen(CanvasPos(press_pos), self.camera(), self.zoom()).0;
        self.pending_pick = Some(PendingPick {
            target,
            press_screen_pos,
            button,
            output,
        });
    }

    /// Resolve a pick armed by `arm_pick` at button release: center the target
    /// (with `reset_zoom`, carrying the viewport back above the threshold, so a
    /// pick is a one-shot navigation, not a mode). A drag already cancelled the
    /// pick via `maybe_promote_pick`, so a surviving pending means the click
    /// stayed within slop.
    ///
    /// Must run *before* the release forwards at `on_pointer_button`'s tail: the
    /// `is_grabbed()` guard catches a gesture/edge-pan grab installed between
    /// press and release, and those grabs self-terminate inside `pointer.button`.
    pub(crate) fn resolve_pick(&mut self, button: u32) {
        let Some(pending) = self.pending_pick.take() else {
            return;
        };
        // A different button lifted — keep waiting for the armed one (mirrors
        // resolve_click_navigate's button-mismatch branch).
        if pending.button != button {
            self.pending_pick = Some(pending);
            return;
        }
        // A grab installed between press and release (3-finger swipe, popup,
        // edge-pan) owns the interaction; a stray center would reset zoom to 1.0.
        if self.seat.get_pointer().is_some_and(|p| p.is_grabbed()) {
            return;
        }
        // A release on a different output can't be compared to the press.
        if self.active_output().as_ref() != Some(&pending.output) {
            return;
        }
        // Zoomed above the threshold while holding — respect that deliberate
        // zoom rather than stomping it back to 1.0.
        if !self.pick_mode() {
            return;
        }
        match pending.target {
            PickTarget::Client(window) => {
                if window.alive() {
                    self.navigate_to_window(&window, true);
                }
            }
            PickTarget::Suspended(id) => {
                if self.find_suspended(id).is_some() {
                    self.center_on_suspended(id, true);
                }
            }
        }
    }

    /// Drop any armed pick — on a fresh press (a new interaction) or on
    /// promotion to a move. Mirrors `cancel_click_navigate`.
    pub(crate) fn cancel_pick(&mut self) {
        self.pending_pick = None;
    }

    /// Reveal and focus `window` on the output it already lives on, without
    /// dragging a different monitor's camera to it: fully visible on its home
    /// output → just focus; any clipping → pan that output into view. A window
    /// off every screen falls back to the active output.
    pub fn activate_window_output_local(&mut self, window: &Window) {
        let Some(home) = self.output_for_window(window) else {
            return;
        };
        // Exit fullscreen before activating a different window on that output
        // — otherwise the target gets focus while hidden behind the
        // fullscreen window, and the visibility check below runs against the
        // parked camera instead of the restored one.
        if self
            .fullscreen_window_on(&home)
            .is_some_and(|fs| &fs != window)
        {
            self.exit_fullscreen_on(&home);
        }
        if self.window_visible_at_least_on(window, &home, ACTIVATION_VISIBLE_THRESHOLD) {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            self.raise_and_focus(window, serial);
        } else {
            self.navigate_to_window_on(window, &home, self.config.zoom_reset_on_activation);
        }
    }

    /// True when `window` already holds keyboard focus and is at least partly on
    /// screen, i.e. an activation request for it asks for a state that already
    /// holds. A focused window panned fully off screen is *not* already active:
    /// activation still has somewhere to take the camera.
    pub fn window_already_active(&self, window: &Window) -> bool {
        let focused = self
            .seat
            .get_keyboard()
            .and_then(|kb| kb.current_focus())
            .is_some_and(|focus| focus_belongs_to_window(&focus.0, window));
        if !focused {
            return false;
        }
        // Pinned windows render in screen space (see `navigate_to_window_on`),
        // so visibility doesn't depend on the camera — skip the check below.
        if self.stage.is_pinned(window) {
            return true;
        }
        self.output_for_window(window).is_some_and(|home| {
            self.window_visible_at_least_on(window, &home, ACTIVATION_ONSCREEN_THRESHOLD)
        })
    }

    /// The window a fresh Alt-Tab cycle should treat as current — what
    /// `update_focus_history` would have recorded: keyboard focus (popup
    /// grabs included), with a focused modal standing in for its parent,
    /// since neither ever enters the focus history. `None` if focus isn't on
    /// a window. Capped against circular parents, like `topmost_modal_child`.
    pub fn cycle_anchor(&self) -> Option<super::StageWindow> {
        // A focused suspended window is the anchor even though it holds no seat
        // keyboard focus and never enters history: it isn't the history head, so
        // a fresh cycle returns to the head rather than stepping past it.
        if let Some(id) = self.gated_suspended_focus() {
            return self
                .stage
                .windows()
                .find(|w| w.suspended().is_some_and(|s| s.id == id))
                .cloned();
        }
        let focus = self.seat.get_keyboard()?.current_focus()?;
        let mut window = self
            .stage
            .windows()
            .find(|w| focus_belongs_to_window(&focus.0, *w))
            .and_then(|w| w.client())
            .cloned()?;
        for _ in 0..10 {
            if !window.is_modal() {
                break;
            }
            let Some(parent) = window
                .parent_surface()
                .and_then(|s| self.window_for_surface(&s))
            else {
                break;
            };
            window = parent;
        }
        Some(super::StageWindow::Client(window))
    }

    /// Dynamic minimum zoom based on the current window layout.
    /// Allows zooming out far enough to see all windows.
    pub fn min_zoom(&self) -> f64 {
        let viewport = self.get_usable_area().size;
        driftwm::canvas::dynamic_min_zoom(
            self.stage
                .windows()
                .filter(|w| self.is_canvas_window(*w))
                .map(|w| {
                    let loc = self.stage.position_of(w).unwrap_or_default();
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
            .stage
            .windows()
            .find(|w| focus_belongs_to_window(surface, *w))
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
            self.stage.push_focus(&window);
        }
    }

    /// Is the window's full snap rect (borders + title bar) inside the active
    /// output's usable area at the current camera and zoom? Returns `false`
    /// for widgets and unmapped windows — they have no meaningful viewport
    /// relation, so callers treat them as "needs movement" and skip them.
    pub fn window_fully_in_viewport<Q>(&self, w: &Q) -> bool
    where
        super::StageWindow: PartialEq<Q>,
    {
        let Some(elem) = self.stage.windows().find(|e| **e == *w) else {
            return false;
        };
        let Some(rect) = self.visual_frame_rect(elem) else {
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
    pub fn window_intersects_viewport_on<Q>(&self, w: &Q, output: &Output) -> bool
    where
        super::StageWindow: PartialEq<Q>,
    {
        let Some(elem) = self.stage.windows().find(|e| **e == *w) else {
            return false;
        };
        let Some(rect) = self.visual_frame_rect(elem) else {
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

    /// Representative point for the directional / nearest navigation searches:
    /// the visual-frame center (content plus the SSD bar strip above it). Works
    /// for a live window or a stand-in; equals `window_visual_center` for a
    /// client (both share `visual_frame_center`).
    pub fn nav_center(&self, w: &super::StageWindow) -> Point<f64, Logical> {
        let loc = self.stage.position_of(w).unwrap_or_default();
        let size = w.geometry().size;
        let bar = self.window_ssd_bar(w) as f64;
        super::visual_frame_center(loc, size, bar)
    }

    /// Nearest window (by canvas distance from `from_center`) that is at least
    /// partially visible on `output`. Excludes `exclude`; widgets, pinned, and
    /// fullscreen windows have no canvas snap rect, so `window_intersects_viewport_on`
    /// skips them.
    pub fn nearest_visible_window_on(
        &self,
        from_center: Point<f64, Logical>,
        output: &Output,
        exclude: &Window,
    ) -> Option<Window> {
        self.stage
            .windows()
            .filter_map(|w| w.client())
            .filter(|w| *w != exclude && self.window_intersects_viewport_on(*w, output))
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
        let destroyed_elem = StageWindow::Client(destroyed.clone());
        let cached_destroyed_rect = destroyed
            .wl_surface()
            .and_then(|s| self.stable_snap_rects.get(&s.id()).copied());
        let destroyed_rect =
            cached_destroyed_rect.or_else(|| self.snap_rect_for(&destroyed_elem))?;

        let mut rects = self.all_windows_with_snap_rects();
        if cached_destroyed_rect.is_some() {
            for (w, r) in &mut rects {
                if w == destroyed {
                    *r = destroyed_rect;
                }
            }
        }
        // Members may traverse through a suspended stand-in, so a client on the
        // far side of a stand-in it was snapped to still resolves as related.
        let cluster =
            driftwm::layout::cluster::cluster_of(&destroyed_elem, &rects, self.config.snap_gap);

        self.stage
            .focus_history()
            .iter()
            .filter_map(|w| w.client())
            .filter(|w| *w != destroyed)
            .find(|w| {
                let elem = StageWindow::Client((*w).clone());
                cluster.contains(&elem)
                    || self
                        .snap_rect_for(&elem)
                        .is_some_and(|r| destroyed_rect.overlaps(&r))
            })
            .cloned()
    }

    /// End Alt-Tab cycling: commit the selected window to focus history.
    pub fn end_cycle(&mut self) {
        self.stage.end_cycle();
    }
}
