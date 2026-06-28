use std::borrow::Cow;

use smithay::backend::renderer::{
    element::{Element, Kind, utils::RescaleRenderElement},
    gles::{
        GlesPixelProgram, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType,
        element::PixelShaderElement,
    },
};
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size};

use super::bridge::GlesBridge;
use super::elements::{OutputRenderElements, corner_round_rect};
use super::renderer::DriftRenderer;

/// Uniform declarations for background shaders. All three are optional:
/// shaders reference only what they need; undeclared uniforms get location -1
/// and pushes become silent no-ops (per GL spec). `u_camera` is canvas→screen
/// offset, `u_zoom` is the canvas→screen scale, `u_time` is seconds since start.
pub(super) const BG_UNIFORMS: &[UniformName<'static>] = &[
    UniformName {
        name: Cow::Borrowed("u_camera"),
        type_: UniformType::_2f,
    },
    UniformName {
        name: Cow::Borrowed("u_time"),
        type_: UniformType::_1f,
    },
    UniformName {
        name: Cow::Borrowed("u_zoom"),
        type_: UniformType::_1f,
    },
];

/// Shadow shader source — soft box-shadow around SSD windows.
const SHADOW_SHADER_SRC: &str = include_str!("../shaders/shadow.glsl");

/// Uniform declarations for the shadow shader.
pub(super) const SHADOW_UNIFORMS: &[UniformName<'static>] = &[
    UniformName {
        name: Cow::Borrowed("u_window_rect"),
        type_: UniformType::_4f,
    },
    UniformName {
        name: Cow::Borrowed("u_radius"),
        type_: UniformType::_1f,
    },
    UniformName {
        name: Cow::Borrowed("u_color"),
        type_: UniformType::_4f,
    },
    UniformName {
        name: Cow::Borrowed("u_corner_radius"),
        type_: UniformType::_1f,
    },
];

/// Compile the shadow shader program. Called once at startup alongside the background shader.
pub fn compile_shadow_shader(renderer: &mut GlesRenderer) -> Option<GlesPixelProgram> {
    match renderer.compile_custom_pixel_shader(SHADOW_SHADER_SRC, SHADOW_UNIFORMS) {
        Ok(shader) => Some(shader),
        Err(e) => {
            tracing::error!("Failed to compile shadow shader: {e}");
            None
        }
    }
}

/// Key that fully determines the precise shadow uniforms.
/// `[body_x0, body_y0, body_x1, body_y1, shadow_x, shadow_y, shadow_w, shadow_h]`
/// in post-zoom physical pixels. Comparing consecutive keys tells us whether the
/// shadow element needs its uniforms refreshed (avoiding spurious commit bumps
/// during fully static frames).
pub type ShadowPhysKey = [i32; 8];

