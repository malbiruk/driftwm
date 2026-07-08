use std::cell::RefCell;
use std::collections::HashSet;
use std::time::Duration;

use smithay::{
    backend::input::{
        Axis, AxisSource, ButtonState, Device, DeviceCapability, Event, InputBackend,
        PointerAxisEvent, PointerButtonEvent,
    },
    input::pointer::{
        AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, Focus, GrabStartData, MotionEvent,
    },
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
    },
    utils::{Point, SERIAL_COUNTER},
    wayland::compositor::with_states,
};

use smithay::wayland::seat::WaylandFocus;

use crate::decorations::DecorationHit;
use crate::grabs::{MoveSurfaceGrab, NavigateGrab, PanGrab, ResizeState, ResizeSurfaceGrab};
use crate::state::{ClusterResizeSnapshot, DriftWm, FocusTarget, PendingMiddleClick};
use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::config::{self, Action, BindingContext, MouseAction};
use driftwm::window_ext::WindowExt;
use smithay::reexports::wayland_server::Resource;

impl DriftWm {
    /// Determine the binding context for the current pointer position.
    pub(super) fn pointer_context(
        &self,
        pos: Point<f64, smithay::utils::Logical>,
    ) -> BindingContext {
        // SSD chrome and the CSD resize margin sit outside the surface bbox, so
        // `element_under` misses them; count them as OnWindow so on-window bindings
        // apply over the chrome, not just the client surface.
        let over_window = self.element_under(pos).is_some()
            || self.canvas_layer_under(pos).is_some()
            || self.decoration_under(pos).is_some();
        if over_window {
            BindingContext::OnWindow
        } else {
            BindingContext::OnCanvas
        }
    }

    /// Look up the mouse-button binding for `mods`/`button`/`context`, paired with
    /// whether it's a *held-modifier* binding. The bool gates SSD chrome: a chrome
    /// margin's context is OnCanvas where bare LMB is also bound (pan), so "a binding
    /// matched" can't suppress chrome on its own — only a held modifier should. That
    /// keeps Mod+LMB panning over a border while a plain click still drives the chrome.
    fn modifier_button_binding(
        &self,
        mods: &smithay::input::keyboard::ModifiersState,
        button: u32,
        context: BindingContext,
    ) -> (Option<MouseAction>, bool) {
        let binding = self
            .config
            .mouse_button_lookup_ctx(mods, button, context)
            .cloned();
        let has_modifier = binding.is_some() && !config::Modifiers::from_state(mods).is_empty();
        (binding, has_modifier)
    }

