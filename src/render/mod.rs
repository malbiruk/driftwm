mod blur;
mod capture;
mod elements;

pub use blur::BlurCache;
pub(crate) use blur::compile_blur_shaders;
pub use capture::{render_screencopy, render_capture_frames};
pub use elements::{
    OutputRenderElements, PixelSnapRescaleElement, RoundedCornerElement,
    TileShaderElement, corner_round_rect,
};

use blur::{BlurLayer, BlurRequestData, process_blur_requests};

use std::borrow::Cow;
use std::time::Duration;

use smithay::{
    backend::renderer::{
        element::{
            Element, Kind,
            memory::MemoryRenderBufferRenderElement,
            surface::WaylandSurfaceRenderElement,
            utils::RescaleRenderElement,
            AsRenderElements,
        },
        gles::{GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType, element::PixelShaderElement},
    },
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    output::Output,
    utils::{Logical, Physical, Point, Rectangle, Scale},
};

use smithay::desktop::layer_map_for_output;
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::{Size, Transform};

use smithay::reexports::wayland_server::Resource;
use smithay::utils::IsAlive;
use smithay::wayland::compositor::with_states;
use smithay::wayland::seat::WaylandFocus;

use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::config::BackgroundKind;

/// Uniform declarations for background shaders.
/// Shaders receive u_camera and u_time.
/// Zoom is handled externally via RescaleRenderElement.
pub const BG_UNIFORMS: &[UniformName<'static>] = &[
    UniformName {
        name: std::borrow::Cow::Borrowed("u_camera"),
        type_: UniformType::_2f,
    },
    UniformName {
        name: std::borrow::Cow::Borrowed("u_time"),
        type_: UniformType::_1f,
    },
];

/// Shadow shader source — soft box-shadow around SSD windows.
const SHADOW_SHADER_SRC: &str = include_str!("../shaders/shadow.glsl");

/// Uniform declarations for the shadow shader.
pub const SHADOW_UNIFORMS: &[UniformName<'static>] = &[
    UniformName {
        name: std::borrow::Cow::Borrowed("u_window_rect"),
        type_: UniformType::_4f,
    },
    UniformName {
        name: std::borrow::Cow::Borrowed("u_radius"),
        type_: UniformType::_1f,
    },
    UniformName {
        name: std::borrow::Cow::Borrowed("u_color"),
        type_: UniformType::_4f,
    },
    UniformName {
        name: std::borrow::Cow::Borrowed("u_corner_radius"),
        type_: UniformType::_1f,
    },
];

/// Compile the shadow shader program. Called once at startup alongside the background shader.
pub fn compile_shadow_shader(renderer: &mut GlesRenderer) -> Option<smithay::backend::renderer::gles::GlesPixelProgram> {
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
    let shadow_post: Rectangle<i32, Physical> = shadow_pre.to_f64().upscale(zoom_scale).to_i32_round();

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
        Uniform::new("u_window_rect", (
            hole_x as f32, hole_y as f32,
            hole_w as f32, hole_h as f32,
        )),
        Uniform::new("u_radius", shadow_radius),
        Uniform::new("u_color", (
            sc[0] as f32 / 255.0, sc[1] as f32 / 255.0,
            sc[2] as f32 / 255.0, sc[3] as f32 / 255.0,
        )),
        Uniform::new("u_corner_radius", corner_logical as f32),
    ];

    let key: ShadowPhysKey = [
        body_post.loc.x, body_post.loc.y,
        body_post.loc.x + body_post.size.w, body_post.loc.y + body_post.size.h,
        shadow_post.loc.x, shadow_post.loc.y,
        shadow_post.size.w, shadow_post.size.h,
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
fn push_shadow_element(
    target: &mut Vec<OutputRenderElements>,
    cache: &mut std::collections::HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        crate::state::ShadowCacheEntry,
    >,
    surface_id: smithay::reexports::wayland_server::backend::ObjectId,
    shader: &smithay::backend::renderer::gles::GlesPixelProgram,
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
        body_pre_zoom, shadow_area, output_scale, zoom, shadow_radius, corner_r_phys,
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
    target.push(OutputRenderElements::Background(
        RescaleRenderElement::from_element(
            elem.clone(),
            Point::<i32, Physical>::from((0, 0)),
            zoom,
        ),
    ));
}

const CORNER_CLIP_SRC: &str = include_str!("../shaders/corner_clip.glsl");

pub const CORNER_CLIP_UNIFORMS: &[UniformName<'static>] = &[
    UniformName { name: Cow::Borrowed("aa_scale"), type_: UniformType::_1f },
    UniformName { name: Cow::Borrowed("geo_size"), type_: UniformType::_2f },
    UniformName { name: Cow::Borrowed("corner_radius"), type_: UniformType::_4f },
    UniformName { name: Cow::Borrowed("input_to_geo"), type_: UniformType::Matrix3x3 },
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

pub const TILE_BG_UNIFORMS: &[UniformName<'static>] = &[
    UniformName { name: Cow::Borrowed("u_camera"), type_: UniformType::_2f },
    UniformName { name: Cow::Borrowed("u_tile_size"), type_: UniformType::_2f },
    UniformName { name: Cow::Borrowed("u_output_size"), type_: UniformType::_2f },
];

pub fn compile_tile_bg_shader(renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
    match renderer.compile_custom_texture_shader(TILE_BG_SRC, TILE_BG_UNIFORMS) {
        Ok(shader) => Some(shader),
        Err(e) => {
            tracing::error!("Failed to compile tile background shader: {e}");
            None
        }
    }
}

const WALLPAPER_BG_SRC: &str = include_str!("../shaders/wallpaper_bg.glsl");

pub fn compile_wallpaper_bg_shader(renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
    match renderer.compile_custom_texture_shader(WALLPAPER_BG_SRC, &[]) {
        Ok(shader) => Some(shader),
        Err(e) => {
            tracing::error!("Failed to compile wallpaper background shader: {e}");
            None
        }
    }
}

/// Build render elements for canvas-positioned layer surfaces (zoomed like windows).
/// Mirrors the window pipeline: position relative to camera, then RescaleRenderElement for zoom.
pub fn build_canvas_layer_elements(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    camera: Point<f64, smithay::utils::Logical>,
    zoom: f64,
) -> Vec<OutputRenderElements> {
    let output_scale = output.current_scale().fractional_scale();
    let mut elements = Vec::new();

    for cl in &state.canvas_layers {
        let Some(pos) = cl.position else { continue; };
        // Camera-relative position (same as render_elements_for_region does for windows)
        let rel: Point<f64, Logical> = Point::from((
            pos.x as f64 - camera.x,
            pos.y as f64 - camera.y,
        ));
        let physical_loc = rel.to_physical_precise_round(output_scale);

        let surface_elements = cl
            .surface
            .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                renderer,
                physical_loc,
                smithay::utils::Scale::from(output_scale),
                1.0,
            );
        elements.extend(surface_elements.into_iter().map(|elem| {
            OutputRenderElements::Window(PixelSnapRescaleElement::from_element(
                elem,
                Point::<i32, Physical>::from((0, 0)),
                zoom,
            ))
        }));
    }

    elements
}

/// Build render elements for all layer surfaces on the given layer.
/// Layer surfaces are screen-fixed (not zoomed), so they use raw WaylandSurfaceRenderElement.
///
/// When `blur_config` is `Some`, layer surfaces whose `namespace()` matches a window rule
/// with `blur = true` will produce `BlurRequestData` entries alongside their render elements.
fn build_layer_elements(
    output: &Output,
    renderer: &mut GlesRenderer,
    layer: WlrLayer,
    blur_config: Option<(&driftwm::config::Config, bool, BlurLayer)>,
) -> (Vec<OutputRenderElements>, Vec<BlurRequestData>) {
    let map = layer_map_for_output(output);
    let output_scale = output.current_scale().fractional_scale();
    let mut elements = Vec::new();
    let mut blur_requests = Vec::new();

    for surface in map.layers_on(layer).rev() {
        let geo = map.layer_geometry(surface).unwrap_or_default();
        let loc = geo.loc.to_physical_precise_round(output_scale);

        let elem_start = elements.len();
        elements.extend(
            surface
                .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                    renderer,
                    loc,
                    smithay::utils::Scale::from(output_scale),
                    1.0,
                )
                .into_iter()
                .map(OutputRenderElements::Layer),
        );

        if let Some((config, blur_enabled, layer_tag)) = blur_config
            && blur_enabled
            && config.resolve_window_rules(surface.namespace(), "").is_some_and(|r| r.blur)
        {
            let elem_count = elements.len() - elem_start;
            let screen_rect = geo.to_physical_precise_round(output_scale);
            blur_requests.push(BlurRequestData {
                surface_id: surface.wl_surface().id(),
                screen_rect,
                elem_start,
                elem_count,
                layer: layer_tag,
            });
        }
    }

    (elements, blur_requests)
}