/// Compute both the uniforms and the phys key for a shadow element.
///
/// * `body_pre_zoom` — the body's pre-zoom physical rect, computed via
///   `to_physical_precise_round(output_scale)` at the call site. For SSD
///   this includes the title-bar strip; for CSD it's the content rect.
/// * `shadow_area` — logical rect of the shadow PixelShaderElement (body ± padding).
/// * `output_scale` — the output's fractional scale.
/// * `zoom` — current viewport zoom.
/// * `shadow_radius` — Gaussian blur extent passed through unchanged.
/// * `corner_radius_phys` — corner radius in post-zoom physical pixels.
///
/// The body's post-zoom rect is obtained via `corner_round_rect` (same chain
/// as `PixelSnapRescaleElement`); the shadow's post-zoom rect via
/// `upscale(zoom).to_i32_round()` (same chain as `RescaleRenderElement`).
/// Both go through `to_physical_precise_round` for the output-scale step first,
/// so this stays correct at fractional HiDPI — not just fractional zoom.
fn shadow_uniforms_precise(
    body_pre_zoom: Rectangle<i32, Physical>,
    shadow_area: Rectangle<i32, Logical>,
    output_scale: Scale<f64>,
    zoom: f64,
    shadow_radius: f32,
    corner_radius_phys: f32,
) -> (Vec<Uniform<'static>>, ShadowPhysKey) {
    use driftwm::config::DecorationConfig;
    let sc = DecorationConfig::SHADOW_COLOR;
    let zoom_scale = Scale::from(zoom);

    // Body post-zoom: corner rounding (matches PixelSnapRescaleElement).
    let body_post = corner_round_rect(body_pre_zoom.to_f64(), zoom_scale);

    // Shadow post-zoom: independent loc/size rounding (matches RescaleRenderElement
    // wrapping PixelShaderElement whose inner geometry = shadow_area.to_physical_precise_round).
    let shadow_pre: Rectangle<i32, Physical> = shadow_area.to_physical_precise_round(output_scale);
    let shadow_post: Rectangle<i32, Physical> =
        shadow_pre.to_f64().upscale(zoom_scale).to_i32_round();

    // Linear map: shader-logical pixels → post-zoom physical pixels.
    let phys_w = shadow_post.size.w.max(1) as f64;
    let phys_h = shadow_post.size.h.max(1) as f64;
    let logical_w = shadow_area.size.w.max(1) as f64;
    let logical_h = shadow_area.size.h.max(1) as f64;
    let px = phys_w / logical_w;
    let py = phys_h / logical_h;

    // Hole rect in shader-logical space — after interpolation the boundary
    // rasterizes at exactly the body's physical pixel edges.
    let hole_x = (body_post.loc.x - shadow_post.loc.x) as f64 / px;
    let hole_y = (body_post.loc.y - shadow_post.loc.y) as f64 / py;
    let hole_w = body_post.size.w as f64 / px;
    let hole_h = body_post.size.h as f64 / py;

    // Corner radius: from post-zoom physical back into shader-logical.
    let corner_logical = corner_radius_phys as f64 / px;

    let uniforms = vec![
        Uniform::new(
            "u_window_rect",
            (hole_x as f32, hole_y as f32, hole_w as f32, hole_h as f32),
        ),
        Uniform::new("u_radius", shadow_radius),
        Uniform::new(
            "u_color",
            (
                sc[0] as f32 / 255.0,
                sc[1] as f32 / 255.0,
                sc[2] as f32 / 255.0,
                sc[3] as f32 / 255.0,
            ),
        ),
        Uniform::new("u_corner_radius", corner_logical as f32),
    ];

    let key: ShadowPhysKey = [
        body_post.loc.x,
        body_post.loc.y,
        body_post.loc.x + body_post.size.w,
        body_post.loc.y + body_post.size.h,
        shadow_post.loc.x,
        shadow_post.loc.y,
        shadow_post.size.w,
        shadow_post.size.h,
    ];

    (uniforms, key)
}

/// Build (or reuse) a cached shadow `PixelShaderElement` for a window body and
/// push it into `target` wrapped in a `RescaleRenderElement`.
///
/// `body_logical` is the rect that casts the shadow — the title-bar+content
/// strip for SSD windows, or the content rect for CSD. The shadow rect is
/// derived by inflating it by `SHADOW_RADIUS.ceil()` on every side.
///
/// Cache invalidation:
/// * post-zoom phys key change → uniforms refreshed (geometry / scale / zoom moved)
/// * opacity change → element reconstructed (alpha is fixed at construction time)
#[allow(clippy::too_many_arguments)]
pub(super) fn push_shadow_element<R: DriftRenderer>(
    target: &mut Vec<OutputRenderElements<R>>,
    cache: &mut std::collections::HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        crate::state::ShadowCacheEntry,
    >,
    surface_id: smithay::reexports::wayland_server::backend::ObjectId,
    shader: &GlesPixelProgram,
    body_logical: Rectangle<f64, Logical>,
    corner_radius_logical: f32,
    opacity: f64,
    output_scale: Scale<f64>,
    zoom: f64,
) {
    use driftwm::config::DecorationConfig;
    let shadow_radius = DecorationConfig::SHADOW_RADIUS;
    let pad = shadow_radius.ceil() as i32;

    let body_x = body_logical.loc.x.round() as i32;
    let body_y = body_logical.loc.y.round() as i32;
    let body_w = body_logical.size.w.round() as i32;
    let body_h = body_logical.size.h.round() as i32;
    let shadow_area = Rectangle::new(
        Point::<i32, Logical>::from((body_x - pad, body_y - pad)),
        Size::<i32, Logical>::from((body_w + 2 * pad, body_h + 2 * pad)),
    );

    let body_pre_zoom: Rectangle<i32, Physical> =
        body_logical.to_physical_precise_round(output_scale);
    let corner_r_phys = corner_radius_logical * output_scale.x as f32 * zoom as f32;
    let (fresh_uniforms, fresh_key) = shadow_uniforms_precise(
        body_pre_zoom,
        shadow_area,
        output_scale,
        zoom,
        shadow_radius,
        corner_r_phys,
    );

    // Alpha is baked into the element at construction; rebuild on opacity change.
    if cache
        .get(&surface_id)
        .is_some_and(|(elem, _)| (elem.alpha() - opacity as f32).abs() > f32::EPSILON)
    {
        cache.remove(&surface_id);
    }

    let (elem, cached_key) = cache.entry(surface_id).or_insert_with(|| {
        let elem = PixelShaderElement::new(
            shader.clone(),
            shadow_area,
            None,
            opacity as f32,
            fresh_uniforms.clone(),
            Kind::Unspecified,
        );
        (elem, Some(fresh_key))
    });

    if *cached_key != Some(fresh_key) {
        *cached_key = Some(fresh_key);
        elem.update_uniforms(fresh_uniforms);
    }
    elem.resize(shadow_area, None);
    target.push(OutputRenderElements::Background(GlesBridge(
        RescaleRenderElement::from_element(
            elem.clone(),
            Point::<i32, Physical>::from((0, 0)),
            zoom,
        ),
    )));
}

