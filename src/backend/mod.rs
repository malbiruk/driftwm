pub mod udev;
pub mod winit;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::WinitGraphicsBackend;

/// Backend abstraction — winit (nested) or udev (real hardware).
/// Only the renderer lives here; udev-specific state (DRM, session, etc.)
/// is captured by calloop closures in udev.rs.
pub enum Backend {
    Winit(Box<WinitGraphicsBackend<GlesRenderer>>),
    Udev(Box<GlesRenderer>),
}

impl Backend {
    pub fn renderer(&mut self) -> &mut GlesRenderer {
        match self {
            Backend::Winit(backend) => backend.renderer(),
            Backend::Udev(renderer) => renderer.as_mut(),
        }
    }
}

/// Spawn XWayland and register it as a calloop event source.
/// On `Ready`, starts the X11 window manager and sets `DISPLAY`.
pub fn spawn_xwayland(
    dh: &smithay::reexports::wayland_server::DisplayHandle,
    loop_handle: &smithay::reexports::calloop::LoopHandle<'static, crate::state::CalloopData>,
) {
    use smithay::xwayland::{XWayland, XWaylandEvent};
    use smithay::xwayland::xwm::X11Wm;
    use std::process::Stdio;

    let (xwayland, client) = match XWayland::spawn(dh, None, std::iter::empty::<(String, String)>(), true, Stdio::null(), Stdio::null(), |_| ()) {
        Ok(pair) => pair,
        Err(err) => {
            tracing::error!("Failed to spawn XWayland: {err}");
            return;
        }
    };

    let handle = loop_handle.clone();
    if let Err(err) = loop_handle.insert_source(xwayland, move |event, _, data| match event {
        XWaylandEvent::Ready { x11_socket, display_number } => {
            tracing::info!("XWayland ready on :{display_number}");
            // SAFETY: no other threads mutate env vars concurrently in the compositor
            unsafe { std::env::set_var("DISPLAY", format!(":{display_number}")); }
            data.state.x11_display = Some(display_number);
            data.state.xwayland_client = Some(client.clone());

            match X11Wm::start_wm(handle.clone(), x11_socket, client.clone()) {
                Ok(wm) => {
                    tracing::info!("X11 window manager started");
                    data.state.x11_wm = Some(wm);
                }
                Err(err) => {
                    tracing::error!("Failed to start X11 WM: {err}");
                }
            }
        }
        XWaylandEvent::Error => {
            tracing::error!("XWayland failed to start");
        }
    }) {
        tracing::error!("Failed to register XWayland event source: {err}");
    }
}
