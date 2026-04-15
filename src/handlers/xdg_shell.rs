use std::cell::RefCell;
use std::collections::HashSet;

use crate::grabs::{MoveSurfaceGrab, ResizeState, ResizeSurfaceGrab};
use crate::state::{DriftWm, FocusTarget, output_state};
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
            .active_output()
            .and_then(|o| self.space.output_geometry(&o))
            .map(|geo| {
                { let cam = self.camera(); let z = self.zoom();
                ((cam.x + geo.size.w as f64 / (2.0 * z)) as i32,
                 (cam.y + geo.size.h as f64 / (2.0 * z)) as i32) }
            })
            .unwrap_or((0, 0));

        // Send initial configure — the client won't render until it gets this.
        // Tiled states are deferred to the first-commit handler (compositor.rs),
        // where rules/app_id are known. Setting Tiled here with no size makes
        // SCTK-based clients (alacritty) read "tiled at 0x0" and commit a
        // 0-width wl_shm buffer, which is a protocol error.
        if let Some(toplevel) = window.toplevel() {
            toplevel.send_configure();
        }

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
            .find(|w| w.wl_surface().as_deref() == Some(&wl_surface))
            .cloned();
        if let Some(window) = window {
            self.enter_fullscreen(&window);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(output) = self.find_fullscreen_output_for_surface(surface.wl_surface()) {
            self.exit_fullscreen_on(&output);
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        let window = self
            .space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(&wl_surface))
            .cloned();
        if let Some(window) = window {
            self.toggle_fit_window(&window);
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        let window = self
            .space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(&wl_surface))
            .cloned();
        if let Some(window) = window {
            self.unfit_window(&window);
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        self.decorations.remove(&wl_surface.id());
        self.render.csd_shadows.remove(&wl_surface.id());
        self.pending_ssd.remove(&wl_surface.id());
        self.pending_center.remove(&wl_surface);
        self.pending_size.remove(&wl_surface);
        self.pending_recenter.remove(&wl_surface.id());
        // Collect first to avoid holding an immutable borrow on space
        let window = self
            .space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(&wl_surface))
            .cloned();
        if let Some(ref window) = window {
            // If this window had a parent, focus the parent (or its modal child)
            let parent_focus = window.parent_surface().and_then(|ps| {
                let parent = self.window_for_surface(&ps)?;
                // If parent has another modal child, focus that instead
                let target = self.topmost_modal_child(&parent)
                    .filter(|mc| mc != window)
                    .unwrap_or(parent);
                target.wl_surface().map(|s| FocusTarget(s.into_owned()))
            });

            let keyboard = self.seat.get_keyboard().unwrap();
            if keyboard
                .current_focus()
                .is_some_and(|f| f.0 == wl_surface)
            {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                // Standalone toplevels have no parent — fall back to the most
                // recent previous window so focus does not vanish (important
                // with focus_follows_mouse when the pointer is over empty canvas).
                let fallback = parent_focus.or_else(|| {
                    self.focus_history
                        .iter()
                        .find(|w| w != &window)
                        .and_then(|w| w.wl_surface().map(|s| FocusTarget(s.into_owned())))
                });
                keyboard.set_focus(self, fallback, serial);
            }
            // If the destroyed window was fullscreen, restore viewport
            let fs_output = self.fullscreen.iter()
                .find(|(_, fs)| &fs.window == window)
                .map(|(o, _)| o.clone());
            if let Some(output) = fs_output {
                let fs = self.fullscreen.remove(&output).unwrap();
                // Set camera/zoom on the specific output
                output_state(&output).camera = fs.saved_camera;
                output_state(&output).zoom = fs.saved_zoom;
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
            .find(|w| w.wl_surface().as_deref() == Some(&wl_surface))
            .cloned()
        else {
            return;
        };

        let pointer = self.seat.get_pointer().unwrap();
        let Some(start_data) = check_grab(&pointer, &wl_surface) else {
            return;
        };

        // Client-initiated xdg move_request: the client asked to move itself,
        // not its cluster neighbors. Clients don't know about clusters, so
        // always single-window.
        let initial_window_location = self.space.element_location(&window).unwrap();
        let grab = MoveSurfaceGrab::new(
            start_data,
            window,
            initial_window_location,
            self.active_output().unwrap(),
            Vec::new(),
            HashSet::new(),
        );
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
            .find(|w| w.wl_surface().as_deref() == Some(&wl_surface))
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

        // Clear fit state — user took manual control
        crate::state::fit::clear_fit_state(&wl_surface);

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

        self.cursor.grab_cursor = true;
        self.cursor.cursor_status = CursorImageStatus::Named(resize_cursor(edges));

        let output = self.active_output().unwrap();
        let last_clamped_location = start_data.location;
        let grab = ResizeSurfaceGrab {
            start_data,
            window,
            edges,
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
            output,
            last_clamped_location,
            last_x11_configure: None,
            snap: driftwm::snap::SnapState::default(),
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
        let active_output = self.active_output();
        let target = if let Some(window) = self
            .space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(&root))
        {
            // Parent is an xdg window — target is the output rect in window-relative coords
            let window_loc = self.space.element_location(window).unwrap_or_default();
            let output_geo = active_output.as_ref()
                .and_then(|o| self.space.output_geometry(o))
                .unwrap_or_default();

            let mut target = output_geo;
            // Translate output rect into window-relative coordinates
            target.loc -= window_loc;
            // Account for nested popups
            target.loc -= get_popup_toplevel_coords(popup);
            target
        } else if let Some(cl) = self
            .canvas_layers
            .iter()
            .find(|cl| cl.surface.wl_surface() == &root)
            && let Some(pos) = cl.position
        {
            // Parent is a canvas-positioned layer surface
            let output_geo = active_output.as_ref()
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