const BORDER_SHADER_SRC: &str = include_str!("../shaders/border.glsl");

pub(super) const BORDER_UNIFORMS: &[UniformName<'static>] = &[
    UniformName {
        name: Cow::Borrowed("u_inner_rect"),
        type_: UniformType::_4f,
    },
    UniformName {
        name: Cow::Borrowed("u_inner_radius"),
        type_: UniformType::_1f,
    },
    UniformName {
        name: Cow::Borrowed("u_border_width"),
        type_: UniformType::_1f,
    },
    UniformName {
        name: Cow::Borrowed("u_color"),
        type_: UniformType::_4f,
    },
];

pub fn compile_border_shader(renderer: &mut GlesRenderer) -> Option<GlesPixelProgram> {
    match renderer.compile_custom_pixel_shader(BORDER_SHADER_SRC, BORDER_UNIFORMS) {
        Ok(shader) => Some(shader),
        Err(e) => {
            tracing::error!("Failed to compile border shader: {e}");
            None
        }
    }
}

/// Key that fully determines border element identity & geometry, in post-zoom
/// physical pixels: `[inner_x0, inner_y0, inner_x1, inner_y1, element_x, element_y,
/// element_w, element_h, border_width, focused_flag]`.
pub type BorderPhysKey = [i32; 10];

#[allow(clippy::too_many_arguments)]
fn border_uniforms_precise(
    inner_pre_zoom: Rectangle<i32, Physical>,
    border_area: Rectangle<i32, Logical>,
    output_scale: Scale<f64>,
    zoom: f64,
    inner_radius_phys: f32,
    border_width_phys: f32,
    color: [u8; 4],
    focused: bool,
) -> (Vec<Uniform<'static>>, BorderPhysKey) {
    let zoom_scale = Scale::from(zoom);

    let inner_post = corner_round_rect(inner_pre_zoom.to_f64(), zoom_scale);
    let border_pre: Rectangle<i32, Physical> = border_area.to_physical_precise_round(output_scale);
    let border_post: Rectangle<i32, Physical> =
        border_pre.to_f64().upscale(zoom_scale).to_i32_round();

    let phys_w = border_post.size.w.max(1) as f64;
    let phys_h = border_post.size.h.max(1) as f64;
    let logical_w = border_area.size.w.max(1) as f64;
    let logical_h = border_area.size.h.max(1) as f64;
    let px = phys_w / logical_w;
    let py = phys_h / logical_h;

    let inner_x = (inner_post.loc.x - border_post.loc.x) as f64 / px;
    let inner_y = (inner_post.loc.y - border_post.loc.y) as f64 / py;
    let inner_w = inner_post.size.w as f64 / px;
    let inner_h = inner_post.size.h as f64 / py;
    let inner_r_logical = inner_radius_phys as f64 / px;
    let border_w_logical = border_width_phys as f64 / px;

    let uniforms = vec![
        Uniform::new(
            "u_inner_rect",
            (
                inner_x as f32,
                inner_y as f32,
                inner_w as f32,
                inner_h as f32,
            ),
        ),
        Uniform::new("u_inner_radius", inner_r_logical as f32),
        Uniform::new("u_border_width", border_w_logical as f32),
        Uniform::new(
            "u_color",
            (
                color[0] as f32 / 255.0,
                color[1] as f32 / 255.0,
                color[2] as f32 / 255.0,
                color[3] as f32 / 255.0,
            ),
        ),
    ];

    let key: BorderPhysKey = [
        inner_post.loc.x,
        inner_post.loc.y,
        inner_post.loc.x + inner_post.size.w,
        inner_post.loc.y + inner_post.size.h,
        border_post.loc.x,
        border_post.loc.y,
        border_post.size.w,
        border_post.size.h,
        border_width_phys.round() as i32,
        focused as i32,
    ];

    (uniforms, key)
}

