pub mod udev;
pub mod winit;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::WinitGraphicsBackend;
use smithay::reexports::wayland_server::Resource;
use smithay::wayland::seat::WaylandFocus;

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
    loop_handle: &smithay::reexports::calloop::LoopHandle<'static, crate::state::DriftWm>,
) {
    use smithay::xwayland::xwm::X11Wm;
    use smithay::xwayland::{XWayland, XWaylandEvent};
    use std::process::Stdio;

    let (xwayland, client) = match XWayland::spawn(
        dh,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        Stdio::null(),
        Stdio::null(),
        |_| (),
    ) {
        Ok(pair) => pair,
        Err(err) => {
            tracing::error!("Failed to spawn XWayland: {err}");
            return;
        }
    };

    let handle = loop_handle.clone();
    if let Err(err) = loop_handle.insert_source(xwayland, move |event, _, data| match event {
        XWaylandEvent::Ready {
            x11_socket,
            display_number,
        } => {
            tracing::info!("XWayland ready on :{display_number}");
            // SAFETY: no other threads mutate env vars concurrently in the compositor
            unsafe {
                std::env::set_var("DISPLAY", format!(":{display_number}"));
            }
            // Export DISPLAY to systemd/D-Bus so D-Bus-activated X11 apps can find XWayland
            if let Err(e) = std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    "systemctl --user import-environment DISPLAY; \
                     hash dbus-update-activation-environment 2>/dev/null && \
                     dbus-update-activation-environment DISPLAY",
                ])
                .spawn()
            {
                tracing::warn!("Failed to export DISPLAY: {e}");
            }
            data.x11_display = Some(display_number);
            data.xwayland_client = Some(client.clone());

            match X11Wm::start_wm(
                handle.clone(),
                &data.display_handle,
                x11_socket,
                client.clone(),
            ) {
                Ok(wm) => {
                    tracing::info!("X11 window manager started");
                    data.x11_wm = Some(wm);
                }
                Err(err) => {
                    tracing::error!("Failed to start X11 WM: {err}");
                }
            }
        }
        XWaylandEvent::Error => {
            tracing::warn!("XWayland crashed, restarting...");

            // Clean up dead X11 state
            data.x11_wm = None;
            data.xwayland_client = None;
            data.x11_display = None;
            data.x11_override_redirect.clear();
            // SAFETY: no other threads mutate env vars concurrently
            unsafe {
                std::env::remove_var("DISPLAY");
            }

            // Collect X11 windows to remove
            let x11_windows: Vec<_> = data
                .space
                .elements()
                .filter(|w| w.x11_surface().is_some())
                .cloned()
                .collect();

            // Restore fullscreen state for any fullscreen X11 windows
            let fs_outputs: Vec<_> = data
                .fullscreen
                .iter()
                .filter(|(_, fs)| x11_windows.contains(&fs.window))
                .map(|(o, _)| o.clone())
                .collect();
            for output in fs_outputs {
                if let Some(fs) = data.fullscreen.remove(&output) {
                    crate::state::output_state(&output).camera = fs.saved_camera;
                    crate::state::output_state(&output).zoom = fs.saved_zoom;
                }
            }
            if !x11_windows.is_empty() {
                data.update_output_from_camera();
            }

            // Clear keyboard focus if it's on a dying X11 window
            let keyboard = data.seat.get_keyboard().unwrap();
            if let Some(focused) = keyboard.current_focus()
                && x11_windows
                    .iter()
                    .any(|w| w.wl_surface().as_deref() == Some(&focused.0))
            {
                keyboard.set_focus(
                    data,
                    None::<crate::state::FocusTarget>,
                    smithay::utils::SERIAL_COUNTER.next_serial(),
                );
            }

            // Unmap X11 windows and clean up associated state
            for w in &x11_windows {
                if let Some(wl_surface) = w.wl_surface() {
                    let id = Resource::id(&*wl_surface);
                    data.decorations.remove(&id);
                    data.render.csd_shadows.remove(&id);
                    data.pending_ssd.remove(&id);
                    data.pending_center.remove(&*wl_surface);
                    data.pending_size.remove(&*wl_surface);
                }
                data.focus_history.retain(|fw| fw != w);
                data.space.unmap_elem(w);
            }

            // Restart XWayland
            spawn_xwayland(&data.display_handle, &handle);

            std::process::Command::new("notify-send")
                .args([
                    "-u",
                    "critical",
                    "XWayland crashed",
                    "X11 apps were lost. XWayland has been restarted.",
                ])
                .spawn()
                .ok();
        }
    }) {
        tracing::error!("Failed to register XWayland event source: {err}");
    }
}
