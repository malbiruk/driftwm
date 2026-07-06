use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::RenderElement;
use smithay::output::Output;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Physical, Rectangle, Scale, Size, Transform};
use smithay::wayland::seat::WaylandFocus;

use super::OutputRenderElements;
use super::renderer::DriftRenderer;

/// Get or create persistent capture state for an output+protocol pair.
fn get_capture_state<'a>(
    map: &'a mut std::collections::HashMap<String, crate::state::CaptureOutputState>,
    key: &str,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    paint_cursors: bool,
    now: std::time::Duration,
) -> &'a mut crate::state::CaptureOutputState {
    let cs = map
        .entry(key.to_owned())
        .or_insert_with(|| crate::state::CaptureOutputState {
            damage_tracker: smithay::backend::renderer::damage::OutputDamageTracker::new(
                size, scale, transform,
            ),
            offscreen_texture: None,
            age: 0,
            last_paint_cursors: paint_cursors,
            last_used: now,
            last_submit: None,
        });
    cs.last_used = now;
    cs
}

/// Minimum interval between capture frames for a `max_capture_fps` setting
/// (0 = unlimited → `None`).
fn capture_min_interval(max_fps: u32) -> Option<std::time::Duration> {
    (max_fps > 0).then(|| std::time::Duration::from_secs_f64(1.0 / max_fps as f64))
}

/// Record a successful screencopy submit time for `max_capture_fps` throttling.
/// Per-output (shared across all wlr-screencopy clients on the output), so a
/// concurrent one-shot grab can briefly throttle a recorder — acceptable.
fn stamp_capture_submit(
    map: &mut std::collections::HashMap<String, crate::state::CaptureOutputState>,
    key: &str,
    use_persistent: bool,
    now: std::time::Duration,
) {
    if use_persistent && let Some(cs) = map.get_mut(key) {
        cs.last_submit = Some(now);
    }
}