/// Resolve which xcursor name to load for the current cursor status.
/// Build the cursor render element(s) for the current frame.
/// `camera` and `zoom` are from the output being rendered.
/// Returns `OutputRenderElements` — either xcursor memory buffers or client surface elements.
pub fn build_cursor_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    camera: Point<f64, smithay::utils::Logical>,
    zoom: f64,
    scale: f64,
    alpha: f32,
) -> Vec<OutputRenderElements> {
    if alpha <= 0.0 {
        return vec![];
    }
    let pointer = state.seat.get_pointer().unwrap();
    let canvas_pos = pointer.current_location();
    let screen_pos = canvas_to_screen(CanvasPos(canvas_pos), camera, zoom).0;
    let physical_pos: Point<f64, Physical> = screen_pos.to_physical_precise_round(scale);

    // Separate the status check from mutable state access (Rust 2024 borrow rules)
    let status = state.cursor.cursor_status.clone();
    let mut result = match status {
        CursorImageStatus::Hidden => vec![],
        CursorImageStatus::Surface(ref surface) => {
            if !surface.alive() {
                state.cursor.cursor_status = CursorImageStatus::default_named();
                return build_xcursor_elements(state, renderer, physical_pos, "default", alpha);
            }
            let hotspot = with_states(surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .map(|d| d.lock().unwrap().hotspot)
                    .unwrap_or_default()
            });
            let pos: Point<i32, Physical> = (
                (physical_pos.x - hotspot.x as f64) as i32,
                (physical_pos.y - hotspot.y as f64) as i32,
            ).into();
            let elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                    renderer,
                    surface,
                    pos,
                    Scale::from(1.0),
                    alpha,
                    Kind::Cursor,
                );
            elems.into_iter().map(|e| OutputRenderElements::CursorSurface(e.into())).collect()
        }
        CursorImageStatus::Named(icon) => {
            build_xcursor_elements(state, renderer, physical_pos, icon.name(), alpha)
        }
    };

    // Render DnD icon at pointer position (separate from cursor — DnD icons
    // don't go through the wl_pointer.set_cursor path and lack hotspot data).
    let mut dnd_elems = Vec::new();
    if let Some(ref icon) = state.dnd_icon {
        if icon.alive() {
            let pos: Point<i32, Physical> = (
                physical_pos.x as i32,
                physical_pos.y as i32,
            ).into();
            let surface_elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                    renderer,
                    icon,
                    pos,
                    Scale::from(1.0),
                    alpha,
                    Kind::Cursor,
                );
            dnd_elems = surface_elems.into_iter().map(|e| OutputRenderElements::CursorSurface(e.into())).collect();
        }
    }

    result.extend(dnd_elems);
    result
}

/// Build xcursor memory buffer elements for a named cursor icon.
fn build_xcursor_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    physical_pos: Point<f64, Physical>,
    name: &'static str,
    alpha: f32,
) -> Vec<OutputRenderElements> {
    let loaded = state.load_xcursor(name).is_some();
    if !loaded && state.load_xcursor("default").is_none() {
        return vec![];
    }
    let key = if loaded { name } else { "default" };
    let cursor_frames = state.cursor.cursor_buffers.get(key).unwrap();

    // Select the active frame
    let frame_idx = if cursor_frames.total_duration_ms == 0 {
        0
    } else {
        let elapsed = state.start_time.elapsed().as_millis() as u32
            % cursor_frames.total_duration_ms;
        let mut acc = 0u32;
        let mut idx = 0;
        for (i, &(_, _, delay)) in cursor_frames.frames.iter().enumerate() {
            acc += delay;
            if elapsed < acc {
                idx = i;
                break;
            }
        }
        idx
    };

    let (buffer, hotspot, _) = &cursor_frames.frames[frame_idx];
    let hotspot = *hotspot;

    let pos = physical_pos - Point::from((hotspot.x as f64, hotspot.y as f64));
    match MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        pos,
        buffer,
        Some(alpha),
        None,
        None,
        Kind::Cursor,
    ) {
        Ok(elem) => vec![OutputRenderElements::Cursor(elem)],
        Err(_) => vec![],
    }
}

