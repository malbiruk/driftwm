use crate::state::{DriftWm, FocusTarget};
use driftwm::window_ext::WindowExt;
use smithay::{
    delegate_xwayland_shell,
    desktop::Window,
    input::pointer::{CursorImageStatus, Focus, GrabStartData},
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{Logical, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::with_states,
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata,
                request_primary_client_selection, set_primary_selection,
            },
        },
        seat::WaylandFocus,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        xwm::{Reorder, ResizeEdge, WmWindowType, X11Wm, XwmHandler, XwmId},
        X11Surface,
    },
};

use super::xdg_shell::resize_cursor;

fn x11_edge_to_xdg(edge: ResizeEdge) -> xdg_toplevel::ResizeEdge {
    match edge {
        ResizeEdge::Top => xdg_toplevel::ResizeEdge::Top,
        ResizeEdge::Bottom => xdg_toplevel::ResizeEdge::Bottom,
        ResizeEdge::Left => xdg_toplevel::ResizeEdge::Left,
        ResizeEdge::Right => xdg_toplevel::ResizeEdge::Right,
        ResizeEdge::TopLeft => xdg_toplevel::ResizeEdge::TopLeft,
        ResizeEdge::TopRight => xdg_toplevel::ResizeEdge::TopRight,
        ResizeEdge::BottomLeft => xdg_toplevel::ResizeEdge::BottomLeft,
        ResizeEdge::BottomRight => xdg_toplevel::ResizeEdge::BottomRight,
    }
}


impl XwmHandler for DriftWm {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.x11_wm.as_mut().expect("X11Wm not started")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::info!("X11 map request: {:?}", window.class());
        if let Err(err) = window.set_mapped(true) {
            tracing::warn!("Failed to set X11 window mapped: {err}");
            return;
        }

        let smithay_window = Window::new_x11_window(window.clone());

        // X11 size is known upfront — center accounting for window size.
        // Check window rules for explicit positioning.
        let class = window.class();
        let title = window.title();
        let rule = self.config.match_window_rule(&class, &title).cloned();

        // Force size if window rule specifies it (X11 configure is synchronous)
        if let Some(ref rule) = rule
            && let Some((w, h)) = rule.size
        {
            let mut rect = window.geometry();
            rect.size = smithay::utils::Size::from((w, h));
            window.configure(rect).ok();
        }
        let geo = window.geometry();

        let pos = if let Some(ref rule) = rule
            && let Some((x, y)) = rule.position
        {
            // Rule coords: window-center, Y-up. Convert to canvas: top-left, Y-down.
            (x - geo.size.w / 2, -y - geo.size.h / 2)
        } else {
            // Account for SSD title bar so the visual center matches viewport center.
            // navigate_to_window uses camera_to_center_window which offsets by bar/2;
            // the spawn position must be the exact inverse so cascade detects collisions.
            let effective = driftwm::config::effective_decoration_mode(
                rule.as_ref().and_then(|r| r.decoration.as_ref()),
                &self.config.decorations.default_mode,
            );
            let will_have_ssd = match effective {
                driftwm::config::DecorationMode::Server => true,
                driftwm::config::DecorationMode::Borderless | driftwm::config::DecorationMode::None => false,
                // Client mode: defer to the X11 MOTIF hint
                driftwm::config::DecorationMode::Client => smithay_window.wants_ssd(),
            };
            let bar = if will_have_ssd { driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT as f64 } else { 0.0 };
            let vc = self.usable_center_screen();
            let centered = self.active_output()
                .and_then(|o| self.space.output_geometry(&o))
                .map(|_| {
                    let cam = self.camera();
                    let z = self.zoom();
                    (
                        (cam.x + vc.x / z).round() as i32 - geo.size.w / 2,
                        (cam.y + bar / 2.0 + vc.y / z).round() as i32 - geo.size.h / 2,
                    )
                })
                .unwrap_or((0, 0));
            self.cascade_position(centered, &smithay_window)
        };

        let activate = rule.as_ref().is_none_or(|r| !r.widget);
        self.space.map_element(smithay_window.clone(), pos, activate);
        self.space.raise_element(&smithay_window, true);
        self.enforce_below_windows();
        // Place the X11 window in X11-root space at its compositor screen
        // position. Subsumes the previous `Rectangle::from_size` initial
        // configure (which left the window at root (0,0) and broke pointer
        // routing for windows larger than the X11 root).
        self.sync_x11_position(&smithay_window);
        // Focus, decorations, and applied_rule storage are deferred to
        // surface_associated(), which fires once the wl_surface is paired.
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!("X11 override-redirect mapped: {:?}", window.class());

