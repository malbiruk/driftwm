pub mod cvt;
pub mod gamma;
pub mod udev;
pub mod winit;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::WinitGraphicsBackend;

use crate::backend::udev::UdevRenderer;
use crate::render::AsGlesRenderer;

/// Backend abstraction — winit (nested) or udev (real hardware).
/// Only the renderer lives here; udev-specific state (DRM, session, etc.)
/// is captured by calloop closures in udev.rs.
pub enum Backend {
    Winit(Box<WinitGraphicsBackend<GlesRenderer>>),
    Udev(Box<UdevRenderer>),
}

impl Backend {
    /// Run `f` with a primary-GPU [`GlesRenderer`] for one-off work (shader
    /// compilation, dmabuf import, off-screen screenshot). For udev this is the
    /// underlying GlesRenderer of the multi-GPU manager's primary render node.
    /// Returns `None` when that renderer is unavailable — e.g. after the
    /// primary GPU was unplugged, which keeps the compositor alive.
    ///
    /// The render loop does NOT go through this — it grabs a full
    /// `MultiGpuRenderer` via `single_renderer` so cross-GPU scanout works.
    pub fn with_renderer<T>(&mut self, f: impl FnOnce(&mut GlesRenderer) -> T) -> Option<T> {
        match self {
            Backend::Winit(backend) => Some(f(backend.renderer())),
            Backend::Udev(udev) => {
                match udev.gpu_manager.single_renderer(&udev.primary_render_node) {
                    Ok(mut renderer) => Some(f(renderer.as_gles_renderer())),
                    Err(err) => {
                        tracing::warn!("primary GPU renderer unavailable: {err:?}");
                        None
                    }
                }
            }
        }
    }

    /// Start importing a committed surface's buffer on the primary GPU before
    /// the next frame needs it, overlapping the import (and any cross-GPU copy)
    /// with the client's remaining work instead of paying it at render time.
    pub fn early_import(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        if let Backend::Udev(udev) = self
            && let Err(err) = udev
                .gpu_manager
                .early_import(udev.primary_render_node, surface)
        {
            tracing::warn!("early import failed: {err:?}");
        }
    }
}