/// Update the cached background shader element for the current camera/zoom.
/// Returns (camera_moved, zoom_changed) for the caller's damage logic.
pub fn update_background_element(
    state: &mut crate::state::DriftWm,
    output: &Output,
    cur_camera: Point<f64, smithay::utils::Logical>,
    cur_zoom: f64,
    last_rendered_camera: Point<f64, smithay::utils::Logical>,
    last_rendered_zoom: f64,
) -> (bool, bool) {
    let camera_moved = cur_camera != last_rendered_camera;
    let zoom_changed = cur_zoom != last_rendered_zoom;
    let output_name = output.name();
    let output_size = crate::state::output_logical_size(output);
    let canvas_w = (output_size.w as f64 / cur_zoom).ceil() as i32;
    let canvas_h = (output_size.h as f64 / cur_zoom).ceil() as i32;
    let canvas_area = Rectangle::from_size((canvas_w, canvas_h).into());

    // Only push new uniforms when something the shader consumes has changed —
    // smithay's update_uniforms unconditionally bumps the element's CommitCounter,
    // which would otherwise cause the full-screen bg to damage every frame and
    // force re-composition of every element above it (blur especially).
    let animated = state.render.background_is_animated;
    let uniforms_stale = camera_moved || zoom_changed || animated;

    if let Some(elem) = state.render.cached_bg_elements.get_mut(&output_name) {
        elem.resize(canvas_area, Some(vec![canvas_area]));
        if uniforms_stale {
            let time_secs = state.start_time.elapsed().as_secs_f32();
            elem.update_uniforms(vec![
                Uniform::new("u_camera", (cur_camera.x as f32, cur_camera.y as f32)),
                Uniform::new("u_time", time_secs),
            ]);
        }
    } else if let Some(elem) = state.render.cached_tile_bg.get_mut(&output_name) {
        elem.resize(canvas_area, Some(vec![canvas_area]));
        if camera_moved || zoom_changed {
            elem.update_uniforms(vec![
                Uniform::new("u_camera", (cur_camera.x as f32, cur_camera.y as f32)),
                Uniform::new("u_tile_size", (elem.tex_w as f32, elem.tex_h as f32)),
                Uniform::new("u_output_size", (canvas_w as f32, canvas_h as f32)),
            ]);
        }
    } else if let Some(elem) = state.render.cached_wallpaper_bg.get_mut(&output_name) {
        // Viewport-fixed: size to the output (not the canvas), and never push uniforms.
        // Skipping update_uniforms keeps the CommitCounter stable across pans/zooms,
        // which is the whole point of wallpaper mode being cheaper than tile mode —
        // blur and elements above don't get damaged for background reasons.
        let output_area = Rectangle::from_size(output_size);
        elem.resize(output_area, Some(vec![output_area]));
    }
    (camera_moved, zoom_changed)
}

/// Build render elements for a locked session: only the lock surface.
/// No compositor cursor — the lock client manages its own visuals.
fn compose_lock_frame(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    _cursor_elements: Vec<OutputRenderElements>,
) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();

    if let Some(lock_surface) = state.lock_surfaces.get(output) {
        let output_scale = output.current_scale().fractional_scale();
        let lock_elements = smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
            renderer,
            lock_surface.wl_surface(),
            (0, 0),
            Scale::from(output_scale),
            1.0,
            Kind::Unspecified,
        );
        elements.extend(lock_elements.into_iter().map(OutputRenderElements::Layer));
    }

    elements
}

/// Wrap every surface element of a window in the corner-clip shader and push
/// into `target`. The clip applies uniformly to the root toplevel and every
/// subsurface, so clients that render content via subsurfaces (Firefox
/// dmabuf, HW-accelerated video) get rounded corners the same as simple
/// single-surface clients.
///
/// `geometry` is the window's geometry rect in screen-logical pre-zoom
/// coords — i.e. where the content rect ends up on the output before zoom.
/// Pixels outside this rect are discarded by the shader, which doubles as
/// the CSD-shadow strip mask the old `u_clip_shadow` uniform used to do.
///
/// `corner_radius` is per-corner in pre-zoom physical pixels, ordered
/// `(top_left, top_right, bottom_right, bottom_left)`. Pass `0` on any
/// corner that should stay square (e.g. top corners under an SSD title
/// bar).
#[allow(clippy::too_many_arguments)]
fn push_corner_clipped_elements(
    target: &mut Vec<OutputRenderElements>,
    elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    shader: &GlesTexProgram,
    geometry: Rectangle<f64, Logical>,
    corner_radius: [f32; 4],
    zoom: f64,
    output_scale: f64,
) {
    let aa_scale = (output_scale * zoom) as f32;
    // Clamp radii so a tiny window doesn't get corners wider than half its
    // side. `max_r` is guarded against ≤0 since a degenerate window can
    // briefly have zero size and `clamp(lo, hi)` panics if `lo > hi`.
    let max_r = ((geometry.size.w.min(geometry.size.h) as f32) * 0.5).max(0.0);
    let clamped = [
        corner_radius[0].clamp(0.0, max_r),
        corner_radius[1].clamp(0.0, max_r),
        corner_radius[2].clamp(0.0, max_r),
        corner_radius[3].clamp(0.0, max_r),
    ];
    for elem in elems {
        target.push(OutputRenderElements::CsdWindow(PixelSnapRescaleElement::from_element(
            RoundedCornerElement::new(
                elem,
                shader.clone(),
                geometry,
                clamped,
                output_scale,
                aa_scale,
            ),
            Point::<i32, Physical>::from((0, 0)),
            zoom,
        )));
    }
}

/// Push window surface elements as plain (no corner clip) zoomed Window elements.
fn push_plain_elements(
    target: &mut Vec<OutputRenderElements>,
    elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    zoom: f64,
) {
    target.extend(elems.into_iter().map(|elem| {
        OutputRenderElements::Window(PixelSnapRescaleElement::from_element(
            elem,
            Point::<i32, Physical>::from((0, 0)),
            zoom,
        ))
    }));
}

