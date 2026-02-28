use smithay::{
    backend::{
        renderer::{
            ImportDma,
            damage::OutputDamageTracker,
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::{
        EventLoop,
        timer::{TimeoutAction, Timer},
    },
    utils::{Point, Transform},
};
use std::time::Duration;

use crate::render::build_cursor_elements;
use crate::backend::Backend;
use crate::state::{CalloopData, log_err};

/// Initialize the winit backend: create a window, set up the output, and
/// start the render loop timer.
pub fn init_winit(
    event_loop: &mut EventLoop<'static, CalloopData>,
    data: &mut CalloopData,
) -> Result<(), Box<dyn std::error::Error>> {
    let (backend, mut winit_evt) = winit::init::<GlesRenderer>()?;
    let size = backend.window_size();

    // Store backend on state so protocol handlers can access the renderer
    data.state.backend = Some(Backend::Winit(Box::new(backend)));
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
    let formats = data
        .state
        .backend
        .as_mut()
        .unwrap()
        .renderer()
        .dmabuf_formats();
    let dmabuf_global = data
        .state
        .dmabuf_state
        .create_global::<crate::state::DriftWm>(&data.display.handle(), formats);
    data.state.dmabuf_global = Some(dmabuf_global);

    crate::render::init_background(&mut data.state, size.to_logical(1));

    // Centre the viewport so canvas origin (0, 0) is in the middle of the screen
    let logical_size = size.to_logical(1);
    data.state.camera = Point::from((
        -(logical_size.w as f64) / 2.0,
        -(logical_size.h as f64) / 2.0,
    ));

    // Map the output into the space at the initial camera position
    data.state
        .space
        .map_output(&output, data.state.camera.to_i32_round());

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
            log_err(
                "dispatch_clients",
                data.display.dispatch_clients(&mut data.state),
            );
            log_err("flush_clients", data.display.flush_clients());

            // --- Delta time ---
            let now = std::time::Instant::now();
            let dt = (now - data.state.last_frame_instant).min(std::time::Duration::from_millis(33));
            data.state.last_frame_instant = now;

            // --- Key repeat for compositor bindings ---
            data.state.apply_key_repeat();

            // --- Scroll momentum ---
            data.state.apply_scroll_momentum();

            // --- Edge auto-pan (window drag near viewport edges) ---
            data.state.apply_edge_pan();

            // --- Camera animation (window navigation) ---
            data.state.apply_camera_animation(dt);

            // --- Zoom animation ---
            data.state.apply_zoom_animation(dt);

            // --- Update cached background element ---
            let (camera_moved, zoom_changed) =
                crate::render::update_background_element(&mut data.state, &output);

            // --- Take backend to split borrow from state ---
            let Backend::Winit(mut backend) = data.state.backend.take().unwrap()  else {
                unreachable!("winit timer with non-winit backend");
            };

            // --- Build cursor + compose frame ---
            let cursor_elements = build_cursor_elements(&mut data.state, backend.renderer());
            let mut age = backend.buffer_age().unwrap_or(0);
            if data.state.background_tile.is_some() && (camera_moved || zoom_changed) {
                age = 0;
            }
            let render_ok = match backend.bind() {
                Ok((renderer, mut framebuffer)) => {
                    let all_elements =
                        crate::render::compose_frame(&data.state, renderer, &output, cursor_elements);
                    let result = damage_tracker.render_output(
                        renderer,
                        &mut framebuffer,
                        age,
                        &all_elements,
                        [0.0f32, 0.0, 0.0, 1.0],
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
            if render_ok && let Err(err) = backend.submit(None) {
                tracing::warn!("Submit error: {err:?}");
            }

            // --- Record camera+zoom for next-frame change detection ---
            data.state.last_rendered_camera = data.state.camera;
            data.state.last_rendered_zoom = data.state.zoom;
            data.state.write_state_file_if_dirty();

            // --- Put backend back ---
            data.state.backend = Some(Backend::Winit(backend));

            // --- Post-render ---
            crate::render::post_render(&mut data.state, &output);
            log_err("flush_clients", data.display.flush_clients());

            TimeoutAction::ToDuration(Duration::from_millis(16))
        })?;

    Ok(())
}
