use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportDma,
            damage::OutputDamageTracker,
            element::{Kind, memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement}, render_elements},
            gles::{
                GlesRenderer, Uniform, UniformName, UniformType,
                element::PixelShaderElement,
            },
        },
        winit::{self, WinitEvent},
    },
    desktop::space::{SpaceRenderElements, space_render_elements},
    input::pointer::CursorImageStatus,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        EventLoop,
    },
    utils::{Physical, Point, Rectangle, Transform},
};
use std::time::Duration;

use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;

use driftwm::canvas::{CanvasPos, canvas_to_screen};
use crate::state::{CalloopData, log_err};

render_elements! {
    pub OutputRenderElements<=GlesRenderer>;
    Background=PixelShaderElement,
    Space=SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Cursor=MemoryRenderBufferRenderElement<GlesRenderer>,
}

/// Uniform declarations shared by all background shaders. Shaders ignore what they don't use.
const BG_UNIFORMS: &[UniformName<'static>] = &[
    UniformName { name: std::borrow::Cow::Borrowed("u_camera"), type_: UniformType::_2f },
    UniformName { name: std::borrow::Cow::Borrowed("u_bg_color"), type_: UniformType::_4f },
    UniformName { name: std::borrow::Cow::Borrowed("u_dot_color"), type_: UniformType::_4f },
    UniformName { name: std::borrow::Cow::Borrowed("u_dot_spacing"), type_: UniformType::_1f },
];

