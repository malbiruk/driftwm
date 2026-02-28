use std::cell::RefCell;

use crate::grabs::{MoveSurfaceGrab, ResizeState, ResizeSurfaceGrab};
use crate::state::{DriftWm, FocusTarget};
use smithay::{
    delegate_xdg_shell,
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Window,
        find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
    },
    input::pointer::{CursorIcon, CursorImageStatus, Focus, GrabStartData},
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{Resource, protocol::{wl_output, wl_seat}},
    },
    utils::{Rectangle, Serial},
    wayland::{
        compositor::with_states,
        seat::WaylandFocus,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
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
            .space
            .outputs()
            .next()
            .and_then(|o| self.space.output_geometry(o))
            .map(|geo| {
                ((self.camera.x + geo.size.w as f64 / (2.0 * self.zoom)) as i32,
                 (self.camera.y + geo.size.h as f64 / (2.0 * self.zoom)) as i32)
            })
            .unwrap_or((0, 0));

        // Send initial configure — the client won't render until it gets this
        window.toplevel().unwrap().send_configure();

        self.space.map_element(window.clone(), pos, true);
        self.space.raise_element(&window, true);
        self.enforce_below_windows();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, Some(FocusTarget(wl_surface.clone())), serial);
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

        let root_focus = FocusTarget(root);
        let Ok(mut grab) =
            self.popups
                .grab_popup(root_focus, kind, &self.seat, serial)
        else {
            return;
        };

        let keyboard = self.seat.get_keyboard().unwrap();
        let pointer = self.seat.get_pointer().unwrap();

        if keyboard.is_grabbed()
            && !(keyboard.has_grab(serial)
                || grab
                    .previous_serial()
                    .is_none_or(|s| keyboard.has_grab(s)))
        {
            grab.ungrab(PopupUngrabStrategy::All);
            return;
        }

        keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
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
        surface.send_configure().ok();
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<wl_output::WlOutput>,
    ) {
        let wl_surface = surface.wl_surface().clone();
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel().unwrap().wl_surface() == &wl_surface)
            .cloned();
        if let Some(window) = window {
            self.enter_fullscreen(&window);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let is_fullscreen = self.fullscreen.as_ref().is_some_and(|fs| {
            fs.window.toplevel().unwrap().wl_surface() == surface.wl_surface()
        });
        if is_fullscreen {
            self.exit_fullscreen();
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        self.pending_center.remove(&wl_surface);
        // Collect first to avoid holding an immutable borrow on space
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel().unwrap().wl_surface() == &wl_surface)
            .cloned();
        if let Some(ref window) = window {
            // If the destroyed window was fullscreen, restore viewport
            if self.fullscreen.as_ref().is_some_and(|fs| &fs.window == window) {
                let fs = self.fullscreen.take().unwrap();
                self.camera = fs.saved_camera;
                self.zoom = fs.saved_zoom;
                self.update_output_from_camera();
            }
            // Remove from focus history before unmapping
            self.focus_history.retain(|w| w != window);
            // Clamp or clear cycle index if cycling is active
            if self.cycle_state.is_some() {
                if self.focus_history.is_empty() {
                    self.cycle_state = None;
                } else if let Some(ref mut idx) = self.cycle_state {
                    *idx = (*idx).min(self.focus_history.len() - 1);
                }
            }
            self.space.unmap_elem(window);
        }
    }

    fn move_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        serial: Serial,
    ) {
        let wl_surface = surface.wl_surface().clone();
        if driftwm::config::applied_rule(&wl_surface).is_some_and(|r| r.widget) {
            return;
        }
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().unwrap().wl_surface() == &wl_surface)
            .cloned()
        else {
            return;
        };

        let pointer = self.seat.get_pointer().unwrap();
        let Some(start_data) = check_grab(&pointer, &wl_surface) else {
            return;
        };

        let initial_window_location = self.space.element_location(&window).unwrap();
        let grab = MoveSurfaceGrab {
            start_data,
            window,
            initial_window_location,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
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
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().unwrap().wl_surface() == &wl_surface)
            .cloned()
        else {
            return;
        };

        let pointer = self.seat.get_pointer().unwrap();
        let Some(start_data) = check_grab(&pointer, &wl_surface) else {
            return;
        };

        let initial_window_location = self.space.element_location(&window).unwrap();
        let initial_window_size = window.geometry().size;

        // Store resize state in the surface data map for commit() repositioning
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

        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
        });

        self.grab_cursor = true;
        self.cursor_status = CursorImageStatus::Named(resize_cursor(edges));

        let grab = ResizeSurfaceGrab {
            start_data,
            window,
            edges,
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
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

impl DriftWm {
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
        let target = if let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().unwrap().wl_surface() == &root)
        {
            // Parent is an xdg window — target is the output rect in window-relative coords
            let window_loc = self.space.element_location(window).unwrap_or_default();
            let output_geo = self
                .space
                .outputs()
                .next()
                .and_then(|o| self.space.output_geometry(o))
                .unwrap_or_default();

            let mut target = output_geo;
            // Translate output rect into window-relative coordinates
            target.loc -= window_loc;
            // Account for nested popups
            target.loc -= get_popup_toplevel_coords(popup);
            target
        } else {
            // Parent is a layer surface — find it in the layer map
            let output = self.space.outputs().next().cloned();
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
