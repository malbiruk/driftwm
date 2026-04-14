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
        gles::{GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType, element::PixelShaderElement},
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

const CORNER_CLIP_SRC: &str = include_str!("../shaders/corner_clip.glsl");

pub const CORNER_CLIP_UNIFORMS: &[UniformName<'static>] = &[
    UniformName { name: Cow::Borrowed("u_size"), type_: UniformType::_2f },
    UniformName { name: Cow::Borrowed("u_geo"), type_: UniformType::_4f },
    UniformName { name: Cow::Borrowed("u_radius"), type_: UniformType::_1f },
    UniformName { name: Cow::Borrowed("u_clip_top"), type_: UniformType::_1f },
    UniformName { name: Cow::Borrowed("u_clip_shadow"), type_: UniformType::_1f },
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

/// Build render elements for X11 override-redirect windows (menus, tooltips, splashes).
/// Same camera/zoom math as managed windows.
fn build_override_redirect_elements(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    camera: Point<f64, Logical>,
    zoom: f64,
) -> Vec<OutputRenderElements> {
    let output_scale = output.current_scale().fractional_scale();
    let scale = Scale::from(output_scale);
    let viewport_size = crate::state::output_logical_size(output);
    let visible_rect = canvas::visible_canvas_rect(camera.to_i32_round(), viewport_size, zoom);

    let mut elements = Vec::new();
    // Reverse: newest OR window = topmost
    for or_surface in state.x11_override_redirect.iter().rev() {
        let Some(wl_surface) = or_surface.wl_surface() else { continue };
        let canvas_pos = state.or_canvas_position(or_surface);
        let or_size = or_surface.geometry().size;
        let or_rect = Rectangle::new(canvas_pos, or_size);
        if !visible_rect.overlaps(or_rect) { continue }

        let render_loc: Point<f64, Logical> = Point::from((
            canvas_pos.x as f64 - camera.x,
            canvas_pos.y as f64 - camera.y,
        ));
        let physical_loc: Point<f64, Physical> = render_loc.to_physical_precise_round(scale);
        let elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                renderer,
                &wl_surface,
                physical_loc.to_i32_round(),
                scale,
                1.0,
                Kind::Unspecified,
            );
        elements.extend(elems.into_iter().map(|elem| {
            OutputRenderElements::Window(PixelSnapRescaleElement::from_element(
                elem,
                Point::<i32, Physical>::from((0, 0)),
                zoom,
            ))
        }));
    }
    elements
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
            && config.match_window_rule(surface.namespace(), "").is_some_and(|r| r.blur)
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
    match status {
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
    }
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

    if let Some(elem) = state.render.cached_bg_elements.get_mut(&output_name) {
        elem.resize(canvas_area, Some(vec![canvas_area]));
        let time_secs = state.start_time.elapsed().as_secs_f32();
        elem.update_uniforms(vec![
            Uniform::new("u_camera", (cur_camera.x as f32, cur_camera.y as f32)),
            Uniform::new("u_time", time_secs),
        ]);
    } else if let Some(elem) = state.render.cached_tile_bg.get_mut(&output_name) {
        elem.resize(canvas_area, Some(vec![canvas_area]));
        elem.update_uniforms(vec![
            Uniform::new("u_camera", (cur_camera.x as f32, cur_camera.y as f32)),
            Uniform::new("u_tile_size", (elem.tex_w as f32, elem.tex_h as f32)),
            Uniform::new("u_output_size", (canvas_w as f32, canvas_h as f32)),
        ]);
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

/// Push window surface elements with corner-clip shader applied to the toplevel buffer.
/// Non-toplevel sub-surfaces (popups, subsurfaces) pass through as plain Window elements.
///
/// `geo`: if `None`, uses the buffer's own size as `u_geo` (SSD — buffer = content).
/// If `Some`, uses the window geometry (CSD — content may be offset within buffer).
#[allow(clippy::too_many_arguments)]
fn push_corner_clipped_elements(
    target: &mut Vec<OutputRenderElements>,
    elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    wl_surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    shader: &GlesTexProgram,
    geo: Option<Rectangle<i32, Logical>>,
    radius: f32,
    clip_top: f32,
    clip_shadow: f32,
    clip_all_corners: bool,
    zoom: f64,
) {
    let toplevel_id = smithay::backend::renderer::element::Id::from_wayland_resource(wl_surface);
    for elem in elems {
        if *elem.id() == toplevel_id {
            let buf = elem.buffer_size();
            let (gx, gy, gw, gh) = match geo {
                Some(g) => (g.loc.x as f32, g.loc.y as f32, g.size.w as f32, g.size.h as f32),
                None => (0.0, 0.0, buf.w as f32, buf.h as f32),
            };
            let uniforms = vec![
                Uniform::new("u_size", (buf.w as f32, buf.h as f32)),
                Uniform::new("u_geo", (gx, gy, gw, gh)),
                Uniform::new("u_radius", radius),
                Uniform::new("u_clip_top", clip_top),
                Uniform::new("u_clip_shadow", clip_shadow),
            ];
            target.push(OutputRenderElements::CsdWindow(PixelSnapRescaleElement::from_element(
                RoundedCornerElement::new(elem, shader.clone(), uniforms, radius as f64, clip_all_corners),
                Point::<i32, Physical>::from((0, 0)),
                zoom,
            )));
        } else {
            target.push(OutputRenderElements::Window(PixelSnapRescaleElement::from_element(
                elem,
                Point::<i32, Physical>::from((0, 0)),
                zoom,
            )));
        }
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
    if !state.render.cached_bg_elements.contains_key(&output.name()) && !state.render.cached_tile_bg.contains_key(&output.name()) {
        let output_size = crate::state::output_logical_size(output);
        init_background(state, renderer, output_size, &output.name());
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

        let elems = window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
            renderer,
            render_loc.to_physical_precise_round(scale),
            scale,
            opacity as f32,
        );

        let target = if is_widget { &mut zoomed_widgets } else { &mut zoomed_normal };
        let elem_start = target.len();
        let mut shadow_count = 0usize;

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

            // Window surface elements — clip bottom corners to match title bar rounding
            if let Some(ref shader) = state.render.corner_clip_shader {
                let radius = state.config.decorations.corner_radius as f32;
                if radius > 0.0 {
                    // SSD: buffer = content, only bottom corners clipped
                    push_corner_clipped_elements(
                        target, elems, &wl_surface, shader,
                        None, radius, 0.0, 0.0, false, zoom,
                    );
                } else {
                    push_plain_elements(target, elems, zoom);
                }
            } else {
                push_plain_elements(target, elems, zoom);
            }

            // Shadow element: cached per-window, rebuilt only on resize.
            // Stable Id lets the damage tracker skip unchanged shadow regions.
            if let Some(ref shader) = state.render.shadow_shader {
                use driftwm::config::DecorationConfig;
                let radius = DecorationConfig::SHADOW_RADIUS;
                let r = radius.ceil() as i32;
                let shadow_w = geom_size.w + 2 * r;
                let shadow_h = geom_size.h + bar_height + 2 * r;
                let shadow_loc: Point<i32, Logical> = Point::from((
                    render_loc.x.round() as i32 - r,
                    render_loc.y.round() as i32 - bar_height - r,
                ));
                let shadow_area = Rectangle::new(shadow_loc, (shadow_w, shadow_h).into());
                let corner_r = state.config.decorations.corner_radius as f32;

                if let Some(deco) = state.decorations.get_mut(&wl_surface.id()) {
                    let content_size = (geom_size.w, geom_size.h);
                    if deco.cached_shadow.as_ref().is_some_and(|s| (s.alpha() - opacity as f32).abs() > f32::EPSILON) {
                        deco.cached_shadow = None;
                        deco.last_shadow_phys_key = None;
                    }
                    // Body pre-zoom physical rect (title + content combined),
                    // via to_physical_precise_round — same chain as inner element.
                    let body_logical: Rectangle<f64, Logical> = Rectangle::new(
                        (render_loc.x, render_loc.y - bar_height as f64).into(),
                        (geom_size.w as f64, (geom_size.h + bar_height) as f64).into(),
                    );
                    let body_pre_zoom: Rectangle<i32, Physical> =
                        body_logical.to_physical_precise_round(scale);
                    let corner_r_phys = corner_r * scale.x as f32 * zoom as f32;
                    let (fresh_uniforms, fresh_key) = shadow_uniforms_precise(
                        body_pre_zoom, shadow_area, scale, zoom, radius, corner_r_phys,
                    );

                    let shadow_elem = if let Some(shadow) = &mut deco.cached_shadow {
                        if deco.shadow_content_size != content_size
                            || deco.last_shadow_phys_key != Some(fresh_key)
                        {
                            deco.shadow_content_size = content_size;
                            deco.last_shadow_phys_key = Some(fresh_key);
                            shadow.update_uniforms(fresh_uniforms);
                        }
                        shadow.resize(shadow_area, None);
                        shadow.clone()
                    } else {
                        deco.shadow_content_size = content_size;
                        deco.last_shadow_phys_key = Some(fresh_key);
                        let elem = PixelShaderElement::new(
                            shader.clone(),
                            shadow_area,
                            None,
                            opacity as f32,
                            fresh_uniforms,
                            Kind::Unspecified,
                        );
                        deco.cached_shadow = Some(elem.clone());
                        elem
                    };
                    target.push(OutputRenderElements::Background(
                        RescaleRenderElement::from_element(
                            shadow_elem,
                            Point::<i32, Physical>::from((0, 0)),
                            zoom,
                        ),
                    ));
                    shadow_count = 1;
                }
            }
        } else if let Some(ref shader) = state.render.corner_clip_shader {
            let geo = window.geometry();
            let radius = state.config.decorations.corner_radius as f32;

            let rule_forced = applied.as_ref().is_some_and(|r| {
                r.decoration != driftwm::config::DecorationMode::Client
            });

            if !rule_forced && !is_fullscreen {
                if radius > 0.0 {
                    // CSD: use window geometry (content may be offset within buffer)
                    push_corner_clipped_elements(
                        target, elems, &wl_surface, shader,
                        Some(geo), radius, 1.0, 1.0, true, zoom,
                    );
                } else {
                    push_plain_elements(target, elems, zoom);
                }

                // Compositor shadow behind CSD windows
                if let Some(ref shadow_shader) = state.render.shadow_shader {
                    use driftwm::config::DecorationConfig;
                    let shadow_radius = DecorationConfig::SHADOW_RADIUS;
                    let sr = shadow_radius.ceil() as i32;
                    let shadow_w = geom_size.w + 2 * sr;
                    let shadow_h = geom_size.h + 2 * sr;
                    // render_loc is the buffer origin; geometry starts at render_loc + geo.loc
                    let shadow_loc: Point<i32, Logical> = Point::from((
                        render_loc.x.round() as i32 + geo.loc.x - sr,
                        render_loc.y.round() as i32 + geo.loc.y - sr,
                    ));
                    let shadow_area = Rectangle::new(shadow_loc, (shadow_w, shadow_h).into());
                    let content_size = (geom_size.w, geom_size.h);
                    let corner_r = state.config.decorations.corner_radius as f32;

                    // Body pre-zoom physical rect (content area),
                    // via to_physical_precise_round — same chain as inner element.
                    let body_logical: Rectangle<f64, Logical> = Rectangle::new(
                        (render_loc.x + geo.loc.x as f64, render_loc.y + geo.loc.y as f64).into(),
                        (geom_size.w as f64, geom_size.h as f64).into(),
                    );
                    let body_pre_zoom: Rectangle<i32, Physical> =
                        body_logical.to_physical_precise_round(scale);
                    let corner_r_phys = corner_r * scale.x as f32 * zoom as f32;
                    let (fresh_uniforms, fresh_key) = shadow_uniforms_precise(
                        body_pre_zoom, shadow_area, scale, zoom, shadow_radius, corner_r_phys,
                    );

                    let shadow_entry = state.render.csd_shadows.entry(wl_surface.id());
                    let (shadow_elem, cached_size, cached_key) = shadow_entry.or_insert_with(|| {
                        let elem = PixelShaderElement::new(
                            shadow_shader.clone(),
                            shadow_area,
                            None,
                            opacity as f32,
                            fresh_uniforms.clone(),
                            Kind::Unspecified,
                        );
                        (elem, content_size, Some(fresh_key))
                    });

                    if *cached_size != content_size || *cached_key != Some(fresh_key) {
                        *cached_size = content_size;
                        *cached_key = Some(fresh_key);
                        shadow_elem.update_uniforms(fresh_uniforms);
                    }
                    shadow_elem.resize(shadow_area, None);
                    target.push(OutputRenderElements::Background(
                        RescaleRenderElement::from_element(
                            shadow_elem.clone(),
                            Point::<i32, Physical>::from((0, 0)),
                            zoom,
                        ),
                    ));
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

    let or_elements = build_override_redirect_elements(state, renderer, output, camera, zoom);

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
    let overlay_prefix = cursor_elements.len() + or_elements.len();
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
            + or_elements.len()
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
    all_elements.extend(or_elements);
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

/// Compile background shader and/or load tile image.
/// Called at startup and on config reload (lazy re-init).
/// On failure, falls back to `DEFAULT_SHADER` — never leaves background uninitialized.
pub fn init_background(state: &mut crate::state::DriftWm, renderer: &mut GlesRenderer, initial_size: Size<i32, smithay::utils::Logical>, output_name: &str) {
    // Try loading tile image first (if configured and no shader_path)
    if state.config.background.shader_path.is_none()
        && let Some(path) = state.config.background.tile_path.as_deref()
    {
        match image::open(path) {
            Ok(img) => {
                let img = img.into_rgba8();
                let (w, h) = img.dimensions();
                let raw = img.into_raw();

                use smithay::backend::renderer::ImportMem;
                use smithay::utils::Buffer;
                match renderer.import_memory(
                    &raw,
                    Fourcc::Abgr8888,
                    Size::<i32, Buffer>::from((w as i32, h as i32)),
                    false,
                ) {
                    Ok(texture) => {
                        if state.render.tile_shader.is_none() {
                            state.render.tile_shader = compile_tile_bg_shader(renderer);
                        }
                        if let Some(ref shader) = state.render.tile_shader {
                            let tw = w as i32;
                            let th = h as i32;
                            let area = Rectangle::from_size(initial_size);
                            let elem = TileShaderElement::new(
                                shader.clone(),
                                texture,
                                tw,
                                th,
                                area,
                                Some(vec![area]),
                                1.0,
                                vec![
                                    Uniform::new("u_camera", (0.0f32, 0.0f32)),
                                    Uniform::new("u_tile_size", (tw as f32, th as f32)),
                                    Uniform::new("u_output_size", (initial_size.w as f32, initial_size.h as f32)),
                                ],
                                Kind::Unspecified,
                            );
                            state.render.cached_tile_bg.insert(output_name.to_string(), elem);
                            return;
                        }
                        tracing::error!("Tile shader compilation failed, using default shader");
                    }
                    Err(e) => {
                        tracing::error!("Failed to upload tile texture: {e}, using default shader");
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to load tile image {path}: {e}, using default shader");
            }
        }
    }

    // Reuse cached shader if already compiled (avoids redundant GPU work
    // when multiple outputs each need a background element).
    let shader = if let Some(ref cached) = state.render.background_shader {
        cached.clone()
    } else {
        let shader_source = if let Some(path) = state.config.background.shader_path.as_deref() {
            match std::fs::read_to_string(path) {
                Ok(src) => src,
                Err(e) => {
                    tracing::error!("Failed to read shader {path}: {e}, using default");
                    driftwm::config::DEFAULT_SHADER.to_string()
                }
            }
        } else {
            driftwm::config::DEFAULT_SHADER.to_string()
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
        
        // Detect if shader is animated (contains u_time)
        state.render.background_is_animated = shader_source.contains("u_time");
        
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

/// Post-render: frame callbacks, space cleanup.
pub fn post_render(state: &mut crate::state::DriftWm, output: &Output) {
    let time = state.start_time.elapsed();

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

        window.send_frame(output, time, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }

    // Layer surface frame callbacks
    {
        let layer_map = layer_map_for_output(output);
        for layer_surface in layer_map.layers() {
            layer_surface.send_frame(output, time, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }

    // Canvas-positioned layer surface frame callbacks
    for cl in &state.canvas_layers {
        cl.surface.send_frame(output, time, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }

    // Override-redirect X11 surface frame callbacks
    for or_surface in &state.x11_override_redirect {
        if let Some(wl_surface) = or_surface.wl_surface() {
            smithay::desktop::utils::send_frames_surface_tree(
                &wl_surface, output, time, Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
    }

    // Cursor surface frame callbacks (animated cursors need these to advance)
    if let CursorImageStatus::Surface(ref surface) = state.cursor.cursor_status {
        smithay::desktop::utils::send_frames_surface_tree(
            surface, output, time, Some(Duration::ZERO),
            |_, _| Some(output.clone()),
        );
    }

    // Lock surface frame callback
    if let Some(lock_surface) = state.lock_surfaces.get(output) {
        smithay::desktop::utils::send_frames_surface_tree(
            lock_surface.wl_surface(),
            output,
            time,
            Some(Duration::ZERO),
            |_, _| Some(output.clone()),
        );
    }

    // Cleanup
    state.space.refresh();
    state.popups.cleanup();
    layer_map_for_output(output).cleanup();
    
    // Schedule continuous redraws for animated background shaders
    // This ensures u_time updates even when there's no other damage
    if state.render.background_is_animated {
        state.mark_all_dirty();
    }
}
