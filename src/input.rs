use std::cell::RefCell;

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, ButtonState, Event, InputBackend, InputEvent, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
    },
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, Focus, GrabStartData, MotionEvent},
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Point, SERIAL_COUNTER},
    wayland::compositor::with_states,
};

use driftwm::canvas::{CanvasPos, ScreenPos, canvas_to_screen, screen_to_canvas};
use driftwm::config::{Action, Direction};
use crate::grabs::{MoveSurfaceGrab, PanGrab, ResizeState, ResizeSurfaceGrab};
use crate::state::{DriftWm, FocusTarget, log_err};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

impl DriftWm {
    /// Process a single input event from any backend (winit, libinput, etc).
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => self.on_keyboard::<I>(event),
            InputEvent::PointerMotionAbsolute { event } => {
                self.on_pointer_motion_absolute::<I>(event)
            }
            InputEvent::PointerButton { event } => self.on_pointer_button::<I>(event),
            InputEvent::PointerAxis { event } => self.on_pointer_axis::<I>(event),
            _ => {}
        }
    }

    fn on_keyboard<I: InputBackend>(&mut self, event: I::KeyboardKeyEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let key_state = event.state();
        let keycode = event.key_code();

        let keyboard = self.seat.get_keyboard().unwrap();

        let action = keyboard.input(
            self,
            keycode,
            key_state,
            serial,
            time,
            |state, modifiers, handle| {
                if key_state == KeyState::Pressed {
                    let sym = handle.modified_sym();
                    if let Some(action) = state.config.lookup(modifiers, sym) {
                        return FilterResult::Intercept(action.clone());
                    }
                }
                FilterResult::Forward
            },
        );

        if let Some(action) = action {
            self.execute_action(&action);
        }
    }

    fn execute_action(&mut self, action: &Action) {
        self.momentum.stop();
        match action {
            Action::SpawnCommand(cmd) => {
                tracing::info!("Spawning: {cmd}");
                let mut parts = cmd.split_whitespace();
                if let Some(program) = parts.next() {
                    log_err(
                        "spawn command",
                        std::process::Command::new(program).args(parts).spawn(),
                    );
                }
            }
            Action::CloseWindow => {
                let keyboard = self.seat.get_keyboard().unwrap();
                if let Some(focus) = keyboard.current_focus() {
                    let window = self
                        .space
                        .elements()
                        .find(|w| w.toplevel().unwrap().wl_surface() == &focus.0)
                        .cloned();
                    if let Some(window) = window {
                        window.toplevel().unwrap().send_close();
                    }
                }
            }
            Action::NudgeWindow(dir) => {
                let keyboard = self.seat.get_keyboard().unwrap();
                if let Some(focus) = keyboard.current_focus() {
                    let window = self
                        .space
                        .elements()
                        .find(|w| w.toplevel().unwrap().wl_surface() == &focus.0)
                        .cloned();
                    if let Some(window) = window
                        && let Some(loc) = self.space.element_location(&window)
                    {
                        let step = 20;
                        let offset = match dir {
                            Direction::Up => (0, -step),
                            Direction::Down => (0, step),
                            Direction::Left => (-step, 0),
                            Direction::Right => (step, 0),
                        };
                        let new_loc = loc + Point::from(offset);
                        self.space.map_element(window, new_loc, false);
                    }
                }
            }
            Action::PanViewport(dir) => {
                let step = 100.0;
                let delta = match dir {
                    Direction::Up => (0.0, -step),
                    Direction::Down => (0.0, step),
                    Direction::Left => (-step, 0.0),
                    Direction::Right => (step, 0.0),
                };
                self.camera += Point::from(delta);
                self.update_output_from_camera();
            }
        }
    }

    fn on_pointer_motion_absolute<I: InputBackend>(
        &mut self,
        event: I::PointerMotionAbsoluteEvent,
    ) {
        let output = match self.space.outputs().next() {
            Some(o) => o.clone(),
            None => return,
        };
        let output_geo = self.space.output_geometry(&output).unwrap();

        // position_transformed gives screen-local coords (0..width, 0..height)
        let screen_pos = event.position_transformed(output_geo.size);
        // Convert to canvas coords by adding camera offset
        let canvas_pos = screen_to_canvas(ScreenPos(screen_pos), self.camera).0;

        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer().unwrap();
        let under = self.surface_under(canvas_pos);

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: canvas_pos,
                serial,
                time: Event::time_msec(&event),
            },
        );
        pointer.frame(self);
    }

    /// Priority order when button pressed:
    /// 1. Mod+Shift + button on window → move (left) or resize (right)
    /// 2. Mod + left-drag → pan canvas (regardless of what's under cursor)
    /// 3. Left-click on empty canvas → pan canvas
    /// 4. Normal click → click-to-focus + forward to client
    fn on_pointer_button<I: InputBackend>(&mut self, event: I::PointerButtonEvent) {
        self.last_scroll_pan = None;
        self.momentum.stop();
        let serial = SERIAL_COUNTER.next_serial();
        let button = event.button_code();
        let button_state = event.state();
        let pointer = self.seat.get_pointer().unwrap();

        if button_state == ButtonState::Pressed {
            let pos = pointer.current_location();
            let keyboard = self.seat.get_keyboard().unwrap();
            let mods = keyboard.modifier_state();
            let wm_mod = self.config.mod_key.is_pressed(&mods);

            // 1. Mod+Shift + button on window → move (left) or resize (right)
            if wm_mod && mods.shift {
                let element_under = self
                    .space
                    .element_under(pos)
                    .map(|(w, _)| w.clone());

                if let Some(window) = element_under {
                    if button == BTN_LEFT {
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

                    if button == BTN_RIGHT {
                        self.start_compositor_resize(&pointer, &window, pos, button, serial);
                        return;
                    }
                }
            }

            // 2. Mod + left-drag → pan canvas (anywhere)
            if wm_mod && button == BTN_LEFT {
                let screen_pos = canvas_to_screen(CanvasPos(pos), self.camera).0;
                let start_data = GrabStartData {
                    focus: None,
                    button,
                    location: pos,
                };
                let grab = PanGrab {
                    start_data,
                    initial_camera: self.camera,
                    start_screen_pos: screen_pos,
                };
                pointer.set_grab(self, grab, serial, Focus::Clear);
                return;
            }

            // 3 & 4. Check what's under the pointer
            let element_under = self
                .space
                .element_under(pos)
                .map(|(w, _)| w.clone());

            if let Some(window) = element_under {
                // 4. Normal click on window: focus + raise + forward
                self.space.raise_element(&window, true);
                keyboard.set_focus(
                    self,
                    Some(FocusTarget(window.toplevel().unwrap().wl_surface().clone())),
                    serial,
                );
            } else {
                // 3. Left-click on empty canvas → pan
                keyboard.set_focus(self, None::<FocusTarget>, serial);

                if button == BTN_LEFT {
                        let screen_pos = canvas_to_screen(CanvasPos(pos), self.camera).0;
                    let start_data = GrabStartData {
                        focus: None,
                        button,
                        location: pos,
                    };
                    let grab = PanGrab {
                        start_data,
                        initial_camera: self.camera,
                        start_screen_pos: screen_pos,
                    };
                    pointer.set_grab(self, grab, serial, Focus::Clear);
                    return;
                }
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

        // Determine edges from pointer position within a 3×3 grid on the window.
        // Corners → diagonal resize, edge strips → cardinal resize.
        let rel_x = pos.x - initial_window_location.x as f64;
        let rel_y = pos.y - initial_window_location.y as f64;
        let w = initial_window_size.w as f64;
        let h = initial_window_size.h as f64;
        let in_left = rel_x < w / 3.0;
        let in_right = rel_x > w * 2.0 / 3.0;
        let in_top = rel_y < h / 3.0;
        let in_bottom = rel_y > h * 2.0 / 3.0;
        let edges = match (in_left, in_right, in_top, in_bottom) {
            (true, _, true, _) => xdg_toplevel::ResizeEdge::TopLeft,
            (_, true, true, _) => xdg_toplevel::ResizeEdge::TopRight,
            (true, _, _, true) => xdg_toplevel::ResizeEdge::BottomLeft,
            (_, true, _, true) => xdg_toplevel::ResizeEdge::BottomRight,
            (true, _, _, _) => xdg_toplevel::ResizeEdge::Left,
            (_, true, _, _) => xdg_toplevel::ResizeEdge::Right,
            (_, _, true, _) => xdg_toplevel::ResizeEdge::Top,
            (_, _, _, true) => xdg_toplevel::ResizeEdge::Bottom,
            _ => xdg_toplevel::ResizeEdge::BottomRight, // center fallback
        };

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

    fn on_pointer_axis<I: InputBackend>(&mut self, event: I::PointerAxisEvent) {
        let keyboard = self.seat.get_keyboard().unwrap();
        let mods = keyboard.modifier_state();
        let wm_mod = self.config.mod_key.is_pressed(&mods);
        let pointer = self.seat.get_pointer().unwrap();

        // Pan viewport when: Mod+scroll, scroll on empty canvas, or
        // continuing a recent scroll-pan (within 150ms, so a window
        // sliding under mid-gesture doesn't steal the scroll).
        let pos = pointer.current_location();
        let over_window = self.space.element_under(pos).is_some();
        let recent_pan = self
            .last_scroll_pan
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(150));
        if wm_mod || !over_window || recent_pan {
            self.last_scroll_pan = Some(std::time::Instant::now());
            let h = event.amount(Axis::Horizontal).unwrap_or(0.0);
            let v = event.amount(Axis::Vertical).unwrap_or(0.0);
            if h != 0.0 || v != 0.0 {
                let s = self.config.scroll_speed;
                let delta = Point::from((h * s, v * s));
                self.momentum.accumulate(delta, self.frame_counter);
                self.camera += delta;
                self.update_output_from_camera();

                // Move pointer by same delta so cursor stays at the same
                // screen position (screen_pos = canvas_pos - camera)
                let new_pos = pos + delta;
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

    /// Find the Wayland surface and local coordinates under the given canvas position.
    /// This is the foundation for all hit-testing — focus, gestures, resize grabs.
    pub fn surface_under(
        &self,
        pos: Point<f64, smithay::utils::Logical>,
    ) -> Option<(FocusTarget, Point<f64, smithay::utils::Logical>)> {
        self.space
            .element_under(pos)
            .and_then(|(window, window_loc)| {
                window
                    .surface_under(
                        pos - window_loc.to_f64(),
                        smithay::desktop::WindowSurfaceType::ALL,
                    )
                    .map(|(surface, surface_loc)| {
                        (FocusTarget(surface), (surface_loc + window_loc).to_f64())
                    })
            })
    }
}

/// Map resize edge to the appropriate directional cursor icon.
fn resize_cursor(edges: xdg_toplevel::ResizeEdge) -> CursorIcon {
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