/// Assemble all render elements for a frame.
/// Caller provides cursor elements (built before taking the renderer).
pub fn compose_frame(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    cursor_elements: Vec<OutputRenderElements>,
) -> Vec<OutputRenderElements> {
    // Session lock: render only lock surface (or black) + cursor
    if !matches!(state.session_lock, crate::state::SessionLock::Unlocked) {
        return compose_lock_frame(state, renderer, output, cursor_elements);
    }

    // Ensure this output has a background element (lazy init per output, and re-init after config reload)
    let name = output.name();
    if !state.render.cached_bg_elements.contains_key(&name)
        && !state.render.cached_tile_bg.contains_key(&name)
        && !state.render.cached_wallpaper_bg.contains_key(&name)
    {
        let output_size = crate::state::output_logical_size(output);
        init_background(state, renderer, output_size, &name);
    }

    // Read per-output state directly — not via active_output() which follows the pointer
    let (camera, zoom) = {
        let os = crate::state::output_state(output);
        (os.camera, os.zoom)
    };

    let viewport_size = crate::state::output_logical_size(output);
    let visible_rect = canvas::visible_canvas_rect(
        camera.to_i32_round(),
        viewport_size,
        zoom,
    );
    let output_scale = output.current_scale().fractional_scale();
    let scale = Scale::from(output_scale);

    // Split windows into normal and widget layers so canvas layers render between them.
    // Replicates render_elements_for_region internals: bbox overlap, camera offset, zoom.
    let mut zoomed_normal: Vec<OutputRenderElements> = Vec::new();
    let mut zoomed_widgets: Vec<OutputRenderElements> = Vec::new();

    let blur_enabled = state.render.blur_down_shader.is_some() && state.render.blur_up_shader.is_some() && state.render.blur_mask_shader.is_some();
    let mut blur_requests: Vec<BlurRequestData> = Vec::new();

    // Focused surface for decoration focus state
    let focused_surface = state
        .seat
        .get_keyboard()
        .and_then(|kb| kb.current_focus())
        .map(|f| f.0);

    for window in state.space.elements().rev() {
        let Some(loc) = state.space.element_location(window) else { continue };
        let geom_loc = window.geometry().loc;
        let geom_size = window.geometry().size;
        let Some(wl_surface) = window.wl_surface() else { continue; };
        let is_fullscreen = state.fullscreen.values().any(|fs| &fs.window == window);
        let has_ssd = !is_fullscreen && state.decorations.contains_key(&wl_surface.id());

        let mut bbox = window.bbox();
        bbox.loc += loc - geom_loc;
        if has_ssd {
            let r = driftwm::config::DecorationConfig::SHADOW_RADIUS.ceil() as i32;
            let bar = driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT;
            bbox.loc.x -= r;
            bbox.loc.y -= bar + r;
            bbox.size.w += 2 * r;
            bbox.size.h += bar + 2 * r;
        }
        if !visible_rect.overlaps(bbox) { continue }

        let render_loc: Point<f64, Logical> = Point::from((
            loc.x as f64 - geom_loc.x as f64 - camera.x,
            loc.y as f64 - geom_loc.y as f64 - camera.y,
        ));
        let applied = driftwm::config::applied_rule(&wl_surface);
        let is_widget = applied.as_ref().is_some_and(|r| r.widget);
        let wants_blur = blur_enabled && applied.as_ref().is_some_and(|r| r.blur);
        let opacity = applied.as_ref().and_then(|r| r.opacity).unwrap_or(1.0);

        // Split elements: toplevel + subsurfaces get corner-clipped, popups
        // don't (they can legitimately extend outside the parent's geometry —
        // GTK menus, tooltips, autocomplete, etc). smithay's
        // `Window::render_elements` bundles popups into one vec, which is why
        // we can't use it directly for Wayland.
        let loc_phys: Point<i32, Physical> = render_loc.to_physical_precise_round(scale);
        let (elems, popup_elems) = if let Some(toplevel) = window.toplevel() {
            let root = toplevel.wl_surface();
            let top = smithay::backend::renderer::element::surface::render_elements_from_surface_tree::<
                _, WaylandSurfaceRenderElement<GlesRenderer>,
            >(renderer, root, loc_phys, scale, opacity as f32, Kind::Unspecified);

            let mut popups: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
            for (popup, popup_offset) in smithay::desktop::PopupManager::popups_for_surface(root) {
                let offset: Point<i32, Physical> = (window.geometry().loc + popup_offset - popup.geometry().loc)
                    .to_physical_precise_round(scale);
                popups.extend(smithay::backend::renderer::element::surface::render_elements_from_surface_tree::<
                    _, WaylandSurfaceRenderElement<GlesRenderer>,
                >(renderer, popup.wl_surface(), loc_phys + offset, scale, opacity as f32, Kind::Unspecified));
            }
            (top, popups)
        } else {
            // No toplevel — render the window's surface tree directly.
            let elems = window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                renderer, loc_phys, scale, opacity as f32,
            );
            (elems, Vec::new())
        };

        let target = if is_widget { &mut zoomed_widgets } else { &mut zoomed_normal };
        let elem_start = target.len();
        let mut shadow_count = 0usize;

        // Popups push FIRST (earlier-in-vec = on-top in smithay z-order),
        // so they sit above the title bar and the clipped window content.
        push_plain_elements(target, popup_elems, zoom);

        if has_ssd {
            let bar_height = driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT;
            let is_focused = focused_surface.as_ref().is_some_and(|f| *f == *wl_surface);

            // Update decoration state (re-render title bar if needed)
            if let Some(deco) = state.decorations.get_mut(&wl_surface.id()) {
                deco.update(geom_size.w, is_focused, &state.config.decorations);
            }

            // Title bar element: positioned above the window
            if let Some(deco) = state.decorations.get(&wl_surface.id()) {
                let bar_loc: Point<f64, Logical> = Point::from((
                    render_loc.x,
                    render_loc.y - bar_height as f64,
                ));
                let bar_physical: Point<f64, Physical> = bar_loc.to_physical_precise_round(scale);
                let bar_alpha = if opacity < 1.0 { Some(opacity as f32) } else { None };
                if let Ok(bar_elem) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    bar_physical,
                    &deco.title_bar,
                    bar_alpha,
                    None,
                    None,
                    Kind::Unspecified,
                ) {
                    target.push(OutputRenderElements::Decoration(
                        PixelSnapRescaleElement::from_element(
                            bar_elem,
                            Point::<i32, Physical>::from((0, 0)),
                            zoom,
                        ),
                    ));
                }
            }

            // Window surface elements — only the bottom corners round
            // (the title bar covers the top edge).
            if let Some(ref shader) = state.render.corner_clip_shader {
                let radius = state.config.decorations.corner_radius as f32;
                if radius > 0.0 {
                    let wg = window.geometry();
                    let geometry = Rectangle::new(
                        Point::<f64, Logical>::from((
                            render_loc.x + wg.loc.x as f64,
                            render_loc.y + wg.loc.y as f64,
                        )),
                        Size::<f64, Logical>::from((wg.size.w as f64, wg.size.h as f64)),
                    );
                    push_corner_clipped_elements(
                        target, elems, shader,
                        geometry, [0.0, 0.0, radius, radius], zoom, output_scale,
                    );
                } else {
                    push_plain_elements(target, elems, zoom);
                }
            } else {
                push_plain_elements(target, elems, zoom);
            }

            // Shadow encloses title bar + content; cached per-surface so the
            // damage tracker can skip unchanged regions across frames.
            if let Some(shader) = state.render.shadow_shader.clone() {
                let body_logical: Rectangle<f64, Logical> = Rectangle::new(
                    (render_loc.x, render_loc.y - bar_height as f64).into(),
                    (geom_size.w as f64, (geom_size.h + bar_height) as f64).into(),
                );
                push_shadow_element(
                    target,
                    &mut state.render.shadow_cache,
                    wl_surface.id(),
                    &shader,
                    body_logical,
                    state.config.decorations.corner_radius as f32,
                    opacity,
                    scale,
                    zoom,
                );
                shadow_count = 1;
            }
        } else if let Some(ref shader) = state.render.corner_clip_shader {
            let geo = window.geometry();
            let radius = state.config.decorations.corner_radius as f32;

            // Only `None` mode opts out of shadow + corner clipping.
            // Client (CSD), Borderless, and untagged windows all get the chrome —
            // any `Server` window would have taken the `has_ssd` branch above.
            let effective = driftwm::config::effective_decoration_mode(
                applied.as_ref().and_then(|r| r.decoration.as_ref()),
                &state.config.decorations.default_mode,
            );
            let bare = matches!(effective, driftwm::config::DecorationMode::None);

            if !bare && !is_fullscreen {
                // Clip pixels outside the geometry rect even when radius=0,
                // so a CSD client's own shadow (drawn in a subsurface beyond
                // geometry) doesn't stack under our compositor shadow and
                // double it up.
                let geometry = Rectangle::new(
                    Point::<f64, Logical>::from((
                        render_loc.x + geo.loc.x as f64,
                        render_loc.y + geo.loc.y as f64,
                    )),
                    Size::<f64, Logical>::from((geo.size.w as f64, geo.size.h as f64)),
                );
                push_corner_clipped_elements(
                    target, elems, shader,
                    geometry, [radius, radius, radius, radius], zoom, output_scale,
                );

                // Compositor shadow behind CSD windows.
                if let Some(shader) = state.render.shadow_shader.clone() {
                    let body_logical: Rectangle<f64, Logical> = Rectangle::new(
                        (render_loc.x + geo.loc.x as f64, render_loc.y + geo.loc.y as f64).into(),
                        (geom_size.w as f64, geom_size.h as f64).into(),
                    );
                    push_shadow_element(
                        target,
                        &mut state.render.shadow_cache,
                        wl_surface.id(),
                        &shader,
                        body_logical,
                        state.config.decorations.corner_radius as f32,
                        opacity,
                        scale,
                        zoom,
                    );
                    shadow_count = 1;
                }
            } else {
                push_plain_elements(target, elems, zoom);
            }
        } else {
            push_plain_elements(target, elems, zoom);
        }

        if wants_blur {
            let elem_count = target.len() - elem_start - shadow_count;
            let screen_loc: Point<i32, Logical> = Point::from((
                (render_loc.x * zoom) as i32,
                (render_loc.y * zoom) as i32,
            ));
            let screen_size: Size<i32, Logical> = if has_ssd {
                let bar = driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT;
                (
                    (geom_size.w as f64 * zoom).ceil() as i32,
                    ((geom_size.h + bar) as f64 * zoom).ceil() as i32,
                ).into()
            } else {
                (
                    (geom_size.w as f64 * zoom).ceil() as i32,
                    (geom_size.h as f64 * zoom).ceil() as i32,
                ).into()
            };
            let screen_rect = Rectangle::new(
                if has_ssd {
                    Point::from((
                        screen_loc.x,
                        screen_loc.y - (driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT as f64 * zoom) as i32,
                    ))
                } else {
                    // CSD windows: geometry starts at render_loc + geo.loc, not at render_loc
                    let geo = window.geometry();
                    Point::from((
                        ((render_loc.x + geo.loc.x as f64) * zoom) as i32,
                        ((render_loc.y + geo.loc.y as f64) * zoom) as i32,
                    ))
                },
                screen_size,
            ).to_physical_precise_round(output_scale);
            blur_requests.push(BlurRequestData {
                surface_id: wl_surface.id(),
                screen_rect,
                elem_start,
                elem_count,
                layer: if is_widget { BlurLayer::Widget } else { BlurLayer::Normal },
            });
        }
    }

    let canvas_layer_elements = build_canvas_layer_elements(state, renderer, output, camera, zoom);

    let outline_elements = build_output_outline_elements(
        state, renderer, output, camera, zoom, viewport_size,
    );

    let bg_elements: Vec<OutputRenderElements> =
        if let Some(elem) = state.render.cached_bg_elements.get(&output.name()) {
            vec![OutputRenderElements::Background(
                RescaleRenderElement::from_element(
                    elem.clone(),
                    Point::<i32, Physical>::from((0, 0)),
                    zoom,
                ),
            )]
        } else if let Some(elem) = state.render.cached_tile_bg.get(&output.name()) {
            vec![OutputRenderElements::TileBg(
                RescaleRenderElement::from_element(
                    elem.clone(),
                    Point::<i32, Physical>::from((0, 0)),
                    zoom,
                ),
            )]
        } else if let Some(elem) = state.render.cached_wallpaper_bg.get(&output.name()) {
            // Viewport-fixed: no zoom rescale, element area is already in output coords.
            vec![OutputRenderElements::WallpaperBg(elem.clone())]
        } else {
            vec![]
        };

    let is_fullscreen = state.is_output_fullscreen(output);
    let (overlay_elements, overlay_blur) = build_layer_elements(
        output, renderer, WlrLayer::Overlay,
        Some((&state.config, blur_enabled, BlurLayer::Overlay)),
    );
    let (top_elements, top_blur) = if !is_fullscreen {
        build_layer_elements(
            output, renderer, WlrLayer::Top,
            Some((&state.config, blur_enabled, BlurLayer::Top)),
        )
    } else {
        (vec![], vec![])
    };
    let (bottom_elements, _) = if !is_fullscreen {
        build_layer_elements(output, renderer, WlrLayer::Bottom, None)
    } else {
        (vec![], vec![])
    };
    let (background_layer_elements, _) = build_layer_elements(output, renderer, WlrLayer::Background, None);

    // Compute prefix offsets so we know where each group lands in all_elements
    let overlay_prefix = cursor_elements.len();
    let top_prefix = overlay_prefix + overlay_elements.len();
    let normal_prefix = top_prefix + top_elements.len();
    let widget_prefix = normal_prefix
        + zoomed_normal.len()
        + canvas_layer_elements.len();

    // Merge blur requests: layer surfaces first (front-to-back), then windows
    let mut all_blur_requests: Vec<BlurRequestData> = Vec::new();
    all_blur_requests.extend(overlay_blur);
    all_blur_requests.extend(top_blur);
    all_blur_requests.extend(blur_requests);

    let mut all_elements: Vec<OutputRenderElements> = Vec::with_capacity(
        cursor_elements.len()
            + overlay_elements.len()
            + top_elements.len()
            + zoomed_normal.len()
            + canvas_layer_elements.len()
            + zoomed_widgets.len()
            + bottom_elements.len()
            + outline_elements.len()
            + bg_elements.len()
            + background_layer_elements.len(),
    );
    all_elements.extend(cursor_elements);
    all_elements.extend(overlay_elements);
    all_elements.extend(top_elements);
    all_elements.extend(zoomed_normal);
    all_elements.extend(canvas_layer_elements);
    all_elements.extend(zoomed_widgets);
    all_elements.extend(bottom_elements);
    all_elements.extend(outline_elements);
    all_elements.extend(bg_elements);
    all_elements.extend(background_layer_elements);

    // Process blur requests: render behind-content, blur, insert
    if !all_blur_requests.is_empty() {
        process_blur_requests(
            state, renderer, output, output_scale,
            &mut all_elements, &all_blur_requests,
            overlay_prefix, top_prefix, normal_prefix, widget_prefix,
        );
    }

    // Prune stale blur cache entries
    if blur_enabled {
        let active_ids: std::collections::HashSet<_> =
            all_blur_requests.iter().map(|r| r.surface_id.clone()).collect();
        state.render.blur_cache.retain(|id, _| active_ids.contains(id));
    }

    all_elements
}

