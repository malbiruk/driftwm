use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Physical, Rectangle, Scale, Size, Transform};

use super::OutputRenderElements;

/// Get or create persistent capture state for an output+protocol pair.
fn get_capture_state<'a>(
    map: &'a mut std::collections::HashMap<String, crate::state::CaptureOutputState>,
    key: &str,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    paint_cursors: bool,
) -> &'a mut crate::state::CaptureOutputState {
    map.entry(key.to_owned()).or_insert_with(|| {
        crate::state::CaptureOutputState {
            damage_tracker: smithay::backend::renderer::damage::OutputDamageTracker::new(size, scale, transform),
            offscreen_texture: None,
            age: 0,
            last_paint_cursors: paint_cursors,
        }
    })
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
    let output_mode_size = output.current_mode().unwrap().size;
    let timestamp = state.start_time.elapsed();
    let capture_key = format!("sc:{}", output.name());

    for screencopy in pending {
        let size = screencopy.buffer_size();
        let paint_cursors = screencopy.overlay_cursor();
        let use_elements: Vec<&OutputRenderElements> = if paint_cursors {
            elements.iter().collect()
        } else {
            elements
                .iter()
                .filter(|e| !matches!(e, OutputRenderElements::Cursor(_) | OutputRenderElements::CursorSurface(_)))
                .collect()
        };

        // Use persistent state for full-output captures (screen recording);
        // one-shot for region captures (partial screenshots).
        let use_persistent = size == output_mode_size;

        if use_persistent
            && let Some(cs) = state.render.capture_state.get_mut(&capture_key)
            && cs.last_paint_cursors != paint_cursors
        {
            cs.age = 0;
            cs.last_paint_cursors = paint_cursors;
        }

        match screencopy.buffer() {
            ScreencopyBuffer::Dmabuf(dmabuf) => {
                let mut dmabuf = dmabuf.clone();
                let cs = if use_persistent {
                    Some(get_capture_state(&mut state.render.capture_state, &capture_key, size, scale, transform, paint_cursors))
                } else {
                    None
                };
                match render_to_dmabuf(renderer, &mut dmabuf, size, scale, transform, &use_elements, cs) {
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
                let cs = if use_persistent {
                    Some(get_capture_state(&mut state.render.capture_state, &capture_key, size, scale, transform, paint_cursors))
                } else {
                    None
                };
                let result = render_to_offscreen(renderer, size, scale, transform, &use_elements, cs);
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
/// When `capture_state` is provided, reuses the damage tracker and texture across frames
/// for incremental rendering. Falls back to one-shot (age=0) when None.
fn render_to_offscreen(
    renderer: &mut GlesRenderer,
    size: smithay::utils::Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: &[&OutputRenderElements],
    capture_state: Option<&mut crate::state::CaptureOutputState>,
) -> Result<smithay::backend::renderer::gles::GlesMapping, Box<dyn std::error::Error>> {
    use smithay::backend::renderer::{Bind, ExportMem, Offscreen};
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::backend::renderer::gles::GlesTexture;

    let buffer_size = size.to_logical(1).to_buffer(1, Transform::Normal);

    if let Some(cs) = capture_state {
        // Reuse or reallocate texture when size changes
        let tex = match &mut cs.offscreen_texture {
            Some((tex, cached_size)) if *cached_size == size => tex,
            slot => {
                let new_tex: GlesTexture = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Xrgb8888, buffer_size)?;
                *slot = Some((new_tex, size));
                cs.damage_tracker = OutputDamageTracker::new(size, scale, transform);
                cs.age = 0;
                &mut slot.as_mut().unwrap().0
            }
        };

        {
            let mut target = renderer.bind(tex)?;
            let _ = cs.damage_tracker.render_output(
                renderer,
                &mut target,
                cs.age,
                elements,
                [0.0f32, 0.0, 0.0, 1.0],
            )?;
        }
        cs.age += 1;

        let target = renderer.bind(tex)?;
        let mapping = renderer.copy_framebuffer(&target, Rectangle::from_size(buffer_size), Fourcc::Xrgb8888)?;
        Ok(mapping)
    } else {
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
        let target = renderer.bind(&mut texture)?;
        let mapping = renderer.copy_framebuffer(&target, Rectangle::from_size(buffer_size), Fourcc::Xrgb8888)?;
        Ok(mapping)
    }
}

/// Render elements directly into a client-provided DMA-BUF (zero CPU copies).
///
/// The caller must choose the correct `transform` for the protocol:
/// - wlr-screencopy: `output.current_transform()` (buffer is raw mode size)
/// - ext-image-copy-capture: `Transform::Normal` (buffer is already transformed)
///
/// When `capture_state` is provided, reuses the damage tracker for incremental rendering.
fn render_to_dmabuf(
    renderer: &mut GlesRenderer,
    dmabuf: &mut smithay::backend::allocator::dmabuf::Dmabuf,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: &[&OutputRenderElements],
    capture_state: Option<&mut crate::state::CaptureOutputState>,
) -> Result<smithay::backend::renderer::sync::SyncPoint, Box<dyn std::error::Error>> {
    use smithay::backend::renderer::Bind;
    use smithay::backend::renderer::damage::OutputDamageTracker;

    let sync = match capture_state {
        Some(cs) => {
            let mut target = renderer.bind(dmabuf)?;
            let result = cs.damage_tracker.render_output(
                renderer,
                &mut target,
                cs.age,
                elements,
                [0.0f32, 0.0, 0.0, 1.0],
            )?.sync;
            cs.age += 1;
            result
        }
        None => {
            let mut target = renderer.bind(dmabuf)?;
            let mut damage_tracker = OutputDamageTracker::new(size, scale, transform);
            damage_tracker.render_output(
                renderer,
                &mut target,
                0,
                elements,
                [0.0f32, 0.0, 0.0, 1.0],
            )?.sync
        }
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
    let output_transform = output.current_transform();
    let output_mode_size = output_transform.transform_size(output.current_mode().unwrap().size);
    let timestamp = state.start_time.elapsed();
    let capture_key = format!("cap:{}", output.name());

    let fail_reason = smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason::Unknown;

    for capture in pending {
        let paint_cursors = capture.paint_cursors;
        let use_elements: Vec<&OutputRenderElements> = if paint_cursors {
            elements.iter().collect()
        } else {
            elements
                .iter()
                .filter(|e| !matches!(e, OutputRenderElements::Cursor(_) | OutputRenderElements::CursorSurface(_)))
                .collect()
        };

        // ext-image-copy-capture buffer_size is already in transformed/logical orientation,
        // matching the element coordinate space — render with Normal (no additional transform).
        let use_persistent = capture.buffer_size == output_mode_size;

        if use_persistent
            && let Some(cs) = state.render.capture_state.get_mut(&capture_key)
            && cs.last_paint_cursors != paint_cursors
        {
            cs.age = 0;
            cs.last_paint_cursors = paint_cursors;
        }

        // Try DMA-BUF first, fall back to SHM
        let ok = if let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(&capture.buffer) {
            let mut dmabuf = dmabuf.clone();
            let cs = if use_persistent {
                Some(get_capture_state(&mut state.render.capture_state, &capture_key, capture.buffer_size, scale, Transform::Normal, paint_cursors))
            } else {
                None
            };
            match render_to_dmabuf(renderer, &mut dmabuf, capture.buffer_size, scale, Transform::Normal, &use_elements, cs) {
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
            let cs = if use_persistent {
                Some(get_capture_state(&mut state.render.capture_state, &capture_key, capture.buffer_size, scale, Transform::Normal, paint_cursors))
            } else {
                None
            };
            let result = render_to_offscreen(renderer, capture.buffer_size, scale, Transform::Normal, &use_elements, cs);
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

/// Fulfill pending `hyprland-toplevel-export-v1` copy requests by rendering
/// each window into the client-provided SHM or DMA-BUF buffer.
/// Fulfill pending `hyprland-toplevel-export-v1` frames by rendering the requested window
/// into the client-provided buffer (DMA-BUF preferred, SHM fallback).
///
/// Called once per render loop iteration from both the udev and winit backends.
pub fn render_hyprland_toplevel_exports(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
) {
    use smithay::backend::renderer::element::AsRenderElements;
    use smithay::backend::renderer::{ExportMem, Renderer};
    use smithay::utils::Point;
    use smithay::wayland::shm;
    use std::ptr;

    use driftwm::protocols::hyprland_toplevel_export::proto::hyprland_toplevel_export_frame_v1::Flags as ToplevelExportFlags;
    use super::OutputRenderElements;

    let pending = std::mem::take(&mut state.pending_hyprland_exports);
    if pending.is_empty() {
        return;
    }

    let timestamp = state.start_time.elapsed();

    for export in pending {
        let frame = export.frame;
        let window = export.window;
        let size = export.buffer_size;
        let buffer = export.buffer;

        // Sanity-check the size; the frame Dispatch should already reject (0,0)
        if size.w <= 0 || size.h <= 0 {
            frame.failed();
            continue;
        }

        // Render the window surfaces at the physical origin
        let raw_elements = window
            .render_elements::<smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<GlesRenderer>>(
                renderer,
                Point::from((0, 0)),
                Scale::from(1.0),
                1.0,
            );

        let elements: Vec<OutputRenderElements> =
            raw_elements.into_iter().map(OutputRenderElements::Layer).collect();
        let element_refs: Vec<&OutputRenderElements> = elements.iter().collect();

        // Try DMA-BUF path first
        if let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(&buffer) {
            let mut dmabuf = dmabuf.clone();
            match render_to_dmabuf(
                renderer,
                &mut dmabuf,
                size,
                Scale::from(1.0),
                Transform::Normal,
                &element_refs,
                None,
            ) {
                Ok(sync) => {
                    if let Err(e) = renderer.wait(&sync) {
                        tracing::warn!("toplevel export: dmabuf sync failed: {e:?}");
                        frame.failed();
                    } else {
                        let secs = timestamp.as_secs();
                        frame.flags(ToplevelExportFlags::empty());
                        frame.ready(
                            (secs >> 32) as u32,
                            (secs & 0xffffffff) as u32,
                            timestamp.subsec_nanos(),
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("toplevel export: dmabuf render failed: {e:?}");
                    frame.failed();
                }
            }
            continue;
        }

        // SHM path
        let result = render_to_offscreen(
            renderer,
            size,
            Scale::from(1.0),
            Transform::Normal,
            &element_refs,
            None,
        );

        match result {
            Ok(mapping) => {
                let copy_ok = shm::with_buffer_contents_mut(&buffer, |shm_buf, shm_len, _| {
                    let bytes = match renderer.map_texture(&mapping) {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::warn!("toplevel export: map_texture failed: {e:?}");
                            return false;
                        }
                    };
                    let copy_len = shm_len.min(bytes.len());
                    unsafe {
                        ptr::copy_nonoverlapping(bytes.as_ptr(), shm_buf.cast(), copy_len);
                    }
                    true
                });

                if matches!(copy_ok, Ok(true)) {
                    let secs = timestamp.as_secs();
                    frame.flags(ToplevelExportFlags::empty());
                    frame.ready(
                        (secs >> 32) as u32,
                        (secs & 0xffffffff) as u32,
                        timestamp.subsec_nanos(),
                    );
                } else {
                    tracing::warn!("toplevel export: SHM copy failed");
                    frame.failed();
                }
            }
            Err(e) => {
                tracing::warn!("toplevel export: offscreen render failed: {e:?}");
                frame.failed();
            }
        }
    }
}
