use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::{Color32F, Frame};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Transform};

use crate::backend::udev::{MultiGpuFrame, MultiGpuRenderer, MultiGpuRendererError};
use crate::render::renderer::AsGlesFrame;

/// Record damage on the MultiFrame for pixels drawn behind its back.
///
/// The MultiFrame's cross-GPU `finish()` only PRIME-copies regions drawn
/// through its own methods; anything painted via `as_gles_frame()` bypasses
/// that tracking, so on a secondary-GPU output those pixels would never reach
/// the scanout buffer (stale trails wherever only bridged elements repaint).
/// A fully transparent `draw_solid` records the damage without visual effect
/// (blending stays enabled for non-opaque colors).
pub fn record_bridged_damage<'render>(
    frame: &mut MultiGpuFrame<'render, '_, '_>,
    dst: Rectangle<i32, Physical>,
    damage: &[Rectangle<i32, Physical>],
) -> Result<(), MultiGpuRendererError<'render>> {
    frame.draw_solid(dst, damage, Color32F::TRANSPARENT)
}

// Adapts a Gles-only element (custom shaders, blur texture) to the multi-GPU
// renderer. These elements are pure effects that always render on the primary
// GPU, so the multi-GPU draw just bridges to the underlying GlesFrame; the
// MultiRenderer copies the result to the scanout GPU. Content elements that can
// be promoted to scanout planes stay generic over the renderer instead.
#[derive(Debug, Clone)]
pub struct GlesBridge<E>(pub E);

impl<E: Element> Element for GlesBridge<E> {
    fn id(&self) -> &Id {
        self.0.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.0.current_commit()
    }
    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.0.location(scale)
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.0.src()
    }
    fn transform(&self) -> Transform {
        self.0.transform()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.0.geometry(scale)
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.0.damage_since(scale, commit)
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.0.opaque_regions(scale)
    }
    fn alpha(&self) -> f32 {
        self.0.alpha()
    }
    fn kind(&self) -> Kind {
        self.0.kind()
    }
    fn is_framebuffer_effect(&self) -> bool {
        self.0.is_framebuffer_effect()
    }
}

impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for GlesBridge<E> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        self.0.draw(frame, src, dst, damage, opaque_regions, cache)
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        self.0.underlying_storage(renderer)
    }

    fn capture_framebuffer(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), GlesError> {
        self.0.capture_framebuffer(frame, src, dst, cache)
    }
}

impl<'render, E> RenderElement<MultiGpuRenderer<'render>> for GlesBridge<E>
where
    E: RenderElement<GlesRenderer>,
{
    fn draw(
        &self,
        frame: &mut MultiGpuFrame<'render, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), MultiGpuRendererError<'render>> {
        record_bridged_damage(frame, dst, damage)?;
        let frame = frame.as_gles_frame();
        RenderElement::<GlesRenderer>::draw(
            &self.0,
            frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )?;
        Ok(())
    }

    fn underlying_storage(
        &self,
        _renderer: &mut MultiGpuRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        // Gles-only effects are not scanout candidates across GPUs.
        None
    }
}
