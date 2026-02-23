use smithay::{
    backend::{
        renderer::{
            ImportDma,
            damage::OutputDamageTracker,
            element::{Kind, memory::MemoryRenderBufferRenderElement},
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent},
    },
    desktop::space::render_output,
    input::pointer::CursorImageStatus,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        EventLoop,
    },
    utils::{Physical, Point, Transform},
};
use std::time::Duration;

use driftwm::canvas::{CanvasPos, canvas_to_screen};
use crate::state::{CalloopData, log_err};

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
                    let result = render_output(
                        &output,
                        renderer,
                        &mut framebuffer,
                        1.0, // alpha
                        age,
                        [&data.state.space],
                        &cursor_elements,
                        &mut damage_tracker,
                        [0.1, 0.1, 0.1, 1.0], // dark grey background
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