/// Draw thin outlines showing where other monitors' viewports sit on the canvas.
fn build_output_outline_elements(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    camera: Point<f64, Logical>,
    zoom: f64,
    viewport_size: Size<i32, Logical>,
) -> Vec<OutputRenderElements> {
    let thickness = state.config.output_outline.thickness;
    if thickness <= 0 { return vec![]; }

    let opacity = state.config.output_outline.opacity as f32;
    if opacity <= 0.0 { return vec![]; }
    let color = state.config.output_outline.color;
    let scale = output.current_scale().fractional_scale();

    let mut elements = Vec::new();

    for other in state.space.outputs() {
        if *other == *output { continue }

        let (other_camera, other_zoom) = {
            let os = crate::state::output_state(other);
            (os.camera, os.zoom)
        };
        let other_size = crate::state::output_logical_size(other);

        // Other output's visible canvas rect
        let other_canvas = canvas::visible_canvas_rect(
            other_camera.to_i32_round(),
            other_size,
            other_zoom,
        );

        // Transform to screen coords on *this* output
        let screen_x = ((other_canvas.loc.x as f64 - camera.x) * zoom) as i32;
        let screen_y = ((other_canvas.loc.y as f64 - camera.y) * zoom) as i32;
        let screen_w = (other_canvas.size.w as f64 * zoom) as i32;
        let screen_h = (other_canvas.size.h as f64 * zoom) as i32;

        // Clip to viewport
        let vp = Rectangle::from_size(viewport_size);
        let outline_rect = Rectangle::new((screen_x, screen_y).into(), (screen_w, screen_h).into());
        if !vp.overlaps(outline_rect) { continue }

        // Draw 4 edges as thin filled buffers
        let edges: [(i32, i32, i32, i32); 4] = [
            (screen_x, screen_y, screen_w, thickness),                         // top
            (screen_x, screen_y + screen_h - thickness, screen_w, thickness),  // bottom
            (screen_x, screen_y, thickness, screen_h),                         // left
            (screen_x + screen_w - thickness, screen_y, thickness, screen_h),  // right
        ];

        for (ex, ey, ew, eh) in edges {
            // Clip edge to viewport
            let x0 = ex.max(0);
            let y0 = ey.max(0);
            let x1 = (ex + ew).min(viewport_size.w);
            let y1 = (ey + eh).min(viewport_size.h);
            if x1 <= x0 || y1 <= y0 { continue }

            let w = x1 - x0;
            let h = y1 - y0;

            let pixels: Vec<u8> = vec![color[0], color[1], color[2], color[3]]
                .into_iter()
                .cycle()
                .take((w * h) as usize * 4)
                .collect();

            let buf = MemoryRenderBuffer::from_slice(
                &pixels,
                Fourcc::Abgr8888,
                (w, h),
                1,
                Transform::Normal,
                None,
            );

            let loc: Point<f64, Physical> = Point::from((x0, y0)).to_f64().to_physical(scale);
            if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
                renderer, loc, &buf, Some(opacity), None, None, Kind::Unspecified,
            ) {
                elements.push(OutputRenderElements::Decoration(
                    PixelSnapRescaleElement::from_element(
                        elem,
                        Point::<i32, Physical>::from((0, 0)),
                        1.0,
                    ),
                ));
            }
        }
    }

    elements
}

