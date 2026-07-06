//! Background rendering for off-screen captures.
//!
//! The live background lives in per-output caches keyed to each output's
//! camera/zoom (see `background.rs`). A capture is a virtual viewport at an
//! arbitrary camera/DPI, so it can't reuse those — it builds fresh background
//! elements per tile, reusing the compiled shaders in `state.render`.
//!
//! Everything here is a deterministic function of canvas position (shaders read
//! `u_camera`; textures sample by canvas coord), so capture tiles stitch with no
//! overlap margin. The one exception, the gigapixel pyramidal-TIFF wallpaper, is
//! captured from a single coarse LOD rather than its lazy chunk pool (full
//! detail would need the streaming pool, which can't render an arbitrary region
//! synchronously): zoomed-out captures look right, extreme-DPI ones go soft.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::gles::{
    GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
    element::PixelShaderElement,
};
use smithay::utils::{Buffer, Logical, Point, Rectangle, Size};

use driftwm::config::BackgroundKind;

use super::OutputRenderElements;
use super::bridge::GlesBridge;
use super::elements::TileShaderElement;

/// Largest dimension of the gigapixel-TIFF coarse LOD decoded for a capture.
/// Bounds the one-shot decode/upload; the level is stretched over the full-res
/// canvas extent, so detail degrades softly.
const TIFF_LOD_CAP: u32 = 4096;

/// A background prepared once for a capture, then emitted per tile.
pub(crate) enum CaptureBackground {
    /// Transparent — `window` captures, fullscreen, or an unavailable background.
    None,
    /// Pixel shader (dot grid / custom / baked-chunk shader, rendered live).
    Shader(GlesPixelProgram),
    /// A texture tiled across the canvas (single image, or a gigapixel TIFF's
    /// coarse LOD). `tile_size` is the wrap period in canvas units.
    Tile {
        shader: GlesTexProgram,
        texture: GlesTexture,
        tex_w: i32,
        tex_h: i32,
        tile_size: (f32, f32),
    },
    /// A screen-fixed wallpaper stretched across `dest` — the canvas footprint of
    /// the output the capture lands on. A region crops it (undistorted, as on
    /// screen); off-screen areas stay transparent.
    Wallpaper {
        shader: GlesTexProgram,
        texture: GlesTexture,
        tex_w: i32,
        tex_h: i32,
        dest: Rectangle<i32, Logical>,
    },
    /// A user shader sampling a texture (`type = "shader"` with a `texture`).
    TexturedShader {
        shader: GlesTexProgram,
        texture: GlesTexture,
        tex_w: i32,
        tex_h: i32,
    },
}

impl CaptureBackground {
    /// Build the background for a capture of `region`. `include` is false for the
    /// `window` target (isolated on transparency) → always `None`.
    pub(crate) fn prepare(
        state: &mut crate::state::DriftWm,
        renderer: &mut GlesRenderer,
        region: Rectangle<i32, Logical>,
        include: bool,
    ) -> Self {
        if !include {
            return Self::None;
        }
        match state.config.background.kind.clone() {
            BackgroundKind::Default => shader_bg(state),
            BackgroundKind::Shader { texture: None, .. } => shader_bg(state),
            BackgroundKind::Shader {
                path,
                texture: Some(tex),
            } => textured_shader_bg(renderer, &path, &tex).unwrap_or(Self::None),
            BackgroundKind::Tile(path) if is_tiff(&path) => {
                tiff_tile_bg(state, renderer, &path).unwrap_or(Self::None)
            }
            BackgroundKind::Tile(path) => {
                image_tile_bg(state, renderer, &path).unwrap_or(Self::None)
            }
            BackgroundKind::Wallpaper(path) => {
                wallpaper_bg(state, renderer, &path, region).unwrap_or(Self::None)
            }
            // No built-in background: captures leave the canvas transparent.
            BackgroundKind::None => Self::None,
        }
    }