        // Pin the X11-root → canvas anchor when an OR-only X11 app
        // (jgmenu / dmenu style) opens its first popup: an OR whose
        // WM_CLASS doesn't match any managed X11 window — so no managed
        // app is its likely creator. Place the OR at the cursor;
        // subsequent ORs in the chain translate from that anchor so
        // their relative offsets are kept. Managed-app unparented ORs
        // (e.g. Steam hover menus) keep going through `pick_anchor`.
        let class = window.class();
        let has_class_match = !class.is_empty()
            && self.space.elements().any(|w| {
                w.x11_surface().is_some_and(|x| x.class() == class)
            });
        if self.or_root_anchor.is_none() && !has_class_match {
            let cursor = self.seat.get_pointer().unwrap().current_location();
            let cursor_i32 = smithay::utils::Point::from((cursor.x as i32, cursor.y as i32));
            self.or_root_anchor = Some(cursor_i32 - window.geometry().loc);
        }

        self.x11_override_redirect.push(window);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::info!("X11 unmapped: {:?}", window.class());
        self.x11_override_redirect.retain(|w| w != &window);
        if self.x11_override_redirect.is_empty() {
            self.or_root_anchor = None;
        }

        // If this OR currently held wl_keyboard focus (via the
        // xwayland-keyboard-grab protocol), clear focus so it doesn't
        // dangle on a dead surface after the grab tears down.
        if let Some(wl) = window.wl_surface() {
            let keyboard = self.seat.get_keyboard().unwrap();
            if keyboard.current_focus().is_some_and(|f| f.0 == wl) {
                keyboard.set_focus(
                    self,
                    None::<FocusTarget>,
                    SERIAL_COUNTER.next_serial(),
                );
            }
        }

