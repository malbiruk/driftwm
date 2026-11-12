//! Drawing decals: still images placed at canvas coordinates.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::ImportMem;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::state::DriftWm;

use super::elements::{OutputRenderElements, PixelSnapRescaleElement};
use super::texel_src;

/// Elements for every decal in view, front-to-back. Locations are output-relative
/// and pre-zoom, like windows', so `PixelSnapRescaleElement` applies the zoom.
pub(super) fn build_decal_elements(
    state: &mut DriftWm,
    renderer: &mut GlesRenderer,
    output_scale: f64,
    camera: Point<f64, Logical>,
    zoom: f64,
    visible_rect: Rectangle<i32, Logical>,
) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();
    let visible = visible_rect.to_f64();

    for index in (0..state.decals.items.len()).rev() {
        let decal = state.decals.items[index].clone();
        let Some(rect) = state.decals.rect_of(&decal) else {
            continue;
        };
        if !visible.overlaps(rect) {
            continue;
        }
        if !state.decals.textures.contains_key(&decal.path) {
            let loaded = load(renderer, &decal.path);
            if loaded.is_none() {
                tracing::warn!("decals: could not load {}", decal.path.display());
            }
            state.decals.textures.insert(decal.path.clone(), loaded);
        }
        let Some(Some(buffer)) = state.decals.textures.get(&decal.path) else {
            continue;
        };
        let Some(size) = state.decals.image_size(&decal.path) else {
            continue;
        };
        let texels = Size::<i32, Physical>::from((size.w, size.h));
        let location: Point<f64, Physical> =
            Point::<f64, Logical>::from((rect.loc.x - camera.x, rect.loc.y - camera.y))
                .to_physical_precise_round(output_scale);
        let element = TextureRenderElement::from_texture_buffer(
            location,
            buffer,
            None,
            Some(texel_src(texels)),
            Some(rect.size.to_i32_round()),
            Kind::Unspecified,
        );
        elements.push(OutputRenderElements::Decal(
            PixelSnapRescaleElement::from_element(
                element,
                Point::<i32, Physical>::from((0, 0)),
                zoom,
            ),
        ));
    }
    elements
}

/// Decode `path` and upload it. Pixels are premultiplied here: the renderer
/// blends textures as premultiplied alpha, so straight alpha would leave a dark
/// fringe around every soft edge.
fn load(renderer: &mut GlesRenderer, path: &std::path::Path) -> Option<TextureBuffer<GlesTexture>> {
    let img = image::open(path).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    let mut raw = img.into_raw();
    for px in raw.chunks_exact_mut(4) {
        let a = px[3] as u16;
        for c in &mut px[..3] {
            *c = ((*c as u16 * a + 127) / 255) as u8;
        }
    }
    let texture = renderer
        .import_memory(
            &raw,
            Fourcc::Abgr8888,
            Size::<i32, Buffer>::from((w as i32, h as i32)),
            false,
        )
        .ok()?;
    Some(TextureBuffer::from_texture(
        renderer,
        texture,
        1,
        Transform::Normal,
        None,
    ))
}