    /// Background elements for one capture tile whose top-left canvas coord is
    /// `tile_camera`, covering `tile_logical` canvas units.
    pub(crate) fn tile_elements(
        &self,
        tile_camera: Point<f64, Logical>,
        tile_logical: Size<i32, Logical>,
        time: f32,
    ) -> Vec<OutputRenderElements> {
        let cam = (tile_camera.x as f32, tile_camera.y as f32);
        let out_size = (tile_logical.w as f32, tile_logical.h as f32);
        match self {
            Self::None => vec![],
            Self::Shader(shader) => {
                let area = Rectangle::from_size(tile_logical);
                let elem = PixelShaderElement::new(
                    shader.clone(),
                    area,
                    Some(vec![area]),
                    1.0,
                    vec![
                        Uniform::new("u_camera", cam),
                        Uniform::new("u_time", time),
                        Uniform::new("u_zoom", 1.0f32),
                    ],
                    Kind::Unspecified,
                );
                vec![OutputRenderElements::Background(GlesBridge(
                    RescaleRenderElement::from_element(elem, (0, 0).into(), 1.0),
                ))]
            }
            Self::Tile {
                shader,
                texture,
                tex_w,
                tex_h,
                tile_size,
            } => {
                let area = Rectangle::from_size(tile_logical);
                let elem = TileShaderElement::new(
                    shader.clone(),
                    texture.clone(),
                    *tex_w,
                    *tex_h,
                    area,
                    Some(vec![area]),
                    1.0,
                    vec![
                        Uniform::new("u_camera", cam),
                        Uniform::new("u_tile_size", *tile_size),
                        Uniform::new("u_output_size", out_size),
                    ],
                    Kind::Unspecified,
                );
                vec![OutputRenderElements::TileBg(
                    RescaleRenderElement::from_element(elem, (0, 0).into(), 1.0),
                )]
            }
            Self::TexturedShader {
                shader,
                texture,
                tex_w,
                tex_h,
            } => {
                let area = Rectangle::from_size(tile_logical);
                let elem = TileShaderElement::new(
                    shader.clone(),
                    texture.clone(),
                    *tex_w,
                    *tex_h,
                    area,
                    Some(vec![area]),
                    1.0,
                    vec![
                        Uniform::new("u_camera", cam),
                        Uniform::new("u_time", time),
                        Uniform::new("u_zoom", 1.0f32),
                        Uniform::new("u_output_size", out_size),
                        Uniform::new("u_texture_size", (*tex_w as f32, *tex_h as f32)),
                    ],
                    Kind::Unspecified,
                );
                vec![OutputRenderElements::TileBg(
                    RescaleRenderElement::from_element(elem, (0, 0).into(), 1.0),
                )]
            }
            Self::Wallpaper {
                shader,
                texture,
                tex_w,
                tex_h,
                dest,
            } => {
                // Place the element at `dest`'s origin in tile-local coords so
                // v_coords [0,1] span the output footprint (the tile sees its
                // slice). The rounded per-tile offset can differ by <1px at
                // fractional `--scale`, so a multi-tile wallpaper may show a
                // hairline seam (unlike the canvas-space shader/tile backgrounds).
                let loc = Point::<i32, Logical>::from((
                    (dest.loc.x as f64 - tile_camera.x).round() as i32,
                    (dest.loc.y as f64 - tile_camera.y).round() as i32,
                ));
                let area = Rectangle::new(loc, dest.size);
                let elem = TileShaderElement::new(
                    shader.clone(),
                    texture.clone(),
                    *tex_w,
                    *tex_h,
                    area,
                    Some(vec![area]),
                    1.0,
                    vec![],
                    Kind::Unspecified,
                );
                vec![OutputRenderElements::WallpaperBg(elem)]
            }
        }
    }
}

fn shader_bg(state: &crate::state::DriftWm) -> CaptureBackground {
    match state.render.background_shader.clone() {
        Some(shader) => CaptureBackground::Shader(shader),
        None => CaptureBackground::None,
    }
}

fn image_tile_bg(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    path: &str,
) -> Option<CaptureBackground> {
    let shader = if state.config.background.mirror_tile {
        state.render.tile_mirror_shader.clone()?
    } else {
        state.render.tile_shader.clone()?
    };
    let (texture, w, h) = load_texture(renderer, path)?;
    Some(CaptureBackground::Tile {
        shader,
        texture,
        tex_w: w,
        tex_h: h,
        tile_size: (w as f32, h as f32),
    })
}

