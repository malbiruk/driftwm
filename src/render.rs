use std::time::Duration;

use smithay::{
    backend::renderer::{
        element::{
            Kind,
            memory::MemoryRenderBufferRenderElement,
            render_elements,
            utils::RescaleRenderElement,
        },
        gles::{GlesRenderer, Uniform, UniformName, UniformType, element::PixelShaderElement},
    },
    input::pointer::CursorImageStatus,
    output::Output,
    utils::{Logical, Physical, Point, Rectangle, Scale},
};

use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::desktop::layer_map_for_output;
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::{Size, Transform};

use smithay::reexports::wayland_server::Resource;
use smithay::wayland::seat::WaylandFocus;

use driftwm::canvas::{self, CanvasPos, canvas_to_screen};

render_elements! {
    pub OutputRenderElements<=GlesRenderer>;
    Background=RescaleRenderElement<PixelShaderElement>,
    Tile=RescaleRenderElement<MemoryRenderBufferRenderElement<GlesRenderer>>,
    Window=RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    Layer=WaylandSurfaceRenderElement<GlesRenderer>,
    Cursor=MemoryRenderBufferRenderElement<GlesRenderer>,
}

// Shadow and Decoration share inner types with Background and Tile respectively.
// We can't add them to render_elements! because it generates conflicting From impls.
// Instead we construct them directly using the existing Background/Tile variants.
// Helpers below create the elements and wrap them in the correct variant.

/// Uniform declarations for background shaders.
/// Shaders receive only u_camera — zoom is handled externally via RescaleRenderElement.
pub const BG_UNIFORMS: &[UniformName<'static>] = &[UniformName {
    name: std::borrow::Cow::Borrowed("u_camera"),
    type_: UniformType::_2f,
}];

/// Shadow shader source — soft box-shadow around SSD windows.
const SHADOW_SHADER_SRC: &str = include_str!("../assets/shaders/shadow.glsl");

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

