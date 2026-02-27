use std::cell::RefCell;
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

use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::config::{self, MouseAction};
use crate::grabs::{MoveSurfaceGrab, NavigateGrab, PanGrab, ResizeState, ResizeSurfaceGrab};
use crate::state::{DriftWm, FocusTarget, PendingMiddleClick};

impl DriftWm {
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
            self.last_scroll_pan = None;
            self.momentum.stop();

            // A 3-finger tap (LRM button map) generates BTN_MIDDLE.
            // Buffer it — if a 3-finger swipe follows within 300ms, suppress
            // the click and enter window-move mode. Otherwise flush to client (paste).
            if button == config::BTN_MIDDLE {
                // Cancel any existing pending click first
                if let Some(old) = self.pending_middle_click.take() {
                    self.loop_handle.remove(old.timer_token);
                    self.flush_middle_click(old.press_time, old.release_time);
                }
                let timer = Timer::from_duration(Duration::from_millis(
                    super::gestures::DOUBLE_TAP_WINDOW_MS,
                ));
                if let Ok(token) = self.loop_handle.insert_source(timer, |_, _, data| {
                    data.state.flush_pending_middle_click();
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
            if self.fullscreen.is_some() {
                if self.config.mouse_button_lookup(&mods, button).is_some() {
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

            // Check configured mouse bindings
            if let Some(action) = self.config.mouse_button_lookup(&mods, button).cloned() {
                match action {
                    MouseAction::MoveWindow => {
                        if let Some((window, _)) =
                            self.space.element_under(pos).map(|(w, l)| (w.clone(), l))
                        {
                            let initial_window_location =
                                self.space.element_location(&window).unwrap();
                            let start_data = GrabStartData {
                                focus: None,
                                button,
                                location: pos,
                            };
                            let grab = MoveSurfaceGrab {
                                start_data,
                                window,
                                initial_window_location,
                            };
                            pointer.set_grab(self, grab, serial, Focus::Clear);
                            return;
                        }
                        // No window under cursor — fall through to normal click
                    }
                    MouseAction::ResizeWindow => {
                        if let Some((window, _)) =
                            self.space.element_under(pos).map(|(w, l)| (w.clone(), l))
                        {
                            self.start_compositor_resize(
                                &pointer, &window, pos, button, serial,
                            );
                            return;
                        }
                        // No window under cursor — fall through
                    }
                    MouseAction::PanViewport => {
                        self.panning = true;
                        let grab = self.make_pan_grab(pos, button, false);
                        pointer.set_grab(self, grab, serial, Focus::Clear);
                        return;
                    }
                    MouseAction::Navigate => {
                        let screen_pos = canvas_to_screen(CanvasPos(pos), self.camera, self.zoom).0;
                        let start_data = GrabStartData {
                            focus: None,
                            button,
                            location: pos,
                        };
                        let grab = NavigateGrab::new(start_data, screen_pos);
                        pointer.set_grab(self, grab, serial, Focus::Clear);
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

            if let Some(window) = element_under {
                // Normal click on window: focus + raise + forward
                self.space.raise_element(&window, true);
                keyboard.set_focus(
                    self,
                    Some(FocusTarget(window.toplevel().unwrap().wl_surface().clone())),
                    serial,
                );
            } else if button == config::BTN_LEFT {
                // Left-click on empty canvas → pan
                self.panning = true;
                let grab = self.make_pan_grab(pos, button, true);
                pointer.set_grab(self, grab, serial, Focus::Clear);
                return;
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

    /// Start a compositor-side resize grab. Edges are inferred from which
    /// quadrant of the window the pointer is in.
    fn start_compositor_resize(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<DriftWm>,
        window: &smithay::desktop::Window,
        pos: Point<f64, smithay::utils::Logical>,
        button: u32,
        serial: smithay::utils::Serial,
    ) {
        let initial_window_location = self.space.element_location(window).unwrap();
        let initial_window_size = window.geometry().size;

        let edges = edges_from_position(pos, initial_window_location, initial_window_size);

        // Store resize state for commit() repositioning
        let wl_surface = window.toplevel().unwrap().wl_surface().clone();
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

        window.toplevel().unwrap().with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
        });

        self.grab_cursor = true;
        self.cursor_status = CursorImageStatus::Named(resize_cursor(edges));

        let start_data = GrabStartData {
            focus: None,
            button,
            location: pos,
        };
        let grab = ResizeSurfaceGrab {
            start_data,
            window: window.clone(),
            edges,
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    pub(super) fn on_pointer_axis<I: InputBackend>(&mut self, event: I::PointerAxisEvent) {
        // When pointer is over a layer surface, forward scroll directly (no pan/zoom)
        if self.pointer_over_layer {
            let pointer = self.seat.get_pointer().unwrap();
            let mut frame = AxisFrame::new(Event::time_msec(&event))
                .source(event.source());
            for axis in [Axis::Horizontal, Axis::Vertical] {
                if let Some(amount) = event.amount(axis) {
                    frame = frame
                        .value(axis, amount)
                        .relative_direction(axis, event.relative_direction(axis));
                }
                if let Some(v120) = event.amount_v120(axis) {
                    frame = frame.v120(axis, v120 as i32);
                }
            }
            pointer.axis(self, frame);
            pointer.frame(self);
            return;
        }

        // During fullscreen: bound scroll exits fullscreen and zooms;
        // plain scroll forwards to the app.
        if self.fullscreen.is_some() {
            let keyboard = self.seat.get_keyboard().unwrap();
            let mods = keyboard.modifier_state();
            if matches!(self.config.mouse_scroll_lookup(&mods), Some(MouseAction::Zoom)) {
                let pointer = self.seat.get_pointer().unwrap();
                let pos = pointer.current_location();
                self.exit_fullscreen_remap_pointer(pos);
                // Fall through to zoom logic below
            } else {
                let pointer = self.seat.get_pointer().unwrap();
                let mut frame = AxisFrame::new(Event::time_msec(&event))
                    .source(event.source());
                for axis in [Axis::Horizontal, Axis::Vertical] {
                    if let Some(amount) = event.amount(axis) {
                        frame = frame
                            .value(axis, amount)
                            .relative_direction(axis, event.relative_direction(axis));
                    }
                    if let Some(v120) = event.amount_v120(axis) {
                        frame = frame.v120(axis, v120 as i32);
                    }
                }
                pointer.axis(self, frame);
                pointer.frame(self);
                return;
            }
        }

        let keyboard = self.seat.get_keyboard().unwrap();
        let mods = keyboard.modifier_state();
        let pointer = self.seat.get_pointer().unwrap();
        let pos = pointer.current_location();

        // Configured scroll binding (Mod+scroll):
        //   Mouse wheel (Wheel) → zoom, cursor-anchored
        //   Trackpad (Finger)   → pan anywhere, ignoring windows
        if matches!(self.config.mouse_scroll_lookup(&mods), Some(MouseAction::Zoom)) {
            let is_trackpad = event.source() == AxisSource::Finger;

            if is_trackpad {
                // Mod+trackpad scroll → pan anywhere (same as 3-finger swipe)
                let h = event.amount(Axis::Horizontal).unwrap_or(0.0);
                let v = event.amount(Axis::Vertical).unwrap_or(0.0);
                if h != 0.0 || v != 0.0 {
                    let s = self.config.scroll_speed;
                    let canvas_delta: Point<f64, smithay::utils::Logical> = Point::from((
                        h * s / self.zoom,
                        v * s / self.zoom,
                    ));
                    self.drift_pan(canvas_delta);
                    let new_pos = pos + canvas_delta;
                    let serial = SERIAL_COUNTER.next_serial();
                    let under = self.surface_under(new_pos);
                    pointer.motion(
                        self,
                        under,
                        &MotionEvent {
                            location: new_pos,
                            serial,
                            time: Event::time_msec(&event),
                        },
                    );
                }
                let frame = AxisFrame::new(Event::time_msec(&event));
                pointer.axis(self, frame);
                pointer.frame(self);
                return;
            }

            // Mouse wheel → zoom (vertical axis), cursor-anchored, immediate
            let v = event.amount(Axis::Vertical)
                .or_else(|| event.amount_v120(Axis::Vertical).map(|v| v * 15.0 / 120.0))
                .unwrap_or(0.0);
            if v != 0.0 {
                let steps = -v * self.config.scroll_speed / 30.0;
                let factor = self.config.zoom_step.powf(steps);
                let new_zoom = (self.zoom * factor).clamp(self.min_zoom(), canvas::MAX_ZOOM);

                if new_zoom != self.zoom {
                    self.overview_return = None;
                    let screen_pos = canvas_to_screen(
                        CanvasPos(pos), self.camera, self.zoom,
                    ).0;
                    self.camera = canvas::zoom_anchor_camera(pos, screen_pos, new_zoom);
                    self.zoom = new_zoom;
                    self.zoom_target = None;
                    self.camera_target = None;
                    self.momentum.stop();
                    self.update_output_from_camera();

                    let under = self.surface_under(pos);
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
            let frame = AxisFrame::new(Event::time_msec(&event));
            pointer.axis(self, frame);
            pointer.frame(self);
            return;
        }

        // Empty canvas (or continuing a recent pan gesture):
        //   Trackpad (Finger) → pan viewport
        //   Mouse wheel (Wheel) → zoom, cursor-anchored
        let over_window = self.space.element_under(pos).is_some();
        let recent_pan = self
            .last_scroll_pan
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(150));
        if !over_window || recent_pan {
            let is_trackpad = event.source() == AxisSource::Finger;

            if is_trackpad {
                self.last_scroll_pan = Some(std::time::Instant::now());
                let h = event.amount(Axis::Horizontal).unwrap_or(0.0);
                let v = event.amount(Axis::Vertical).unwrap_or(0.0);
                if h != 0.0 || v != 0.0 {
                    let s = self.config.scroll_speed;
                    let canvas_delta: Point<f64, smithay::utils::Logical> = Point::from((
                        h * s / self.zoom,
                        v * s / self.zoom,
                    ));
                    self.drift_pan(canvas_delta);

                    let new_pos = pos + canvas_delta;
                    let serial = SERIAL_COUNTER.next_serial();
                    let under = self.surface_under(new_pos);
                    pointer.motion(
                        self,
                        under,
                        &MotionEvent {
                            location: new_pos,
                            serial,
                            time: Event::time_msec(&event),
                        },
                    );
                }
            } else {
                // Mouse wheel on empty canvas → zoom
                let v = event.amount(Axis::Vertical)
                    .or_else(|| event.amount_v120(Axis::Vertical).map(|v| v * 15.0 / 120.0))
                    .unwrap_or(0.0);
                if v != 0.0 {
                    let steps = -v * self.config.scroll_speed / 30.0;
                    let factor = self.config.zoom_step.powf(steps);
                    let new_zoom = (self.zoom * factor).clamp(self.min_zoom(), canvas::MAX_ZOOM);

                    if new_zoom != self.zoom {
                        self.overview_return = None;
                        let screen_pos = canvas_to_screen(
                            CanvasPos(pos), self.camera, self.zoom,
                        ).0;
                        self.camera = canvas::zoom_anchor_camera(pos, screen_pos, new_zoom);
                        self.zoom = new_zoom;
                        self.zoom_target = None;
                        self.camera_target = None;
                        self.momentum.stop();
                        self.update_output_from_camera();

                        let under = self.surface_under(pos);
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
            let frame = AxisFrame::new(Event::time_msec(&event));
            pointer.axis(self, frame);
            pointer.frame(self);
            return;
        }

        // Over a window without Mod: forward scroll to the client
        let mut frame = AxisFrame::new(Event::time_msec(&event))
            .source(event.source());

        for axis in [Axis::Horizontal, Axis::Vertical] {
            if let Some(amount) = event.amount(axis) {
                frame = frame
                    .value(axis, amount)
                    .relative_direction(axis, event.relative_direction(axis));
            }
            if let Some(v120) = event.amount_v120(axis) {
                frame = frame.v120(axis, v120 as i32);
            }
        }

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
        let screen_pos = canvas_to_screen(CanvasPos(canvas_pos), self.camera, self.zoom).0;
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
