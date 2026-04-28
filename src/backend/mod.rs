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