/// Build (or reuse) a cached border `PixelShaderElement` and push it into
/// `target` wrapped in a `RescaleRenderElement`. `inner_logical` is the
/// content rect the border wraps; the border element extends
/// `border_width_logical` outside it on every side.
#[allow(clippy::too_many_arguments)]
pub(super) fn push_border_element<R: DriftRenderer>(
    target: &mut Vec<OutputRenderElements<R>>,
    cache: &mut std::collections::HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        crate::state::BorderCacheEntry,
    >,
    surface_id: smithay::reexports::wayland_server::backend::ObjectId,
    shader: &GlesPixelProgram,
    inner_logical: Rectangle<f64, Logical>,
    inner_radius_logical: f32,
    border_width_logical: i32,
    color: [u8; 4],
    focused: bool,
    opacity: f64,
    output_scale: Scale<f64>,
    zoom: f64,
) {
    if border_width_logical <= 0 {
        return;
    }
    let bw = border_width_logical;
    let inner_x = inner_logical.loc.x.round() as i32;
    let inner_y = inner_logical.loc.y.round() as i32;
    let inner_w = inner_logical.size.w.round() as i32;
    let inner_h = inner_logical.size.h.round() as i32;

    // Snap stroke width to whole physical pixels (1px floor) so every side
    // paints the same integer count of pixels regardless of fractional scale
    // or zoom. Then size the element with one extra physical pixel of slack:
    // smithay's default loc/size rounding can shift the element's physical
    // extent by ±1 px, so without slack the stroke band falls partially
    // outside the rasterized rect on one side and gets clipped (the cause
    // of the 1-px-vs-2-px asymmetry).
    let total_scale = output_scale.x * zoom;
    let border_w_phys = ((bw as f64) * total_scale).round().max(1.0) as f32;
    let pad_logical = (((border_w_phys as f64) + 1.0) / total_scale).ceil() as i32;
    let border_area = Rectangle::new(
        Point::<i32, Logical>::from((inner_x - pad_logical, inner_y - pad_logical)),
        Size::<i32, Logical>::from((inner_w + 2 * pad_logical, inner_h + 2 * pad_logical)),
    );

    let inner_pre_zoom: Rectangle<i32, Physical> =
        inner_logical.to_physical_precise_round(output_scale);
    let inner_r_phys = inner_radius_logical * output_scale.x as f32 * zoom as f32;

    let (fresh_uniforms, fresh_key) = border_uniforms_precise(
        inner_pre_zoom,
        border_area,
        output_scale,
        zoom,
        inner_r_phys,
        border_w_phys,
        color,
        focused,
    );

    if cache
        .get(&surface_id)
        .is_some_and(|(elem, _)| (elem.alpha() - opacity as f32).abs() > f32::EPSILON)
    {
        cache.remove(&surface_id);
    }

    let (elem, cached_key) = cache.entry(surface_id).or_insert_with(|| {
        let elem = PixelShaderElement::new(
            shader.clone(),
            border_area,
            None,
            opacity as f32,
            fresh_uniforms.clone(),
            Kind::Unspecified,
        );
        (elem, Some(fresh_key))
    });

    if *cached_key != Some(fresh_key) {
        *cached_key = Some(fresh_key);
        elem.update_uniforms(fresh_uniforms);
    }
    elem.resize(border_area, None);
    target.push(OutputRenderElements::Background(GlesBridge(
        RescaleRenderElement::from_element(
            elem.clone(),
            Point::<i32, Physical>::from((0, 0)),
            zoom,
        ),
    )));
}

const CORNER_CLIP_SRC: &str = include_str!("../shaders/corner_clip.glsl");