/// Compile background shader and/or load tile/wallpaper image.
/// Called at startup and on config reload (lazy re-init).
/// On failure, falls back to `DEFAULT_SHADER` — never leaves background uninitialized.
pub fn init_background(state: &mut crate::state::DriftWm, renderer: &mut GlesRenderer, initial_size: Size<i32, smithay::utils::Logical>, output_name: &str) {
    // Each branch is responsible for leaving `background_is_animated` correct:
    //   - texture branches (tile/wallpaper) set it false on success.
    //   - shader branch re-derives it on cache miss; on cache hit (second
    //     output hotplug with the same shader) the flag is already correct
    //     from the first init.
    // Resetting eagerly here would clobber that flag on second-monitor cache
    // hits and silently freeze animated shaders on the new output.
    let texture_init: Option<bool> = match state.config.background.kind.clone() {
        BackgroundKind::Tile(path) => Some(try_init_texture_bg(
            state, renderer, initial_size, output_name, &path, TextureBgMode::Tile,
        )),
        BackgroundKind::Wallpaper(path) => Some(try_init_texture_bg(
            state, renderer, initial_size, output_name, &path, TextureBgMode::Wallpaper,
        )),
        BackgroundKind::Shader(_) | BackgroundKind::Default => None,
    };

    if texture_init != Some(true) {
        init_shader_bg(state, renderer, initial_size, output_name);
    }
}

