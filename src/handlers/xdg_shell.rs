use std::cell::RefCell;

use crate::grabs::{MoveGrab, ResizeState, ResizeSurfaceGrab};
use crate::state::{DriftWm, FocusTarget, PopupGrabState, StageWindow, output_state};
use crate::surface_tree::focus_belongs_to_toplevel;
use driftwm::window_ext::WindowExt;
use smithay::{
    delegate_xdg_shell,
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Window,
        find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
    },
    input::pointer::{CursorIcon, CursorImageStatus, Focus, GrabStartData},
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Resource,
            protocol::{wl_output, wl_seat},
        },
    },
    utils::{Point, Rectangle, Serial},
    wayland::{
        compositor::with_states,
        input_method::InputMethodSeat,
        seat::WaylandFocus,
        shell::{
            wlr_layer::KeyboardInteractivity,
            xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState},
        },
    },
};

impl XdgShellHandler for DriftWm {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        tracing::info!("New toplevel surface");
        let wl_surface = surface.wl_surface().clone();
        let window = Window::new_wayland_window(surface);

        // Place at screen center (no size offset — size unknown until first commit).
        // The pending_center set will trigger proper centering once size is known.
        let pos = self
            .active_output()
            .and_then(|o| self.space.output_geometry(&o))
            .map(|geo| {
                let cam = self.camera();
                let z = self.zoom();
                (
                    (cam.x + geo.size.w as f64 / (2.0 * z)) as i32,
                    (cam.y + geo.size.h as f64 / (2.0 * z)) as i32,
                )
            })
            .unwrap_or((0, 0));

        // Snapshot the focus anchor so `auto_placement_pos` can anchor a new
        // window beside it once mapping steals focus; see `focused_anchor_element`.
        let prev_focus = self.focused_anchor_element();
        self.auto_anchor_snapshot
            .insert(wl_surface.clone(), prev_focus);

        // Initial configure is deferred to ensure_initial_configure in
        // compositor.rs first-commit handler so rule-resolved state (size,
        // decoration_mode, tiled) can be batched into a single configure.
        // Sending one here would produce a configure with unresolved state,
        // and a second on first commit — SDL2/SCTK clients have historically
        // desynced on back-to-back initial configures.
        self.map_window(window.clone(), pos.into(), false);
        self.raise_window(&window, false);
        self.enforce_below_windows();
        // Don't focus here: a pre-buffer wl_keyboard.enter is unusable, and
        // set_focus is a no-op when the target is unchanged, so focusing now
        // would trap the client unfocused (the on-commit re-focus does nothing).
        // Focus is delivered once mapped, on first commit; activation is
        // likewise deferred to that commit so it can ride the placement
        // configure instead of arriving as a premature standalone one.
        self.pending_center.insert(wl_surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        tracing::info!("New popup surface");

        let popup = PopupKind::Xdg(surface);
        self.unconstrain_popup(&popup);

        if let Err(err) = self.popups.track_popup(popup) {
            tracing::warn!("error tracking popup: {err}");
        }
    }

    fn grab(&mut self, surface: PopupSurface, _seat: wl_seat::WlSeat, serial: Serial) {
        tracing::info!("Popup grab requested");
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };

        // Reject grabs whose root isn't the focused surface — xdg-shell requires
        // popup grabs to follow user input on the focused window. A misbehaving
        // (or hostile) client otherwise steals keyboard focus from a hidden window.
        if !self.popup_grab_allowed(&root) {
            let _ = smithay::desktop::PopupManager::dismiss_popup(&root, &kind);
            return;
        }

        let root_focus = FocusTarget(root.clone());
        let Ok(mut grab) = self.popups.grab_popup(root_focus, kind, &self.seat, serial) else {
            return;
        };

        let keyboard = self.seat.get_keyboard().unwrap();
        let pointer = self.seat.get_pointer().unwrap();

        // Give a pointer grab only when the root can't hold the keyboard — a
        // layer surface with no keyboard interactivity, or any popup while an
        // input method owns the keyboard. A keyboard grab there would be torn
        // down on the next focus recompute, since focus can't land on the root.
        let has_keyboard_grab = !self.seat.input_method().keyboard_grabbed()
            && self.layer_interactivity(&root) != Some(KeyboardInteractivity::None);

