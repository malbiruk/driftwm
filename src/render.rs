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
    utils::{Physical, Point, Rectangle},
};

use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::desktop::layer_map_for_output;
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::{Size, Transform};

use driftwm::canvas::{self, CanvasPos, canvas_to_screen};

render_elements! {
    pub OutputRenderElements<=GlesRenderer>;
    Background=RescaleRenderElement<PixelShaderElement>,
    Tile=RescaleRenderElement<MemoryRenderBufferRenderElement<GlesRenderer>>,
    Window=RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    Layer=WaylandSurfaceRenderElement<GlesRenderer>,
    Cursor=MemoryRenderBufferRenderElement<GlesRenderer>,
}

/// Uniform declarations for background shaders.
/// Shaders receive only u_camera — zoom is handled externally via RescaleRenderElement.
pub const BG_UNIFORMS: &[UniformName<'static>] = &[UniformName {
    name: std::borrow::Cow::Borrowed("u_camera"),
    type_: UniformType::_2f,
}];

/// Build tiled background elements for the current frame.
///
/// Each tile is a (w+2)×(h+2) buffer with the last col/row duplicated,
/// stepped at the original (w, h) interval. The 1px overlap covers any
/// sub-pixel rounding gaps from RescaleRenderElement at fractional zoom.
pub fn build_tile_background_elements(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    let scale = output.current_scale().integer_scale();
    let output_size = output
        .current_mode()
        .map(|m| m.size.to_logical(scale))
        .unwrap_or((1, 1).into());

    let Some((tile_buf, tw, th)) = &state.background_tile else {
        return vec![];
    };
    let tw = *tw;
    let th = *th;
    if tw <= 0 || th <= 0 {
        return vec![];
    }

    let cam_x = state.camera.x;
    let cam_y = state.camera.y;
    let zoom = state.zoom;

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
pub fn build_cursor_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
) -> Vec<MemoryRenderBufferRenderElement<GlesRenderer>> {
    let pointer = state.seat.get_pointer().unwrap();
    let canvas_pos = pointer.current_location();
    // Custom elements are in screen-local physical coords
    let screen_pos = canvas_to_screen(CanvasPos(canvas_pos), state.camera, state.zoom).0;
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
    let (buffer, hotspot) = state.cursor_buffers.get(key).unwrap();
    let hotspot = *hotspot;

    let pos = physical_pos - Point::from((hotspot.x as f64, hotspot.y as f64));
    match MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        pos,
        buffer,
        None,
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
) -> (bool, bool) {
    let camera_moved = state.camera != state.last_rendered_camera;
    let zoom_changed = state.zoom != state.last_rendered_zoom;
    if let Some(ref mut elem) = state.cached_bg_element {
        let scale = output.current_scale().integer_scale();
        let output_size = output
            .current_mode()
            .map(|m| m.size.to_logical(scale))
            .unwrap_or((1, 1).into());
        let canvas_w = (output_size.w as f64 / state.zoom).ceil() as i32;
        let canvas_h = (output_size.h as f64 / state.zoom).ceil() as i32;
        let canvas_area = Rectangle::from_size((canvas_w, canvas_h).into());
        elem.resize(canvas_area, Some(vec![canvas_area]));
        if camera_moved || zoom_changed {
            elem.update_uniforms(vec![Uniform::new(
                "u_camera",
                (state.camera.x as f32, state.camera.y as f32),
            )]);
        }
    }
    (camera_moved, zoom_changed)
}

/// Assemble all render elements for a frame.
/// Caller provides cursor elements (built before taking the renderer).
pub fn compose_frame(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    cursor_elements: Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) -> Vec<OutputRenderElements> {
    // Lazy re-init background after config reload cleared the cached state
    if state.background_shader.is_none()
        && state.cached_bg_element.is_none()
        && state.background_tile.is_none()
    {
        let output_size = output
            .current_mode()
            .map(|m| m.size.to_logical(output.current_scale().integer_scale()))
            .unwrap_or((1, 1).into());
        init_background(state, renderer, output_size);
    }

    let viewport_size = state.get_viewport_size();
    let visible_rect = canvas::visible_canvas_rect(
        state.camera.to_i32_round(),
        viewport_size,
        state.zoom,
    );
    let output_scale = output.current_scale().fractional_scale();
    let window_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = state
        .space
        .render_elements_for_region(renderer, &visible_rect, output_scale, 1.0);

    let zoomed_windows: Vec<OutputRenderElements> = window_elements
        .into_iter()
        .map(|elem| {
            OutputRenderElements::Window(RescaleRenderElement::from_element(
                elem,
                Point::<i32, Physical>::from((0, 0)),
                state.zoom,
            ))
        })
        .collect();

    let bg_elements: Vec<OutputRenderElements> =
        if let Some(ref elem) = state.cached_bg_element {
            vec![OutputRenderElements::Background(
                RescaleRenderElement::from_element(
                    elem.clone(),
                    Point::<i32, Physical>::from((0, 0)),
                    state.zoom,
                ),
            )]
        } else if state.background_tile.is_some() {
            build_tile_background_elements(state, renderer, output)
        } else {
            vec![]
        };

    let is_fullscreen = state.fullscreen.is_some();
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
            + zoomed_windows.len()
            + bottom_elements.len()
            + bg_elements.len()
            + background_layer_elements.len(),
    );
    all_elements.extend(cursor_elements.into_iter().map(OutputRenderElements::Cursor));
    all_elements.extend(overlay_elements);
    all_elements.extend(top_elements);
    all_elements.extend(zoomed_windows);
    all_elements.extend(bottom_elements);
    all_elements.extend(bg_elements);
    all_elements.extend(background_layer_elements);
    all_elements
}

/// Compile background shader and/or load tile image.
/// Called at startup and on config reload (lazy re-init).
/// On failure, falls back to `DEFAULT_SHADER` — never leaves background uninitialized.
pub fn init_background(state: &mut crate::state::DriftWm, renderer: &mut GlesRenderer, initial_size: Size<i32, smithay::utils::Logical>) {
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

    // Shader path: custom or default
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

    let shader = match renderer.compile_custom_pixel_shader(&shader_source, BG_UNIFORMS) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to compile shader: {e}, using default");
            renderer
                .compile_custom_pixel_shader(driftwm::config::DEFAULT_SHADER, BG_UNIFORMS)
                .expect("Default shader must compile")
        }
    };
    state.background_shader = Some(shader.clone());

    let area = Rectangle::from_size(initial_size);
    state.cached_bg_element = Some(PixelShaderElement::new(
        shader,
        area,
        Some(vec![area]),
        1.0,
        vec![Uniform::new("u_camera", (0.0f32, 0.0f32))],
        Kind::Unspecified,
    ));
}

/// Post-render: frame callbacks, foreign toplevel refresh, space cleanup.
pub fn post_render(state: &mut crate::state::DriftWm, output: &Output) {
    // Foreign toplevel refresh
    {
        let keyboard = state.seat.get_keyboard().unwrap();
        let focused = keyboard.current_focus().map(|f| f.0);
        driftwm::protocols::foreign_toplevel::refresh::<crate::state::DriftWm>(
            &mut state.foreign_toplevel_state,
            &state.space,
            focused.as_ref(),
            output,
        );
    }

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

    // Cleanup
    state.space.refresh();
    state.popups.cleanup();
    layer_map_for_output(output).cleanup();
}