/// Build tiled background elements for the current frame.
///
/// Each tile is a (w+2)×(h+2) buffer with the last col/row duplicated,
/// stepped at the original (w, h) interval. The 1px overlap covers any
/// sub-pixel rounding gaps from RescaleRenderElement at fractional zoom.
pub fn build_tile_background_elements(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    camera: Point<f64, smithay::utils::Logical>,
    zoom: f64,
) -> Vec<OutputRenderElements> {
    let scale = output.current_scale().integer_scale();
    let output_size = output
        .current_mode()
        .map(|m| output.current_transform().transform_size(m.size.to_logical(scale)))
        .unwrap_or((1, 1).into());

    let Some((tile_buf, tw, th)) = &state.background_tile else {
        return vec![];
    };
    let tw = *tw;
    let th = *th;
    if tw <= 0 || th <= 0 {
        return vec![];
    }

    let cam_x = camera.x;
    let cam_y = camera.y;

    // Visible canvas area: viewport / zoom
    let visible_w = output_size.w as f64 / zoom;
    let visible_h = output_size.h as f64 / zoom;

    // First visible tile: snap camera to tile grid
    let start_x = (cam_x / tw as f64).floor() as i64 * tw as i64;
    let start_y = (cam_y / th as f64).floor() as i64 * th as i64;
    let end_x = (cam_x + visible_w).ceil() as i64;
    let end_y = (cam_y + visible_h).ceil() as i64;

    let mut elements = Vec::new();
    let mut ty = start_y;
    while ty < end_y {
        let mut tx = start_x;
        while tx < end_x {
            let canvas_rel_x = tx as f64 - cam_x;
            let canvas_rel_y = ty as f64 - cam_y;
            let pos: Point<f64, Physical> = (canvas_rel_x, canvas_rel_y).into();

            if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                pos,
                tile_buf,
                None,
                None,
                None,
                Kind::Unspecified,
            ) {
                elements.push(OutputRenderElements::Tile(
                    RescaleRenderElement::from_element(
                        elem,
                        Point::<i32, Physical>::from((0, 0)),
                        zoom,
                    ),
                ));
            }
            tx += tw as i64;
        }
        ty += th as i64;
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
    let camera_i32 = camera.to_i32_round();
    let mut elements = Vec::new();

    for cl in &state.canvas_layers {
        let Some(pos) = cl.position else { continue; };
        // Camera-relative position (same as render_elements_for_region does for windows)
        let rel = pos - camera_i32;
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
            OutputRenderElements::Window(RescaleRenderElement::from_element(
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
pub fn build_layer_elements(
    output: &Output,
    renderer: &mut GlesRenderer,
    layer: WlrLayer,
) -> Vec<OutputRenderElements> {
    let map = layer_map_for_output(output);
    let output_scale = output.current_scale().fractional_scale();
    let mut elements = Vec::new();

    for surface in map.layers_on(layer).rev() {
        let geo = map.layer_geometry(surface).unwrap_or_default();
        let loc = geo.loc.to_physical_precise_round(output_scale);
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
    }

    elements
}

/// Resolve which xcursor name to load for the current cursor status.
pub fn cursor_icon_name(status: &CursorImageStatus) -> Option<&'static str> {
    match status {
        CursorImageStatus::Hidden => None,
        CursorImageStatus::Named(icon) => Some(icon.name()),
        // Client-provided surface cursor — fall back to default for now
        CursorImageStatus::Surface(_) => Some("default"),
    }
}

/// Build the cursor render element(s) for the current frame.
/// `camera` and `zoom` are from the output being rendered.
pub fn build_cursor_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    camera: Point<f64, smithay::utils::Logical>,
    zoom: f64,
    alpha: f32,
) -> Vec<MemoryRenderBufferRenderElement<GlesRenderer>> {
    if alpha <= 0.0 {
        return vec![];
    }
    let pointer = state.seat.get_pointer().unwrap();
    let canvas_pos = pointer.current_location();
    // Custom elements are in screen-local physical coords
    let screen_pos = canvas_to_screen(CanvasPos(canvas_pos), camera, zoom).0;
    let physical_pos: Point<f64, Physical> = (screen_pos.x, screen_pos.y).into();

    // Extract cursor name before borrowing state mutably for load_xcursor
    let Some(name) = cursor_icon_name(&state.cursor_status) else {
        return vec![];
    };

    // Try loading by CSS name, fall back to "default"
    let loaded = state.load_xcursor(name).is_some();
    if !loaded && state.load_xcursor("default").is_none() {
        return vec![];
    }
    let key = if loaded { name } else { "default" };
    let cursor_frames = state.cursor_buffers.get(key).unwrap();

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
        Ok(elem) => vec![elem],
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
    if let Some(elem) = state.cached_bg_elements.get_mut(&output_name) {
        let scale = output.current_scale().integer_scale();
        let output_size = output
            .current_mode()
            .map(|m| output.current_transform().transform_size(m.size.to_logical(scale)))
            .unwrap_or((1, 1).into());
        let canvas_w = (output_size.w as f64 / cur_zoom).ceil() as i32;
        let canvas_h = (output_size.h as f64 / cur_zoom).ceil() as i32;
        let canvas_area = Rectangle::from_size((canvas_w, canvas_h).into());
        elem.resize(canvas_area, Some(vec![canvas_area]));
        // Always update — with multiple outputs the shared element may have
        // another output's camera from the previous render_frame call.
        elem.update_uniforms(vec![Uniform::new(
            "u_camera",
            (cur_camera.x as f32, cur_camera.y as f32),
        )]);
    }
    (camera_moved, zoom_changed)
}

/// Build render elements for a locked session: only the lock surface.
/// No compositor cursor — the lock client manages its own visuals.
fn compose_lock_frame(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    _cursor_elements: Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
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

/// Assemble all render elements for a frame.
/// Caller provides cursor elements (built before taking the renderer).
pub fn compose_frame(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    cursor_elements: Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) -> Vec<OutputRenderElements> {
    // Session lock: render only lock surface (or black) + cursor
    if !matches!(state.session_lock, crate::state::SessionLock::Unlocked) {
        return compose_lock_frame(state, renderer, output, cursor_elements);
    }

    // Ensure this output has a background element (lazy init per output, and re-init after config reload)
    if !state.cached_bg_elements.contains_key(&output.name()) && state.background_tile.is_none() {
        let output_size = output
            .current_mode()
            .map(|m| output.current_transform().transform_size(
                m.size.to_logical(output.current_scale().integer_scale()),
            ))
            .unwrap_or((1, 1).into());
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

        let render_loc: Point<i32, Logical> = loc - geom_loc - visible_rect.loc;
        let elems = window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
            renderer,
            render_loc.to_physical_precise_round(scale),
            scale,
            1.0,
        );

        let is_widget = driftwm::config::applied_rule(&wl_surface)
            .is_some_and(|r| r.widget);

        let target = if is_widget { &mut zoomed_widgets } else { &mut zoomed_normal };

        if has_ssd {
            let bar_height = driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT;
            let is_focused = focused_surface.as_ref().is_some_and(|f| *f == *wl_surface);

            // Update decoration state (re-render title bar if needed)
            if let Some(deco) = state.decorations.get_mut(&wl_surface.id()) {
                deco.update(geom_size.w, is_focused, &state.config.decorations);
            }

            // Title bar element: positioned above the window
            if let Some(deco) = state.decorations.get(&wl_surface.id()) {
                let bar_loc = render_loc + Point::from((0, -bar_height));
                let bar_physical: Point<f64, Physical> = bar_loc.to_physical_precise_round(scale);
                if let Ok(bar_elem) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    bar_physical,
                    &deco.title_bar,
                    None,
                    None,
                    None,
                    Kind::Unspecified,
                ) {
                    target.push(OutputRenderElements::Tile(
                        RescaleRenderElement::from_element(
                            bar_elem,
                            Point::<i32, Physical>::from((0, 0)),
                            zoom,
                        ),
                    ));
                }
            }

            // Window surface elements
            target.extend(elems.into_iter().map(|elem| {
                OutputRenderElements::Window(RescaleRenderElement::from_element(
                    elem,
                    Point::<i32, Physical>::from((0, 0)),
                    zoom,
                ))
            }));

            // Shadow element: cached per-window, rebuilt only on resize.
            // Stable Id lets the damage tracker skip unchanged shadow regions.
            if let Some(ref shader) = state.shadow_shader {
                use driftwm::config::DecorationConfig;
                let radius = DecorationConfig::SHADOW_RADIUS;
                let r = radius.ceil() as i32;
                let shadow_w = geom_size.w + 2 * r;
                let shadow_h = geom_size.h + bar_height + 2 * r;
                let shadow_loc = render_loc + Point::from((-r, -bar_height - r));
                let shadow_area = Rectangle::new(shadow_loc, (shadow_w, shadow_h).into());

                if let Some(deco) = state.decorations.get_mut(&wl_surface.id()) {
                    let content_size = (geom_size.w, geom_size.h);
                    let shadow_elem = if let Some(shadow) = &mut deco.cached_shadow {
                        if deco.shadow_content_size != content_size {
                            deco.shadow_content_size = content_size;
                            let sc = DecorationConfig::SHADOW_COLOR;
                            shadow.update_uniforms(vec![
                                Uniform::new("u_window_rect", (
                                    r as f32, r as f32,
                                    geom_size.w as f32, (geom_size.h + bar_height) as f32,
                                )),
                                Uniform::new("u_radius", radius),
                                Uniform::new("u_color", (
                                    sc[0] as f32 / 255.0, sc[1] as f32 / 255.0,
                                    sc[2] as f32 / 255.0, sc[3] as f32 / 255.0,
                                )),
                                Uniform::new("u_corner_radius", DecorationConfig::CORNER_RADIUS as f32),
                            ]);
                        }
                        shadow.resize(shadow_area, None);
                        shadow.clone()
                    } else {
                        deco.shadow_content_size = content_size;
                        let sc = DecorationConfig::SHADOW_COLOR;
                        let elem = PixelShaderElement::new(
                            shader.clone(),
                            shadow_area,
                            None,
                            1.0,
                            vec![
                                Uniform::new("u_window_rect", (
                                    r as f32, r as f32,
                                    geom_size.w as f32, (geom_size.h + bar_height) as f32,
                                )),
                                Uniform::new("u_radius", radius),
                                Uniform::new("u_color", (
                                    sc[0] as f32 / 255.0, sc[1] as f32 / 255.0,
                                    sc[2] as f32 / 255.0, sc[3] as f32 / 255.0,
                                )),
                                Uniform::new("u_corner_radius", DecorationConfig::CORNER_RADIUS as f32),
                            ],
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
                }
            }
        } else {
            target.extend(elems.into_iter().map(|elem| {
                OutputRenderElements::Window(RescaleRenderElement::from_element(
                    elem,
                    Point::<i32, Physical>::from((0, 0)),
                    zoom,
                ))
            }));
        }
    }

    let canvas_layer_elements = build_canvas_layer_elements(state, renderer, output, camera, zoom);

    let outline_elements = build_output_outline_elements(
        state, renderer, output, camera, zoom, viewport_size,
    );

    let bg_elements: Vec<OutputRenderElements> =
        if let Some(elem) = state.cached_bg_elements.get(&output.name()) {
            vec![OutputRenderElements::Background(
                RescaleRenderElement::from_element(
                    elem.clone(),
                    Point::<i32, Physical>::from((0, 0)),
                    zoom,
                ),
            )]
        } else if state.background_tile.is_some() {
            build_tile_background_elements(state, renderer, output, camera, zoom)
        } else {
            vec![]
        };

    let is_fullscreen = state.is_output_fullscreen(output);
    let overlay_elements = build_layer_elements(output, renderer, WlrLayer::Overlay);
    let top_elements = if !is_fullscreen {
        build_layer_elements(output, renderer, WlrLayer::Top)
    } else {
        vec![]
    };
    let bottom_elements = if !is_fullscreen {
        build_layer_elements(output, renderer, WlrLayer::Bottom)
    } else {
        vec![]
    };
    let background_layer_elements = build_layer_elements(output, renderer, WlrLayer::Background);

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
    all_elements.extend(cursor_elements.into_iter().map(OutputRenderElements::Cursor));
    all_elements.extend(overlay_elements);
    all_elements.extend(top_elements);
    all_elements.extend(zoomed_normal);
    all_elements.extend(canvas_layer_elements);
    all_elements.extend(zoomed_widgets);
    all_elements.extend(bottom_elements);
    all_elements.extend(outline_elements);
    all_elements.extend(bg_elements);
    all_elements.extend(background_layer_elements);
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
                elements.push(OutputRenderElements::Tile(
                    RescaleRenderElement::from_element(
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

                // Build (w+2)×(h+2) buffer: duplicate last 2 cols/rows so adjacent
                // tiles overlap by 2 opaque pixels, covering sub-pixel rounding gaps.
                let pad = 2usize;
                let ew = w as usize + pad;
                let eh = h as usize + pad;
                let mut expanded = vec![0u8; ew * eh * 4];
                for y in 0..h as usize {
                    let src_row = y * w as usize * 4;
                    let dst_row = y * ew * 4;
                    expanded[dst_row..dst_row + w as usize * 4]
                        .copy_from_slice(&raw[src_row..src_row + w as usize * 4]);
                    let last_px = &raw[src_row + (w as usize - 1) * 4..src_row + w as usize * 4];
                    for p in 0..pad {
                        let dst = dst_row + (w as usize + p) * 4;
                        expanded[dst..dst + 4].copy_from_slice(last_px);
                    }
                }
                let last_row: Vec<u8> = expanded[(h as usize - 1) * ew * 4..h as usize * ew * 4].to_vec();
                for p in 0..pad {
                    let dst = (h as usize + p) * ew * 4;
                    expanded[dst..dst + ew * 4].copy_from_slice(&last_row);
                }

                let buffer = MemoryRenderBuffer::from_slice(
                    &expanded,
                    Fourcc::Abgr8888,
                    (ew as i32, eh as i32),
                    1,
                    Transform::Normal,
                    None,
                );
                state.background_tile = Some((buffer, w as i32, h as i32));
                return;
            }
            Err(e) => {
                tracing::error!("Failed to load tile image {path}: {e}, using default shader");
            }
        }
    }

    // Reuse cached shader if already compiled (avoids redundant GPU work
    // when multiple outputs each need a background element).
    let shader = if let Some(ref cached) = state.background_shader {
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
        state.background_shader = Some(compiled.clone());
        compiled
    };

    let area = Rectangle::from_size(initial_size);
    state.cached_bg_elements.insert(output_name.to_string(), PixelShaderElement::new(
        shader,
        area,
        Some(vec![area]),
        1.0,
        vec![Uniform::new("u_camera", (0.0f32, 0.0f32))],
        Kind::Unspecified,
    ));
}

/// Fulfill pending screencopy requests by rendering to offscreen textures.
pub fn render_screencopy(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[OutputRenderElements],
) {
    use smithay::backend::renderer::{ExportMem, Renderer};
    use smithay::wayland::shm;
    use driftwm::protocols::screencopy::ScreencopyBuffer;
    use std::ptr;

    // Extract only requests for this output, keep the rest
    let mut pending = Vec::new();
    let mut i = 0;
    while i < state.pending_screencopies.len() {
        if state.pending_screencopies[i].output() == output {
            pending.push(state.pending_screencopies.swap_remove(i));
        } else {
            i += 1;
        }
    }

    if pending.is_empty() {
        return;
    }

    let output_scale = output.current_scale().fractional_scale();
    let scale = Scale::from(output_scale);
    let transform = output.current_transform();
    let timestamp = state.start_time.elapsed();

    for screencopy in pending {
        let size = screencopy.buffer_size();
        let use_elements: Vec<&OutputRenderElements> = if screencopy.overlay_cursor() {
            elements.iter().collect()
        } else {
            elements
                .iter()
                .filter(|e| !matches!(e, OutputRenderElements::Cursor(_)))
                .collect()
        };

        match screencopy.buffer() {
            ScreencopyBuffer::Dmabuf(dmabuf) => {
                let mut dmabuf = dmabuf.clone();
                match render_to_dmabuf(renderer, &mut dmabuf, size, scale, transform, &use_elements) {
                    Ok(sync) => {
                        if let Err(e) = renderer.wait(&sync) {
                            tracing::warn!("screencopy: dmabuf sync wait failed: {e:?}");
                            continue; // screencopy Drop sends failed()
                        }
                        screencopy.submit(false, timestamp);
                    }
                    Err(e) => {
                        tracing::warn!("screencopy: dmabuf render failed: {e:?}");
                    }
                }
            }
            ScreencopyBuffer::Shm(wl_buffer) => {
                let result = render_to_offscreen(renderer, size, scale, transform, &use_elements);
                match result {
                    Ok(mapping) => {
                        let copy_ok =
                            shm::with_buffer_contents_mut(wl_buffer, |shm_buf, shm_len, _data| {
                                let bytes = match renderer.map_texture(&mapping) {
                                    Ok(b) => b,
                                    Err(e) => {
                                        tracing::warn!("screencopy: map_texture failed: {e:?}");
                                        return false;
                                    }
                                };
                                let copy_len = shm_len.min(bytes.len());
                                unsafe {
                                    ptr::copy_nonoverlapping(bytes.as_ptr(), shm_buf.cast(), copy_len);
                                }
                                true
                            });

                        match copy_ok {
                            Ok(true) => {
                                screencopy.submit(false, timestamp);
                            }
                            _ => {
                                tracing::warn!("screencopy: SHM buffer copy failed");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("screencopy: offscreen render failed: {e:?}");
                    }
                }
            }
        }
    }
}

/// Render elements to an offscreen texture and download the pixels.
fn render_to_offscreen(
    renderer: &mut GlesRenderer,
    size: smithay::utils::Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: &[&OutputRenderElements],
) -> Result<smithay::backend::renderer::gles::GlesMapping, Box<dyn std::error::Error>> {
    use smithay::backend::renderer::{Bind, ExportMem, Offscreen};
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::backend::renderer::gles::GlesTexture;

    let buffer_size = size.to_logical(1).to_buffer(1, Transform::Normal);

    let mut texture: GlesTexture = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Xrgb8888, buffer_size)?;

    {
        let mut target = renderer.bind(&mut texture)?;
        let mut damage_tracker = OutputDamageTracker::new(size, scale, transform);
        let _ = damage_tracker.render_output(
            renderer,
            &mut target,
            0,
            elements,
            [0.0f32, 0.0, 0.0, 1.0],
        )?;
    }

    // Re-bind texture to copy pixels
    let target = renderer.bind(&mut texture)?;
    let mapping = renderer.copy_framebuffer(&target, Rectangle::from_size(buffer_size), Fourcc::Xrgb8888)?;

    Ok(mapping)
}

/// Render elements directly into a client-provided DMA-BUF (zero CPU copies).
///
/// The caller must choose the correct `transform` for the protocol:
/// - wlr-screencopy: `output.current_transform()` (buffer is raw mode size)
/// - ext-image-copy-capture: `Transform::Normal` (buffer is already transformed)
fn render_to_dmabuf(
    renderer: &mut GlesRenderer,
    dmabuf: &mut smithay::backend::allocator::dmabuf::Dmabuf,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: &[&OutputRenderElements],
) -> Result<smithay::backend::renderer::sync::SyncPoint, Box<dyn std::error::Error>> {
    use smithay::backend::renderer::Bind;
    use smithay::backend::renderer::damage::OutputDamageTracker;

    let sync = {
        let mut target = renderer.bind(dmabuf)?;
        let mut damage_tracker = OutputDamageTracker::new(size, scale, transform);
        damage_tracker.render_output(
            renderer,
            &mut target,
            0,
            elements,
            [0.0f32, 0.0, 0.0, 1.0],
        )?.sync
    };

    Ok(sync)
}

/// Fulfill pending ext-image-copy-capture frames by rendering to offscreen textures.
pub fn render_capture_frames(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[OutputRenderElements],
) {
    use smithay::backend::renderer::{ExportMem, Renderer};
    use smithay::wayland::shm;
    use std::ptr;

    // Promote any sessions waiting for damage on this output
    state
        .image_copy_capture_state
        .promote_waiting_frames(output, &mut state.pending_captures);

    // Extract captures for this output
    let mut pending = Vec::new();
    let mut i = 0;
    while i < state.pending_captures.len() {
        if &state.pending_captures[i].output == output {
            pending.push(state.pending_captures.swap_remove(i));
        } else {
            i += 1;
        }
    }

    if pending.is_empty() {
        return;
    }

    let output_scale = output.current_scale().fractional_scale();
    let scale = Scale::from(output_scale);
    let timestamp = state.start_time.elapsed();

    let fail_reason = smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason::Unknown;

    for capture in pending {
        let use_elements: Vec<&OutputRenderElements> = if capture.paint_cursors {
            elements.iter().collect()
        } else {
            elements
                .iter()
                .filter(|e| !matches!(e, OutputRenderElements::Cursor(_)))
                .collect()
        };

        // ext-image-copy-capture buffer_size is already in transformed/logical orientation,
        // matching the element coordinate space — render with Normal (no additional transform).

        // Try DMA-BUF first, fall back to SHM
        let ok = if let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(&capture.buffer) {
            let mut dmabuf = dmabuf.clone();
            match render_to_dmabuf(renderer, &mut dmabuf, capture.buffer_size, scale, Transform::Normal, &use_elements) {
                Ok(sync) => {
                    if let Err(e) = renderer.wait(&sync) {
                        tracing::warn!("capture: dmabuf sync wait failed: {e:?}");
                        false
                    } else {
                        true
                    }
                }
                Err(e) => {
                    tracing::warn!("capture: dmabuf render failed: {e:?}");
                    false
                }
            }
        } else {
            let result = render_to_offscreen(renderer, capture.buffer_size, scale, Transform::Normal, &use_elements);
            match result {
                Ok(mapping) => {
                    shm::with_buffer_contents_mut(&capture.buffer, |shm_buf, shm_len, _data| {
                        let bytes = match renderer.map_texture(&mapping) {
                            Ok(b) => b,
                            Err(e) => {
                                tracing::warn!("capture: map_texture failed: {e:?}");
                                return false;
                            }
                        };
                        let copy_len = shm_len.min(bytes.len());
                        unsafe {
                            ptr::copy_nonoverlapping(bytes.as_ptr(), shm_buf.cast(), copy_len);
                        }
                        true
                    })
                    .unwrap_or(false)
                }
                Err(e) => {
                    tracing::warn!("capture: offscreen render failed: {e:?}");
                    false
                }
            }
        };

        if ok {
            let w = capture.buffer_size.w;
            let h = capture.buffer_size.h;
            capture.frame.transform(smithay::utils::Transform::Normal.into());
            capture.frame.damage(0, 0, w, h);
            let tv_sec_hi = (timestamp.as_secs() >> 32) as u32;
            let tv_sec_lo = (timestamp.as_secs() & 0xFFFFFFFF) as u32;
            let tv_nsec = timestamp.subsec_nanos();
            capture.frame.presentation_time(tv_sec_hi, tv_sec_lo, tv_nsec);
            capture.frame.ready();

            let frame_data = capture.frame.data::<std::sync::Mutex<driftwm::protocols::image_copy_capture::CaptureFrameData>>();
            if let Some(fd) = frame_data {
                let fd = fd.lock().unwrap();
                state.image_copy_capture_state.frame_done(&fd.session);
            }
        } else {
            capture.frame.failed(fail_reason);
        }
    }
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
    // Frame callbacks to windows
    let time = state.start_time.elapsed();
    for window in state.space.elements() {
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
}
