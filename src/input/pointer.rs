use std::cell::RefCell;
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

use std::rc::Rc;

use crate::decorations::DecorationHit;
use crate::grabs::{
    MoveSurfaceGrab, NavigateGrab, PanGrab, ResizeState, ResizeSurfaceGrab, SuspendedMoveGrab,
    SuspendedResizeGrab,
};
use crate::input::DecoTarget;
use crate::state::{
    CLICK_NAVIGATE_SLOP, ClusterResizeSnapshot, DriftWm, FocusTarget, PendingMiddleClick,
    PickTarget, StageWindow, SuspendedWindow, ZoomAnimationAnchor,
};
use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::config::{self, BindingContext, MouseAction};
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

    /// Keep `held_buttons` in sync with a button event, and drain the
    /// pick-swallowed set on release. Must run for every button event on every
    /// dispatch path (including the locked-session one), or a release missed
    /// while locked leaves a stuck entry that suppresses hot corners until the
    /// button is pressed and released again — and, for the pick set, suppresses
    /// a later real release. Returns `true` when this release lifts a button
    /// whose press pick mode swallowed, so the caller suppresses the client
    /// forward too.
    pub(super) fn track_held_button(&mut self, button: u32, state: ButtonState) -> bool {
        if state == ButtonState::Pressed {
            self.held_buttons.insert(button);
            false
        } else {
            self.held_buttons.remove(&button);
            self.pick_swallowed_buttons.remove(&button)
        }
    }

    /// Priority order when button pressed:
    /// 1. Configured mouse bindings (move, resize, pan, etc.)
    /// 2. Normal click on window → focus + raise + forward to client
    /// 3. Left-click on empty canvas → pan canvas
    pub(super) fn on_pointer_button<I: InputBackend>(&mut self, event: I::PointerButtonEvent) {
        let button = event.button_code();
        let button_state = event.state();
        let released_pick_swallow = self.track_held_button(button, button_state);

        // Outputs can transiently disappear (cable unplug, GPU resume race);
        // bail out so downstream active_output() / position lookups can't panic.
        if self.space.outputs().next().is_none() {
            return;
        }
        let serial = SERIAL_COUNTER.next_serial();
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
            // A new press is a fresh interaction, so drop any armed/deferred
            // navigate unconditionally — unlike resolve's button-mismatch branch,
            // which keeps the pending because another held button lifting isn't
            // a new interaction.
            self.cancel_click_navigate();
            // A new press is a fresh interaction: drop any armed pick so it
            // can't fire after an unrelated click (mirrors the line above).
            self.cancel_pick();
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

            // During fullscreen the window fills the screen, so bindings resolve
            // in the OnWindow context. Grab bindings exit fullscreen up front and
            // dispatch on the restored canvas; discrete actions dispatch like
            // keybindings (execute_action's guard exits when needed); unbound
            // clicks forward to the app.
            if self.is_fullscreen() {
                let fs_lookup = self
                    .config
                    .mouse_button_lookup_ctx(&mods, button, BindingContext::OnWindow)
                    .cloned();
                match fs_lookup {
                    Some(MouseAction::Action(_)) => {
                        // Deferring to execute_action keeps its was_fullscreen
                        // snapshot live — no pre-exit stash that could strand
                        // if the post-exit dispatch lookup missed.
                    }
                    Some(_) => {
                        // The exit warps the pointer to keep its screen spot.
                        self.exit_fullscreen();
                        pos = pointer.current_location();
                    }
                    None => {
                        // Reclaim keyboard focus for the fullscreen window before
                        // forwarding — hover on another output may have moved focus
                        // to its window, and a plain forward wouldn't restore it.
                        // Skip when it already holds focus so a click doesn't re-emit
                        // a keyboard enter (and a popup grab keeps its focus).
                        if let Some(surface) = self
                            .active_fullscreen_window()
                            .and_then(|w| w.wl_surface().map(|s| FocusTarget(s.into_owned())))
                        {
                            let already = self
                                .window_focus_surface()
                                .is_some_and(|f| f.0 == surface.0);
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
            }

            // Layer surfaces: just forward (no compositor grabs). A press grants
            // keyboard focus to an `OnDemand` layer under the pointer.
            if self.pointer_over_layer {
                if button_state == ButtonState::Pressed {
                    let layer = pointer.current_focus().map(|f| f.0);
                    self.focus_layer_if_on_demand(layer, serial);
                    self.maybe_grab_screen_space_click(&pointer, button, serial);
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

            // Pick mode (below `zoom_interact_min`): a canvas window or stand-in
            // is one uniform target — swallow the press and arm a center / drag
            // move. Sits before try_suspended_button so the stand-in chrome (×,
            // relaunch label, resize border) is bypassed below the threshold.
            if self.try_pick_button(&pointer, pos, button, serial, mods) {
                return;
            }

            // Suspended windows are opaque: any button over one is consumed here
            // (chrome interactions, window move/resize, else focus + swallow),
            // never leaking to a window/layer beneath. Modifier bindings that
            // aren't window move/resize pass through (bindings beat chrome).
            if self.try_suspended_button(&pointer, pos, button, serial, mods) {
                return;
            }

            // `modifier_binding` gates the chrome paths below.
            let context = self.pointer_context(pos);
            let (binding, modifier_binding) = self.modifier_button_binding(&mods, button, context);

            // SSD decoration clicks: title bar → move, close button → close, resize border → resize
            if !modifier_binding
                && let Some((DecoTarget::Client(window), hit)) = self.decoration_under(pos)
            {
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
                                // A clean press-release here is still a focus click
                                // (a bottom-clipped SSD window may show only its
                                // title bar); a real title-bar drag exceeds the slop.
                                // Defer: the title bar's own double-click-fit must
                                // beat the pan.
                                self.arm_click_navigate(&window, pos, button, true);
                                let Some(initial_window_location) = self.stage.position_of(&window)
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
                                // Moving re-anchors the window, so a fill restore
                                // point (which includes position) no longer applies.
                                self.stage.clear_fill(&window);
                                self.arm_interactive_move(&window);
                                let grab = MoveSurfaceGrab::new(
                                    start_data,
                                    window,
                                    initial_window_location,
                                    output,
                                    Vec::new(),
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

                            let Some(initial_window_location) = self.stage.position_of(&window)
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
                            let cluster_members = if want_cluster {
                                self.cluster_snapshot_for_drag(
                                    &StageWindow::Client(window.clone()),
                                    initial_window_location,
                                )
                            } else {
                                Vec::new()
                            };
                            // Re-anchoring invalidates any fill restore point —
                            // for the primary and every member dragged along.
                            self.stage.clear_fill(&window);
                            for (member, _) in &cluster_members {
                                self.stage.clear_fill(member);
                            }
                            self.arm_interactive_move(&window);
                            let grab = MoveSurfaceGrab::new(
                                start_data,
                                window,
                                initial_window_location,
                                output,
                                cluster_members,
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
                        } else if let Some((DecoTarget::Suspended(s), _)) =
                            self.decoration_under(pos)
                        {
                            // Occlusion-aware `element_under` returns nothing over
                            // a stand-in; raise the stand-in itself instead of a
                            // client hidden beneath it.
                            self.focus_and_raise_suspended(s.id);
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
                    // Arm auto-navigate; resolved on this button's release so a
                    // click-drag inside the client never slides the canvas. No
                    // defer: content clicks pan immediately (protecting a client's
                    // double-click isn't the compositor's job).
                    self.arm_click_navigate(window, pos, button, false);
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

        // A pick-mode press was swallowed; its release resolves the pick and is
        // itself swallowed (never forwarded to a client that never saw the
        // press). Resolve *before* the forward below, because that forward tears
        // down any active grab and resolve's is_grabbed() guard must still see a
        // gesture/edge-pan grab. A promoted move grab, though, must still receive
        // the release to self-terminate, so only suppress the forward when no
        // grab is active.
        let released = button_state == ButtonState::Released;
        let suppress_forward = released && released_pick_swallow && !pointer.is_grabbed();
        if released {
            self.resolve_pick(button);
        }

        if !suppress_forward {
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

        // Resolve only after the release forwards to the client, so the app
        // still sees the click. (Inert in pick mode: the press was consumed
        // before arm_click_navigate could run.)
        if released {
            self.resolve_click_navigate(button, pointer.current_location());
        }
    }

    /// Keep a click-drag on the screen-space target under the pointer (wlr
    /// layer or pinned window) in screen coordinates: smithay's default click
    /// grab would freeze the canvas-adjusted focus offset for the whole drag,
    /// scaling the motion the client sees at zoom != 1. No-op while another
    /// grab is live — e.g. a layer's own popup grab must keep routing the
    /// click, since replacing it only releases its keyboard half and the popup
    /// would linger with no dismiss-on-click-outside.
    fn maybe_grab_screen_space_click(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        button: u32,
        serial: smithay::utils::Serial,
    ) {
        if pointer.is_grabbed() {
            return;
        }
        let Some(target) = pointer.current_focus() else {
            return;
        };
        let canvas_pos_0 = pointer.current_location();
        let screen_pos_0 = canvas_to_screen(CanvasPos(canvas_pos_0), self.camera(), self.zoom()).0;
        // Pick variant: in pick mode a canvas window yields None here, so
        // `focus != target` bails — the intended outcome, since this grab is
        // only for screen-space (layer / pinned) content, never a pick target.
        let Some((focus, adjusted_0)) = self.pointer_focus_under_pick(screen_pos_0, canvas_pos_0)
        else {
            return;
        };
        if focus != target {
            return;
        }
        let screen_loc = driftwm::canvas::screen_space_origin(
            adjusted_0,
            CanvasPos(canvas_pos_0),
            driftwm::canvas::ScreenPos(screen_pos_0),
        )
        .0;
        let start_data = GrabStartData {
            focus: Some((focus, adjusted_0)),
            button,
            location: canvas_pos_0,
        };
        let grab = crate::grabs::ScreenSpaceClickGrab {
            start_data,
            screen_loc,
        };
        pointer.set_grab(self, grab, serial, smithay::input::pointer::Focus::Keep);
    }

    /// Dispatch a button press over a suspended window. Suspended windows are
    /// opaque, so this consumes every button over one (returning `true`) unless
    /// a non-move/resize modifier binding should beat the chrome, in which case
    /// it defers to normal dispatch.
    pub(crate) fn try_suspended_button(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: smithay::utils::Serial,
        mods: smithay::input::keyboard::ModifiersState,
    ) -> bool {
        let Some((DecoTarget::Suspended(s), hit)) = self.decoration_under(pos) else {
            return false;
        };
        let id = s.id;

        // A held-modifier move/resize binding grabs the stand-in; other modifier
        // bindings defer to normal dispatch. A bare binding does NOT beat chrome —
        // the opaque frame acts like a title bar.
        let (binding, modifier_binding) =
            self.modifier_button_binding(&mods, button, BindingContext::OnWindow);
        match binding {
            Some(action @ (MouseAction::MoveWindow | MouseAction::MoveSnappedWindows))
                if modifier_binding =>
            {
                // Only `MoveSnappedWindows` carries the cluster; plain
                // `MoveWindow` stays single-window — same as the client path.
                let want_cluster = matches!(action, MouseAction::MoveSnappedWindows);
                self.focus_and_raise_suspended(id);
                self.start_suspended_move(pointer, &s, pos, button, serial, want_cluster);
                return true;
            }
            Some(action @ (MouseAction::ResizeWindow | MouseAction::ResizeWindowSnapped))
                if modifier_binding =>
            {
                // Derive cluster participation from the binding variant, like a
                // client's modifier resize — not from the SSD-border config flag.
                let want_cluster = matches!(action, MouseAction::ResizeWindowSnapped);
                self.focus_and_raise_suspended(id);
                self.start_suspended_resize(pointer, &s, pos, button, serial, None, want_cluster);
                return true;
            }
            Some(_) if modifier_binding => return false,
            _ => {}
        }

        if button == config::BTN_LEFT {
            match hit {
                DecorationHit::CloseButton => self.dismiss_suspended(id),
                DecorationHit::Label => {
                    self.focus_and_raise_suspended(id);
                    self.relaunch_suspended(id);
                }
                DecorationHit::TitleBar => {
                    self.focus_and_raise_suspended(id);
                    self.start_suspended_move(pointer, &s, pos, button, serial, false);
                }
                DecorationHit::ResizeBorder(edge) => {
                    self.focus_and_raise_suspended(id);
                    // A border drag has no modifier context, so it follows the
                    // config flag — same as a client's SSD-border resize.
                    let want_cluster = self.config.decoration_resize_snapped;
                    self.start_suspended_resize(
                        pointer,
                        &s,
                        pos,
                        button,
                        serial,
                        Some(edge),
                        want_cluster,
                    );
                }
                DecorationHit::Body => {
                    // The body is focus-only for every stand-in — the bar drags.
                    self.focus_and_raise_suspended(id);
                }
            }
            return true;
        }

        // Any other button over the opaque frame: focus + swallow so nothing
        // beneath receives it.
        self.focus_and_raise_suspended(id);
        true
    }

    /// Dispatch a button press in pick mode (below `zoom_interact_min`), where a
    /// canvas window or stand-in is one uniform target: the whole body picks or
    /// drag-moves it, chrome and all. Returns `true` when consumed. Bypassed
    /// above the threshold, and empty canvas falls through to on-canvas bindings.
    pub(crate) fn try_pick_button(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: smithay::utils::Serial,
        mods: smithay::input::keyboard::ModifiersState,
    ) -> bool {
        if !self.pick_mode() {
            return false;
        }
        // A live popup grab must keep routing the press or the popup can never
        // dismiss on click-outside (PopupPointerGrab::button ungrabs only on a
        // Pressed event), same hazard guarded in maybe_grab_screen_space_click.
        if pointer.is_grabbed() {
            return false;
        }
        // Configured held-modifier bindings win, so e.g. alt+drag still moves a
        // single window. Hardcode OnWindow rather than calling pointer_context
        // (which runs three hit-tests the target lookup below repeats);
        // try_suspended_button re-runs this same lookup harmlessly on the
        // fall-through.
        let (_, modifier_binding) =
            self.modifier_button_binding(&mods, button, BindingContext::OnWindow);
        if modifier_binding {
            return false;
        }

        // Resolve the target through the shared helper so the swallowed press,
        // the hover affordance and the scroll fallback can't disagree on what a
        // click hits — chrome included, which an element_under (surface-bbox)
        // lookup would miss, leaving the SSD title bar / close / borders live.
        let Some(target) = self.pick_target_under(pos) else {
            // Empty canvas (or a widget / canvas layer, which stay interactive):
            // fall through to the normal on-canvas dispatch.
            return false;
        };

        // Record the swallowed press so its release is swallowed too (drained in
        // track_held_button). Focus + raise like the suspended-tail precedent; a
        // left press also arms the center to fire on release.
        self.pick_swallowed_buttons.insert(button);
        match &target {
            PickTarget::Client(window) => self.raise_and_focus(window, serial),
            PickTarget::Suspended(id) => self.focus_and_raise_suspended(*id),
        }
        if button == config::BTN_LEFT {
            self.arm_pick(target, pos, button);
        }
        true
    }

    /// Once a pick-mode press has dragged past the click slop, promote it to a
    /// move grab (drag anywhere moves the target) and cancel the armed center.
    /// Called from both motion handlers before `pointer.motion` so the grab sees
    /// the triggering event. Returns `true` when it installed a grab.
    pub(crate) fn maybe_promote_pick(
        &mut self,
        canvas_pos: Point<f64, smithay::utils::Logical>,
    ) -> bool {
        let Some(pending) = self.pending_pick.as_ref() else {
            return false;
        };
        let button = pending.button;
        let output = pending.output.clone();
        let press_screen_pos = pending.press_screen_pos;
        let target = pending.target.clone();

        // A release lost while locked or after an output drop never reaches the
        // resolve tail, leaving the pick armed. Without this, the first drag
        // afterwards would install a move grab with no button held — the window
        // glued to the cursor. held_buttons is kept in sync on every path.
        if !self.held_buttons.contains(&button) {
            self.cancel_pick();
            return false;
        }
        let pointer = self.seat.get_pointer().unwrap();
        // set_grab would overwrite a live grab (e.g. a concurrent popup).
        if pointer.is_grabbed() {
            return false;
        }
        // Cross-output travel makes the press screen coords incomparable, same
        // guard as PendingClickNavigate::output.
        if self.active_output().as_ref() != Some(&output) {
            return false;
        }
        let cur_screen = canvas_to_screen(CanvasPos(canvas_pos), self.camera(), self.zoom()).0;
        let dx = cur_screen.x - press_screen_pos.x;
        let dy = cur_screen.y - press_screen_pos.y;
        if dx * dx + dy * dy <= CLICK_NAVIGATE_SLOP * CLICK_NAVIGATE_SLOP {
            return false;
        }

        let serial = SERIAL_COUNTER.next_serial();
        // A drag, not a click: drop the center (else dragging past the slop and
        // back within it would move *and* center).
        self.cancel_pick();
        match target {
            PickTarget::Client(window) => {
                let Some(initial_window_location) = self.stage.position_of(&window) else {
                    return false;
                };
                let Some(output) = self.active_output() else {
                    return false;
                };
                // The fill restore point references the pre-drag position.
                self.stage.clear_fill(&window);
                self.arm_interactive_move(&window);
                let start_data = GrabStartData {
                    focus: None,
                    button,
                    location: canvas_pos,
                };
                let grab = MoveSurfaceGrab::new(
                    start_data,
                    window,
                    initial_window_location,
                    output,
                    Vec::new(),
                );
                // Own the cursor for the drag; the grab's unset restores it only
                // because grab_cursor is set here.
                self.cursor.grab_cursor = true;
                self.cursor.cursor_status = CursorImageStatus::Named(CursorIcon::Grabbing);
                pointer.set_grab(self, grab, serial, Focus::Clear);
            }
            PickTarget::Suspended(id) => {
                let Some(s) = self.find_suspended(id) else {
                    return false;
                };
                // Take the cursor only once the grab is certain — start_suspended_move
                // has its own silent bails, and a stale grab_cursor would latch
                // the Grabbing icon forever (update_decoration_cursor early-returns
                // on it). SuspendedMoveGrab clears fill and installs the grab;
                // MoveSurfaceGrab can't move a stand-in.
                if !self.start_suspended_move(&pointer, &s, canvas_pos, button, serial, false) {
                    return false;
                }
                self.cursor.grab_cursor = true;
                self.cursor.cursor_status = CursorImageStatus::Named(CursorIcon::Grabbing);
            }
        }
        true
    }

    /// Returns `true` when the grab was installed; `false` on a silent bail
    /// (no position or no active output), so callers that own the cursor can
    /// avoid latching a grab icon on a move that never started.
    pub(super) fn start_suspended_move(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        s: &Rc<SuspendedWindow>,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: smithay::utils::Serial,
        want_cluster: bool,
    ) -> bool {
        let element = StageWindow::Suspended(s.clone());
        let Some(origin) = self.stage.position_of(&element) else {
            return false;
        };
        let Some(output) = self.active_output() else {
            return false;
        };
        // Only a cluster-move binding (`MoveSnappedWindows`) carries the
        // cluster; a plain move / title-bar / body drag stays single-window,
        // mirroring the client move dispatch.
        let cluster_members = if want_cluster {
            self.cluster_snapshot_for_drag(&element, origin)
        } else {
            Vec::new()
        };
        // Re-anchoring the primary and every member invalidates their fill
        // restore points.
        self.stage.clear_fill(&element);
        for (member, _) in &cluster_members {
            self.stage.clear_fill(member);
        }
        let start_data = GrabStartData {
            focus: None,
            button,
            location: pos,
        };
        let grab = SuspendedMoveGrab::new(start_data, s.id, output, origin, pos, cluster_members);
        pointer.set_grab(self, grab, serial, Focus::Clear);
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn start_suspended_resize(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        s: &Rc<SuspendedWindow>,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: smithay::utils::Serial,
        explicit_edge: Option<xdg_toplevel::ResizeEdge>,
        want_cluster: bool,
    ) {
        let element = StageWindow::Suspended(s.clone());
        let Some(origin) = self.stage.position_of(&element) else {
            return;
        };
        let Some(output) = self.active_output() else {
            return;
        };
        let size = s.size.get();
        let edges = explicit_edge.unwrap_or_else(|| edges_from_position(pos, origin, size));
        self.cursor.grab_cursor = true;
        self.cursor.cursor_status = CursorImageStatus::Named(resize_cursor(edges));
        let start_data = GrabStartData {
            focus: None,
            button,
            location: pos,
        };
        // Snapshot the cluster only when the caller opted in — the binding
        // variant (`ResizeWindowSnapped`) for a modifier resize, the config flag
        // for an SSD-border drag — mirroring the client resize path.
        let cluster_resize = if want_cluster {
            self.cluster_snapshot_for_resize(&element, edges)
        } else {
            ClusterResizeSnapshot::empty()
        };
        let grab = SuspendedResizeGrab::new(
            start_data,
            s.id,
            edges,
            origin,
            size,
            pos,
            output,
            cluster_resize,
        );
        pointer.set_grab(self, grab, serial, Focus::Clear);
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
        if !self.stage.has_pinned() {
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
                    let edge = self
                        .stage
                        .pin_of(window)
                        .map(|site| site.screen_pos)
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
            // Same policy as the unpinned click branch: a widget takes keyboard
            // focus without a raise (or MRU entry).
            if window.is_widget() {
                self.set_window_focus(
                    window.wl_surface().map(|s| FocusTarget(s.into_owned())),
                    serial,
                );
            } else {
                self.raise_and_focus(window, serial);
            }
        }
        if button_state == ButtonState::Pressed {
            self.maybe_grab_screen_space_click(pointer, button, serial);
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
        let Some(site) = self.stage.pin_of(window).cloned() else {
            return;
        };
        let Some(output) = self.output_by_name(&site.output) else {
            return;
        };
        let screen_pos = site.screen_pos;
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
        self.arm_interactive_move(window);
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
        let Some(initial_window_location) = self.stage.position_of(window) else {
            return;
        };
        let initial_window_size = window.geometry().size;

        let edges = explicit_edge.unwrap_or_else(|| {
            // Pinned windows live in screen space — infer the edge against their
            // screen rect, since the canvas-space inference is wrong at zoom != 1.
            // (Pinned dispatch already passes an explicit edge; this keeps the
            // function correct for any future inferred-edge caller.)
            if let Some((sp, output)) = self.stage.pin_of(window).and_then(|site| {
                self.output_by_name(&site.output)
                    .map(|o| (site.screen_pos, o))
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

        // Clear fit/fill state — user took manual control
        self.stage.clear_fit(window);
        self.stage.clear_fill(window);

        // Pinned windows resize in screen space; capture their `screen_pos` and
        // fixed output so the grab and the commit-time reposition use the right
        // anchor. `None` for normal canvas windows.
        let pinned_site = self.stage.pin_of(window).cloned();
        let pinned_initial_screen_pos = pinned_site.as_ref().map(|s| s.screen_pos);
        let pinned_output = pinned_site
            .as_ref()
            .and_then(|s| self.output_by_name(&s.output));

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
            self.cluster_snapshot_for_resize(&StageWindow::Client(window.clone()), edges)
        } else {
            ClusterResizeSnapshot::empty()
        };
        let constraints = crate::grabs::SizeConstraints::for_window(window);
        let locked_ratio = crate::grabs::locked_ratio_for(window, initial_window_size);
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
            locked_ratio,
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
        let mut pos = pointer.current_location();
        let source = event.source();

        // Scroll over an opaque suspended window is swallowed: there's no client
        // to forward it to, and it must not pan/zoom the canvas beneath.
        if matches!(
            self.decoration_under(pos),
            Some((DecoTarget::Suspended(_), _))
        ) {
            let frame = AxisFrame::new(Event::time_msec(&event));
            pointer.axis(self, frame);
            pointer.frame(self);
            return;
        }

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

        // During fullscreen the window fills the screen. Scroll dispatch only
        // implements continuous pan/zoom, so exit fullscreen for those and run
        // them on the restored canvas; any other bound scroll falls through
        // unexited (a no-op below); unbound scroll forwards to the app.
        if self.is_fullscreen() {
            match self
                .config
                .mouse_scroll_lookup_ctx(&mods, source, BindingContext::OnWindow)
                .cloned()
            {
                Some(MouseAction::PanViewport | MouseAction::Zoom) => {
                    // Dispatch below anchors pan/zoom on `pos`, so it must be
                    // the post-exit position (the exit warps the pointer).
                    self.exit_fullscreen();
                    pos = pointer.current_location();
                }
                Some(_) => {}
                None => {
                    let frame = build_client_axis_frame::<I>(&event);
                    pointer.axis(self, frame);
                    pointer.frame(self);
                    return;
                }
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
        let mut action = self
            .config
            .mouse_scroll_lookup_ctx(&mods, source, context)
            .cloned();

        // Pick mode cuts a canvas window off from pointer focus, so a bare
        // scroll over one finds no binding (OnWindow is empty and the `anywhere`
        // scroll defaults are all mod-qualified) and would dispatch to a client
        // that can't receive it — dying silently. Retry against OnCanvas so it
        // pans instead. Gated on a non-widget canvas window actually being under
        // the pointer (the same surface-tree test pick mode suppresses on), so
        // widget / canvas-layer scroll stays interactive and still reaches the
        // client.
        if action.is_none() && self.pick_mode() && self.pick_target_under(pos).is_some() {
            action = self
                .config
                .mouse_scroll_lookup_ctx(&mods, source, BindingContext::OnCanvas)
                .cloned();
        }

        if let Some(action) = action {
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
                        // Suspended-aware cascade: a stand-in under the panned
                        // cursor yields no focus, matching a real motion. Uses
                        // the pick variant — routing only the obvious motion
                        // sites would let this re-dispatch restore client focus
                        // on every scroll event, undoing the pick guard.
                        let screen_pos =
                            canvas_to_screen(CanvasPos(new_pos), self.camera(), self.zoom()).0;
                        let under = self.pointer_focus_under_pick(screen_pos, new_pos);
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
                        let base_zoom = self.zoom_target().unwrap_or(cur_zoom);
                        let target_zoom =
                            (base_zoom * factor).clamp(self.min_zoom(), canvas::MAX_ZOOM);

                        if target_zoom != base_zoom {
                            let screen_pos =
                                canvas_to_screen(CanvasPos(pos), self.camera(), cur_zoom).0;
                            self.with_output_state(|os| {
                                os.zoom_target = Some(target_zoom);
                                os.zoom_animation_anchor = Some(ZoomAnimationAnchor {
                                    canvas: pos,
                                    screen: screen_pos,
                                });
                                os.camera_target = None;
                                os.overview_return = None;
                                os.momentum.stop();
                            });
                            // No resync here: zoom is unchanged until the
                            // animation ticks, which warp through the guarded
                            // focus path.
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
