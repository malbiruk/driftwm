use std::time::Duration;

use smithay::{
    backend::input::{
        Axis, AxisSource, ButtonState, Device, DeviceCapability, Event, InputBackend,
        PointerAxisEvent, PointerButtonEvent,
    },
    input::pointer::{AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, Focus, GrabStartData},
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
    },
    utils::{Point, SERIAL_COUNTER, Serial},
};

use smithay::wayland::seat::WaylandFocus;

use std::rc::Rc;

use crate::decorations::DecorationHit;
use crate::grabs::{MoveGrab, NavigateGrab, PanGrab, ResizeGrab};
use crate::input::{DecoTarget, HitKind, PinnedChrome};
use crate::state::{
    CLICK_NAVIGATE_SLOP, ClusterMember, ClusterResizeSnapshot, DriftWm, FocusTarget,
    PendingMiddleClick, PickTarget, StageWindow, SuspendedWindow, ZoomAnimationAnchor,
};
use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::config::{self, Action, BindingContext, Direction, MouseAction};
use driftwm::window_ext::WindowExt;
use smithay::reexports::wayland_server::Resource;

impl DriftWm {
    /// Determine the binding context for the current pointer position.
    pub(crate) fn pointer_context(
        &self,
        pos: Point<f64, smithay::utils::Logical>,
    ) -> BindingContext {
        // Every canvas-space walk below skips pinned windows, so a real one under
        // the pointer needs its own screen-space arm — without it a click on a
        // pinned window resolves OnCanvas. This is a plain "is anything here"
        // disjunction, not a cascade: the arms are independent and none of them
        // outranks another, so their order carries no meaning.
        //
        // `has_pinned` gates the conversion because it costs two output-state
        // locks and this runs per scroll notch and per gesture begin.
        let over_pinned = self.stage.has_pinned() && {
            let screen_pos = canvas_to_screen(CanvasPos(pos), self.camera(), self.zoom()).0;
            self.pinned_window_under(screen_pos, pos).is_some()
                || !matches!(self.pinned_decoration_under(screen_pos), PinnedChrome::Miss)
                || (!self.config.resize_on_border
                    && self.pinned_resize_margin_under(screen_pos).is_some())
        };
        // SSD chrome and the CSD resize margin sit outside the surface bbox, so
        // `element_under` misses them; count them as OnWindow so on-window bindings
        // apply over the chrome, not just the client surface.
        //
        // `decoration_under` and `pinned_decoration_under` reach resize margins only
        // while `resize_on_border` is on, so with the option off both canvas and
        // pinned windows need ungated arms here to keep inert margins binding OnWindow.
        let over_window = over_pinned
            || self.element_under(pos).is_some()
            || self.canvas_layer_under(pos).is_some()
            || self.decoration_under(pos).is_some()
            || (!self.config.resize_on_border && self.resize_margin_under(pos));
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

    /// Whether `window`'s `pass_mouse` rule claims this trigger, so the binding
    /// is discarded and the event takes the ordinary unbound path to the client.
    fn pass_mouse_claims(
        &self,
        window: &smithay::desktop::Window,
        mods: &smithay::input::keyboard::ModifiersState,
        trigger: config::MouseTrigger,
    ) -> bool {
        self.live_rule_for(window)
            .is_some_and(|r| r.pass_mouse.allows(mods, trigger))
    }

    /// The toplevel whose content is under `pos`, and so can claim a binding
    /// there. A hit on the compositor's own chrome (title bar, close button,
    /// resize border) answers `None` — chrome is never `pass_mouse`'s to give.
    ///
    /// Pinned windows are skipped by the ordinary canvas-space walk, so they
    /// get their own screen-space check first, ahead of the canvas window
    /// their rect covers.
    ///
    /// Called only once a binding has matched, since `pointer_context`'s own
    /// hit-tests return none of them — this keeps the walk off every unbound
    /// event.
    fn pass_mouse_target_under(
        &self,
        pos: Point<f64, smithay::utils::Logical>,
    ) -> Option<smithay::desktop::Window> {
        if self.stage.has_pinned() {
            let screen_pos = canvas_to_screen(CanvasPos(pos), self.camera(), self.zoom()).0;
            match self.pinned_decoration_under(screen_pos) {
                PinnedChrome::Hit(..) => return None,
                PinnedChrome::Covered => return self.pinned_element_under(screen_pos),
                // An inert margin is chrome too, and the canvas walk below
                // would answer for the window drawn behind the pin.
                PinnedChrome::Miss if self.pinned_resize_margin_under(screen_pos).is_some() => {
                    return None;
                }
                PinnedChrome::Miss => {}
            }
        }
        match self.topmost_under(pos, |_, _| true)? {
            (StageWindow::Client(w), HitKind::Content) => Some(w),
            _ => None,
        }
    }

    /// Whether the fullscreen window on the active output claims `trigger`.
    fn fullscreen_claims(
        &self,
        mods: &smithay::input::keyboard::ModifiersState,
        trigger: config::MouseTrigger,
    ) -> bool {
        self.active_fullscreen_window()
            .is_some_and(|w| self.pass_mouse_claims(&w, mods, trigger))
    }

    /// Whether the toplevel whose content is under `pos` claims `trigger`.
    /// Callers own the pick-mode gate; `try_pinned_button` is the one caller
    /// that leaves it off.
    fn target_claims(
        &self,
        pos: Point<f64, smithay::utils::Logical>,
        mods: &smithay::input::keyboard::ModifiersState,
        trigger: config::MouseTrigger,
    ) -> bool {
        self.pass_mouse_target_under(pos)
            .is_some_and(|w| self.pass_mouse_claims(&w, mods, trigger))
    }

    /// Whether the subject of a wheel notch at `pos` claims it via `pass_mouse`.
    /// On a fullscreen output the subject is the fullscreen window, since the
    /// parked canvas bbox there is not what the notch acts over.
    ///
    /// A canvas subject is skipped in pick mode: below `interact_min` a
    /// canvas window has no pointer focus to forward to.
    fn pass_mouse_claims_notch(
        &self,
        pos: Point<f64, smithay::utils::Logical>,
        mods: &smithay::input::keyboard::ModifiersState,
        up: bool,
        context: BindingContext,
    ) -> bool {
        let trigger = config::MouseTrigger::for_wheel_step(up);
        if self.is_fullscreen() {
            self.fullscreen_claims(mods, trigger)
        } else if context == BindingContext::OnWindow && !self.pick_mode() {
            self.target_claims(pos, mods, trigger)
        } else {
            false
        }
    }

    /// Keep `held_buttons` in sync with a button event, and drain the
    /// swallowed set on release. Must run for every button event on every
    /// dispatch path (including the locked-session one), or a release missed
    /// while locked leaves a stuck entry that suppresses hot corners until the
    /// button is pressed and released again — and, for the swallowed set,
    /// suppresses a later real release. Returns `true` when this release lifts
    /// a button whose press was swallowed, so the caller suppresses the client
    /// forward too.
    pub(super) fn track_held_button(&mut self, button: u32, state: ButtonState) -> bool {
        if state == ButtonState::Pressed {
            self.held_buttons.insert(button);
            false
        } else {
            self.held_buttons.remove(&button);
            self.swallowed_buttons.remove(&button)
        }
    }

    /// Priority order when button pressed:
    /// 1. Configured mouse bindings (move, resize, pan, etc.)
    /// 2. Normal click on window → focus + raise + forward to client
    /// 3. Left-click on empty canvas → pan canvas
    ///
    /// `forward_serial` is the serial another stream already announced this
    /// press with. Only a press forwarded to a client, and the implicit grab it
    /// installs, carries it: a binding's grab gets its own, or a client holding
    /// the other stream's serial could cite it to replace that grab.
    pub(super) fn on_pointer_button<I: InputBackend, E: PointerButtonEvent<I>>(
        &mut self,
        event: E,
        forward_serial: Option<Serial>,
    ) {
        let button = event.button_code();
        let button_state = event.state();
        let released_swallowed = self.track_held_button(button, button_state);

        // Outputs can transiently disappear (cable unplug, GPU resume race);
        // bail out so downstream active_output() / position lookups can't panic.
        if self.space.outputs().next().is_none() {
            return;
        }
        // A scene change and this press can share one dispatch — a bar unmaps,
        // the finger lands — and the routing below reads `pointer_over_layer`
        // and focus, which are only as fresh as the last pull.
        self.refresh_pointer_focus();
        let serial = SERIAL_COUNTER.next_serial();
        let forward_serial = forward_serial.unwrap_or(serial);
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

            // A pen withholds `wl_pointer` focus from a client with the tool
            // bound, so where its press landed is read off the pick wherever
            // the mouse path reads it off that focus.
            let pen_pick = self.pointer_pen.is_some().then(|| {
                let pos = pointer.current_location();
                let screen_pos = canvas_to_screen(CanvasPos(pos), self.camera(), self.zoom()).0;
                self.pointer_focus_under_pick(screen_pos, pos)
                    .map(|(focus, _)| focus.0)
            });
            // smithay's popup grab dismisses the menu on a press whose
            // `wl_pointer` focus is not the popup's client — which a tap on the
            // menu of a withheld client never has. That client already has the
            // tap on tablet-v2; a tap elsewhere falls through and dismisses.
            if let Some(pick) = &pen_pick
                && pointer.current_focus().is_none()
                && let Some(popup) = self.popup_grab.as_ref().and_then(|g| g.grab.current_grab())
                && pick
                    .as_ref()
                    .is_some_and(|s| s.id().same_client_as(&popup.0.id()))
            {
                return;
            }

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
                // An Overlay layer still renders over the window, and only what
                // the layer branch below runs applies over it.
                let mut fs_lookup = if self.pointer_over_layer {
                    self.layer_button_binding(&pointer, &mods, button)
                } else {
                    self.config
                        .mouse_button_lookup_ctx(&mods, button, BindingContext::OnWindow)
                        .cloned()
                };
                // A `pass_mouse` claim becomes "no binding", so the `None` arm
                // below reclaims keyboard focus and forwards to the client. It
                // needs its own gate here: the `Action(_)` arm falls through
                // without returning.
                if fs_lookup.is_some()
                    && !self.pointer_over_layer
                    && self.fullscreen_claims(&mods, config::MouseTrigger::Button(button))
                {
                    fs_lookup = None;
                }
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
                                serial: forward_serial,
                                time: Event::time_msec(&event),
                            },
                        );
                        pointer.frame(self);
                        return;
                    }
                }
            }

            // A press grants keyboard focus to an `OnDemand` layer under the
            // pointer.
            if self.pointer_over_layer {
                if self.try_layer_button(&pointer, pos, button, serial, &mods) {
                    return;
                }
                if button_state == ButtonState::Pressed {
                    let layer = pen_pick.unwrap_or_else(|| pointer.current_focus().map(|f| f.0));
                    self.focus_layer_if_on_demand(layer, serial);
                    self.maybe_grab_screen_space_click(&pointer, button, forward_serial);
                }
                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial: forward_serial,
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
                forward_serial,
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
            let (mut binding, mut modifier_binding) =
                self.modifier_button_binding(&mods, button, context);

            // `pass_mouse`: clearing `modifier_binding` too keeps the decoration
            // branch below live, so chrome always stays compositor-owned. Off in
            // pick mode: there a canvas window has no pointer focus to forward to.
            if binding.is_some()
                && context == BindingContext::OnWindow
                && !self.pick_mode()
                && self.target_claims(pos, &mods, config::MouseTrigger::Button(button))
            {
                binding = None;
                modifier_binding = false;
            }

            // SSD decoration clicks: title bar → move, close button → close, resize border → resize
            if !modifier_binding
                && let Some((DecoTarget::Client(window), hit)) = self.decoration_under(pos)
            {
                // Decoration interactions must only apply to the topmost window.
                // Otherwise a lower SSD title bar/border can steal clicks through
                // an overlapping window.
                if self
                    .surface_under(pos, None)
                    .and_then(|(target, _)| self.window_for_surface_root(&target.0))
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
                        // Chrome counts: the modifier suppressed the decoration
                        // branch above, so a title-bar press must still resolve
                        // to its window. The walk skips pinned windows.
                        if let Some(window) = self.topmost_client_under(pos)
                            && let Some(surface) = window.wl_surface()
                            && !config::applied_rule(&surface).is_some_and(|r| r.widget)
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
                            let grab = MoveGrab::new(
                                start_data,
                                window,
                                initial_window_location,
                                output,
                                cluster_members,
                            );
                            pointer.set_grab(self, grab, serial, Focus::Clear);
                            return;
                        }
                        // Empty canvas or a widget — fall through to normal click
                    }
                    MouseAction::ResizeWindow | MouseAction::ResizeWindowSnapped => {
                        // Opt-in cluster propagation: only
                        // `ResizeWindowSnapped` captures the cluster; plain
                        // `ResizeWindow` builds an empty snapshot so the
                        // grab behaves like pre-slice-2 single-window resize.
                        let want_cluster = matches!(action, MouseAction::ResizeWindowSnapped);
                        if let Some(window) = self.topmost_client_under(pos)
                            && !window
                                .wl_surface()
                                .and_then(|s| config::applied_rule(&s))
                                .is_some_and(|r| r.widget)
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
                        // Empty canvas or a widget — fall through
                    }
                    MouseAction::PanViewport => {
                        let from_empty = context == BindingContext::OnCanvas;
                        self.start_pan_grab(&pointer, pos, button, serial, from_empty);
                        return;
                    }
                    MouseAction::CenterNearest => {
                        self.start_navigate_grab(
                            &pointer,
                            pos,
                            button,
                            serial,
                            Action::CenterNearest,
                        );
                        return;
                    }
                    MouseAction::FocusNearest => {
                        self.start_navigate_grab(
                            &pointer,
                            pos,
                            button,
                            serial,
                            Action::FocusNearest,
                        );
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

        // A swallowed press has its release swallowed too (never forwarded to a
        // client that never saw the press), and a pick-mode release resolves
        // the pick. Resolve *before* the forward below, because that forward tears
        // down any active grab and resolve's is_grabbed() guard must still see a
        // gesture/edge-pan grab. A promoted move grab, though, must still receive
        // the release to self-terminate, so only suppress the forward when no
        // grab is active.
        let released = button_state == ButtonState::Released;
        let suppress_forward = released && released_swallowed && !pointer.is_grabbed();
        if released {
            self.resolve_pick(button);
        }

        if !suppress_forward {
            pointer.button(
                self,
                &ButtonEvent {
                    button,
                    state: button_state,
                    serial: forward_serial,
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

    /// The binding a press over a screen-space layer surface runs: an
    /// `anywhere` one, since on-window and on-canvas bindings never apply there
    /// (`pointer_context` would read the canvas beneath the layer). A window
    /// binding has no window to act on over a layer, so it counts as unbound.
    /// So does everything while a grab owns the pointer: a layer's open popup
    /// or a click held on it keeps routing the press, and a binding's grab
    /// would replace it, orphaning the popup.
    fn layer_button_binding(
        &self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        mods: &smithay::input::keyboard::ModifiersState,
        button: u32,
    ) -> Option<MouseAction> {
        if pointer.is_grabbed() {
            return None;
        }
        self.config
            .mouse_button_lookup_ctx(mods, button, BindingContext::Anywhere)
            .filter(|b| {
                matches!(
                    b,
                    MouseAction::PanViewport
                        | MouseAction::CenterNearest
                        | MouseAction::FocusNearest
                        | MouseAction::Action(_)
                )
            })
            .cloned()
    }

    /// Run [`Self::layer_button_binding`] for a press over a layer. Returns
    /// `true` when the press is consumed; the layer then sees neither it nor
    /// its release.
    fn try_layer_button(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: Serial,
        mods: &smithay::input::keyboard::ModifiersState,
    ) -> bool {
        match self.layer_button_binding(pointer, mods, button) {
            Some(MouseAction::PanViewport) => {
                self.start_pan_grab(pointer, pos, button, serial, false);
            }
            Some(MouseAction::CenterNearest) => {
                self.start_navigate_grab(pointer, pos, button, serial, Action::CenterNearest);
            }
            Some(MouseAction::FocusNearest) => {
                self.start_navigate_grab(pointer, pos, button, serial, Action::FocusNearest);
            }
            Some(MouseAction::Action(action)) => {
                // No grab is left to take the release.
                self.swallowed_buttons.insert(button);
                self.execute_action(&action);
            }
            _ => return false,
        }
        true
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
        self.swallowed_buttons.insert(button);
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
                let grab = MoveGrab::new(
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
                // on it). start_suspended_move clears fill and installs the grab.
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
        self.arm_interactive_move(&s.id);
        let grab = MoveGrab::new(start_data, s.id, origin, output, cluster_members);
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
        // Before the anchor is taken, as at every other resize seed site: the
        // cancel is what stops a flight from moving the camera the anchor is
        // projected through.
        self.arm_interactive_move(&s.id);
        let suspended_constraints =
            crate::grabs::SizeConstraints::for_suspended(self.suspended_chrome());
        let (start_screen, start_zoom) = crate::grabs::resize_screen_anchor(&output, pos);
        let grab = ResizeGrab {
            start_data,
            target: ClusterMember::Suspended(s.id),
            edges,
            initial_window_location: origin,
            initial_window_size: size,
            last_window_size: size,
            output,
            start_screen,
            start_zoom,
            last_clamped_location: pos,
            snap: driftwm::layout::snap::SnapState::default(),
            constraints: suspended_constraints,
            cluster_resize,
            pinned_initial_screen_pos: None,
            touch_start: None,
            touch_slots: 0,
            locked_ratio: None,
        };
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
        forward_serial: smithay::utils::Serial,
        mods: smithay::input::keyboard::ModifiersState,
        time: u32,
    ) -> bool {
        if !self.stage.has_pinned() {
            return false;
        }
        let screen_pos = canvas_to_screen(CanvasPos(pos), self.camera(), self.zoom()).0;

        // `modifier_binding` gates the pinned chrome path below, as on the canvas path.
        let (mut binding, mut modifier_binding) =
            self.modifier_button_binding(&mods, button, BindingContext::OnWindow);

        // `pass_mouse` resolves its subject here rather than reusing the owner
        // lookup below: the chrome arm in between deliberately hits outside the
        // surface, and clearing `modifier_binding` has to reach it.
        if binding.is_some() && self.target_claims(pos, &mods, config::MouseTrigger::Button(button))
        {
            binding = None;
            modifier_binding = false;
        }

        if !modifier_binding
            && button == config::BTN_LEFT
            && let PinnedChrome::Hit(window, hit) = self.pinned_decoration_under(screen_pos)
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

        // Any press the bare-left chrome arm above did not consume is still the
        // pin's over its chrome or resize margin: a bound action acts on it, an
        // unbound press focuses it. Neither band gives pointer focus, so the
        // forward below reaches no client, but it still installs the implicit
        // grab that keeps focus off a client until release.
        let Some(window) = self.pinned_owner_under(screen_pos) else {
            return false;
        };
        if let Some(action) = binding
            && !window.is_widget()
        {
            match action {
                MouseAction::MoveWindow | MouseAction::MoveSnappedWindows => {
                    self.raise_and_focus(&window, serial);
                    self.start_pinned_move(pointer, &window, pos, button, serial);
                    return true;
                }
                MouseAction::ResizeWindow | MouseAction::ResizeWindowSnapped => {
                    self.raise_and_focus(&window, serial);
                    // Infer the edge in screen space against the pinned rect.
                    let edge = self
                        .stage
                        .pin_of(&window)
                        .map(|site| site.screen_pos)
                        .map(|sp| edges_from_position(screen_pos, sp, window.geometry().size));
                    self.start_compositor_resize_with_edge(
                        pointer, &window, pos, button, serial, edge, false,
                    );
                    return true;
                }
                MouseAction::Action(ref a) => {
                    self.raise_and_focus(&window, serial);
                    let a = a.clone();
                    self.execute_action(&a);
                    return true;
                }
                // Viewport actions aren't pinned-specific — defer to normal dispatch.
                MouseAction::PanViewport
                | MouseAction::CenterNearest
                | MouseAction::FocusNearest => return false,
                _ => {}
            }
        }
        // Same policy as the unpinned click branch: a widget takes keyboard
        // focus without a raise (or MRU entry).
        if window.is_widget() {
            self.set_window_focus(
                window.wl_surface().map(|s| FocusTarget(s.into_owned())),
                serial,
            );
        } else {
            self.raise_and_focus(&window, serial);
        }
        if button_state == ButtonState::Pressed {
            self.maybe_grab_screen_space_click(pointer, button, forward_serial);
        }
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state: button_state,
                serial: forward_serial,
                time,
            },
        );
        pointer.frame(self);
        true
    }

    /// Start a screen-space move grab for a pinned window. The grab tracks the
    /// cursor with the fixed screen-offset captured here.
    ///
    /// Returns `true` when the grab was installed; `false` on a silent bail (the
    /// window isn't pinned, or its pin output is gone), so callers that raise or
    /// focus around the drag can hold that back until the grab is certain.
    pub(crate) fn start_pinned_move(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        window: &smithay::desktop::Window,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: smithay::utils::Serial,
    ) -> bool {
        let Some(site) = self.stage.pin_of(window).cloned() else {
            return false;
        };
        let Some(output) = self.output_by_name(&site.output) else {
            return false;
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
        let grab = MoveGrab::new_pinned(start_data, window.clone(), output, grab_offset);
        pointer.set_grab(self, grab, serial, Focus::Clear);
        true
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
    ) -> bool {
        self.start_compositor_resize_with_edge(
            pointer,
            window,
            pos,
            button,
            serial,
            None,
            want_cluster,
        )
    }

    /// Returns `true` when the grab was installed. Every bail comes before the
    /// first side effect, so `false` means nothing was touched — no fit/fill
    /// clear, no `ResizeState`, no `Resizing` on the toplevel, no grab cursor —
    /// so a caller that raised or focused for this resize can hold that back
    /// instead of stranding a half-started resize. The gesture arms do exactly
    /// that; the pointer callers here raise first and ignore the result, since
    /// their own pickers already rule every bail out.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_compositor_resize_with_edge(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        window: &smithay::desktop::Window,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: smithay::utils::Serial,
        explicit_edge: Option<xdg_toplevel::ResizeEdge>,
        want_cluster: bool,
    ) -> bool {
        let Some(initial_window_location) = self.stage.position_of(window) else {
            return false;
        };
        let initial_window_size = window.geometry().size;

        let Some(wl_surface) = window.wl_surface().map(|s| s.into_owned()) else {
            return false;
        };

        // Pinned windows resize in screen space; capture their `screen_pos` and
        // fixed output so the grab and the commit-time reposition use the right
        // anchor. `None` for normal canvas windows.
        let pinned_site = self.stage.pin_of(window).cloned();
        let pinned_initial_screen_pos = pinned_site.as_ref().map(|s| s.screen_pos);
        let pinned_output = pinned_site
            .as_ref()
            .and_then(|s| self.output_by_name(&s.output));
        let Some(output) = pinned_output.or_else(|| self.active_output()) else {
            return false;
        };

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

        self.begin_client_resize(
            window,
            &wl_surface,
            edges,
            initial_window_size,
            pinned_initial_screen_pos,
        );

        self.cursor.grab_cursor = true;
        self.cursor.cursor_status = CursorImageStatus::Named(resize_cursor(edges));

        let start_data = GrabStartData {
            focus: None,
            button,
            location: pos,
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
        let (start_screen, start_zoom) = crate::grabs::resize_screen_anchor(&output, pos);
        let grab = ResizeGrab {
            start_data,
            target: ClusterMember::Client(window.clone()),
            edges,
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
            output,
            start_screen,
            start_zoom,
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
        true
    }

    pub(super) fn on_pointer_axis<I: InputBackend>(&mut self, event: I::PointerAxisEvent) {
        if self.space.outputs().next().is_none() {
            return;
        }
        // A scene change and this scroll can share one dispatch, and the
        // routing below is only as fresh as the last pull.
        self.refresh_pointer_focus();
        // Over a layer surface only `anywhere` bindings apply, and what they
        // leave unbound is the layer's scroll. The canvas-space hit tests below
        // read what lies beneath the layer, so each one skips it.
        let over_layer = self.pointer_over_layer;

        let keyboard = self.seat.get_keyboard().unwrap();
        let mods = keyboard.modifier_state();
        let pointer = self.seat.get_pointer().unwrap();
        let mut pos = pointer.current_location();
        let source = event.source();

        // Capped once above every branch below, so whichever of them owns
        // this scroll — a client, `drift_pan`, or nothing — sees the same
        // amounts.
        let amounts = self.damp_axis_amounts::<I>(&event);

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
            let notch_context = if over_layer {
                BindingContext::Anywhere
            } else if self.is_fullscreen() {
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
                && (over_layer || !self.pass_mouse_claims_notch(pos, &mods, v < 0.0, notch_context))
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
                self.reset_scroll_window();
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
            // An Overlay layer still renders over the window.
            let fs_context = if over_layer {
                BindingContext::Anywhere
            } else {
                BindingContext::OnWindow
            };
            let mut fs_scroll = self
                .config
                .mouse_scroll_lookup_ctx(&mods, source, fs_context)
                .cloned();
            if fs_scroll.is_some()
                && !over_layer
                && self.fullscreen_claims(&mods, config::MouseTrigger::for_axis_source(source))
            {
                fs_scroll = None;
            }
            match fs_scroll {
                Some(MouseAction::PanViewport | MouseAction::Zoom) => {
                    // Dispatch below anchors pan/zoom on `pos`, so it must be
                    // the post-exit position (the exit warps the pointer).
                    self.exit_fullscreen();
                    pos = pointer.current_location();
                }
                Some(_) => {}
                None => {
                    let frame = build_client_axis_frame::<I>(&event, amounts);
                    self.forward_client_axis(frame, amounts);
                    return;
                }
            }
        }

        // Compute context — recent_pan stickiness forces OnCanvas to prevent
        // jitter when a window slides under the pointer during a pan gesture.
        let recent_pan = self.last_scroll_pan().is_some_and(|t: std::time::Instant| {
            t.elapsed() < std::time::Duration::from_millis(150)
        });
        let context = if over_layer {
            BindingContext::Anywhere
        } else if recent_pan {
            BindingContext::OnCanvas
        } else {
            self.pointer_context(pos)
        };

        // Single lookup: context-aware
        let mut action = self
            .config
            .mouse_scroll_lookup_ctx(&mods, source, context)
            .cloned();

        // `pass_mouse`: the `recent_pan` override above already resolves
        // OnCanvas, so a claim here can't fire mid-pan.
        if action.is_some()
            && context == BindingContext::OnWindow
            && !self.pick_mode()
            && self.target_claims(pos, &mods, config::MouseTrigger::for_axis_source(source))
        {
            action = None;
        }

        // Pick mode cuts a canvas window off from pointer focus, so a bare
        // scroll over one finds no binding (OnWindow is empty and the `anywhere`
        // scroll defaults are all mod-qualified) and would dispatch to a client
        // that can't receive it — dying silently. Retry against OnCanvas so it
        // pans instead. Gated on a non-widget canvas window actually being under
        // the pointer (the same surface-tree test pick mode suppresses on), so
        // widget / canvas-layer scroll stays interactive and still reaches the
        // client.
        if action.is_none()
            && !over_layer
            && self.pick_mode()
            && self.pick_target_under(pos).is_some()
        {
            action = self
                .config
                .mouse_scroll_lookup_ctx(&mods, source, BindingContext::OnCanvas)
                .cloned();
        }

        if let Some(action) = action {
            // Set by the pan branch, so the tail below keeps the window it
            // just recorded into.
            let mut panned = false;
            match action {
                MouseAction::PanViewport => {
                    // Damped, like a client gets: `drift_pan` feeds the
                    // velocity tracker the canvas flings from.
                    let h = amounts.0.unwrap_or(0.0);
                    let v = amounts.1.unwrap_or(0.0);
                    if h != 0.0 || v != 0.0 {
                        if source == AxisSource::Finger {
                            self.set_last_scroll_pan(Some(std::time::Instant::now()));
                        }
                        // Delivered, not swallowed, so it counts toward the
                        // speed the next cap measures against.
                        panned = true;
                        self.record_scroll_sample(amounts, Event::time_msec(&event));
                        let s = self.config.trackpad_speed;
                        let canvas_delta: Point<f64, smithay::utils::Logical> =
                            Point::from((h * s / self.zoom(), v * s / self.zoom()));
                        let canvas_delta = self.drift_pan(canvas_delta, Event::time_msec(&event));
                        // Zero means the camera did not move: a fullscreen output
                        // refusing the pan, a `trackpad_speed` of 0, or no active
                        // output to pan. The cursor is where it already was, so
                        // dispatching would only flush a same-position event and
                        // re-run the hit test — in the fullscreen case, over a
                        // scene the cull has already removed.
                        if canvas_delta.x != 0.0 || canvas_delta.y != 0.0 {
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
                            self.dispatch_pointer_motion(
                                under,
                                new_pos,
                                serial,
                                Event::time_msec(&event),
                            );
                        }
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
                            self.user_moved_camera();
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
            if !panned {
                self.reset_scroll_window();
            }
            let frame = AxisFrame::new(Event::time_msec(&event));
            pointer.axis(self, frame);
            pointer.frame(self);
            return;
        }

        // No binding matched — forward scroll to the client, unless the pointer
        // sits on an opaque suspended window: there's no client to forward to,
        // and an unbound scroll must not pan/zoom the canvas beneath. The
        // swallow lives at the tail, not ahead of the binding lookup, so a
        // bound scroll (an `anywhere` alt+wheel zoom, mod+wheel pan) still
        // fires over a stand-in like over any window.
        if !over_layer
            && matches!(
                self.decoration_under(pos),
                Some((DecoTarget::Suspended(_), _))
            )
        {
            self.reset_scroll_window();
            let frame = AxisFrame::new(Event::time_msec(&event));
            pointer.axis(self, frame);
            pointer.frame(self);
            return;
        }
        let frame = build_client_axis_frame::<I>(&event, amounts);
        self.forward_client_axis(frame, amounts);
    }

    /// Drop the sliding window. Called wherever a scroll is consumed rather
    /// than delivered: nobody saw those deltas, so they are not this gesture.
    fn reset_scroll_window(&mut self) {
        self.scroll_window.clear();
        self.scroll_window_time = None;
        self.scroll_last_frame = None;
    }

    /// Cap this event's finger amounts at [`SCROLL_CLAMP`] times the speed of
    /// the gesture so far, and hand back what everything downstream must use.
    ///
    /// **Why this exists.** As fingers leave a pad the contact centroid shifts,
    /// and some pads report that as a run of accelerating deltas — measured at
    /// 2-5x the gesture's median on the pad this was written for. A client reads
    /// the run as a fast flick and scrolls on after the gesture ended, and
    /// `drift_pan`'s velocity tracker banks it as canvas momentum, so the cap
    /// sits above the fork between the two. Nothing is delayed, dropped or
    /// re-stamped: holding frames back to cancel them later releases them in
    /// batches, which a client times as an even bigger flick.
    ///
    /// Read-only against the window; recording is
    /// [`Self::record_scroll_sample`], which only the paths that deliver the
    /// scroll call. Non-finger sources and damping off pass through untouched.
    fn damp_axis_amounts<I: InputBackend>(
        &mut self,
        event: &I::PointerAxisEvent,
    ) -> (Option<f64>, Option<f64>) {
        let raw = (event.amount(Axis::Horizontal), event.amount(Axis::Vertical));
        if event.source() != AxisSource::Finger || !self.config.trackpad.scroll_liftoff_damping {
            self.reset_scroll_window();
            return raw;
        }

        // Motionless fingers produce no events, so a window from before a
        // pause describes a gesture that is already over.
        let time = Event::time_msec(event);
        if self
            .scroll_window_time
            .is_some_and(|t| time.wrapping_sub(t) > SCROLL_IDLE_MS)
        {
            self.reset_scroll_window();
        }

        // An axis with too few samples of its own borrows the other's speed:
        // the centroid shifts in two dimensions, so the sideways half of the
        // burst lands on an axis a vertical gesture never touched.
        let (mean_h, mean_v) = self.scroll_window_mean_abs();
        let mut damped = raw;
        cap_axis(&mut damped.0, mean_h.or(mean_v).map(|m| m * SCROLL_CLAMP));
        cap_axis(&mut damped.1, mean_v.or(mean_h).map(|m| m * SCROLL_CLAMP));
        damped
    }

    /// Record a delivered finger-scroll sample in the sliding window.
    fn record_scroll_sample(&mut self, amounts: (Option<f64>, Option<f64>), time: u32) {
        if self.scroll_window.len() == SCROLL_WINDOW_LEN {
            self.scroll_window.pop_front();
        }
        self.scroll_window.push_back(amounts);
        self.scroll_window_time = Some(time);
    }

    /// Forward a scroll frame to the client, recording `amounts` — the damped
    /// values it was built from — as this gesture's latest sample. A
    /// finger-lift parts with a delta of its own ahead of the stop; see
    /// [`Self::damp_axis_amounts`] for why.
    fn forward_client_axis(&mut self, frame: AxisFrame, amounts: (Option<f64>, Option<f64>)) {
        let is_finger = frame.source == Some(AxisSource::Finger);
        if !is_finger || !self.config.trackpad.scroll_liftoff_damping {
            self.reset_scroll_window();
            self.send_axis_frame(frame);
            return;
        }

        if frame.stop.0 || frame.stop.1 {
            self.send_parting_delta(frame.time);
            self.reset_scroll_window();
            self.send_axis_frame(frame);
            return;
        }

        self.record_scroll_sample(amounts, frame.time);
        self.scroll_last_frame = Some(frame);
        self.send_axis_frame(frame);
    }

    /// Mean absolute delta per axis, each axis counting only the events that
    /// carried it — counting an untouched axis as zero would dilute it.
    /// `None` below [`SCROLL_WINDOW_MIN`] samples, where the average is still
    /// the opening of the gesture rather than its speed.
    fn scroll_window_mean_abs(&self) -> (Option<f64>, Option<f64>) {
        let mut sum = (0.0, 0.0);
        let mut count = (0usize, 0usize);
        for (h, v) in &self.scroll_window {
            if let Some(h) = h {
                sum.0 += h.abs();
                count.0 += 1;
            }
            if let Some(v) = v {
                sum.1 += v.abs();
                count.1 += 1;
            }
        }
        let mean =
            |sum: f64, count: usize| (count >= SCROLL_WINDOW_MIN).then(|| sum / count as f64);
        (mean(sum.0, count.0), mean(sum.1, count.1))
    }

    /// Send the parting delta a finger-lift gets instead of the pad's own
    /// burst: the window's signed mean scaled by [`SCROLL_FLING`], on the
    /// source and in the direction of the frames before it.
    fn send_parting_delta(&mut self, time: u32) {
        let Some(template) = self.scroll_last_frame else {
            return;
        };
        if self.scroll_window.is_empty() {
            return;
        }
        // Per axis over its own samples, like the cap. No minimum, unlike the
        // cap: a short flick is exactly the gesture to report.
        let mut sum = (0.0, 0.0);
        let mut count = (0usize, 0usize);
        for (dh, dv) in &self.scroll_window {
            if let Some(dh) = dh {
                sum.0 += dh;
                count.0 += 1;
            }
            if let Some(dv) = dv {
                sum.1 += dv;
                count.1 += 1;
            }
        }
        let mean = |sum: f64, count: usize| {
            if count == 0 {
                0.0
            } else {
                sum / count as f64 * SCROLL_FLING
            }
        };
        let (h, v) = (mean(sum.0, count.0), mean(sum.1, count.1));
        if h == 0.0 && v == 0.0 {
            return;
        }
        let mut parting = template;
        parting.time = time;
        parting.axis = (h, v);
        parting.v120 = None;
        parting.stop = (false, false);
        self.send_axis_frame(parting);
    }

    fn send_axis_frame(&mut self, frame: AxisFrame) {
        let pointer = self.seat.get_pointer().unwrap();
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    /// Start a PanGrab for click-drag viewport panning.
    fn start_pan_grab(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        canvas_pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: Serial,
        from_empty_canvas: bool,
    ) {
        self.set_panning(true);
        let Some(output) = self.active_output() else {
            return;
        };
        let screen_pos = canvas_to_screen(CanvasPos(canvas_pos), self.camera(), self.zoom()).0;
        let grab = PanGrab {
            start_data: GrabStartData {
                focus: None,
                button,
                location: canvas_pos,
            },
            last_screen_pos: screen_pos,
            start_screen_pos: screen_pos,
            from_empty_canvas,
            dragged: false,
            output,
            last_clamped_location: canvas_pos,
        };
        // The grab ignores the warps a flight sends it, so a
        // flight left running would fly on under the hand.
        self.cancel_animations_on(&grab.output);
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    fn start_navigate_grab(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        canvas_pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: Serial,
        action: fn(Direction) -> Action,
    ) {
        let Some(output) = self.active_output() else {
            return;
        };
        let screen_pos = canvas_to_screen(CanvasPos(canvas_pos), self.camera(), self.zoom()).0;
        let start_data = GrabStartData {
            focus: None,
            button,
            location: canvas_pos,
        };
        let grab = NavigateGrab::new(start_data, screen_pos, output, action);
        pointer.set_grab(self, grab, serial, Focus::Clear);
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

/// Cap on a finger delta, as a multiple of the sliding average of the deltas
/// before it. The burst runs 2-5x the gesture's median, a deliberate flick
/// ramps up far more gently, and the average is itself built from capped
/// deltas — so the useful range is narrow and just above 1.
const SCROLL_CLAMP: f64 = 1.1;

/// The parting delta at a finger-lift, as a fraction of that average.
const SCROLL_FLING: f64 = 0.3;

/// How many deltas the sliding average spans — at the ~4 ms a pad reports in,
/// the last ~30 ms of the gesture.
const SCROLL_WINDOW_LEN: usize = 8;

/// Samples an axis needs before it caps anything.
const SCROLL_WINDOW_MIN: usize = 3;

/// How much silence ends a gesture, in event-time milliseconds. The same delay
/// the canvas takes as the fingers having stopped, so the two halves agree.
const SCROLL_IDLE_MS: u32 =
    crate::state::viewport_animation::MOMENTUM_LAUNCH_DELAY.as_millis() as u32;

/// Cap `value` at `limit`, in whichever direction it points. `None` is an axis
/// with no measured speed yet and `Some(0.0)` a measured standstill; neither
/// caps anything, since a cap of zero would stop the scroll dead.
fn cap_axis(value: &mut Option<f64>, limit: Option<f64>) {
    let (Some(v), Some(limit)) = (value.as_mut(), limit) else {
        return;
    };
    if limit > 0.0 && v.abs() > limit {
        *v = v.signum() * limit;
    }
}

/// Build an `AxisFrame` that faithfully forwards a scroll event to a client,
/// including `axis_stop` when the user lifts fingers from the trackpad.
///
/// libinput finger-lift semantics: `amount(axis) == Some(0.0)` means the
/// gesture ended for this axis (send `axis_stop`). `amount(axis) == None`
/// means the axis wasn't part of this event at all (send nothing).
fn build_client_axis_frame<I: InputBackend>(
    event: &I::PointerAxisEvent,
    amounts: (Option<f64>, Option<f64>),
) -> AxisFrame {
    let mut frame = AxisFrame::new(Event::time_msec(event)).source(event.source());
    let is_finger = event.source() == AxisSource::Finger;
    // Finger-lift: no axis carries non-zero data. Covers both Some(0.0)
    // (newer libinput) and None-for-all-axes (older libinput). Read from
    // `amounts` so the frame has one source for its values.
    let is_stop =
        is_finger && !amounts.0.is_some_and(|a| a != 0.0) && !amounts.1.is_some_and(|a| a != 0.0);
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let amount = match axis {
            Axis::Horizontal => amounts.0,
            _ => amounts.1,
        };
        if let Some(amount) = amount {
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
