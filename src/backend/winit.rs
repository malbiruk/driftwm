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
use crate::state::{DriftWm, init_output_state};

/// Initialize the winit backend: create a window, set up the output, and
/// start the render loop timer.
pub fn init_winit(
    event_loop: &mut EventLoop<'static, DriftWm>,
    data: &mut DriftWm,
) -> Result<(), Box<dyn std::error::Error>> {
    let (backend, mut winit_evt) = winit::init::<GlesRenderer>()?;
    let size = backend.window_size();

    // Store backend on state so protocol handlers can access the renderer
    data.backend = Some(Backend::Winit(Box::new(backend)));
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
    output.create_global::<crate::state::DriftWm>(&data.display_handle);

    // Create DMA-BUF global — advertise GPU buffer formats to clients
    let formats = data
        .backend
        .as_mut()
        .unwrap()
        .renderer()
        .dmabuf_formats();
    let dmabuf_global = data
        .dmabuf_state
        .create_global::<crate::state::DriftWm>(&data.display_handle, formats);
    data.dmabuf_global = Some(dmabuf_global);

    {
        let mut backend = data.backend.take().unwrap();
        crate::render::init_background(data, backend.renderer(), size.to_logical(1), "winit");
        data.shadow_shader = crate::render::compile_shadow_shader(backend.renderer());
        data.backend = Some(backend);
    }

    // Centre the viewport so canvas origin (0, 0) is in the middle of the screen
    let logical_size = size.to_logical(1);
    let initial_camera = Point::from((
        -(logical_size.w as f64) / 2.0,
        -(logical_size.h as f64) / 2.0,
    ));

    // Initialize per-output state for this output
    init_output_state(&output, initial_camera, data.config.friction, Point::from((0, 0)));
    data.focused_output = Some(output.clone());

    // Map the output into the space at the initial camera position
    data
        .space
        .map_output(&output, initial_camera.to_i32_round());

    // Notify output management clients about the winit output
    {
        use driftwm::protocols::output_management::{OutputHeadState, ModeInfo};
        let mut heads = std::collections::HashMap::new();
        heads.insert(
            "winit".to_string(),
            OutputHeadState {
                name: "winit".to_string(),
                description: "driftwm winit virtual output".to_string(),
                make: "driftwm".to_string(),
                model: "winit".to_string(),
                serial_number: String::new(),
                physical_size: (0, 0),
                modes: vec![ModeInfo {
                    width: size.w,
                    height: size.h,
                    refresh: 60_000,
                    preferred: true,
                }],
                current_mode_index: Some(0),
                position: (0, 0),
                transform: Transform::Flipped180,
                scale: 1.0,
            },
        );
        driftwm::protocols::output_management::notify_changes::<crate::state::DriftWm>(
            &mut data.output_management_state,
            heads,
        );
    }

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    // Render loop: fires immediately, then re-arms at ~60fps
    let timer = Timer::immediate();
    event_loop
        .handle()
        .insert_source(timer, move |_, _, data| {
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
                    data.process_input_event(event);
                }
                WinitEvent::CloseRequested => {
                    stop = true;
                }
                _ => {}
            });

            if stop {
                data.loop_signal.stop();
                return TimeoutAction::Drop;
            }

            // --- Flush Wayland client messages before rendering ---
            data.display_handle.flush_clients().ok();

            // --- Delta time ---
            let now = std::time::Instant::now();
            let dt = (now - data.last_frame_instant()).min(std::time::Duration::from_millis(33));
            data.set_last_frame_instant(now);

            // --- Key repeat for compositor bindings ---
            data.apply_key_repeat();

            // --- Scroll momentum ---
            data.apply_scroll_momentum(dt);

            // --- Edge auto-pan (window drag near viewport edges) ---
            data.apply_edge_pan();

            // --- Zoom animation (before camera so recomputed target is used) ---
            data.apply_zoom_animation(dt);

            // --- Camera animation (window navigation) ---
            data.apply_camera_animation(dt);

            // --- Exec loading cursor timeout ---
            data.check_exec_cursor_timeout();

            // --- Read per-output state for this frame ---
            let (cur_camera, cur_zoom, last_cam, last_zoom) = {
                let os = crate::state::output_state(&output);
                (os.camera, os.zoom, os.last_rendered_camera, os.last_rendered_zoom)
            };

            // --- Update cached background element ---
            let (camera_moved, zoom_changed) =
                crate::render::update_background_element(data, &output, cur_camera, cur_zoom, last_cam, last_zoom);

            // --- Take backend to split borrow from state ---
            let Backend::Winit(mut backend) = data.backend.take().unwrap()  else {
                unreachable!("winit timer with non-winit backend");
            };

            // --- Build cursor + compose frame ---
            let cursor_elements = build_cursor_elements(data, backend.renderer(), cur_camera, cur_zoom, 1.0);
            let mut age = backend.buffer_age().unwrap_or(0);
            if data.background_tile.is_some() && (camera_moved || zoom_changed) {
                age = 0;
            }
            let render_ok = match backend.bind() {
                Ok((renderer, mut framebuffer)) => {
                    let all_elements =
                        crate::render::compose_frame(data, renderer, &output, cursor_elements);
                    crate::render::render_screencopy(data, renderer, &output, &all_elements);
                    crate::render::render_capture_frames(data, renderer, &output, &all_elements);
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
            {
                let mut os = crate::state::output_state(&output);
                os.last_rendered_camera = os.camera;
                os.last_rendered_zoom = os.zoom;
            }
            data.write_state_file_if_dirty();

            // --- Put backend back ---
            data.backend = Some(Backend::Winit(backend));

            // --- Post-render ---
            crate::render::refresh_foreign_toplevels(data);
            crate::render::post_render(data, &output);
            data.display_handle.flush_clients().ok();

            TimeoutAction::ToDuration(Duration::from_millis(16))
        })?;

    Ok(())
}
