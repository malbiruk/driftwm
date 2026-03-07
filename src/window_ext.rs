use smithay::desktop::Window;
use smithay::utils::{Logical, Rectangle, Size};

/// Extension trait on `Window` for operations that differ per window type
/// (Wayland vs X11). Avoids `.toplevel().unwrap()` which panics for X11 windows.
pub trait WindowExt {
    fn send_close(&self);
    fn app_id_or_class(&self) -> Option<String>;
    fn window_title(&self) -> Option<String>;
    fn enter_fullscreen_configure(&self, size: Size<i32, Logical>);
    fn exit_fullscreen_configure(&self);
}

impl WindowExt for Window {
    fn send_close(&self) {
        if let Some(toplevel) = self.toplevel() {
            toplevel.send_close();
        } else if let Some(x11) = self.x11_surface() {
            x11.close().ok();
        }
    }

    fn app_id_or_class(&self) -> Option<String> {
        if let Some(toplevel) = self.toplevel() {
            smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .and_then(|d| d.lock().ok())
                    .and_then(|guard| guard.app_id.clone())
            })
        } else {
            self.x11_surface().map(|x11| x11.class())
        }
    }

    fn window_title(&self) -> Option<String> {
        if let Some(toplevel) = self.toplevel() {
            smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .and_then(|d| d.lock().ok())
                    .and_then(|guard| guard.title.clone())
            })
        } else {
            self.x11_surface().map(|x11| x11.title())
        }
    }

    fn enter_fullscreen_configure(&self, size: Size<i32, Logical>) {
        if let Some(toplevel) = self.toplevel() {
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.size = Some(size);
            });
            toplevel.send_configure();
        } else if let Some(x11) = self.x11_surface() {
            x11.set_fullscreen(true).ok();
            x11.configure(Rectangle::from_size(size)).ok();
        }
    }

    fn exit_fullscreen_configure(&self) {
        if let Some(toplevel) = self.toplevel() {
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                state.size = None;
            });
            toplevel.send_configure();
        } else if let Some(x11) = self.x11_surface() {
            x11.set_fullscreen(false).ok();
        }
    }
}