    /// Priority order when button pressed:
    /// 1. Configured mouse bindings (move, resize, pan, etc.)
    /// 2. Normal click on window → focus + raise + forward to client
    /// 3. Left-click on empty canvas → pan canvas
    pub(super) fn on_pointer_button<I: InputBackend>(&mut self, event: I::PointerButtonEvent) {
        // Outputs can transiently disappear (cable unplug, GPU resume race);
        // bail out so downstream active_output() / element_location() can't panic.
        if self.space.outputs().next().is_none() {
            return;
        }
        let serial = SERIAL_COUNTER.next_serial();
        let button = event.button_code();
        let button_state = event.state();
        let pointer = self.seat.get_pointer().unwrap();

        // Buffer BTN_MIDDLE release while a pending click is waiting
        if button == config::BTN_MIDDLE
            && button_state == ButtonState::Released
            && let Some(ref mut pending) = self.pending_middle_click
        {
            pending.release_time = Some(Event::time_msec(&event));
            return;
        }

        if button_state == ButtonState::Pressed {
            self.set_last_scroll_pan(None);
            self.with_output_state(|os| os.momentum.stop());

            // A 3-finger tap (LRM button map) generates BTN_MIDDLE.
            // Buffer it — if a 3-finger swipe follows within 300ms, suppress
            // the click and enter window-move mode. Otherwise flush to client (paste).
            // Gate buffering to gesture-capable devices — only touchpads emit the
            // 3-finger swipe; a real mouse's middle click must not be delayed.
            // Skip too when a modifier binding matches (e.g. alt+middle).
            if button == config::BTN_MIDDLE
                && event.device().has_capability(DeviceCapability::Gesture)
                && {
                    let kb = self.seat.get_keyboard().unwrap();
                    let ctx = self.pointer_context(pointer.current_location());
                    self.config
                        .mouse_button_lookup_ctx(&kb.modifier_state(), button, ctx)
                        .is_none()
                }
            {
                // Cancel any existing pending click first
                if let Some(old) = self.pending_middle_click.take() {
                    self.loop_handle.remove(old.timer_token);
                    self.flush_middle_click(old.press_time, old.release_time);
                }
                let timer = Timer::from_duration(Duration::from_millis(
                    super::gestures::DOUBLE_TAP_WINDOW_MS,
                ));
                if let Ok(token) =
                    self.loop_handle
                        .insert_source(timer, |_, _, data: &mut DriftWm| {
                            data.flush_pending_middle_click();
                            TimeoutAction::Drop
                        })
                {
                    self.pending_middle_click = Some(PendingMiddleClick {
                        press_time: Event::time_msec(&event),
                        release_time: None,
                        timer_token: token,
                    });
                    return;
                }
            }
            let mut pos = pointer.current_location();
            let keyboard = self.seat.get_keyboard().unwrap();
            let mods = keyboard.modifier_state();

            // During fullscreen: bound clicks exit fullscreen first and
            // proceed to compositor grabs; plain clicks forward to the app.
            // ToggleFullscreen is special — exiting IS the action, so return immediately.
            if self.is_fullscreen() {
                // In fullscreen the window fills the screen — treat as OnWindow
                let fs_lookup =
                    self.config
                        .mouse_button_lookup_ctx(&mods, button, BindingContext::OnWindow);
                if matches!(
                    fs_lookup,
                    Some(MouseAction::Action(
                        Action::ToggleFullscreen | Action::FitWindow | Action::FitWindowSnapped
                    ))
                ) {
                    self.exit_fullscreen_remap_pointer(pos);
                    return;
                } else if fs_lookup.is_some() {
                    pos = self.exit_fullscreen_remap_pointer(pos);
                } else {
                    // Reclaim keyboard focus for the fullscreen window before
                    // forwarding — hover on another output may have moved focus
                    // to its window, and a plain forward wouldn't restore it.
                    // Skip when it already holds focus so a click doesn't re-emit
                    // a keyboard enter (and a popup grab keeps its focus).
                    if let Some(surface) = self
                        .active_fullscreen_window()
                        .and_then(|w| w.wl_surface().map(|s| FocusTarget(s.into_owned())))
                    {
                        let already = self.window_focus.as_ref().is_some_and(|f| f.0 == surface.0);
                        if !already {
                            let focus_serial = SERIAL_COUNTER.next_serial();
                            self.set_window_focus(Some(surface), focus_serial);
                        }
                    }
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button,
                            state: button_state,
                            serial,
                            time: Event::time_msec(&event),
                        },
                    );
                    pointer.frame(self);
                    return;
                }
            }