        if let Some(smithay_window) = self.find_x11_window(&window) {
            if let Some(wl_surface) = smithay_window.wl_surface() {
                let keyboard = self.seat.get_keyboard().unwrap();
                if keyboard.current_focus().is_some_and(|f| f.0 == *wl_surface) {
                    keyboard.set_focus(
                        self,
                        None::<FocusTarget>,
                        SERIAL_COUNTER.next_serial(),
                    );
                }
                self.decorations.remove(&wl_surface.id());
                self.render.csd_shadows.remove(&wl_surface.id());
                self.pending_ssd.remove(&wl_surface.id());
                self.pending_center.remove(&*wl_surface);
                self.pending_size.remove(&*wl_surface);
                self.pending_recenter.remove(&wl_surface.id());
            }

            let fs_output = self
                .fullscreen
                .iter()
                .find(|(_, fs)| fs.window == smithay_window)
                .map(|(o, _)| o.clone());
            if let Some(output) = fs_output {
                let fs = self.fullscreen.remove(&output).unwrap();
                crate::state::output_state(&output).camera = fs.saved_camera;
                crate::state::output_state(&output).zoom = fs.saved_zoom;
                self.update_output_from_camera();
            }

            self.focus_history.retain(|w| w != &smithay_window);
            self.space.unmap_elem(&smithay_window);
            self.update_x11_stacking_order();
        }
    }

    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.unmapped_window(xwm, window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let old_geo = window.geometry();
        let mut new_geo = old_geo;
        if let Some(w) = w {
            new_geo.size.w = w as i32;
        }
        if let Some(h) = h {
            new_geo.size.h = h as i32;
        }
        // Honor position + size from the request so X11 CSD resize works
        // from all edges. The X11 loc is internal to XWayland; we apply the
        // delta to the compositor Space position.
        if let Some(x) = x {
            new_geo.loc.x = x;
        }
        if let Some(y) = y {
            new_geo.loc.y = y;
        }
        window.configure(new_geo).ok();

        // Apply X11 position delta to Space element location, but only
        // during an active resize — otherwise apps reposition themselves
        // to their remembered X11 root coords after map, undoing our centering.
        let dx = new_geo.loc.x - old_geo.loc.x;
        let dy = new_geo.loc.y - old_geo.loc.y;
        if (dx != 0 || dy != 0)
            && let Some(smithay_window) = self.find_x11_window(&window)
            && let Some(wl_surface) = smithay_window.wl_surface()
            && let Some(loc) = self.space.element_location(&smithay_window)
        {
            let is_resizing = with_states(&wl_surface, |states| {
                states
                    .data_map
                    .get::<std::cell::RefCell<crate::grabs::ResizeState>>()
                    .is_some_and(|s| !matches!(*s.borrow(), crate::grabs::ResizeState::Idle))
            });
            if is_resizing {
                let new_loc = loc + smithay::utils::Point::from((dx, dy));
                self.space.map_element(smithay_window.clone(), new_loc, false);
            }
            // Re-establish root_pos = canvas - camera. For resize this is a
            // no-op (client's request already matches); for self-repositioning
            // apps it overrides the client's chosen root coords (we own them).
            self.sync_x11_position(&smithay_window);
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<smithay::xwayland::xwm::X11Window>,
    ) {
    }

    fn resize_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32, edge: ResizeEdge) {
        let Some(smithay_window) = self.find_x11_window(&window) else { return };
        let Some(wl_surface) = smithay_window.wl_surface().map(|s| s.into_owned()) else { return };

        let pointer = self.seat.get_pointer().unwrap();
        let start_data = GrabStartData {
            focus: Some((FocusTarget(wl_surface.clone()), pointer.current_location())),
            button: 0x110, // BTN_LEFT
            location: pointer.current_location(),
        };

        let xdg_edge = x11_edge_to_xdg(edge);
        let initial_window_location = self.space.element_location(&smithay_window).unwrap();
        let initial_window_size = smithay_window.geometry().size;

        // Store resize state for commit() repositioning
        with_states(&wl_surface, |states| {
            states
                .data_map
                .get_or_insert(|| std::cell::RefCell::new(crate::grabs::ResizeState::Idle))
                .replace(crate::grabs::ResizeState::Resizing {
                    edges: xdg_edge,
                    initial_window_location,
                    initial_window_size,
                });
        });

        self.cursor.grab_cursor = true;
        self.cursor.cursor_status = CursorImageStatus::Named(resize_cursor(xdg_edge));

        let output = self.active_output().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        let grab = crate::grabs::ResizeSurfaceGrab {
            start_data,
            window: smithay_window,
            edges: xdg_edge,
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
            output,
            last_clamped_location: pointer.current_location(),
            last_x11_configure: None,
            snap: driftwm::snap::SnapState::default(),
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32) {
        let Some(smithay_window) = self.find_x11_window(&window) else { return };
        let Some(wl_surface) = smithay_window.wl_surface().map(|s| s.into_owned()) else { return };

        if driftwm::config::applied_rule(&wl_surface).is_some_and(|r| r.widget) {
            return;
        }

        let pointer = self.seat.get_pointer().unwrap();
        let start_data = GrabStartData {
            focus: Some((FocusTarget(wl_surface), pointer.current_location())),
            button: 0x110, // BTN_LEFT
            location: pointer.current_location(),
        };

        // Client-initiated X11 move: same reasoning as xdg_shell —
        // always single-window.
        let initial_window_location = self.space.element_location(&smithay_window).unwrap();
        let grab = crate::grabs::MoveSurfaceGrab::new(
            start_data,
            smithay_window,
            initial_window_location,
            self.active_output().unwrap(),
            Vec::new(),
            std::collections::HashSet::new(),
        );
        let serial = SERIAL_COUNTER.next_serial();
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    fn allow_selection_access(&mut self, _xwm: XwmId, _sel: SelectionTarget) -> bool {
        true
    }

    fn send_selection(&mut self, _xwm: XwmId, sel: SelectionTarget, mime: String, fd: std::os::fd::OwnedFd) {
        // X11 client wants to read the current Wayland-side selection.
        match sel {
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.seat, mime, fd).ok();
            }
            SelectionTarget::Primary => {
                request_primary_client_selection(&self.seat, mime, fd).ok();
            }
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, sel: SelectionTarget, mimes: Vec<String>) {
        // X11 client claimed ownership of a selection — register it on the Wayland side.
        match sel {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mimes, ());
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mimes, ());
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, sel: SelectionTarget) {
        // X11 client dropped a selection — clear Wayland side only if it was compositor-set.
        match sel {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata::<Self>(&self.seat).is_some() {
                    clear_data_device_selection(&self.display_handle, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata::<Self>(&self.seat).is_some() {
                    clear_primary_selection(&self.display_handle, &self.seat);
                }
            }
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(smithay_window) = self.find_x11_window(&window) {
            self.enter_fullscreen(&smithay_window);
        }
    }

    fn minimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        window.set_mapped(true).ok();
    }

    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(w) = self.find_x11_window(&window) {
            self.toggle_fit_window(&w);
        }
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(w) = self.find_x11_window(&window) {
            self.unfit_window(&w);
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(smithay_window) = self.find_x11_window(&window)
            && let Some(wl_surface) = smithay_window.wl_surface()
            && let Some(output) = self.find_fullscreen_output_for_surface(&wl_surface)
        {
            self.exit_fullscreen_on(&output);
        }
    }
}