fn wallpaper_bg(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    path: &str,
    region: Rectangle<i32, Logical>,
) -> Option<CaptureBackground> {
    let shader = state.render.wallpaper_shader.clone()?;
    let dest = wallpaper_dest(state, region)?;
    let (texture, w, h) = load_texture(renderer, path)?;
    Some(CaptureBackground::Wallpaper {
        shader,
        texture,
        tex_w: w,
        tex_h: h,
        dest,
    })
}

/// The wallpaper's canvas footprint: the viewport rect of the output the capture
/// region sits on (wallpaper is screen-fixed and fills that output), falling back
/// to the active output.
fn wallpaper_dest(
    state: &crate::state::DriftWm,
    region: Rectangle<i32, Logical>,
) -> Option<Rectangle<i32, Logical>> {
    let center = Point::<i32, Logical>::from((
        region.loc.x + region.size.w / 2,
        region.loc.y + region.size.h / 2,
    ));
    let output = state
        .space
        .outputs()
        .find(|o| crate::state::output_viewport_rect(o).contains(center))
        .cloned()
        .or_else(|| state.active_output())?;
    Some(crate::state::output_viewport_rect(&output))
}

fn textured_shader_bg(
    renderer: &mut GlesRenderer,
    path: &str,
    texture_path: &str,
) -> Option<CaptureBackground> {
    let src = std::fs::read_to_string(path).ok()?;
    let shader = super::shaders::compile_textured_bg_shader(renderer, &src).ok()?;
    let (texture, w, h) = load_texture(renderer, texture_path)?;
    Some(CaptureBackground::TexturedShader {
        shader,
        texture,
        tex_w: w,
        tex_h: h,
    })
}

/// Decode a single coarse LOD of a pyramidal TIFF and tile it at the full-res
/// canvas extent (so it lands exactly where the live chunked path would, just
/// lower-res).
fn tiff_tile_bg(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    path: &str,
) -> Option<CaptureBackground> {
    use super::tile_chunks_tiff::TiffSource;
    use smithay::backend::renderer::ImportMem;

    let shader = state.render.tile_shader.clone()?;
    let mut source = TiffSource::open(path).ok()?;
    let lods: Vec<_> = source.lods().to_vec();
    let (full_w, full_h) = lods.first()?.image_dims;

    // Finest LOD that fits one bounded texture; fall back to the coarsest.
    let lod_idx = lods
        .iter()
        .position(|m| m.image_dims.0.max(m.image_dims.1) <= TIFF_LOD_CAP)
        .unwrap_or(lods.len() - 1);
    let meta = lods[lod_idx];
    let tiles_across = meta.image_dims.0.div_ceil(meta.tile_dims.0);
    let tiles_down = meta.image_dims.1.div_ceil(meta.tile_dims.1);
    let n = tiles_across.max(tiles_down);
    let block = source.read_block(lod_idx as u32, 0, 0, n).ok()?;

    let texture = renderer
        .import_memory(
            &block.rgba,
            Fourcc::Abgr8888,
            Size::<i32, Buffer>::from((block.width as i32, block.height as i32)),
            false,
        )
        .ok()?;
    Some(CaptureBackground::Tile {
        shader,
        texture,
        tex_w: block.width as i32,
        tex_h: block.height as i32,
        tile_size: (full_w as f32, full_h as f32),
    })
}

fn load_texture(renderer: &mut GlesRenderer, path: &str) -> Option<(GlesTexture, i32, i32)> {
    use smithay::backend::renderer::ImportMem;

    let img = image::open(path).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    let raw = img.into_raw();
    let texture = renderer
        .import_memory(
            &raw,
            Fourcc::Abgr8888,
            Size::<i32, Buffer>::from((w as i32, h as i32)),
            false,
        )
        .ok()?;
    Some((texture, w as i32, h as i32))
}

fn is_tiff(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".tif") || lower.ends_with(".tiff")
}