pub(super) const CORNER_CLIP_UNIFORMS: &[UniformName<'static>] = &[
    UniformName {
        name: Cow::Borrowed("aa_scale"),
        type_: UniformType::_1f,
    },
    UniformName {
        name: Cow::Borrowed("geo_size"),
        type_: UniformType::_2f,
    },
    UniformName {
        name: Cow::Borrowed("corner_radius"),
        type_: UniformType::_4f,
    },
    UniformName {
        name: Cow::Borrowed("input_to_geo"),
        type_: UniformType::Matrix3x3,
    },
];

pub fn compile_corner_clip_shader(renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
    match renderer.compile_custom_texture_shader(CORNER_CLIP_SRC, CORNER_CLIP_UNIFORMS) {
        Ok(shader) => Some(shader),
        Err(e) => {
            tracing::error!("Failed to compile corner clip shader: {e}");
            None
        }
    }
}

const TILE_BG_SRC: &str = include_str!("../shaders/tile_bg.glsl");

pub(super) const TILE_BG_UNIFORMS: &[UniformName<'static>] = &[
    UniformName {
        name: Cow::Borrowed("u_camera"),
        type_: UniformType::_2f,
    },
    UniformName {
        name: Cow::Borrowed("u_tile_size"),
        type_: UniformType::_2f,
    },
    UniformName {
        name: Cow::Borrowed("u_output_size"),
        type_: UniformType::_2f,
    },
];

pub(super) fn compile_tile_bg_shader(renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
    match renderer.compile_custom_texture_shader(TILE_BG_SRC, TILE_BG_UNIFORMS) {
        Ok(shader) => Some(shader),
        Err(e) => {
            tracing::error!("Failed to compile tile background shader: {e}");
            None
        }
    }
}

/// Mirror-tiling variant of [`compile_tile_bg_shader`] that folds seams into
/// reflections. A separate program (not a uniform) keeps the wrap mode off the
/// plain tile program shared with the gigapixel-TIFF fallback plane.
pub(super) fn compile_tile_bg_mirror_shader(renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
    let src = format!("#define MIRROR\n{TILE_BG_SRC}");
    match renderer.compile_custom_texture_shader(&src, TILE_BG_UNIFORMS) {
        Ok(shader) => Some(shader),
        Err(e) => {
            tracing::error!("Failed to compile mirror tile background shader: {e}");
            None
        }
    }
}

const WALLPAPER_BG_SRC: &str = include_str!("../shaders/wallpaper_bg.glsl");

pub(super) fn compile_wallpaper_bg_shader(renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
    match renderer.compile_custom_texture_shader(WALLPAPER_BG_SRC, &[]) {
        Ok(shader) => Some(shader),
        Err(e) => {
            tracing::error!("Failed to compile wallpaper background shader: {e}");
            None
        }
    }
}

/// Uniforms for a user `type = "shader"` background that samples a `texture`.
/// Same camera/zoom/time set as `BG_UNIFORMS`, plus the output and image sizes:
/// texture shaders get neither smithay's built-in `size` nor `textureSize()`
/// (GLSL ES 1.0), so a shader needs both passed in to map `v_coords` → canvas
/// → texel UV. `tex`/`alpha` are provided by the texture-shader path itself.
pub(super) const BG_TEX_UNIFORMS: &[UniformName<'static>] = &[
    UniformName {
        name: Cow::Borrowed("u_camera"),
        type_: UniformType::_2f,
    },
    UniformName {
        name: Cow::Borrowed("u_time"),
        type_: UniformType::_1f,
    },
    UniformName {
        name: Cow::Borrowed("u_zoom"),
        type_: UniformType::_1f,
    },
    UniformName {
        name: Cow::Borrowed("u_output_size"),
        type_: UniformType::_2f,
    },
    UniformName {
        name: Cow::Borrowed("u_texture_size"),
        type_: UniformType::_2f,
    },
];

/// Compile a user-supplied background shader as a *texture* shader so it can
/// sample the configured image via the built-in `tex` sampler.
pub(super) fn compile_textured_bg_shader(
    renderer: &mut GlesRenderer,
    src: &str,
) -> Result<GlesTexProgram, String> {
    renderer
        .compile_custom_texture_shader(src, BG_TEX_UNIFORMS)
        .map_err(|e| format!("compile error: {e}"))
}