#[derive(Copy, Clone)]
enum TextureBgMode {
    Tile,
    Wallpaper,
}

/// Returns true on success. On failure, the caller falls back to shader mode.
fn try_init_texture_bg(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    initial_size: Size<i32, smithay::utils::Logical>,
    output_name: &str,
    path: &str,
    mode: TextureBgMode,
) -> bool {
    let Some((texture, w, h)) = load_image_to_texture(renderer, path) else {
        return false;
    };

    let shader_slot = match mode {
        TextureBgMode::Tile => &mut state.render.tile_shader,
        TextureBgMode::Wallpaper => &mut state.render.wallpaper_shader,
    };
    if shader_slot.is_none() {
        *shader_slot = match mode {
            TextureBgMode::Tile => compile_tile_bg_shader(renderer),
            TextureBgMode::Wallpaper => compile_wallpaper_bg_shader(renderer),
        };
    }
    let Some(shader) = shader_slot.clone() else {
        tracing::error!("{:?} shader compilation failed, using default shader", match mode {
            TextureBgMode::Tile => "Tile",
            TextureBgMode::Wallpaper => "Wallpaper",
        });
        return false;
    };

    let area = Rectangle::from_size(initial_size);
    let uniforms = match mode {
        TextureBgMode::Tile => vec![
            Uniform::new("u_camera", (0.0f32, 0.0f32)),
            Uniform::new("u_tile_size", (w as f32, h as f32)),
            Uniform::new("u_output_size", (initial_size.w as f32, initial_size.h as f32)),
        ],
        // Wallpaper shader has no camera/zoom/time uniforms — image stretches to v_coords [0,1].
        TextureBgMode::Wallpaper => vec![],
    };
    let elem = TileShaderElement::new(
        shader,
        texture,
        w,
        h,
        area,
        Some(vec![area]),
        1.0,
        uniforms,
        Kind::Unspecified,
    );
    let target = match mode {
        TextureBgMode::Tile => &mut state.render.cached_tile_bg,
        TextureBgMode::Wallpaper => &mut state.render.cached_wallpaper_bg,
    };
    target.insert(output_name.to_string(), elem);
    // Tile/wallpaper modes have no per-frame uniform updates, so the
    // animated-flag must be false. (A stale `true` from a prior animated
    // shader would force every-frame redraws and defeat damage savings.)
    state.render.background_is_animated = false;
    true
}

fn load_image_to_texture(
    renderer: &mut GlesRenderer,
    path: &str,
) -> Option<(GlesTexture, i32, i32)> {
    use smithay::backend::renderer::ImportMem;
    use smithay::utils::Buffer;

    let img = match image::open(path) {
        Ok(img) => img.into_rgba8(),
        Err(e) => {
            tracing::error!("Failed to load image {path}: {e}, using default shader");
            return None;
        }
    };
    let (w, h) = img.dimensions();
    let raw = img.into_raw();
    match renderer.import_memory(
        &raw,
        Fourcc::Abgr8888,
        Size::<i32, Buffer>::from((w as i32, h as i32)),
        false,
    ) {
        Ok(texture) => Some((texture, w as i32, h as i32)),
        Err(e) => {
            tracing::error!("Failed to upload texture from {path}: {e}, using default shader");
            None
        }
    }
}

fn init_shader_bg(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    initial_size: Size<i32, smithay::utils::Logical>,
    output_name: &str,
) {
    // Reuse cached shader if already compiled (avoids redundant GPU work
    // when multiple outputs each need a background element).
    let shader = if let Some(ref cached) = state.render.background_shader {
        cached.clone()
    } else {
        let shader_source = match &state.config.background.kind {
            BackgroundKind::Shader(path) => match std::fs::read_to_string(path) {
                Ok(src) => src,
                Err(e) => {
                    tracing::error!("Failed to read shader {path}: {e}, using default");
                    driftwm::config::DEFAULT_SHADER.to_string()
                }
            },
            _ => driftwm::config::DEFAULT_SHADER.to_string(),
        };

        let compiled = match renderer.compile_custom_pixel_shader(&shader_source, BG_UNIFORMS) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to compile shader: {e}, using default");
                renderer
                    .compile_custom_pixel_shader(driftwm::config::DEFAULT_SHADER, BG_UNIFORMS)
                    .expect("Default shader must compile")
            }
        };

        state.render.background_is_animated = shader_source.contains("uniform float u_time");
        state.render.background_shader = Some(compiled.clone());
        compiled
    };

    let area = Rectangle::from_size(initial_size);
    let time_secs = state.start_time.elapsed().as_secs_f32();
    state.render.cached_bg_elements.insert(output_name.to_string(), PixelShaderElement::new(
        shader,
        area,
        Some(vec![area]),
        1.0,
        vec![
            Uniform::new("u_camera", (0.0f32, 0.0f32)),
            Uniform::new("u_time", time_secs),
        ],
        Kind::Unspecified,
    ));
}

/// Sync foreign-toplevel protocol state with the current window list.
/// Call once per frame iteration (not per-output).
pub fn refresh_foreign_toplevels(state: &mut crate::state::DriftWm) {
    let keyboard = state.seat.get_keyboard().unwrap();
    let focused = keyboard.current_focus().map(|f| f.0);
    let outputs: Vec<Output> = state.space.outputs().cloned().collect();
    driftwm::protocols::foreign_toplevel::refresh::<crate::state::DriftWm>(
        &mut state.foreign_toplevel_state,
        &state.space,
        focused.as_ref(),
        &outputs,
    );
}

/// Per-surface throttling state for frame callbacks. Tracks the (output,
/// sequence) at which we last delivered a frame callback. A client that
/// commits a fresh frame within the same vsync cycle does not get another
/// callback — without this, a vsync-ignoring client (e.g. some Wine games)
/// can busy-loop the compositor: commit -> we render -> we send callback ->
/// client commits immediately -> we render again, ad infinitum.
struct SurfaceFrameThrottlingState {
    last_sent_at: std::cell::RefCell<Option<(Output, u32)>>,
}