/// Initialize the winit backend: create a window, set up the output, and
/// start the render loop timer.
pub fn init_winit(
    event_loop: &mut EventLoop<'static, CalloopData>,
    data: &mut CalloopData,
) -> Result<(), Box<dyn std::error::Error>> {
    let (backend, mut winit_evt) = winit::init::<GlesRenderer>()?;

    // Store backend on state so protocol handlers can access the renderer
    data.state.backend = Some(backend);

    // Create an Output representing the winit window (a virtual monitor)
    let size = data.state.backend.as_ref().unwrap().window_size();
    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(), // unknown physical size
            subpixel: Subpixel::Unknown,
            make: "driftwm".to_string(),
            model: "winit".to_string(),
        },
    );
    let mode = Mode {
        size,
        refresh: 60_000, // 60 Hz in mHz
    };
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, None);
    output.set_preferred(mode);

    // Advertise the output as a wl_output global so clients can see it
    output.create_global::<crate::state::DriftWm>(&data.display.handle());

    // Create DMA-BUF global — advertise GPU buffer formats to clients
    let formats = data.state.backend.as_mut().unwrap().renderer().dmabuf_formats();
    let dmabuf_global = data.state.dmabuf_state.create_global::<crate::state::DriftWm>(
        &data.display.handle(),
        formats,
    );
    data.state.dmabuf_global = Some(dmabuf_global);

    // Compile background shader if shader_path is set
    if let Some(path) = data.state.config.background.shader_path.as_deref() {
        let shader_source = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("Failed to read shader {path}: {e}"));
        let shader = data.state.backend.as_mut().unwrap().renderer()
            .compile_custom_pixel_shader(&shader_source, BG_UNIFORMS)
            .expect("Failed to compile background shader");
        data.state.background_shader = Some(shader);
    }

    // Load tile image if tile_path is set (and no shader — shader takes priority)
    if data.state.background_shader.is_none()
        && let Some(path) = data.state.config.background.tile_path.as_deref()
    {
        let img = image::open(path)
            .unwrap_or_else(|e| panic!("Failed to load tile image {path}: {e}"))
            .into_rgba8();
        let (w, h) = img.dimensions();
        let buffer = MemoryRenderBuffer::from_slice(
            &img.into_raw(),
            Fourcc::Abgr8888,
            (w as i32, h as i32),
            1,
            Transform::Normal,
            None,
        );
        data.state.background_tile = Some((buffer, w as i32, h as i32));
    }

    // Map the output into the space at (0, 0)
    data.state.space.map_output(&output, (0, 0));

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    // Render loop: fires immediately, then re-arms at ~60fps
    let timer = Timer::immediate();
    event_loop
        .handle()
        .insert_source(timer, move |_, _, data| {
            // --- Advance frame counter ---
            data.state.frame_counter = data.state.frame_counter.wrapping_add(1);

            // --- Dispatch winit events ---
            let mut stop = false;
            winit_evt.dispatch_new_events(|event| match event {
                WinitEvent::Resized { size, scale_factor } => {
                    let new_mode = Mode {
                        size,
                        refresh: 60_000,
                    };
                    output.change_current_state(
                        Some(new_mode),
                        None,
                        Some(smithay::output::Scale::Fractional(scale_factor)),
                        None,
                    );
                }
                WinitEvent::Input(event) => {
                    data.state.process_input_event(event);
                }
                WinitEvent::CloseRequested => {
                    stop = true;
                }
                _ => {}
            });

            if stop {
                data.state.loop_signal.stop();
                return TimeoutAction::Drop;
            }

            // --- Dispatch Wayland client messages before rendering ---
            log_err("dispatch_clients", data.display.dispatch_clients(&mut data.state));
            log_err("flush_clients", data.display.flush_clients());

            // --- Scroll momentum ---
            data.state.apply_scroll_momentum();

            // --- Sync camera → output position ---
            data.state.update_output_from_camera();

            // --- Take backend to split borrow from state ---
            let mut backend = data.state.backend.take().unwrap();

            // --- Build cursor element ---
            let cursor_elements = build_cursor_elements(
                &mut data.state,
                backend.renderer(),
            );

            // --- Render ---
            let age = backend.buffer_age().unwrap_or(0);
            let render_ok = match backend.bind() {
                Ok((renderer, mut framebuffer)) => {
                    // Collect space elements (windows)
                    let space_result = space_render_elements(
                        renderer,
                        [&data.state.space],
                        &output,
                        1.0,
                    );
                    let space_elements = match space_result {
                        Ok(elems) => elems,
                        Err(err) => {
                            tracing::warn!("Space render elements error: {err:?}");
                            vec![]
                        }
                    };

                    // Build background element
                    let bg_elements = build_background_elements(
                        &data.state,
                        renderer,
                        &output,
                    );

                    // Compose all elements: cursor (top) → windows → background (bottom)
                    let clear_color = data.state.config.background.bg_color;
                    let mut all_elements: Vec<OutputRenderElements> = Vec::with_capacity(
                        cursor_elements.len() + space_elements.len() + bg_elements.len(),
                    );
                    all_elements.extend(cursor_elements.into_iter().map(OutputRenderElements::Cursor));
                    all_elements.extend(space_elements.into_iter().map(OutputRenderElements::Space));
                    all_elements.extend(bg_elements);

                    let result = damage_tracker.render_output(
                        renderer,
                        &mut framebuffer,
                        age,
                        &all_elements,
                        clear_color,
                    );
                    if let Err(err) = result {
                        tracing::warn!("Render error: {err:?}");
                    }
                    true
                }
                Err(err) => {
                    tracing::warn!("Backend bind error: {err:?}");
                    false
                }
            };
            if render_ok
                && let Err(err) = backend.submit(None)
            {
                tracing::warn!("Submit error: {err:?}");
            }

            // --- Put backend back ---
            data.state.backend = Some(backend);

            // --- Post-render: send frame callbacks to clients ---
            let time = data.state.start_time.elapsed();
            for window in data.state.space.elements() {
                window.send_frame(
                    &output,
                    time,
                    Some(Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            }

            // --- Cleanup ---
            data.state.space.refresh();
            data.state.popups.cleanup();
            log_err("flush_clients", data.display.flush_clients());

            TimeoutAction::ToDuration(Duration::from_millis(16))
        })?;

    Ok(())
}

/// Build the background render element(s) for the current frame.
///
/// Priority: shader > tile > solid bg_color (via clear color, returns empty).
fn build_background_elements(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    let scale = output.current_scale().integer_scale();
    let output_size = output
        .current_mode()
        .map(|m| m.size.to_logical(scale))
        .unwrap_or((1, 1).into());

    // Shader background
    if let Some(shader) = state.background_shader.clone() {
        let bg = &state.config.background;
        let area = Rectangle::from_size(output_size);
        let uniforms = vec![
            Uniform::new("u_camera", (state.camera.x as f32, state.camera.y as f32)),
            Uniform::new("u_bg_color", bg.bg_color),
            // Pass zero-values for optional uniforms — shaders ignore what they don't use
            Uniform::new("u_dot_color", [0.0f32; 4]),
            Uniform::new("u_dot_spacing", 0.0f32),
        ];

        return vec![OutputRenderElements::Background(PixelShaderElement::new(
            shader,
            area,
            Some(vec![area]),
            1.0,
            uniforms,
            Kind::Unspecified,
        ))];
    }

    // Tile background — repeat the image across the visible viewport
    if let Some((tile_buf, tw, th)) = &state.background_tile {
        let tw = *tw;
        let th = *th;
        if tw > 0 && th > 0 {
            let cam_x = state.camera.x;
            let cam_y = state.camera.y;

            // First visible tile: snap camera to tile grid (floor toward negative infinity)
            let start_x = (cam_x / tw as f64).floor() as i64 * tw as i64;
            let start_y = (cam_y / th as f64).floor() as i64 * th as i64;

            let mut elements = Vec::new();
            let end_x = (cam_x + output_size.w as f64).ceil() as i64;
            let end_y = (cam_y + output_size.h as f64).ceil() as i64;
            let mut ty = start_y;
            while ty < end_y {
                let mut tx = start_x;
                while tx < end_x {
                    // Canvas position → screen position (subtract camera)
                    let screen_x = tx as f64 - cam_x;
                    let screen_y = ty as f64 - cam_y;
                    let pos: Point<f64, Physical> = (screen_x, screen_y).into();

                    if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
                        renderer, pos, tile_buf, None, None, None, Kind::Unspecified,
                    ) {
                        elements.push(OutputRenderElements::Cursor(elem));
                    }
                    tx += tw as i64;
                }
                ty += th as i64;
            }
            return elements;
        }
    }

    // No background source — solid bg_color via clear color
    vec![]
}

/// Resolve which xcursor name to load for the current cursor status.
fn cursor_icon_name(status: &CursorImageStatus) -> Option<&'static str> {
    match status {
        CursorImageStatus::Hidden => None,
        CursorImageStatus::Named(icon) => Some(icon.name()),
        // Client-provided surface cursor — fall back to default for now
        CursorImageStatus::Surface(_) => Some("default"),
    }
}

/// Build the cursor render element(s) for the current frame.
fn build_cursor_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
) -> Vec<MemoryRenderBufferRenderElement<GlesRenderer>> {
    let pointer = state.seat.get_pointer().unwrap();
    let canvas_pos = pointer.current_location();
    // Custom elements are in screen-local physical coords
    let screen_pos = canvas_to_screen(CanvasPos(canvas_pos), state.camera).0;
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
        renderer, pos, buffer, None, None, None, Kind::Cursor,
    ) {
        Ok(elem) => vec![elem],
        Err(_) => vec![],
    }
}
