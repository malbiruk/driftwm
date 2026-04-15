use std::cell::RefCell;
use std::collections::HashSet;
use std::time::Duration;

use smithay::{
    backend::input::{
        Axis, AxisSource, ButtonState, Event, InputBackend, PointerAxisEvent, PointerButtonEvent,
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

use smithay::reexports::wayland_server::Resource;
use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::config::{self, Action, BindingContext, MouseAction};
use driftwm::window_ext::WindowExt;
use crate::decorations::DecorationHit;
use crate::grabs::{MoveSurfaceGrab, NavigateGrab, PanGrab, ResizeState, ResizeSurfaceGrab};
use crate::state::{DriftWm, FocusTarget, PendingMiddleClick};

impl DriftWm {
    /// Determine the binding context for the current pointer position.
    pub(super) fn pointer_context(&self, pos: Point<f64, smithay::utils::Logical>) -> BindingContext {
        let over_window = self.space.element_under(pos).is_some();
        if over_window || self.canvas_layer_under(pos).is_some() {
            BindingContext::OnWindow
        } else {
            BindingContext::OnCanvas
        }
    }

    /// Priority order when button pressed:
    /// 1. Configured mouse bindings (move, resize, pan, etc.)
    /// 2. Normal click on window → focus + raise + forward to client
    /// 3. Left-click on empty canvas → pan canvas
    pub(super) fn on_pointer_button<I: InputBackend>(&mut self, event: I::PointerButtonEvent) {
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

            // X11 popup outside-click dismissal: when a popup-menu OR
            // (jgmenu, dmenu, xmenu) is alive and the click lands outside
            // any OR, synthesize an Esc to the topmost OR's wl_surface.
            // jgmenu's X11 keyboard grab catches the Esc and dismisses the
            // menu naturally. Routing pointer events directly to the OR
            // with out-of-bounds coords doesn't work — XWayland drops
            // motion whose focus surface isn't really under the cursor.
            // Skip Tooltip/Notification/Splash window types: those don't
            // dismiss on Esc and we'd just steal focus pointlessly.
            let pos = pointer.current_location();
            if self.override_redirect_under(pos).is_none()
                && let Some(or) = self
                    .x11_override_redirect
                    .iter()
                    .rev()
                    .find(|x| {
                        use smithay::xwayland::xwm::WmWindowType;
                        !matches!(
                            x.window_type(),
                            Some(WmWindowType::Tooltip
                                | WmWindowType::Notification
                                | WmWindowType::Splash)
                        )
                    })
                && let Some(or_wl) = or.wl_surface()
            {
                self.synth_esc_to_or(or_wl);
            }

            // A 3-finger tap (LRM button map) generates BTN_MIDDLE.
            // Buffer it — if a 3-finger swipe follows within 300ms, suppress
            // the click and enter window-move mode. Otherwise flush to client (paste).
            // Skip buffering when a modifier binding matches (e.g. alt+middle → fullscreen).
            if button == config::BTN_MIDDLE
                && {
                    let kb = self.seat.get_keyboard().unwrap();
                    let ctx = self.pointer_context(pointer.current_location());
                    self.config.mouse_button_lookup_ctx(&kb.modifier_state(), button, ctx).is_none()
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
                if let Ok(token) = self.loop_handle.insert_source(timer, |_, _, data: &mut DriftWm| {
                    data.flush_pending_middle_click();
                    TimeoutAction::Drop
                }) {
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
                let fs_lookup = self.config.mouse_button_lookup_ctx(&mods, button, BindingContext::OnWindow);
                if matches!(fs_lookup, Some(MouseAction::Action(Action::ToggleFullscreen | Action::FitWindow))) {
                    self.exit_fullscreen_remap_pointer(pos);
                    return;
                } else if fs_lookup.is_some() {
                    pos = self.exit_fullscreen_remap_pointer(pos);
                } else {
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

            // Layer surfaces: just forward (no compositor grabs)
            if self.pointer_over_layer {
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

            // SSD decoration clicks: title bar → move, close button → close, resize border → resize
            if let Some((window, hit)) = self.decoration_under(pos) {
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
                let Some(wl_surface) = window.wl_surface().map(|s| s.into_owned()) else { return; };
                let is_widget = config::applied_rule(&wl_surface)
                    .is_some_and(|r| r.widget);

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
                                self.toggle_fit_window(&window);
                                return;
                            }
                            self.last_titlebar_click = Some((now, surface_id));

                            // Focus + raise (with modal redirect) + start move grab.
                            // Alt+drag on the titlebar moves a single window;
                            // cluster drag is a separate explicit action
                            // (`MoveSnappedWindows`, default Alt+Shift+Left).
                            self.raise_and_focus(&window, serial);
                            let initial_window_location =
                                self.space.element_location(&window).unwrap();
                            let start_data = GrabStartData {
                                focus: None,
                                button,
                                location: pos,
                            };
                            let grab = MoveSurfaceGrab::new(
                                start_data,
                                window,
                                initial_window_location,
                                self.active_output().unwrap(),
                                Vec::new(),
                                HashSet::new(),
                            );
                            pointer.set_grab(self, grab, serial, Focus::Clear);
                            return;
                        }
                        DecorationHit::ResizeBorder(edge) if !is_widget => {
                            self.raise_and_focus(&window, serial);
                            self.start_compositor_resize_with_edge(
                                &pointer, &window, pos, button, serial, Some(edge),
                            );
                            return;
                        }
                        _ => {
                            // Widget title bar or other — just focus
                            keyboard.set_focus(
                                self,
                                Some(FocusTarget(wl_surface)),
                                serial,
                            );
                        }
                    }
                }
                }
            }

            // Check configured mouse bindings (context-aware)
            let context = self.pointer_context(pos);
            if let Some(action) = self.config.mouse_button_lookup_ctx(&mods, button, context).cloned() {
                match action {
                    MouseAction::MoveWindow | MouseAction::MoveSnappedWindows => {
                        let want_cluster =
                            matches!(action, MouseAction::MoveSnappedWindows);
                        if let Some((window, _)) =
                            self.space.element_under(pos).map(|(w, l)| (w.clone(), l))
                            && let Some(surface) = window.wl_surface()
                            && !config::applied_rule(&surface).is_some_and(|r| r.widget)
                        {
                            self.raise_and_focus(&window, serial);

                            let initial_window_location =
                                self.space.element_location(&window).unwrap();
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
                                self.active_output().unwrap(),
                                cluster_members,
                                cluster_member_surfaces,
                            );
                            pointer.set_grab(self, grab, serial, Focus::Clear);
                            return;
                        }
                        // No window or pinned — fall through to normal click
                    }
                    MouseAction::ResizeWindow => {
                        if let Some((window, _)) =
                            self.space.element_under(pos).map(|(w, l)| (w.clone(), l))
                            && !window.wl_surface()
                                .and_then(|s| config::applied_rule(&s))
                                .is_some_and(|r| r.widget)
                        {
                            self.raise_and_focus(&window, serial);

                            self.start_compositor_resize(
                                &pointer, &window, pos, button, serial,
                            );
                            return;
                        }
                        // No window or pinned — fall through
                    }
                    MouseAction::PanViewport => {
                        self.set_panning(true);
                        let from_empty = context == BindingContext::OnCanvas;
                        let grab = self.make_pan_grab(pos, button, from_empty);
                        pointer.set_grab(self, grab, serial, Focus::Clear);
                        return;
                    }
                    MouseAction::CenterNearest => {
                        let screen_pos = canvas_to_screen(CanvasPos(pos), self.camera(), self.zoom()).0;
                        let start_data = GrabStartData {
                            focus: None,
                            button,
                            location: pos,
                        };
                        let grab = NavigateGrab::new(start_data, screen_pos, self.active_output().unwrap());
                        pointer.set_grab(self, grab, serial, Focus::Clear);
                        return;
                    }
                    MouseAction::Action(ref action) => {
                        if let Some((window, _)) =
                            self.space.element_under(pos).map(|(w, l)| (w.clone(), l))
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
            let element_under = self
                .space
                .element_under(pos)
                .map(|(w, _)| w.clone());

            if let Some(ref window) = element_under {
                let is_widget = window.wl_surface()
                    .and_then(|s| config::applied_rule(&s))
                    .is_some_and(|r| r.widget);
                if !is_widget {
                    // Normal window: raise + focus (with modal redirect)
                    self.raise_and_focus(window, serial);
                } else if let Some((focus, _)) = self.canvas_layer_under(pos) {
                    // Widget window but canvas layer is above it: focus the layer
                    keyboard.set_focus(self, Some(focus), serial);
                } else {
                    // Widget window with no canvas layer above: focus the widget
                    keyboard.set_focus(
                        self,
                        window.wl_surface().map(|s| FocusTarget(s.into_owned())),
                        serial,
                    );
                }
            } else if let Some((focus, _)) = self.canvas_layer_under(pos) {
                keyboard.set_focus(self, Some(focus), serial);
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

    /// Start a compositor-side resize grab. If `explicit_edge` is provided, use it;
    /// otherwise infer edges from pointer position within the window.
    pub(super) fn start_compositor_resize(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        window: &smithay::desktop::Window,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: smithay::utils::Serial,
    ) {
        self.start_compositor_resize_with_edge(pointer, window, pos, button, serial, None);
    }

    pub(super) fn start_compositor_resize_with_edge(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        window: &smithay::desktop::Window,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: smithay::utils::Serial,
        explicit_edge: Option<xdg_toplevel::ResizeEdge>,
    ) {
        let initial_window_location = self.space.element_location(window).unwrap();
        let initial_window_size = window.geometry().size;

        let edges = explicit_edge
            .unwrap_or_else(|| edges_from_position(pos, initial_window_location, initial_window_size));

        // Store resize state for commit() repositioning
        let Some(wl_surface) = window.wl_surface().map(|s| s.into_owned()) else { return; };

        // Clear fit state — user took manual control
        crate::state::fit::clear_fit_state(&wl_surface);

        with_states(&wl_surface, |states| {
            states
                .data_map
                .get_or_insert(|| RefCell::new(ResizeState::Idle))
                .replace(ResizeState::Resizing {
                    edges,
                    initial_window_location,
                    initial_window_size,
                });
        });

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
            });
        }

        self.cursor.grab_cursor = true;
        self.cursor.cursor_status = CursorImageStatus::Named(resize_cursor(edges));

        let start_data = GrabStartData {
            focus: None,
            button,
            location: pos,
        };
        let output = self.active_output().unwrap();
        let grab = ResizeSurfaceGrab {
            start_data,
            window: window.clone(),
            edges,
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
            output,
            last_clamped_location: pos,
            last_x11_configure: None,
            snap: driftwm::snap::SnapState::default(),
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    pub(super) fn on_pointer_axis<I: InputBackend>(&mut self, event: I::PointerAxisEvent) {
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

        // During fullscreen: bound scroll exits fullscreen first; plain scroll forwards.
        if self.is_fullscreen() {
            if self.config.mouse_scroll_lookup_ctx(&mods, source, BindingContext::OnWindow).is_some() {
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
        let recent_pan = self
            .last_scroll_pan()
            .is_some_and(|t: std::time::Instant| t.elapsed() < std::time::Duration::from_millis(150));
        let context = if recent_pan {
            BindingContext::OnCanvas
        } else {
            self.pointer_context(pos)
        };

        // Single lookup: context-aware
        if let Some(action) = self.config.mouse_scroll_lookup_ctx(&mods, source, context).cloned() {
            match action {
                MouseAction::PanViewport => {
                    let h = event.amount(Axis::Horizontal).unwrap_or(0.0);
                    let v = event.amount(Axis::Vertical).unwrap_or(0.0);
                    if h != 0.0 || v != 0.0 {
                        if source == AxisSource::Finger {
                            self.set_last_scroll_pan(Some(std::time::Instant::now()));
                        }
                        let s = self.config.trackpad_speed;
                        let canvas_delta: Point<f64, smithay::utils::Logical> = Point::from((
                            h * s / self.zoom(),
                            v * s / self.zoom(),
                        ));
                        self.drift_pan(canvas_delta);
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
                    let v = event.amount(Axis::Vertical)
                        .or_else(|| event.amount_v120(Axis::Vertical).map(|v| v * 15.0 / 120.0))
                        .unwrap_or(0.0);
                    if v != 0.0 {
                        let steps = -v / 30.0;
                        let factor = self.config.zoom_step.powf(steps);
                        let cur_zoom = self.zoom();
                        let new_zoom = (cur_zoom * factor).clamp(self.min_zoom(), canvas::MAX_ZOOM);

                        if new_zoom != cur_zoom {
                            let screen_pos = canvas_to_screen(
                                CanvasPos(pos), self.camera(), cur_zoom,
                            ).0;
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
    ) -> PanGrab {
        let screen_pos = canvas_to_screen(CanvasPos(canvas_pos), self.camera(), self.zoom()).0;
        PanGrab {
            start_data: GrabStartData {
                focus: None,
                button,
                location: canvas_pos,
            },
            last_screen_pos: screen_pos,
            start_screen_pos: screen_pos,
            from_empty_canvas,
            dragged: false,
            output: self.active_output().unwrap(),
        }
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