            // Layer surfaces: just forward (no compositor grabs). A press grants
            // keyboard focus to an `OnDemand` layer under the pointer.
            if self.pointer_over_layer {
                if button_state == ButtonState::Pressed {
                    let layer = pointer.current_focus().map(|f| f.0);
                    self.focus_layer_if_on_demand(layer, serial);
                }
                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: Event::time_msec(&event),
                    },
                );
                pointer.frame(self);
                return;
            }

            // Screen-pinned windows live above normal windows in screen space;
            // decoration_under / element_under are canvas-space and miss them at
            // zoom != 1, so dispatch their clicks separately.
            if self.try_pinned_button(
                &pointer,
                pos,
                button,
                button_state,
                serial,
                mods,
                Event::time_msec(&event),
            ) {
                return;
            }

            // `modifier_binding` gates the chrome paths below.
            let context = self.pointer_context(pos);
            let (binding, modifier_binding) = self.modifier_button_binding(&mods, button, context);

            // SSD decoration clicks: title bar → move, close button → close, resize border → resize
            if !modifier_binding && let Some((window, hit)) = self.decoration_under(pos) {
                // Decoration interactions must only apply to the topmost window.
                // Otherwise a lower SSD title bar/border can steal clicks through
                // an overlapping window.
                if self
                    .surface_under(pos, None)
                    .and_then(|(target, _)| self.window_for_surface(&target.0))
                    .is_some_and(|top| top != window)
                {
                    // Occluded decoration hit; continue normal dispatch.
                } else {
                    let Some(wl_surface) = window.wl_surface().map(|s| s.into_owned()) else {
                        return;
                    };
                    let is_widget = config::applied_rule(&wl_surface).is_some_and(|r| r.widget);

                    if button == config::BTN_LEFT {
                        match hit {
                            DecorationHit::CloseButton => {
                                window.send_close();
                                return;
                            }
                            DecorationHit::TitleBar if !is_widget => {
                                // Double-click → toggle fit
                                let now = std::time::Instant::now();
                                let surface_id = wl_surface.id();
                                if let Some((prev_time, prev_id)) = self.last_titlebar_click.take()
                                    && prev_id == surface_id
                                    && now.duration_since(prev_time) < Duration::from_millis(300)
                                {
                                    self.raise_and_focus(&window, serial);
                                    self.decoration_toggle_fit(&window);
                                    return;
                                }
                                self.last_titlebar_click = Some((now, surface_id));

                                // Focus + raise (with modal redirect) + start move grab.
                                // Alt+drag on the titlebar moves a single window;
                                // cluster drag is a separate explicit action
                                // (`MoveSnappedWindows`, default Alt+Shift+Left).
                                self.raise_and_focus(&window, serial);
                                let Some(initial_window_location) =
                                    self.space.element_location(&window)
                                else {
                                    return;
                                };
                                let Some(output) = self.active_output() else {
                                    return;
                                };
                                let start_data = GrabStartData {
                                    focus: None,
                                    button,
                                    location: pos,
                                };
                                let grab = MoveSurfaceGrab::new(
                                    start_data,
                                    window,
                                    initial_window_location,
                                    output,
                                    Vec::new(),
                                    HashSet::new(),
                                );
                                pointer.set_grab(self, grab, serial, Focus::Clear);
                                return;
                            }
                            DecorationHit::ResizeBorder(edge) if !is_widget => {
                                self.raise_and_focus(&window, serial);
                                // Edge-drag on the SSD border has no modifier
                                // context, so it follows the config flag.
                                let want_cluster = self.config.decoration_resize_snapped;
                                self.start_compositor_resize_with_edge(
                                    &pointer,
                                    &window,
                                    pos,
                                    button,
                                    serial,
                                    Some(edge),
                                    want_cluster,
                                );
                                return;
                            }
                            _ => {
                                // Widget title bar or other — just focus
                                self.set_window_focus(Some(FocusTarget(wl_surface)), serial);
                            }
                        }
                    }
                }
            }

            // Dispatch the matched mouse binding (move, resize, pan, etc.)
            if let Some(action) = binding {
                match action {
                    MouseAction::MoveWindow | MouseAction::MoveSnappedWindows => {
                        let want_cluster = matches!(action, MouseAction::MoveSnappedWindows);
                        if let Some((window, _)) =
                            self.element_under(pos).map(|(w, l)| (w.clone(), l))
                            && let Some(surface) = window.wl_surface()
                            && !config::applied_rule(&surface).is_some_and(|r| r.widget)
                            && !self.is_pinned(&window)
                        {
                            self.raise_and_focus(&window, serial);

                            let Some(initial_window_location) =
                                self.space.element_location(&window)
                            else {
                                return;
                            };
                            let Some(output) = self.active_output() else {
                                return;
                            };
                            let start_data = GrabStartData {
                                focus: None,
                                button,
                                location: pos,
                            };
                            // Only MoveSnappedWindows captures the cluster;
                            // plain MoveWindow stays strictly single-window.
                            let (cluster_members, cluster_member_surfaces) = if want_cluster {
                                self.cluster_snapshot_for_drag(&window, initial_window_location)
                            } else {
                                (Vec::new(), HashSet::new())
                            };
                            let grab = MoveSurfaceGrab::new(
                                start_data,
                                window,
                                initial_window_location,
                                output,
                                cluster_members,
                                cluster_member_surfaces,
                            );
                            pointer.set_grab(self, grab, serial, Focus::Clear);
                            return;
                        }
                        // No window or pinned — fall through to normal click
                    }
                    MouseAction::ResizeWindow | MouseAction::ResizeWindowSnapped => {
                        // Opt-in cluster propagation: only
                        // `ResizeWindowSnapped` captures the cluster; plain
                        // `ResizeWindow` builds an empty snapshot so the
                        // grab behaves like pre-slice-2 single-window resize.
                        let want_cluster = matches!(action, MouseAction::ResizeWindowSnapped);
                        if let Some((window, _)) =
                            self.element_under(pos).map(|(w, l)| (w.clone(), l))
                            && !window
                                .wl_surface()
                                .and_then(|s| config::applied_rule(&s))
                                .is_some_and(|r| r.widget)
                            && !self.is_pinned(&window)
                        {
                            self.raise_and_focus(&window, serial);

                            self.start_compositor_resize(
                                &pointer,
                                &window,
                                pos,
                                button,
                                serial,
                                want_cluster,
                            );
                            return;
                        }
                        // No window or pinned — fall through
                    }
                    MouseAction::PanViewport => {
                        self.set_panning(true);
                        let from_empty = context == BindingContext::OnCanvas;
                        let Some(grab) = self.make_pan_grab(pos, button, from_empty) else {
                            return;
                        };
                        pointer.set_grab(self, grab, serial, Focus::Clear);
                        return;
                    }
                    MouseAction::CenterNearest => {
                        let Some(output) = self.active_output() else {
                            return;
                        };
                        let screen_pos =
                            canvas_to_screen(CanvasPos(pos), self.camera(), self.zoom()).0;
                        let start_data = GrabStartData {
                            focus: None,
                            button,
                            location: pos,
                        };
                        let grab = NavigateGrab::new(start_data, screen_pos, output);
                        pointer.set_grab(self, grab, serial, Focus::Clear);
                        return;
                    }
                    MouseAction::Action(ref action) => {
                        if let Some((window, _)) =
                            self.element_under(pos).map(|(w, l)| (w.clone(), l))
                        {
                            self.raise_and_focus(&window, serial);
                        }
                        self.execute_action(action);
                        return;
                    }
                    MouseAction::Zoom => {} // n/a for button clicks
                }
            }

            // Hardcoded fallbacks: click-to-focus, empty-canvas-pan
            let element_under = self.element_under(pos).map(|(w, _)| w.clone());

            if let Some(ref window) = element_under {
                let is_widget = window
                    .wl_surface()
                    .and_then(|s| config::applied_rule(&s))
                    .is_some_and(|r| r.widget);
                if !is_widget {
                    // Normal window: raise + focus (with modal redirect)
                    self.raise_and_focus(window, serial);
                } else if let Some((focus, _)) = self.canvas_layer_under(pos) {
                    // Widget window but a canvas layer is above it: grant the
                    // layer keyboard focus only if it requests it (on-demand).
                    self.focus_layer_if_on_demand(Some(focus.0), serial);
                } else {
                    // Widget window with no canvas layer above: focus the widget
                    self.set_window_focus(
                        window.wl_surface().map(|s| FocusTarget(s.into_owned())),
                        serial,
                    );
                }
            } else if let Some((focus, _)) = self.canvas_layer_under(pos) {
                self.focus_layer_if_on_demand(Some(focus.0), serial);
            }
        }

        pointer.button(
            self,
            &ButtonEvent {
                button,
                state: button_state,
                serial,
                time: Event::time_msec(&event),
            },
        );
        pointer.frame(self);
    }

    /// Dispatch a left/other button press over a screen-pinned window in screen
    /// coords: SSD decoration (close / title-bar move / resize border), then
    /// mouse-binding move/resize, else focus + forward the click to the client.
    /// Returns `true` if the press was consumed (caller should stop dispatching).
    #[allow(clippy::too_many_arguments)]
    fn try_pinned_button(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        button_state: ButtonState,
        serial: smithay::utils::Serial,
        mods: smithay::input::keyboard::ModifiersState,
        time: u32,
    ) -> bool {
        if self.pinned.is_empty() {
            return false;
        }
        let screen_pos = canvas_to_screen(CanvasPos(pos), self.camera(), self.zoom()).0;

        // `modifier_binding` gates the pinned chrome path below, as on the canvas path.
        let (binding, modifier_binding) =
            self.modifier_button_binding(&mods, button, BindingContext::OnWindow);

        if !modifier_binding
            && button == config::BTN_LEFT
            && let Some((window, hit)) = self.pinned_decoration_under(screen_pos)
        {
            let is_widget = window
                .wl_surface()
                .and_then(|s| config::applied_rule(&s))
                .is_some_and(|r| r.widget);
            match hit {
                DecorationHit::CloseButton => window.send_close(),
                DecorationHit::TitleBar if !is_widget => {
                    self.raise_and_focus(&window, serial);
                    self.start_pinned_move(pointer, &window, pos, button, serial);
                }
                DecorationHit::ResizeBorder(edge) if !is_widget => {
                    self.raise_and_focus(&window, serial);
                    self.start_compositor_resize_with_edge(
                        pointer,
                        &window,
                        pos,
                        button,
                        serial,
                        Some(edge),
                        false,
                    );
                }
                _ => {
                    if let Some(s) = window.wl_surface() {
                        self.set_window_focus(Some(FocusTarget(s.into_owned())), serial);
                    }
                }
            }
            return true;
        }

        let Some((focus, _)) = self.pinned_window_under(screen_pos, pos) else {
            return false;
        };
        let pinned_window = self.window_for_surface(&focus.0);
        if let Some(action) = binding
            && let Some(ref window) = pinned_window
            && !window.is_widget()
        {
            match action {
                MouseAction::MoveWindow | MouseAction::MoveSnappedWindows => {
                    self.raise_and_focus(window, serial);
                    self.start_pinned_move(pointer, window, pos, button, serial);
                    return true;
                }
                MouseAction::ResizeWindow | MouseAction::ResizeWindowSnapped => {
                    self.raise_and_focus(window, serial);
                    // Infer the edge in screen space against the pinned rect.
                    let edge = window
                        .wl_surface()
                        .and_then(|s| self.pinned.get(&s.id()).map(|p| p.screen_pos))
                        .map(|sp| edges_from_position(screen_pos, sp, window.geometry().size));
                    self.start_compositor_resize_with_edge(
                        pointer, window, pos, button, serial, edge, false,
                    );
                    return true;
                }
                MouseAction::Action(ref a) => {
                    self.raise_and_focus(window, serial);
                    let a = a.clone();
                    self.execute_action(&a);
                    return true;
                }
                // Viewport actions aren't pinned-specific — defer to normal dispatch.
                MouseAction::PanViewport | MouseAction::CenterNearest => return false,
                _ => {}
            }
        }
        if let Some(ref window) = pinned_window {
            self.raise_and_focus(window, serial);
        }
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state: button_state,
                serial,
                time,
            },
        );
        pointer.frame(self);
        true
    }

    /// Start a screen-space move grab for a pinned window. The grab tracks the
    /// cursor with the fixed screen-offset captured here.
    pub(crate) fn start_pinned_move(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        window: &smithay::desktop::Window,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: smithay::utils::Serial,
    ) {
        let Some(id) = window.wl_surface().map(|s| s.id()) else {
            return;
        };
        let Some((output, screen_pos)) = self
            .pinned
            .get(&id)
            .map(|p| (p.output.clone(), p.screen_pos))
        else {
            return;
        };
        let (camera, zoom) = {
            let os = crate::state::output_state(&output);
            (os.camera, os.zoom)
        };
        let cursor_screen = canvas_to_screen(CanvasPos(pos), camera, zoom).0;
        let grab_offset = screen_pos.to_f64() - cursor_screen;
        let start_data = GrabStartData {
            focus: None,
            button,
            location: pos,
        };
        let grab = MoveSurfaceGrab::new_pinned(start_data, window.clone(), output, grab_offset);
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    /// Start a compositor-side resize grab. If `explicit_edge` is provided, use it;
    /// otherwise infer edges from pointer position within the window.
    ///
    /// `want_cluster = true` snapshots the focused window's snap cluster so
    /// neighbors are translated along with the resize (opt-in). `false` keeps
    /// resize strictly single-window — the grab still runs the cluster code
    /// path, but over an empty snapshot that short-circuits to no-op.
    pub(super) fn start_compositor_resize(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        window: &smithay::desktop::Window,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: smithay::utils::Serial,
        want_cluster: bool,
    ) {
        self.start_compositor_resize_with_edge(
            pointer,
            window,
            pos,
            button,
            serial,
            None,
            want_cluster,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_compositor_resize_with_edge(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        window: &smithay::desktop::Window,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: smithay::utils::Serial,
        explicit_edge: Option<xdg_toplevel::ResizeEdge>,
        want_cluster: bool,
    ) {
        let Some(initial_window_location) = self.space.element_location(window) else {
            return;
        };
        let initial_window_size = window.geometry().size;

        let edges = explicit_edge.unwrap_or_else(|| {
            // Pinned windows live in screen space — infer the edge against their
            // screen rect, since the canvas-space inference is wrong at zoom != 1.
            // (Pinned dispatch already passes an explicit edge; this keeps the
            // function correct for any future inferred-edge caller.)
            if let Some((sp, output)) = window.wl_surface().and_then(|s| {
                self.pinned
                    .get(&s.id())
                    .map(|p| (p.screen_pos, p.output.clone()))
            }) {
                let (camera, zoom) = {
                    let os = crate::state::output_state(&output);
                    (os.camera, os.zoom)
                };
                let screen_pos = canvas_to_screen(CanvasPos(pos), camera, zoom).0;
                edges_from_position(screen_pos, sp, initial_window_size)
            } else {
                edges_from_position(pos, initial_window_location, initial_window_size)
            }
        });

        // Store resize state for commit() repositioning
        let Some(wl_surface) = window.wl_surface().map(|s| s.into_owned()) else {
            return;
        };

        // Clear fit state — user took manual control
        self.stage.clear_fit(window);

        // Pinned windows resize in screen space; capture their `screen_pos` and
        // fixed output so the grab and the commit-time reposition use the right
        // anchor. `None` for normal canvas windows.
        let pinned_initial_screen_pos = self.pinned.get(&wl_surface.id()).map(|p| p.screen_pos);
        let pinned_output = self.pinned.get(&wl_surface.id()).map(|p| p.output.clone());

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

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
                // Mirror the fit-state clear above so the client's view stays
                // in sync — otherwise its own restore button dispatches an
                // unmaximize_request that `unfit_window` would silently drop.
                state.states.unset(xdg_toplevel::State::Maximized);
            });
        }

        self.cursor.grab_cursor = true;
        self.cursor.cursor_status = CursorImageStatus::Named(resize_cursor(edges));

        let start_data = GrabStartData {
            focus: None,
            button,
            location: pos,
        };
        let Some(output) = pinned_output.clone().or_else(|| self.active_output()) else {
            return;
        };
        // Only snapshot the cluster when the caller opted in. Pinned windows
        // never cluster (they're off-canvas), so force the empty snapshot.
        // For single-window resize (`want_cluster = false`) we hand the grab an
        // empty snapshot so `cluster_resize.members.is_empty()` short-circuits
        // the motion-time cascade and `snap_targets` sees no exclusions —
        // exactly the pre-slice-2 behavior.
        let cluster_resize = if want_cluster && pinned_initial_screen_pos.is_none() {
            self.cluster_snapshot_for_resize(window, edges)
        } else {
            ClusterResizeSnapshot::empty()
        };
        let constraints = crate::grabs::SizeConstraints::for_window(window);
        let grab = ResizeSurfaceGrab {
            start_data,
            window: window.clone(),
            edges,
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
            output,
            last_clamped_location: pos,
            snap: driftwm::layout::snap::SnapState::default(),
            constraints,
            cluster_resize,
            pinned_initial_screen_pos,
            touch_start: None,
            touch_slots: 0,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    pub(super) fn on_pointer_axis<I: InputBackend>(&mut self, event: I::PointerAxisEvent) {
        if self.space.outputs().next().is_none() {
            return;
        }
        // When pointer is over a layer surface, forward scroll directly (no pan/zoom)
        if self.pointer_over_layer {
            let pointer = self.seat.get_pointer().unwrap();
            let frame = build_client_axis_frame::<I>(&event);
            pointer.axis(self, frame);
            pointer.frame(self);
            return;
        }

        let keyboard = self.seat.get_keyboard().unwrap();
        let mods = keyboard.modifier_state();
        let pointer = self.seat.get_pointer().unwrap();
        let pos = pointer.current_location();
        let source = event.source();

        // Discrete wheel-notch bindings (wheel-up / wheel-down) run any
        // action once per notch — volume on mod+shift+scroll and the like.
        // Wheel sources only: finger and continuous scrolling have no
        // notches. Checked before the fullscreen block so a notch action
        // fires like a keybinding, without exiting fullscreen.
        if matches!(source, AxisSource::Wheel | AxisSource::WheelTilt) {
            let v = event
                .amount_v120(Axis::Vertical)
                .map(|v| v / 120.0)
                .or_else(|| event.amount(Axis::Vertical).map(|v| v / 15.0))
                .unwrap_or(0.0);
            let notch_context = if self.is_fullscreen() {
                // The fullscreen window fills the screen.
                BindingContext::OnWindow
            } else {
                self.pointer_context(pos)
            };
            if v != 0.0
                && let Some(MouseAction::Action(act)) = self
                    .config
                    .mouse_wheel_step_lookup_ctx(&mods, v < 0.0, notch_context)
                    .cloned()
            {
                // High-resolution wheels emit sub-notch v120 deltas;
                // accumulate to whole notches so a flick fires the action
                // once per notch, not once per event.
                if self.wheel_notch_accum != 0.0 && self.wheel_notch_accum.signum() != v.signum() {
                    self.wheel_notch_accum = 0.0;
                }
                self.wheel_notch_accum += v;
                let whole = self.wheel_notch_accum.abs().floor();
                self.wheel_notch_accum -= self.wheel_notch_accum.signum() * whole;
                let notches = (whole as u32).min(10);
                for _ in 0..notches {
                    self.execute_action(&act);
                }
                // Consume sub-notch events too — the binding owns this
                // scroll; forwarding the fractions would double-handle it.
                let frame = AxisFrame::new(Event::time_msec(&event));
                pointer.axis(self, frame);
                pointer.frame(self);
                return;
            }
        }

        // During fullscreen: bound scroll exits fullscreen first; plain scroll forwards.
        if self.is_fullscreen() {
            if self
                .config
                .mouse_scroll_lookup_ctx(&mods, source, BindingContext::OnWindow)
                .is_some()
            {
                self.exit_fullscreen_remap_pointer(pos);
                // Fall through to dispatch below
            } else {
                let frame = build_client_axis_frame::<I>(&event);
                pointer.axis(self, frame);
                pointer.frame(self);
                return;
            }
        }

        // Compute context — recent_pan stickiness forces OnCanvas to prevent
        // jitter when a window slides under the pointer during a pan gesture.
        let recent_pan = self.last_scroll_pan().is_some_and(|t: std::time::Instant| {
            t.elapsed() < std::time::Duration::from_millis(150)
        });
        let context = if recent_pan {
            BindingContext::OnCanvas
        } else {
            self.pointer_context(pos)
        };

        // Single lookup: context-aware
        if let Some(action) = self
            .config
            .mouse_scroll_lookup_ctx(&mods, source, context)
            .cloned()
        {
            match action {
                MouseAction::PanViewport => {
                    let h = event.amount(Axis::Horizontal).unwrap_or(0.0);
                    let v = event.amount(Axis::Vertical).unwrap_or(0.0);
                    if h != 0.0 || v != 0.0 {
                        if source == AxisSource::Finger {
                            self.set_last_scroll_pan(Some(std::time::Instant::now()));
                        }
                        let s = self.config.trackpad_speed;
                        let canvas_delta: Point<f64, smithay::utils::Logical> =
                            Point::from((h * s / self.zoom(), v * s / self.zoom()));
                        self.drift_pan(canvas_delta, Event::time_msec(&event));
                        let new_pos = pos + canvas_delta;
                        let serial = SERIAL_COUNTER.next_serial();
                        let under = self.surface_under(new_pos, None);
                        pointer.motion(
                            self,
                            under,
                            &MotionEvent {
                                location: new_pos,
                                serial,
                                time: Event::time_msec(&event),
                            },
                        );
                    } else if source == AxisSource::Finger {
                        // amount(axis) == Some(0.0) or None → finger lifted, launch momentum
                        self.launch_momentum();
                    }
                }
                MouseAction::Zoom => {
                    let v = event
                        .amount(Axis::Vertical)
                        .or_else(|| event.amount_v120(Axis::Vertical).map(|v| v * 15.0 / 120.0))
                        .unwrap_or(0.0);
                    if v != 0.0 {
                        let steps = -v / 30.0 * self.config.zoom_mouse_speed;
                        let factor = self.config.zoom_step.powf(steps);
                        let cur_zoom = self.zoom();
                        let new_zoom = (cur_zoom * factor).clamp(self.min_zoom(), canvas::MAX_ZOOM);

                        if new_zoom != cur_zoom {
                            let screen_pos =
                                canvas_to_screen(CanvasPos(pos), self.camera(), cur_zoom).0;
                            let new_camera = canvas::zoom_anchor_camera(pos, screen_pos, new_zoom);
                            self.with_output_state(|os| {
                                os.camera = new_camera;
                                os.zoom = new_zoom;
                                os.zoom_target = None;
                                os.zoom_animation_center = None;
                                os.camera_target = None;
                                os.overview_return = None;
                                os.momentum.stop();
                            });
                            self.update_output_from_camera();

                            let under = self.surface_under(pos, None);
                            let serial = SERIAL_COUNTER.next_serial();
                            pointer.motion(
                                self,
                                under,
                                &MotionEvent {
                                    location: pos,
                                    serial,
                                    time: Event::time_msec(&event),
                                },
                            );
                        }
                    }
                }
                _ => {} // other mouse actions don't apply to scroll
            }
            let frame = AxisFrame::new(Event::time_msec(&event));
            pointer.axis(self, frame);
            pointer.frame(self);
            return;
        }

        // No binding matched — forward scroll to the client
        let frame = build_client_axis_frame::<I>(&event);
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    /// Build a PanGrab for click-drag viewport panning.
    fn make_pan_grab(
        &self,
        canvas_pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        from_empty_canvas: bool,
    ) -> Option<PanGrab> {
        let screen_pos = canvas_to_screen(CanvasPos(canvas_pos), self.camera(), self.zoom()).0;
        Some(PanGrab {
            start_data: GrabStartData {
                focus: None,
                button,
                location: canvas_pos,
            },
            last_screen_pos: screen_pos,
            start_screen_pos: screen_pos,
            from_empty_canvas,
            dragged: false,
            output: self.active_output()?,
            last_clamped_location: canvas_pos,
        })
    }
}

/// Determine resize edges from pointer position within a 3×3 grid on the window.
/// Corners → diagonal resize, edge strips → cardinal resize, center → BottomRight fallback.
pub(super) fn edges_from_position(
    pos: Point<f64, smithay::utils::Logical>,
    window_loc: Point<i32, smithay::utils::Logical>,
    window_size: smithay::utils::Size<i32, smithay::utils::Logical>,
) -> xdg_toplevel::ResizeEdge {
    let rel_x = pos.x - window_loc.x as f64;
    let rel_y = pos.y - window_loc.y as f64;
    let w = window_size.w as f64;
    let h = window_size.h as f64;
    let in_left = rel_x < w / 3.0;
    let in_right = rel_x > w * 2.0 / 3.0;
    let in_top = rel_y < h / 3.0;
    let in_bottom = rel_y > h * 2.0 / 3.0;
    match (in_left, in_right, in_top, in_bottom) {
        (true, _, true, _) => xdg_toplevel::ResizeEdge::TopLeft,
        (_, true, true, _) => xdg_toplevel::ResizeEdge::TopRight,
        (true, _, _, true) => xdg_toplevel::ResizeEdge::BottomLeft,
        (_, true, _, true) => xdg_toplevel::ResizeEdge::BottomRight,
        (true, _, _, _) => xdg_toplevel::ResizeEdge::Left,
        (_, true, _, _) => xdg_toplevel::ResizeEdge::Right,
        (_, _, true, _) => xdg_toplevel::ResizeEdge::Top,
        (_, _, _, true) => xdg_toplevel::ResizeEdge::Bottom,
        _ => xdg_toplevel::ResizeEdge::BottomRight,
    }
}

/// Build an `AxisFrame` that faithfully forwards a scroll event to a client,
/// including `axis_stop` when the user lifts fingers from the trackpad.
///
/// libinput finger-lift semantics: `amount(axis) == Some(0.0)` means the
/// gesture ended for this axis (send `axis_stop`). `amount(axis) == None`
/// means the axis wasn't part of this event at all (send nothing).
fn build_client_axis_frame<I: InputBackend>(event: &I::PointerAxisEvent) -> AxisFrame {
    let mut frame = AxisFrame::new(Event::time_msec(event)).source(event.source());
    let is_finger = event.source() == AxisSource::Finger;
    // Finger-lift: no axis carries non-zero data. Covers both Some(0.0)
    // (newer libinput) and None-for-all-axes (older libinput).
    let is_stop = is_finger
        && !event.amount(Axis::Horizontal).is_some_and(|a| a != 0.0)
        && !event.amount(Axis::Vertical).is_some_and(|a| a != 0.0);
    for axis in [Axis::Horizontal, Axis::Vertical] {
        if let Some(amount) = event.amount(axis) {
            if amount != 0.0 {
                frame = frame
                    .value(axis, amount)
                    .relative_direction(axis, event.relative_direction(axis));
            } else if is_finger {
                frame = frame.stop(axis);
            }
        } else if is_stop {
            // Axis absent from a finger-lift event — still send stop
            frame = frame.stop(axis);
        }
        if let Some(v120) = event.amount_v120(axis) {
            frame = frame.v120(axis, v120 as i32);
        }
    }
    frame
}

/// Map resize edge to the appropriate directional cursor icon.
pub(super) fn resize_cursor(edges: xdg_toplevel::ResizeEdge) -> CursorIcon {
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