impl XWaylandShellHandler for DriftWm {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, wl_surface: WlSurface, surface: X11Surface) {
        tracing::debug!("X11 surface associated: {:?}", surface.class());

        // Clear loading cursor (same as compositor.rs commit path for Wayland windows)
        if self.cursor.exec_cursor_deadline.take().is_some() {
            self.cursor.exec_cursor_show_at = None;
            self.cursor.cursor_status = CursorImageStatus::default_named();
        }

        let Some(smithay_window) = self.find_x11_window(&surface) else {
            return;
        };

        // Apply window rules — store in wl_surface data_map (now available)
        let class = surface.class();
        let title = surface.title();
        let rule = self.config.match_window_rule(&class, &title).cloned();
        if let Some(ref rule) = rule {
            let applied = driftwm::config::AppliedWindowRule::from(rule);
            with_states(&wl_surface, |states| {
                states.data_map.insert_if_missing_threadsafe(|| {
                    std::sync::Mutex::new(applied.clone())
                });
                *states.data_map.get::<std::sync::Mutex<driftwm::config::AppliedWindowRule>>()
                    .unwrap().lock().unwrap() = applied;
            });
        }

        // SSD decorations: title bar widget is created only for `Server` mode.
        // For Client mode we still respect MOTIF hints (existing X11 fallback).
        let effective = driftwm::config::effective_decoration_mode(
            rule.as_ref().and_then(|r| r.decoration.as_ref()),
            &self.config.decorations.default_mode,
        );
        let wants_titlebar = match effective {
            driftwm::config::DecorationMode::Server => true,
            driftwm::config::DecorationMode::Borderless | driftwm::config::DecorationMode::None => false,
            driftwm::config::DecorationMode::Client => smithay_window.wants_ssd(),
        };

        if wants_titlebar {
            let geo = smithay_window.geometry();
            if geo.size.w > 0 && !self.decorations.contains_key(&wl_surface.id()) {
                let deco = crate::decorations::WindowDecoration::new(
                    geo.size.w, true, &self.config.decorations,
                );
                self.decorations.insert(wl_surface.id(), deco);
                self.pending_ssd.insert(wl_surface.id());
            }
        }

        // Focus + navigate — skip for widgets and child/utility X11 windows
        let is_child = surface.is_transient_for().is_some()
            || !matches!(surface.window_type(), None | Some(WmWindowType::Normal));
        let should_focus = !is_child && rule.as_ref().is_none_or(|r| !r.widget);
        if should_focus {
            let reset = self.config.zoom_reset_on_new_window;
            self.navigate_to_window(&smithay_window, reset);
        } else {
            // Widget: refocus previous window if this stole focus
            self.focus_history.retain(|w| w != &smithay_window);
            if let Some(prev) = self.focus_history.first().cloned() {
                let serial = SERIAL_COUNTER.next_serial();
                let keyboard = self.seat.get_keyboard().unwrap();
                let focus = prev.wl_surface().map(|s| FocusTarget(s.into_owned()));
                keyboard.set_focus(self, focus, serial);
            }
        }
    }
}

delegate_xwayland_shell!(DriftWm);

use smithay::delegate_xwayland_keyboard_grab;
use smithay::wayland::xwayland_keyboard_grab::XWaylandKeyboardGrabHandler;

impl XWaylandKeyboardGrabHandler for DriftWm {
    fn keyboard_focus_for_xsurface(&self, surface: &WlSurface) -> Option<FocusTarget> {
        self.find_x11_surface_by_wl(surface)
            .map(|_| FocusTarget(surface.clone()))
    }
}
delegate_xwayland_keyboard_grab!(DriftWm);