        // Refuse a grab that would clobber an unrelated live grab. A nested
        // submenu is fine — its serial (or its parent's, via previous_serial)
        // matches the existing grab.
        let keyboard_mismatch = has_keyboard_grab
            && keyboard.is_grabbed()
            && !(keyboard.has_grab(serial)
                || grab.previous_serial().is_none_or(|s| keyboard.has_grab(s)));
        let pointer_mismatch = pointer.is_grabbed()
            && !(pointer.has_grab(serial)
                || grab.previous_serial().is_none_or(|s| pointer.has_grab(s)));
        if keyboard_mismatch || pointer_mismatch {
            grab.ungrab(PopupUngrabStrategy::All);
            return;
        }

        if has_keyboard_grab {
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        self.popup_grab = Some(PopupGrabState {
            root,
            grab,
            has_keyboard_grab,
        });
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.positioner = positioner;
        });
        self.unconstrain_popup(&PopupKind::Xdg(surface.clone()));
        surface.send_repositioned(token);
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        output: Option<wl_output::WlOutput>,
    ) {
        let wl_surface = surface.wl_surface().clone();
        let client_output = output.and_then(|wo| smithay::output::Output::from_resource(&wo));
        // Defer until the first sized commit — geometry is still (0,0)
        // here, which would poison `saved_size`, and the initial-commit
        // positioning block would clobber the fullscreen map.
        if self.pending_center.contains(&wl_surface) {
            self.pending_fullscreen.insert(wl_surface, client_output);
            return;
        }
        let window = self.window_for_surface(&wl_surface);
        if let Some(window) = window {
            let target = self.resolve_fullscreen_output(&wl_surface, client_output);
            self.enter_fullscreen(&window, target);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.pending_fullscreen.remove(surface.wl_surface());
        if let Some(output) = self.find_fullscreen_output_for_surface(surface.wl_surface()) {
            self.exit_fullscreen_on(&output);
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        if self.pending_center.contains(&wl_surface) {
            self.pending_fit.insert(wl_surface);
            return;
        }
        let window = self.window_for_surface(&wl_surface);
        if let Some(window) = window {
            self.decoration_fit(&window);
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        self.pending_fit.remove(&wl_surface);
        let window = self.window_for_surface(&wl_surface);
        if let Some(window) = window {
            self.decoration_unfit(&window);
        }
    }

    // driftwm has no minimize concept, but a client that minimizes itself
    // stalls: xdg-shell carries no "minimized" state, so the toolkit stops
    // drawing and only clears its internal minimized flag on a configure with
    // `Activated`. The exclusive-activation gate would suppress that configure
    // when the window is already the activated one (idempotent, sends nothing),
    // so force `Activated` unconditionally here to un-stick the client.
    fn minimize_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        // Collect first to avoid holding an immutable borrow on the stage
        let window = self.window_for_surface(&wl_surface);
        // Restore the per-output camera/zoom first if the destroyed window
        // was fullscreen. Keyed by surface, not the window lookup above: on
        // the crash path `Space::refresh` may already have dropped the dead
        // window, and skipping this would leak the fullscreen entry and leave
        // the camera parked forever. (The focus chooser below also evaluates
        // visibility against the home output's camera, which stays parked
        // until this runs.)
        let fs_output = self.find_fullscreen_output_for_surface(&wl_surface);
        // Captured before the teardown drops the entry: a markless
        // (suspend_on_close) conversion of a fullscreen self-close must seat
        // the stand-in at the pre-fullscreen rect, not the fullscreen-sized
        // buffer parked at the camera origin.
        let fullscreen_restore_rect = fs_output.as_ref().and_then(|output| {
            self.stage
                .fullscreen_on(&output.name())
                .map(|entry| Rectangle::new(entry.saved_location, entry.saved_size))
        });
        if let Some(ref output) = fs_output {
            self.stage.take_fullscreen(&output.name());
            // Two statements, not one `if let`: the scrutinee's MutexGuard
            // lives to the end of the body, deadlocking the re-lock inside.
            let ret = output_state(output).fullscreen_return.take();
            if let Some(ret) = ret {
                self.restore_fullscreen_view(output, ret.camera, ret.zoom);
            }
        }
        // Belt and suspenders: a dead window whose surface no longer resolves
        // can't match above; sweep any fullscreen entry it left behind.
        self.reap_dead_fullscreen();

        // A live suspend mark, or an eligible client-initiated close under
        // `suspend_on_close`, converts the window into a compositor-drawn
        // stand-in in place instead of destroying it. Runs before the
        // focus-follow / unmap path below and before `cleanup_surface_state`,
        // which the conversion relies on to purge the surface-keyed state.
        if let Some(window) = &window
            && let Some(conv) =
                self.resolve_suspend_conversion(&wl_surface, window, fullscreen_restore_rect)
        {
            self.convert_to_suspended(window, &wl_surface, conv);
            self.cleanup_surface_state(&wl_surface);
            return;
        }

        if let Some(ref window) = window {
            // Pick a window to follow when the destroyed one was focused.
            // Priority: explicit parent (xdg_toplevel.set_parent), then the
            // MRU history entry that's spatially related (cluster member or
            // overlap), then plain MRU. Spatial fallback covers transient/
            // OAuth windows that auto-placement snapped near the launcher
            // but don't carry a parent_surface relation.
            let follow = window
                .parent_surface()
                .and_then(|ps| {
                    let parent = self.window_for_surface(&ps)?;
                    Some(
                        self.topmost_modal_child(&parent)
                            .filter(|mc| mc != window)
                            .unwrap_or(parent),
                    )
                })
                .or_else(|| self.first_spatially_related_in_history(window));

            // When auto-navigation is off, dropping an off-screen follow target
            // guarantees focus never lands somewhere the user can't see.
            let follow = follow
                .filter(|t| self.config.auto_navigate_on_close || self.window_fully_in_viewport(t));

            let keyboard = self.seat.get_keyboard().unwrap();
            let current_focus = keyboard.current_focus();
            let no_keyboard_focus = current_focus.is_none();
            let focus_on_this_toplevel = current_focus
                .as_ref()
                .is_some_and(|f| focus_belongs_to_toplevel(&f.0, &wl_surface));
            let was_last_focused = self
                .stage
                .focus_history()
                .first()
                .is_some_and(|last_focused| last_focused == window);
            // A focused suspended window holds no seat focus and isn't in
            // history, so `no_keyboard_focus`/`was_last_focused` would otherwise
            // fire and steal focus from it when any unrelated window is
            // destroyed. The user is looking at the suspended window — leave it.
            let focus_is_suspended = matches!(
                self.window_focus,
                Some(crate::state::FocusIntent::Suspended(_))
            );
            if !focus_is_suspended
                && (focus_on_this_toplevel || was_last_focused || no_keyboard_focus)
            {
                if let Some(target) = follow {
                    // Pan only if the follow target isn't already fully on
                    // screen — set_focus alone is enough when the user can
                    // already see where focus is going.
                    if self.window_fully_in_viewport(&target) {
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        self.raise_and_focus(&target, serial);
                    } else {
                        self.navigate_to_window(&target, false);
                    }
                } else {
                    // No follow target. Pick first MRU entry; if it's
                    // completely off the destroyed window's home output, fall
                    // back to the nearest visible window (by canvas distance
                    // from the destroyed window's center). If nothing is
                    // visible, clear focus rather than focus the void.
                    //
                    // Prefer the fullscreen output when the destroyed window
                    // was fullscreen: after the camera restore above, the
                    // window's location is still pinned to the old fullscreen
                    // camera, so output_for_window may misroute to the
                    // cursor's monitor.
                    let home = fs_output
                        .clone()
                        .or_else(|| self.output_for_window(window))
                        .or_else(|| self.active_output());
                    let mru = self
                        .stage
                        .focus_history()
                        .iter()
                        .filter_map(|w| w.client())
                        .find(|w| *w != window)
                        .cloned();
                    let target = match (home.as_ref(), mru) {
                        (Some(out), Some(m)) if self.window_intersects_viewport_on(&m, out) => {
                            Some(m)
                        }
                        (Some(out), _) => {
                            let from = self.window_visual_center(window).unwrap_or_else(|| {
                                let loc = self.stage.position_of(window).unwrap_or_default();
                                let size = window.geometry().size;
                                Point::from((
                                    loc.x as f64 + size.w as f64 / 2.0,
                                    loc.y as f64 + size.h as f64 / 2.0,
                                ))
                            });
                            self.nearest_visible_window_on(from, out, window)
                        }
                        (None, _) => None,
                    };

                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    if let Some(target) = target {
                        self.raise_and_focus(&target, serial);
                    } else {
                        self.set_window_focus(None, serial);
                    }
                }
            }
            self.unmap_window(window);
            // The window may have sat under the cursor; re-target pointer focus
            // now that it's gone so clicks don't fall into the destroyed surface.
            self.refresh_pointer_focus();
        }
        // Must run after the focus-follow block above: that derives the dying
        // window's snap rect from stable_snap_rects (and, on a cache miss, live
        // geometry reading its decorations/pinned entries), so clear last.
        self.cleanup_surface_state(&wl_surface);
    }

    fn move_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, serial: Serial) {
        let wl_surface = surface.wl_surface().clone();
        if driftwm::config::applied_rule(&wl_surface).is_some_and(|r| r.widget) {
            return;
        }
        let Some(window) = self.window_for_surface(&wl_surface) else {
            return;
        };

        // Client-initiated xdg move_request: the client asked to move itself,
        // not its cluster neighbors. Clients don't know about clusters, so
        // always single-window.
        let pointer = self.seat.get_pointer().unwrap();
        if let Some(start_data) = check_grab(&pointer, &wl_surface) {
            // Pinned windows move in screen space. A canvas grab would only
            // shuffle the loc-synced canvas position while the window keeps
            // rendering at its fixed `screen_pos` — i.e. the drag would do nothing.
            if self.stage.is_pinned(&window) {
                self.start_pinned_move(
                    &pointer,
                    &window,
                    start_data.location,
                    start_data.button,
                    serial,
                );
                return;
            }
            let Some(initial_window_location) = self.stage.position_of(&window) else {
                return;
            };
            let Some(output) = self.active_output() else {
                return;
            };
            // Moving re-anchors the window, invalidating any fill restore point.
            self.stage.clear_fill(&window);
            self.arm_interactive_move(&window);
            let grab = MoveGrab::new(
                start_data,
                window,
                initial_window_location,
                output,
                Vec::new(),
            );
            pointer.set_grab(self, grab, serial, Focus::Clear);
            return;
        }

        // Touch-initiated move: a CSD client dragging its own titlebar requests
        // the move off a touch grab, not a pointer grab. (SSD titlebars are moved
        // by the compositor's own hit-test in `input/touch.rs`, never via here.)
        if let Some(touch) = self.seat.get_touch()
            && let Some(touch_start) = check_touch_grab(&touch, &wl_surface)
        {
            // Stop any camera coast before building the grab — the offset
            // converts the down-time canvas point with the live camera, so a
            // coast between down and this request would skew it.
            if self.stage.is_pinned(&window) {
                self.cancel_animations();
                let Some(grab) = self.build_touch_pinned_move_grab(&window, touch_start, 1) else {
                    return;
                };
                // Revoke the client's in-flight touch sequence (see the canvas branch below).
                touch.cancel(self);
                self.arm_interactive_move(&window);
                touch.set_grab(self, grab, serial);
                return;
            }
            let Some(initial_window_location) = self.stage.position_of(&window) else {
                return;
            };
            // The initiating touch-down resolved and stored the touch's output as
            // the focused output; reuse it (no input device on this client path).
            let Some(output) = self.active_output() else {
                return;
            };
            // The finger's `down` was forwarded to the client (Forward mode), and
            // smithay routes touch motion/up to the slot's stored focus regardless
            // of the grab — so cancel the client's sequence before the compositor
            // takes over the drag, or it keeps receiving the whole sequence.
            touch.cancel(self);
            // Moving re-anchors the window, invalidating any fill restore point.
            self.stage.clear_fill(&window);
            self.arm_interactive_move(&window);
            let grab = MoveGrab::new_touch(
                touch_start,
                window,
                initial_window_location,
                output,
                1,
                Vec::new(),
            );
            touch.set_grab(self, grab, serial);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let wl_surface = surface.wl_surface().clone();
        if driftwm::config::applied_rule(&wl_surface).is_some_and(|r| r.widget) {
            return;
        }
        let Some(window) = self.window_for_surface(&wl_surface) else {
            return;
        };

        let pointer = self.seat.get_pointer().unwrap();
        let Some(start_data) = check_grab(&pointer, &wl_surface) else {
            return;
        };

        let Some(initial_window_location) = self.stage.position_of(&window) else {
            return;
        };
        let initial_window_size = window.geometry().size;

        // Bail before any side effects — leaving ResizeState, the toplevel's
        // Resizing flag, and the cursor in "resize" mode without an active
        // grab would desync the client and the visual cursor.
        let Some(output) = self.active_output() else {
            return;
        };

        // Clear fit/fill state — user took manual control
        self.stage.clear_fit(&window);
        self.stage.clear_fill(&window);

        // Pinned windows resize in screen space (see start_compositor_resize_with_edge).
        let pinned_site = self.stage.pin_of(&window).cloned();
        let pinned_initial_screen_pos = pinned_site.as_ref().map(|s| s.screen_pos);
        let pinned_output = pinned_site
            .as_ref()
            .and_then(|s| self.output_by_name(&s.output));

        // Store resize state in the surface data map for commit() repositioning
        with_states(&wl_surface, |states| {
            states
                .data_map
                .get_or_insert(|| RefCell::new(ResizeState::Idle))
                .replace(ResizeState::Resizing {
                    edges,
                    initial_window_location,
                    initial_window_size,
                    initial_screen_pos: pinned_initial_screen_pos,
                });
        });

        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
        });

        self.cursor.grab_cursor = true;
        self.cursor.cursor_status = CursorImageStatus::Named(resize_cursor(edges));

        let last_clamped_location = start_data.location;
        // CSD windows trigger resize through xdg_toplevel.resize() when
        // the user drags the client-drawn border. Honor the config flag so
        // edge-drag propagation behaves identically for SSD and CSD windows.
        let cluster_resize =
            if self.config.decoration_resize_snapped && pinned_initial_screen_pos.is_none() {
                self.cluster_snapshot_for_resize(&StageWindow::Client(window.clone()), edges)
            } else {
                crate::state::ClusterResizeSnapshot::empty()
            };
        let constraints = crate::grabs::SizeConstraints::for_window(&window);
        let locked_ratio = crate::grabs::locked_ratio_for(&window, initial_window_size);
        let grab = ResizeSurfaceGrab {
            start_data,
            window,
            edges,
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
            output: pinned_output.unwrap_or(output),
            last_clamped_location,
            snap: driftwm::layout::snap::SnapState::default(),
            constraints,
            cluster_resize,
            pinned_initial_screen_pos,
            touch_start: None,
            touch_slots: 0,
            locked_ratio,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }
}

