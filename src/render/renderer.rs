// TODO(multi-gpu): drop this allow once the render path consumes these traits.
#![allow(dead_code)]

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::gles::{GlesFrame, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{
    Bind, ExportMem, ImportAll, ImportMem, Offscreen, Renderer, RendererSuper, Texture,
};

use crate::backend::udev::{MultiGpuFrame, MultiGpuRenderer};

// Shared renderer bounds so render code can stay generic over both the concrete
// GlesRenderer (winit, single-GPU) and the multi-GPU MultiRenderer path. The
// associated types sidestep unstable associated-type bounds.
pub trait DriftRenderer:
    ImportAll
    + ImportMem
    + ExportMem
    + Bind<Dmabuf>
    + Offscreen<GlesTexture>
    + Renderer<TextureId = Self::DriftTextureId, Error = Self::DriftError>
    + AsGlesRenderer
{
    type DriftTextureId: Texture + Clone + Send + 'static;
    type DriftError: std::error::Error
        + Send
        + Sync
        + From<<GlesRenderer as RendererSuper>::Error>
        + 'static;
}

impl<R> DriftRenderer for R
where
    R: ImportAll + ImportMem + ExportMem + Bind<Dmabuf> + Offscreen<GlesTexture> + AsGlesRenderer,
    R::TextureId: Texture + Clone + Send + 'static,
    R::Error:
        std::error::Error + Send + Sync + From<<GlesRenderer as RendererSuper>::Error> + 'static,
{
    type DriftTextureId = R::TextureId;
    type DriftError = R::Error;
}

// Reach the underlying GlesRenderer for Gles-only work (custom shader programs).
pub trait AsGlesRenderer {
    fn as_gles_renderer(&mut self) -> &mut GlesRenderer;
}

impl AsGlesRenderer for GlesRenderer {
    fn as_gles_renderer(&mut self) -> &mut GlesRenderer {
        self
    }
}

impl AsGlesRenderer for MultiGpuRenderer<'_> {
    fn as_gles_renderer(&mut self) -> &mut GlesRenderer {
        self.as_mut()
    }
}

// Same idea at frame granularity, for draw calls that need a raw GlesFrame.
pub trait AsGlesFrame<'frame, 'buffer>
where
    Self: 'frame,
{
    fn as_gles_frame(&mut self) -> &mut GlesFrame<'frame, 'buffer>;
}

impl<'frame, 'buffer> AsGlesFrame<'frame, 'buffer> for GlesFrame<'frame, 'buffer> {
    fn as_gles_frame(&mut self) -> &mut GlesFrame<'frame, 'buffer> {
        self
    }
}

impl<'frame, 'buffer> AsGlesFrame<'frame, 'buffer> for MultiGpuFrame<'_, 'frame, 'buffer> {
    fn as_gles_frame(&mut self) -> &mut GlesFrame<'frame, 'buffer> {
        self.as_mut()
    }
}