impl Default for SurfaceFrameThrottlingState {
    fn default() -> Self {
        Self { last_sent_at: std::cell::RefCell::new(None) }
    }
}

fn frame_callback_filter<'a>(
    output: &'a Output,
    sequence: u32,
) -> impl FnMut(
    &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    &smithay::wayland::compositor::SurfaceData,
) -> Option<Output> + Copy + 'a {
    move |_surface, states| {
        let throttling = states
            .data_map
            .get_or_insert(SurfaceFrameThrottlingState::default);
        let mut last = throttling.last_sent_at.borrow_mut();
        if let Some((last_output, last_sequence)) = &*last
            && last_output == output
            && *last_sequence == sequence
        {
            return None;
        }
        *last = Some((output.clone(), sequence));
        Some(output.clone())
    }
}

/// Update each visible surface's primary-scanout-output to `output`. Smithay
/// uses this to decide where to deliver presentation feedback. Must be called
/// after `compositor.render_frame()` so we have render-element states.
pub fn update_primary_scanout_output(
    state: &crate::state::DriftWm,
    output: &Output,
    states: &smithay::backend::renderer::element::RenderElementStates,
) {
    use smithay::desktop::utils::update_surface_primary_scanout_output;
    use smithay::wayland::compositor::with_surface_tree_downward;
    use smithay::wayland::compositor::TraversalAction;

    for window in state.space.elements() {
        window.with_surfaces(|surface, surface_data| {
            update_surface_primary_scanout_output(
                surface,
                output,
                surface_data,
                None,
                states,
                smithay::backend::renderer::element::default_primary_scanout_output_compare,
            );
        });
    }

    let layer_map = layer_map_for_output(output);
    for layer_surface in layer_map.layers() {
        layer_surface.with_surfaces(|surface, surface_data| {
            update_surface_primary_scanout_output(
                surface,
                output,
                surface_data,
                None,
                states,
                smithay::backend::renderer::element::default_primary_scanout_output_compare,
            );
        });
    }
    drop(layer_map);

    for cl in &state.canvas_layers {
        with_surface_tree_downward(
            cl.surface.wl_surface(),
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, surface_data, _| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    surface_data,
                    None,
                    states,
                    smithay::backend::renderer::element::default_primary_scanout_output_compare,
                );
            },
            |_, _, _| true,
        );
    }

    if let Some(lock_surface) = state.lock_surfaces.get(output) {
        with_surface_tree_downward(
            lock_surface.wl_surface(),
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, surface_data, _| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    surface_data,
                    None,
                    states,
                    smithay::backend::renderer::element::default_primary_scanout_output_compare,
                );
            },
            |_, _, _| true,
        );
    }

}

/// Collect presentation-feedback callbacks from all surfaces visible on `output`.
/// Hand the result to `compositor.queue_frame()` and let `frame_submitted()`
/// return it to be consumed by `presented()` on VBlank.
pub fn take_presentation_feedback(
    state: &crate::state::DriftWm,
    output: &Output,
    states: &smithay::backend::renderer::element::RenderElementStates,
) -> smithay::desktop::utils::OutputPresentationFeedback {
    use smithay::desktop::utils::{
        OutputPresentationFeedback, surface_presentation_feedback_flags_from_states,
        surface_primary_scanout_output, take_presentation_feedback_surface_tree,
    };

    let mut feedback = OutputPresentationFeedback::new(output);

    for window in state.space.elements() {
        window.take_presentation_feedback(
            &mut feedback,
            surface_primary_scanout_output,
            |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
        );
    }

    let layer_map = layer_map_for_output(output);
    for layer_surface in layer_map.layers() {
        layer_surface.take_presentation_feedback(
            &mut feedback,
            surface_primary_scanout_output,
            |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
        );
    }
    drop(layer_map);

    for cl in &state.canvas_layers {
        take_presentation_feedback_surface_tree(
            cl.surface.wl_surface(),
            &mut feedback,
            surface_primary_scanout_output,
            |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
        );
    }

    if let Some(lock_surface) = state.lock_surfaces.get(output) {
        take_presentation_feedback_surface_tree(
            lock_surface.wl_surface(),
            &mut feedback,
            surface_primary_scanout_output,
            |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
        );
    }

    feedback
}

/// Post-render: frame callbacks, space cleanup.
pub fn post_render(state: &mut crate::state::DriftWm, output: &Output) {
    let time = state.start_time.elapsed();
    let sequence = crate::state::output_state(output).frame_callback_sequence;

    // Only send frame callbacks to visible windows — off-screen clients
    // naturally throttle to zero FPS without callbacks.
    let (camera, zoom) = {
        let os = crate::state::output_state(output);
        (os.camera, os.zoom)
    };
    let viewport_size = crate::state::output_logical_size(output);
    let visible_rect = canvas::visible_canvas_rect(
        camera.to_i32_round(),
        viewport_size,
        zoom,
    );

    for window in state.space.elements() {
        let Some(loc) = state.space.element_location(window) else { continue };
        let geom_loc = window.geometry().loc;
        let mut bbox = window.bbox();
        bbox.loc += loc - geom_loc;
        if !visible_rect.overlaps(bbox) { continue }

        window.send_frame(output, time, Some(Duration::ZERO), frame_callback_filter(output, sequence));
    }

    // Layer surface frame callbacks
    {
        let layer_map = layer_map_for_output(output);
        for layer_surface in layer_map.layers() {
            layer_surface.send_frame(output, time, Some(Duration::ZERO), frame_callback_filter(output, sequence));
        }
    }

    // Canvas-positioned layer surface frame callbacks
    for cl in &state.canvas_layers {
        cl.surface.send_frame(output, time, Some(Duration::ZERO), frame_callback_filter(output, sequence));
    }

    // Cursor surface frame callbacks (animated cursors need these to advance)
    if let CursorImageStatus::Surface(ref surface) = state.cursor.cursor_status {
        smithay::desktop::utils::send_frames_surface_tree(
            surface, output, time, Some(Duration::ZERO),
            frame_callback_filter(output, sequence),
        );
    }

    // Lock surface frame callback
    if let Some(lock_surface) = state.lock_surfaces.get(output) {
        smithay::desktop::utils::send_frames_surface_tree(
            lock_surface.wl_surface(),
            output,
            time,
            Some(Duration::ZERO),
            frame_callback_filter(output, sequence),
        );
    }

    // Cleanup
    state.space.refresh();
    state.popups.cleanup();
    layer_map_for_output(output).cleanup();
}