delegate_xdg_shell!(DriftWm);

/// Validate that the pointer has an active grab starting on the given surface.
/// Returns the `GrabStartData` if the button click that started the grab
/// originated on this surface (preventing a client from stealing another's grab).
fn check_grab(
    pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> Option<GrabStartData<DriftWm>> {
    let start_data = pointer.grab_start_data()?;
    let (focus, _) = start_data.focus.as_ref()?;

    // The button press must have been on this surface (or a child of it)
    if !focus.same_client_as(&surface.id()) {
        return None;
    }

    Some(start_data)
}

/// Touch analogue of `check_grab`: validate that the active touch grab started
/// on (a child of) the given surface, returning its start data.
fn check_touch_grab(
    touch: &smithay::input::touch::TouchHandle<DriftWm>,
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> Option<smithay::input::touch::GrabStartData<DriftWm>> {
    let start_data = touch.grab_start_data()?;
    let (focus, _) = start_data.focus.as_ref()?;

    if !focus.same_client_as(&surface.id()) {
        return None;
    }

    Some(start_data)
}

impl DriftWm {
    /// Decide whether a popup grab on `root` should be honored.
    ///
    /// Rules:
    ///   - When the session is locked, only the active lock surface(s) may grab.
    ///   - When `root` is a regular xdg-toplevel, it must currently hold keyboard
    ///     focus. Otherwise a hidden / unfocused client could steal input.
    ///   - Layer-shell and canvas-layer parents always pass — they grab as part
    ///     of normal panel/menu behavior and aren't governed by window focus.
    pub(crate) fn popup_grab_allowed(
        &self,
        root: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> bool {
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            return self
                .lock_surfaces
                .values()
                .any(|ls| ls.wl_surface() == root);
        }

        let is_window = self
            .stage
            .windows()
            .any(|w| w.wl_surface().as_deref() == Some(root));
        if !is_window {
            return true;
        }

        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.current_focus().is_some_and(|f| &f.0 == root)
    }

    /// Apply xdg positioner constraint adjustments so the popup stays within
    /// the output bounds. Works for both xdg-toplevel and layer-shell parents.
    pub(crate) fn unconstrain_popup(&self, popup: &PopupKind) {
        let PopupKind::Xdg(surface) = popup else {
            return;
        };

        let Ok(root) = find_popup_root_surface(popup) else {
            return;
        };

        // The target rect for constraining, in parent-surface-relative coordinates.
        // We need to figure out where the root surface is on the output and express
        // the output bounds relative to the popup's toplevel.
        let active_output = self.active_output();
        let target = if let Some(window) = self
            .stage
            .windows()
            .find(|w| w.wl_surface().as_deref() == Some(&root))
        {
            // Parent is an xdg window — target is the *visible canvas area* in
            // window-relative coords. We must use visible_canvas_rect (not raw
            // output_geo) because the screen is a (camera, zoom) viewport onto
            // the canvas: when zoomed/panned, output_geo translated by window_loc
            // describes a phantom screen far from the popup's anchor, and the
            // positioner mis-flips the popup to "fit" it.
            let window_loc = self.stage.position_of(window).unwrap_or_default();
            let viewport_size = active_output
                .as_ref()
                .and_then(|o| self.space.output_geometry(o))
                .map(|g| g.size)
                .unwrap_or_default();

            let mut target = driftwm::canvas::visible_canvas_rect(
                self.camera().to_i32_round(),
                viewport_size,
                self.zoom(),
            );
            target.loc -= window_loc;
            target.loc -= get_popup_toplevel_coords(popup);
            target
        } else if let Some(cl) = self
            .canvas_layers
            .iter()
            .find(|cl| cl.surface.wl_surface() == &root)
            && let Some(pos) = cl.position
        {
            // Parent is a canvas-positioned layer surface
            let output_geo = active_output
                .as_ref()
                .and_then(|o| self.space.output_geometry(o))
                .unwrap_or_default();
            // Constrain to the visible canvas area (accounts for zoom)
            let viewport_size = output_geo.size;
            let mut target = driftwm::canvas::visible_canvas_rect(
                self.camera().to_i32_round(),
                viewport_size,
                self.zoom(),
            );
            // Translate to layer-surface-relative coordinates
            target.loc -= pos;
            target.loc -= get_popup_toplevel_coords(popup);
            target
        } else {
            // Parent is a layer surface — find it in the layer map
            let output = self.active_output();
            let output = match output {
                Some(o) => o,
                None => return,
            };
            let output_geo = self.space.output_geometry(&output).unwrap_or_default();
            let map = layer_map_for_output(&output);
            let layer_geo = map
                .layers()
                .find(|l| l.wl_surface() == &root)
                .and_then(|l| map.layer_geometry(l))
                .unwrap_or_default();
            drop(map);

            let mut target = Rectangle::from_size(output_geo.size);
            // Translate into layer-surface-relative coordinates
            target.loc -= layer_geo.loc;
            target.loc -= get_popup_toplevel_coords(popup);
            target
        };

        surface.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
        surface.send_configure().ok();
    }
}

/// Map resize edge to the appropriate directional cursor icon.
pub(crate) fn resize_cursor(edges: xdg_toplevel::ResizeEdge) -> CursorIcon {
    match edges {
        xdg_toplevel::ResizeEdge::Top => CursorIcon::NResize,
        xdg_toplevel::ResizeEdge::Bottom => CursorIcon::SResize,
        xdg_toplevel::ResizeEdge::Left => CursorIcon::WResize,
        xdg_toplevel::ResizeEdge::Right => CursorIcon::EResize,
        xdg_toplevel::ResizeEdge::TopLeft => CursorIcon::NwResize,
        xdg_toplevel::ResizeEdge::TopRight => CursorIcon::NeResize,
        xdg_toplevel::ResizeEdge::BottomLeft => CursorIcon::SwResize,
        xdg_toplevel::ResizeEdge::BottomRight => CursorIcon::SeResize,
        _ => CursorIcon::Default,
    }
}