/// Fulfill pending screencopy requests by rendering to offscreen textures.
pub fn render_screencopy<R: DriftRenderer>(
    state: &mut crate::state::DriftWm,
    renderer: &mut R,
    output: &Output,
    elements: &[OutputRenderElements<R>],
) where
    OutputRenderElements<R>: RenderElement<R>,
{
    use driftwm::protocols::screencopy::ScreencopyBuffer;

    use smithay::wayland::shm;
    use std::ptr;

    let timestamp = state.start_time.elapsed();
    let min_interval = capture_min_interval(state.config.backend.max_capture_fps);
    let capture_key = format!("sc:{}", output.name());

    // Only copy_with_damage captures are rate-limited by max_capture_fps; a
    // too-soon frame stays queued and retries on a later render. Plain one-shot
    // copy (e.g. grim) is never throttled.
    let throttle_with_damage = min_interval.is_some_and(|iv| {
        state
            .render
            .capture_state
            .get(&capture_key)
            .and_then(|cs| cs.last_submit)
            .is_some_and(|last| timestamp.saturating_sub(last) < iv)
    });

    // Extract requests for this output; rate-limited copy_with_damage frames
    // stay queued.
    let mut pending = Vec::new();
    let mut i = 0;
    while i < state.pending_screencopies.len() {
        let sc = &state.pending_screencopies[i];
        if sc.output() == output && !(throttle_with_damage && sc.with_damage()) {
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

    for screencopy in pending {
        let size = screencopy.buffer_size();
        let paint_cursors = screencopy.overlay_cursor();
        let use_elements: Vec<&OutputRenderElements<R>> = if paint_cursors {
            elements.iter().collect()
        } else {
            elements
                .iter()
                .filter(|e| {
                    !matches!(
                        e,
                        OutputRenderElements::Cursor(_) | OutputRenderElements::CursorSurface(_)
                    )
                })
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
                    Some(get_capture_state(
                        &mut state.render.capture_state,
                        &capture_key,
                        size,
                        scale,
                        transform,
                        paint_cursors,
                        timestamp,
                    ))
                } else {
                    None
                };
                match render_to_dmabuf(
                    renderer,
                    &mut dmabuf,
                    size,
                    scale,
                    transform,
                    &use_elements,
                    cs,
                ) {
                    Ok(sync) => {
                        if let Err(e) = renderer.wait(&sync) {
                            tracing::warn!("screencopy: dmabuf sync wait failed: {e:?}");
                            continue; // screencopy Drop sends failed()
                        }
                        stamp_capture_submit(
                            &mut state.render.capture_state,
                            &capture_key,
                            use_persistent,
                            timestamp,
                        );
                        screencopy.submit(false, timestamp);
                    }
                    Err(e) => {
                        tracing::warn!("screencopy: dmabuf render failed: {e:?}");
                    }
                }
            }
            ScreencopyBuffer::Shm(wl_buffer) => {
                let cs = if use_persistent {
                    Some(get_capture_state(
                        &mut state.render.capture_state,
                        &capture_key,
                        size,
                        scale,
                        transform,
                        paint_cursors,
                        timestamp,
                    ))
                } else {
                    None
                };
                let result = render_to_offscreen(
                    renderer,
                    size,
                    scale,
                    transform,
                    Fourcc::Xrgb8888,
                    &use_elements,
                    cs,
                );
                match result {
                    Ok(bytes) => {
                        let copy_ok =
                            shm::with_buffer_contents_mut(wl_buffer, |shm_buf, shm_len, _data| {
                                let copy_len = shm_len.min(bytes.len());
                                unsafe {
                                    ptr::copy_nonoverlapping(
                                        bytes.as_ptr(),
                                        shm_buf.cast(),
                                        copy_len,
                                    );
                                }
                            });

                        match copy_ok {
                            Ok(()) => {
                                stamp_capture_submit(
                                    &mut state.render.capture_state,
                                    &capture_key,
                                    use_persistent,
                                    timestamp,
                                );
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
/// Clear color for a given fourcc — opaque black for X-prefixed (no alpha
/// channel), transparent for A-prefixed (alpha preserved through render).
fn clear_color_for(format: Fourcc) -> [f32; 4] {
    match format {
        Fourcc::Argb8888
        | Fourcc::Abgr8888
        | Fourcc::Rgba8888
        | Fourcc::Bgra8888
        | Fourcc::Argb2101010
        | Fourcc::Abgr2101010
        | Fourcc::Rgba1010102
        | Fourcc::Bgra1010102
        | Fourcc::Argb16161616f
        | Fourcc::Abgr16161616f => [0.0, 0.0, 0.0, 0.0],
        _ => [0.0, 0.0, 0.0, 1.0],
    }
}

fn render_to_offscreen<R: DriftRenderer>(
    renderer: &mut R,
    size: smithay::utils::Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    format: Fourcc,
    elements: &[&OutputRenderElements<R>],
    capture_state: Option<&mut crate::state::CaptureOutputState>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>>
where
    OutputRenderElements<R>: RenderElement<R>,
{
    use smithay::backend::renderer::Offscreen;
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::backend::renderer::gles::GlesTexture;

    let buffer_size = size.to_logical(1).to_buffer(1, Transform::Normal);
    let clear = clear_color_for(format);

    // Bind, render AND read back all through R: on a cross-GPU MultiRenderer,
    // Offscreen/Bind allocate the texture on the scanout GPU, so a readback via
    // the primary GlesRenderer would bind it on the wrong GL context.
    // R's ExportMem routes copy_framebuffer/map_texture to the owning GPU.
    if let Some(cs) = capture_state {
        // Reuse or reallocate texture when size changes
        let tex = match &mut cs.offscreen_texture {
            Some((tex, cached_size)) if *cached_size == size => tex,
            slot => {
                let new_tex: GlesTexture =
                    Offscreen::<GlesTexture>::create_buffer(renderer, format, buffer_size)?;
                *slot = Some((new_tex, size));
                cs.damage_tracker = OutputDamageTracker::new(size, scale, transform);
                cs.age = 0;
                &mut slot.as_mut().unwrap().0
            }
        };

        let mut target = renderer.bind(tex)?;
        let _ = cs
            .damage_tracker
            .render_output(renderer, &mut target, cs.age, elements, clear)?;
        cs.age += 1;

        let mapping =
            renderer.copy_framebuffer(&target, Rectangle::from_size(buffer_size), format)?;
        Ok(renderer.map_texture(&mapping)?.to_vec())
    } else {
        let mut texture: GlesTexture =
            Offscreen::<GlesTexture>::create_buffer(renderer, format, buffer_size)?;
        let mut target = renderer.bind(&mut texture)?;
        let mut damage_tracker = OutputDamageTracker::new(size, scale, transform);
        let _ = damage_tracker.render_output(renderer, &mut target, 0, elements, clear)?;
        let mapping =
            renderer.copy_framebuffer(&target, Rectangle::from_size(buffer_size), format)?;
        Ok(renderer.map_texture(&mapping)?.to_vec())
    }
}

/// One-shot offscreen render of `elements` into a tightly-packed RGBA8 buffer
/// (`image`-crate byte order). `size` is the buffer's physical pixel size;
/// `scale` matches the scale the elements were composed at. `Abgr8888` puts
/// bytes in R,G,B,A order with a transparent clear, so content lands on a
/// transparent canvas.
pub(crate) fn render_elements_to_rgba<R: DriftRenderer>(
    renderer: &mut R,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    elements: &[&OutputRenderElements<R>],
) -> Result<Vec<u8>, String>
where
    OutputRenderElements<R>: RenderElement<R>,
{
    render_to_offscreen(
        renderer,
        size,
        scale,
        Transform::Normal,
        Fourcc::Abgr8888,
        elements,
        None,
    )
    .map_err(|e| format!("offscreen render failed: {e:?}"))
}

/// Render elements directly into a client-provided DMA-BUF (zero CPU copies).
///
/// The caller passes the output transform so the buffer is filled in scanout
/// orientation (raw mode size); the client orients it via the output transform
/// (ext-image-copy-capture in the frame's `transform` event, wlr-screencopy
/// out-of-band from `wl_output`).
///
/// When `capture_state` is provided, reuses the damage tracker for incremental rendering.
fn render_to_dmabuf<R: DriftRenderer>(
    renderer: &mut R,
    dmabuf: &mut smithay::backend::allocator::dmabuf::Dmabuf,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: &[&OutputRenderElements<R>],
    capture_state: Option<&mut crate::state::CaptureOutputState>,
) -> Result<smithay::backend::renderer::sync::SyncPoint, Box<dyn std::error::Error>>
where
    OutputRenderElements<R>: RenderElement<R>,
{
    use smithay::backend::allocator::Buffer;
    use smithay::backend::renderer::damage::OutputDamageTracker;

    let clear = clear_color_for(dmabuf.format().code);
    let sync = match capture_state {
        Some(cs) => {
            let mut target = renderer.bind(dmabuf)?;
            let result = cs
                .damage_tracker
                .render_output(renderer, &mut target, cs.age, elements, clear)?
                .sync;
            cs.age += 1;
            result
        }
        None => {
            let mut target = renderer.bind(dmabuf)?;
            let mut damage_tracker = OutputDamageTracker::new(size, scale, transform);
            damage_tracker
                .render_output(renderer, &mut target, 0, elements, clear)?
                .sync
        }
    };

    Ok(sync)
}

/// Fulfill pending ext-image-copy-capture frames by rendering to offscreen textures.
pub fn render_capture_frames<R: DriftRenderer>(
    state: &mut crate::state::DriftWm,
    renderer: &mut R,
    output: &Output,
    elements: &[OutputRenderElements<R>],
) where
    OutputRenderElements<R>: RenderElement<R>,
{
    use smithay::wayland::shm;
    use std::ptr;

    let timestamp = state.start_time.elapsed();
    let min_interval = capture_min_interval(state.config.backend.max_capture_fps);

    // Promote sessions waiting for damage on this output (max_capture_fps gated).
    state.image_copy_capture_state.promote_waiting_frames(
        output,
        &mut state.pending_captures,
        timestamp,
        min_interval,
    );

    // Extract captures for this output (toplevel captures are routed
    // separately by `render_toplevel_captures`).
    let mut pending = Vec::new();
    let mut i = 0;
    while i < state.pending_captures.len() {
        let matches = matches!(
            &state.pending_captures[i].kind,
            driftwm::protocols::image_copy_capture::PendingCaptureKind::Output(o) if o == output,
        );
        if matches {
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
    let output_mode_size = output.current_mode().unwrap().size;
    let capture_key = format!("cap:{}", output.name());

    let fail_reason = smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason::Unknown;

    for capture in pending {
        let paint_cursors = capture.paint_cursors;
        let use_elements: Vec<&OutputRenderElements<R>> = if paint_cursors {
            elements.iter().collect()
        } else {
            elements
                .iter()
                .filter(|e| {
                    !matches!(
                        e,
                        OutputRenderElements::Cursor(_) | OutputRenderElements::CursorSurface(_)
                    )
                })
                .collect()
        };

        // Persistent damage-tracker state only applies to full-output captures.
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
                Some(get_capture_state(
                    &mut state.render.capture_state,
                    &capture_key,
                    capture.buffer_size,
                    scale,
                    output_transform,
                    paint_cursors,
                    timestamp,
                ))
            } else {
                None
            };
            match render_to_dmabuf(
                renderer,
                &mut dmabuf,
                capture.buffer_size,
                scale,
                output_transform,
                &use_elements,
                cs,
            ) {
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
                Some(get_capture_state(
                    &mut state.render.capture_state,
                    &capture_key,
                    capture.buffer_size,
                    scale,
                    output_transform,
                    paint_cursors,
                    timestamp,
                ))
            } else {
                None
            };
            let result = render_to_offscreen(
                renderer,
                capture.buffer_size,
                scale,
                output_transform,
                Fourcc::Xrgb8888,
                &use_elements,
                cs,
            );
            match result {
                Ok(bytes) => {
                    shm::with_buffer_contents_mut(&capture.buffer, |shm_buf, shm_len, _data| {
                        let copy_len = shm_len.min(bytes.len());
                        unsafe {
                            ptr::copy_nonoverlapping(bytes.as_ptr(), shm_buf.cast(), copy_len);
                        }
                    })
                    .is_ok()
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
            capture.frame.transform(output_transform.into());
            capture.frame.damage(0, 0, w, h);
            let tv_sec_hi = (timestamp.as_secs() >> 32) as u32;
            let tv_sec_lo = (timestamp.as_secs() & 0xFFFFFFFF) as u32;
            let tv_nsec = timestamp.subsec_nanos();
            capture
                .frame
                .presentation_time(tv_sec_hi, tv_sec_lo, tv_nsec);
            capture.frame.ready();

            let frame_data = capture
                .frame
                .data::<std::sync::Mutex<driftwm::protocols::image_copy_capture::CaptureFrameData>>(
                );
            if let Some(fd) = frame_data {
                let fd = fd.lock().unwrap();
                state.image_copy_capture_state.frame_done(&fd.session);
            }
        } else {
            capture.frame.failed(fail_reason);
        }
    }
}

/// Fulfill pending ext-image-copy-capture frames whose source is a toplevel
/// window. Renders the client surface tree at scale 1, with the window's
/// geometry origin at (0,0) — no SSD chrome, no shadow.
///
/// Currently invoked from each output's render path; the first call drains
/// `pending_captures` so subsequent per-output calls are cheap no-ops.
///
/// Note: `paint_cursors` is silently ignored for toplevel captures —
/// compositing the cursor onto a window-relative buffer requires intersecting
/// the cursor position against the captured window's geometry, which isn't
/// wired in yet. Per-window screencast clients typically don't request it.
pub fn render_toplevel_captures<R: DriftRenderer>(
    state: &mut crate::state::DriftWm,
    renderer: &mut R,
) where
    OutputRenderElements<R>: RenderElement<R>,
{
    use driftwm::protocols::image_copy_capture::PendingCaptureKind;

    use smithay::backend::renderer::element::Kind;
    use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
    use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
    use smithay::utils::Point;
    use smithay::wayland::shm;
    use std::ptr;

    let timestamp = state.start_time.elapsed();
    let min_interval = capture_min_interval(state.config.backend.max_capture_fps);

    state
        .image_copy_capture_state
        .promote_waiting_toplevel_frames(&mut state.pending_captures, timestamp, min_interval);

    let mut pending = Vec::new();
    let mut i = 0;
    while i < state.pending_captures.len() {
        if matches!(
            &state.pending_captures[i].kind,
            PendingCaptureKind::Toplevel(_)
        ) {
            pending.push(state.pending_captures.swap_remove(i));
        } else {
            i += 1;
        }
    }
    if pending.is_empty() {
        return;
    }

    let scale = Scale::from(1.0);
    let fail_reason = smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason::Unknown;

    for capture in pending {
        let PendingCaptureKind::Toplevel(ref surface) = capture.kind else {
            continue;
        };

        // Locate the window. If it's gone or has zero geometry, fail the frame.
        let window = state
            .space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(surface))
            .cloned();
        let Some(window) = window else {
            capture.frame.failed(fail_reason);
            continue;
        };

        let geo = window.geometry();
        if geo.size.w <= 0 || geo.size.h <= 0 {
            capture.frame.failed(fail_reason);
            continue;
        }

        // Shift surface tree so the window's geometry origin lands at (0,0)
        // in the capture buffer. CSD-using clients (GTK, Chromium) report
        // a non-zero geometry offset to skip invisible shadow padding.
        // geometry() is Logical → convert to Physical via the capture scale
        // (currently 1.0; will matter when HiDPI capture lands).
        let origin = Point::<i32, smithay::utils::Logical>::from((-geo.loc.x, -geo.loc.y))
            .to_physical_precise_round(scale);

        let surface_elems = render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<R>>(
            renderer,
            surface,
            origin,
            scale,
            1.0,
            Kind::Unspecified,
        );

        // Walk popups attached to this surface (xdg dropdown menus,
        // tooltips, autocomplete). They aren't part of the toplevel's
        // subsurface tree — without this, captures miss any open menu.
        let mut popup_elems: Vec<WaylandSurfaceRenderElement<R>> = Vec::new();
        for (popup, popup_offset) in smithay::desktop::PopupManager::popups_for_surface(surface) {
            // Popup's geometry origin in capture-buffer space is `popup_offset`
            // (popup is positioned relative to the parent's geometry origin,
            // which we placed at (0,0)). Surface-tree origin is then
            // `popup_offset - popup.geometry().loc` to absorb popup CSD inset.
            let popup_geo = popup.geometry();
            let popup_origin = Point::<i32, smithay::utils::Logical>::from((
                popup_offset.x - popup_geo.loc.x,
                popup_offset.y - popup_geo.loc.y,
            ))
            .to_physical_precise_round(scale);
            popup_elems.extend(render_elements_from_surface_tree::<
                _,
                WaylandSurfaceRenderElement<R>,
            >(
                renderer,
                popup.wl_surface(),
                popup_origin,
                scale,
                1.0,
                Kind::Unspecified,
            ));
        }

        // Wrap in OutputRenderElements (the Layer variant is a plain
        // WaylandSurfaceRenderElement passthrough). Popups go FIRST so
        // they sit above the surface tree in smithay's z-order.
        let elems: Vec<OutputRenderElements<R>> = popup_elems
            .into_iter()
            .chain(surface_elems)
            .map(OutputRenderElements::Layer)
            .collect();
        let elems_refs: Vec<&OutputRenderElements<R>> = elems.iter().collect();

        // Cache key derived from the captured surface — keeps GlesTexture +
        // damage tracker alive across frames instead of reallocating per frame.
        let cap_key = format!("cap-tl:{:?}", surface.id());
        let cs = Some(get_capture_state(
            &mut state.render.capture_state,
            &cap_key,
            capture.buffer_size,
            scale,
            Transform::Normal,
            capture.paint_cursors,
            timestamp,
        ));

        // Try DMA-BUF first, fall back to SHM.
        let ok = if let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(&capture.buffer) {
            let mut dmabuf = dmabuf.clone();
            match render_to_dmabuf(
                renderer,
                &mut dmabuf,
                capture.buffer_size,
                scale,
                Transform::Normal,
                &elems_refs,
                cs,
            ) {
                Ok(sync) => match renderer.wait(&sync) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!("toplevel capture: dmabuf sync wait failed: {e:?}");
                        false
                    }
                },
                Err(e) => {
                    tracing::warn!("toplevel capture: dmabuf render failed: {e:?}");
                    false
                }
            }
        } else {
            match render_to_offscreen(
                renderer,
                capture.buffer_size,
                scale,
                Transform::Normal,
                Fourcc::Argb8888,
                &elems_refs,
                cs,
            ) {
                Ok(bytes) => {
                    shm::with_buffer_contents_mut(&capture.buffer, |shm_buf, shm_len, _data| {
                        let copy_len = shm_len.min(bytes.len());
                        unsafe {
                            ptr::copy_nonoverlapping(bytes.as_ptr(), shm_buf.cast(), copy_len);
                        }
                    })
                    .is_ok()
                }
                Err(e) => {
                    tracing::warn!("toplevel capture: offscreen render failed: {e:?}");
                    false
                }
            }
        };

        if ok {
            let w = capture.buffer_size.w;
            let h = capture.buffer_size.h;
            capture.frame.transform(Transform::Normal.into());
            capture.frame.damage(0, 0, w, h);
            let tv_sec_hi = (timestamp.as_secs() >> 32) as u32;
            let tv_sec_lo = (timestamp.as_secs() & 0xFFFFFFFF) as u32;
            let tv_nsec = timestamp.subsec_nanos();
            capture
                .frame
                .presentation_time(tv_sec_hi, tv_sec_lo, tv_nsec);
            capture.frame.ready();

            let frame_data = capture
                .frame
                .data::<std::sync::Mutex<driftwm::protocols::image_copy_capture::CaptureFrameData>>(
                );
            if let Some(fd) = frame_data {
                let fd = fd.lock().unwrap();
                state.image_copy_capture_state.frame_done(&fd.session);
            }
        } else {
            capture.frame.failed(fail_reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::capture_min_interval;
    use std::time::Duration;

    #[test]
    fn zero_fps_is_unlimited() {
        assert_eq!(capture_min_interval(0), None);
    }

    #[test]
    fn positive_fps_maps_to_interval() {
        assert_eq!(capture_min_interval(1), Some(Duration::from_secs(1)));
        assert_eq!(
            capture_min_interval(60),
            Some(Duration::from_secs_f64(1.0 / 60.0))
        );
    }
}
