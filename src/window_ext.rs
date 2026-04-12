use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle, Size};

/// Extension trait on `Window` for operations that differ per window type
/// (Wayland vs X11). Avoids `.toplevel().unwrap()` which panics for X11 windows.
pub trait WindowExt {
    fn send_close(&self);
    fn app_id_or_class(&self) -> Option<String>;
    fn window_title(&self) -> Option<String>;
    /// Whether the window wants compositor-drawn (server-side) decorations.
    /// For X11: checks MOTIF hints. For Wayland: checks xdg-decoration mode.
    fn wants_ssd(&self) -> bool;
    fn enter_fullscreen_configure(&self, size: Size<i32, Logical>);
    fn exit_fullscreen_configure(&self, saved_size: Size<i32, Logical>);
    fn enter_fit_configure(&self, size: Size<i32, Logical>);
    fn exit_fit_configure(&self, saved_size: Size<i32, Logical>);
    /// The parent surface set via xdg_toplevel.set_parent (Wayland) or
    /// WM_TRANSIENT_FOR (X11). Returns None for X11 (follow-up).
    fn parent_surface(&self) -> Option<WlSurface>;
    /// Whether this is a modal dialog (xdg-dialog-v1). Non-modal parented
    /// windows (palettes, find dialogs) return false.
    fn is_modal(&self) -> bool;
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

    fn wants_ssd(&self) -> bool {
        if let Some(_toplevel) = self.toplevel() {
            // Wayland: SSD is negotiated via xdg-decoration protocol,
            // handled in handlers/mod.rs (XdgDecorationHandler). Not checked here.
            false
        } else if let Some(x11) = self.x11_surface() {
            // is_decorated() = true means CLIENT draws decorations (no SSD needed)
            // is_decorated() = false means no MOTIF hints or app wants WM decorations
            !x11.is_decorated()
        } else {
            false
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

    fn exit_fullscreen_configure(&self, saved_size: Size<i32, Logical>) {
        if let Some(toplevel) = self.toplevel() {
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                if state.states.contains(xdg_toplevel::State::Maximized) {
                    state.size = Some(saved_size);
                } else {
                    state.size = None;
                }
            });
            toplevel.send_configure();
        } else if let Some(x11) = self.x11_surface() {
            x11.set_fullscreen(false).ok();
            x11.configure(Rectangle::new(x11.geometry().loc, saved_size))
                .ok();
        }
    }

    fn enter_fit_configure(&self, size: Size<i32, Logical>) {
        if let Some(toplevel) = self.toplevel() {
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Maximized);
                state.size = Some(size);
            });
            toplevel.send_configure();
        } else if let Some(x11) = self.x11_surface() {
            x11.set_maximized(true).ok();
            x11.configure(Rectangle::new(x11.geometry().loc, size)).ok();
        }
    }

    fn exit_fit_configure(&self, saved_size: Size<i32, Logical>) {
        if let Some(toplevel) = self.toplevel() {
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Maximized);
                state.size = Some(saved_size);
            });
            toplevel.send_configure();
        } else if let Some(x11) = self.x11_surface() {
            x11.set_maximized(false).ok();
            x11.configure(Rectangle::new(x11.geometry().loc, saved_size))
                .ok();
        }
    }

    fn parent_surface(&self) -> Option<WlSurface> {
        if let Some(toplevel) = self.toplevel() {
            smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .and_then(|d| d.lock().ok())
                    .and_then(|guard| guard.parent.clone())
            })
        } else {
            // X11 transient-for returns a window ID, not a WlSurface — follow-up
            None
        }
    }

    fn is_modal(&self) -> bool {
        if let Some(toplevel) = self.toplevel() {
            smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .and_then(|d| d.lock().ok())
                    .is_some_and(|guard| {
                        guard.dialog_hint
                            == smithay::wayland::shell::xdg::dialog::ToplevelDialogHint::Modal
                    })
            })
        } else {
            false
        }
    }
}
