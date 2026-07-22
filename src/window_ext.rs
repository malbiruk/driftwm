use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Size};
use smithay::wayland::shell::xdg::ToplevelSurface;

/// Set all four Tiled states. GTK et al. read Tiled as "drop your shadow +
/// rounded corners" even when they ignore xdg-decoration, so client chrome
/// stops colliding with ours.
///
/// Caveat: SCTK terminals (Alacritty) read `Tiled + size=None` as "stay at
/// current tile size" instead of "pick preferred" — so exit-fit /
/// exit-fullscreen always send saved_size explicitly.
pub fn set_tiled_states(toplevel: &ToplevelSurface) {
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
    toplevel.with_pending_state(|state| {
        state.states.set(xdg_toplevel::State::TiledLeft);
        state.states.set(xdg_toplevel::State::TiledRight);
        state.states.set(xdg_toplevel::State::TiledTop);
        state.states.set(xdg_toplevel::State::TiledBottom);
    });
}

/// Inverse of `set_tiled_states`.
pub fn unset_tiled_states(toplevel: &ToplevelSurface) {
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
    toplevel.with_pending_state(|state| {
        state.states.unset(xdg_toplevel::State::TiledLeft);
        state.states.unset(xdg_toplevel::State::TiledRight);
        state.states.unset(xdg_toplevel::State::TiledTop);
        state.states.unset(xdg_toplevel::State::TiledBottom);
    });
}

/// Wire-protocol mode for clients. Anything non-Client → SSD on the wire.
pub fn decoration_mode_to_wire(
    mode: &crate::config::DecorationMode,
) -> smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode{
    use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
    match mode {
        crate::config::DecorationMode::Client => Mode::ClientSide,
        _ => Mode::ServerSide,
    }
}

/// Window operations not directly available on smithay's shape, or that
/// need to read xdg-toplevel surface state.
pub trait WindowExt {
    fn send_close(&self);
    fn app_id_or_class(&self) -> Option<String>;
    fn window_title(&self) -> Option<String>;
    /// Always false — SSD is negotiated via xdg-decoration.
    fn wants_ssd(&self) -> bool;
    fn enter_fullscreen_configure(&self, size: Size<i32, Logical>);
    fn exit_fullscreen_configure(&self, saved_size: Size<i32, Logical>);
    fn enter_fit_configure(&self, size: Size<i32, Logical>);
    fn exit_fit_configure(&self, saved_size: Size<i32, Logical>);
    fn parent_surface(&self) -> Option<WlSurface>;
    /// True only for xdg-dialog-v1 modal dialogs. Non-modal parented windows
    /// (palettes, find dialogs) return false.
    fn is_modal(&self) -> bool;
    /// Widgets are persistent canvas furniture (excluded from close, nudge,
    /// focus-cycle, etc).
    fn is_widget(&self) -> bool;
    /// True only for a compositor-drawn suspended window (no client surface).
    fn is_suspended(&self) -> bool {
        false
    }
}

impl WindowExt for Window {
    fn send_close(&self) {
        if let Some(toplevel) = self.toplevel() {
            toplevel.send_close();
        }
    }

    fn app_id_or_class(&self) -> Option<String> {
        let toplevel = self.toplevel()?;
        smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok())
                .and_then(|guard| guard.app_id.clone())
        })
    }

    fn window_title(&self) -> Option<String> {
        let toplevel = self.toplevel()?;
        smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok())
                .and_then(|guard| guard.title.clone())
        })
    }

    fn wants_ssd(&self) -> bool {
        false
    }

    fn enter_fullscreen_configure(&self, size: Size<i32, Logical>) {
        let Some(toplevel) = self.toplevel() else {
            return;
        };
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
        toplevel.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.size = Some(size);
        });
        toplevel.send_configure();
    }

    fn exit_fullscreen_configure(&self, saved_size: Size<i32, Logical>) {
        let Some(toplevel) = self.toplevel() else {
            return;
        };
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
        // Keep Tiled, send saved_size explicitly. See exit_fit_configure.
        toplevel.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Fullscreen);
            state.size = Some(saved_size);
        });
        toplevel.send_configure();
    }

    fn enter_fit_configure(&self, size: Size<i32, Logical>) {
        let Some(toplevel) = self.toplevel() else {
            return;
        };
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
        toplevel.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Maximized);
            state.size = Some(size);
        });
        toplevel.send_configure();
    }

    fn exit_fit_configure(&self, saved_size: Size<i32, Logical>) {
        let Some(toplevel) = self.toplevel() else {
            return;
        };
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
        // Keep Tiled set so GTK/Chromium suppress CSD (otherwise repeated
        // toggles shrink the window). Send saved_size explicitly so SCTK
        // doesn't read "Tiled + None" as "stay at current size".
        toplevel.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Maximized);
            state.size = Some(saved_size);
        });
        toplevel.send_configure();
    }

    fn parent_surface(&self) -> Option<WlSurface> {
        let toplevel = self.toplevel()?;
        smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok())
                .and_then(|guard| guard.parent.clone())
        })
    }

    fn is_modal(&self) -> bool {
        let Some(toplevel) = self.toplevel() else {
            return false;
        };
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
    }

    fn is_widget(&self) -> bool {
        use smithay::wayland::seat::WaylandFocus;
        self.wl_surface()
            .as_deref()
            .and_then(crate::config::applied_rule)
            .is_some_and(|r| r.widget)
    }
}
