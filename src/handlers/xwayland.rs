use crate::state::{CalloopData, DriftWm, FocusTarget};
use smithay::{
    delegate_xwayland_shell,
    desktop::Window,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{Logical, Rectangle, SERIAL_COUNTER},
    wayland::{
        selection::SelectionTarget,
        seat::WaylandFocus,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        xwm::{Reorder, ResizeEdge, X11Wm, XwmHandler, XwmId},
        X11Surface,
    },
};

// ---- Calloop wrappers (X11Wm::start_wm requires D = CalloopData) ----

impl XwmHandler for CalloopData {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        XwmHandler::xwm_state(&mut self.state, xwm)
    }
    fn new_window(&mut self, xwm: XwmId, window: X11Surface) {
        XwmHandler::new_window(&mut self.state, xwm, window);
    }
    fn new_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        XwmHandler::new_override_redirect_window(&mut self.state, xwm, window);
    }
    fn map_window_request(&mut self, xwm: XwmId, window: X11Surface) {
        XwmHandler::map_window_request(&mut self.state, xwm, window);
    }
    fn mapped_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        XwmHandler::mapped_override_redirect_window(&mut self.state, xwm, window);
    }
    fn unmapped_window(&mut self, xwm: XwmId, window: X11Surface) {
        XwmHandler::unmapped_window(&mut self.state, xwm, window);
    }
    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        XwmHandler::destroyed_window(&mut self.state, xwm, window);
    }
    fn configure_request(
        &mut self, xwm: XwmId, window: X11Surface,
        x: Option<i32>, y: Option<i32>, w: Option<u32>, h: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        XwmHandler::configure_request(&mut self.state, xwm, window, x, y, w, h, reorder);
    }
    fn configure_notify(
        &mut self, xwm: XwmId, window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        above: Option<smithay::xwayland::xwm::X11Window>,
    ) {
        XwmHandler::configure_notify(&mut self.state, xwm, window, geometry, above);
    }
    fn resize_request(&mut self, xwm: XwmId, window: X11Surface, button: u32, edge: ResizeEdge) {
        XwmHandler::resize_request(&mut self.state, xwm, window, button, edge);
    }
    fn move_request(&mut self, xwm: XwmId, window: X11Surface, button: u32) {
        XwmHandler::move_request(&mut self.state, xwm, window, button);
    }
    fn allow_selection_access(&mut self, xwm: XwmId, sel: SelectionTarget) -> bool {
        XwmHandler::allow_selection_access(&mut self.state, xwm, sel)
    }
    fn send_selection(&mut self, xwm: XwmId, sel: SelectionTarget, mime: String, fd: std::os::fd::OwnedFd) {
        XwmHandler::send_selection(&mut self.state, xwm, sel, mime, fd);
    }
    fn new_selection(&mut self, xwm: XwmId, sel: SelectionTarget, mimes: Vec<String>) {
        XwmHandler::new_selection(&mut self.state, xwm, sel, mimes);
    }
    fn cleared_selection(&mut self, xwm: XwmId, sel: SelectionTarget) {
        XwmHandler::cleared_selection(&mut self.state, xwm, sel);
    }
}

impl XWaylandShellHandler for CalloopData {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        XWaylandShellHandler::xwayland_shell_state(&mut self.state)
    }
    fn surface_associated(&mut self, xwm: XwmId, wl_surface: WlSurface, surface: X11Surface) {
        XWaylandShellHandler::surface_associated(&mut self.state, xwm, wl_surface, surface);
    }
}

// ---- Primary impls on DriftWm (Wayland dispatch uses DriftWm as state type) ----

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

        // X11 size is known upfront — center accounting for window size
        let geo = window.geometry();
        let pos = self
            .active_output()
            .and_then(|o| self.space.output_geometry(&o))
            .map(|viewport| {
                let cam = self.camera();
                let z = self.zoom();
                (
                    (cam.x + viewport.size.w as f64 / (2.0 * z)) as i32 - geo.size.w / 2,
                    (cam.y + viewport.size.h as f64 / (2.0 * z)) as i32 - geo.size.h / 2,
                )
            })
            .unwrap_or((0, 0));

        window
            .configure(Rectangle::from_size(geo.size))
            .ok();

        self.space.map_element(smithay_window.clone(), pos, true);
        self.space.raise_element(&smithay_window, true);
        self.enforce_below_windows();
        // Focus and pending_center are deferred to surface_associated(),
        // which fires once the wl_surface is paired via xwayland-shell serial.
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!("X11 override-redirect mapped: {:?}", window.class());
        self.x11_override_redirect.push(window);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::info!("X11 unmapped: {:?}", window.class());
        self.x11_override_redirect.retain(|w| w != &window);

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
                self.pending_ssd.remove(&wl_surface.id());
                self.pending_center.remove(&*wl_surface);
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
        }
    }

    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.unmapped_window(xwm, window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let mut geo = window.geometry();
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        window.configure(Rectangle::from_size(geo.size)).ok();
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<smithay::xwayland::xwm::X11Window>,
    ) {
    }

    fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32, _edge: ResizeEdge) {
        // TODO: initiate ResizeSurfaceGrab
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {
        // TODO: initiate MoveSurfaceGrab
    }

    fn allow_selection_access(&mut self, _xwm: XwmId, _sel: SelectionTarget) -> bool {
        true
    }

    fn send_selection(&mut self, _xwm: XwmId, sel: SelectionTarget, mime: String, fd: std::os::fd::OwnedFd) {
        if let Some(wm) = self.x11_wm.as_mut() {
            wm.send_selection(sel, mime, fd, self.loop_handle.clone()).ok();
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, sel: SelectionTarget, mimes: Vec<String>) {
        if let Some(wm) = self.x11_wm.as_mut() {
            wm.new_selection(sel, Some(mimes)).ok();
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, sel: SelectionTarget) {
        if let Some(wm) = self.x11_wm.as_mut() {
            wm.new_selection(sel, None).ok();
        }
    }
}

impl XWaylandShellHandler for DriftWm {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, wl_surface: WlSurface, surface: X11Surface) {
        tracing::debug!("X11 surface associated: {:?}", surface.class());

        // Only act if this window is in our space (was map_window_request'd)
        if self.find_x11_window(&surface).is_none() {
            return;
        }

        // X11 windows are already centered at map time (size is known upfront),
        // so no pending_center needed. Just set focus now that the wl_surface exists.
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, Some(FocusTarget(wl_surface)), serial);
    }
}

delegate_xwayland_shell!(DriftWm);
