use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use smithay::{
    backend::{
        allocator::{
            Format, Fourcc, Modifier,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType,
            compositor::{DrmCompositor, FrameError, FrameFlags, PrimaryPlaneElement},
            exporter::gbm::GbmFramebufferExporter,
        },
        egl::{EGLContext, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::ImportDma,
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{self, UdevBackend, UdevEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{Dispatcher, EventLoop, timer::{TimeoutAction, Timer}},
        drm::control::{self, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
    },
    utils::{DeviceFd, Transform},
    wayland::{dmabuf::DmabufFeedbackBuilder, drm_syncobj::supports_syncobj_eventfd},
};

use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use driftwm::config::{OutputMode as ConfigOutputMode, OutputPosition};
use crate::render::OutputRenderElements;
use crate::backend::Backend;
use crate::state::{DriftWm, init_output_state};

const SUPPORTED_COLOR_FORMATS: &[Fourcc] = &[
    Fourcc::Xrgb8888,
    Fourcc::Xbgr8888,
    Fourcc::Argb8888,
    Fourcc::Abgr8888,
];

type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;

struct DeviceData {
    drm: DrmDevice,
    gbm: GbmDevice<DrmDeviceFd>,
    drm_scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    render_formats: Vec<Format>,
    libinput: Libinput,
}

struct SurfaceData {
    compositor: GbmDrmCompositor,
    output: Output,
    connector: connector::Handle,
    make: String,
    model: String,
    serial_number: String,
}

/// Opaque handle to udev backend device data. Returned by init_udev,
/// passed to render_if_needed. main.rs never sees internals.
pub(crate) struct UdevDevice(Rc<RefCell<DeviceData>>);

/// Tick animations once for all outputs, mark dirty CRTCs, then render.
pub(crate) fn render_if_needed(device: &UdevDevice, data: &mut DriftWm) {
    // Fast path: nothing needs attention — skip all work when idle
    if data.redraws_needed.is_empty()
        && !data.has_active_animations()
        && !data.render.background_is_animated
        && !data.output_config_dirty
    {
        return;
    }

    // 1. Tick animations once for all outputs (before device borrow)
    data.tick_all_animations();

    let mut dev = device.0.borrow_mut();

    // 2. Mark CRTCs dirty for per-output animations
    for (&crtc, surface) in dev.surfaces.iter() {
        if data.output_has_active_animations(&surface.output) {
            data.redraws_needed.insert(crtc);
        }
    }

    // 3. Global animations (key repeat, cursor, animated bg) → mark all dirty
    // mark_all_dirty() uses active_crtcs on DriftWm, not dev.surfaces
    if data.held_action.is_some()
        || data.cursor.exec_cursor_show_at.is_some()
        || data.cursor.exec_cursor_deadline.is_some()
        || data.cursor_is_animated()
        || data.render.background_is_animated
    {
        data.mark_all_dirty();
    }

    // 4. Foreign toplevel refresh (once per frame, not per-output)
    crate::render::refresh_foreign_toplevels(data);

    // 4b. Re-notify output management clients after apply_output_config
    if data.output_config_dirty {
        data.output_config_dirty = false;
        let head_state = collect_output_state_from_surfaces(&dev.surfaces, &dev.drm);
        driftwm::protocols::output_management::notify_changes::<DriftWm>(
            &mut data.output_management_state,
            head_state,
        );
    }

    // 5. Render outputs that need it
    for (&crtc, surface) in dev.surfaces.iter_mut() {
        if data.redraws_needed.contains(&crtc)
            && !data.frames_pending.contains(&crtc)
        {
            render_frame(data, &mut surface.compositor, &surface.output, crtc);
        }
    }
}

pub fn init_udev(
    event_loop: &mut EventLoop<'static, DriftWm>,
    data: &mut DriftWm,
) -> Result<UdevDevice, Box<dyn std::error::Error>> {
    // 1. Create libseat session
    let (mut session, session_notifier) = LibSeatSession::new()
        .map_err(|e| format!("Failed to create session (are you running from a TTY?): {e}"))?;
    let seat_name = session.seat();
    tracing::info!("Session created on seat: {seat_name}");

    // 2. Enumerate GPUs — UdevBackend gives us all DRM devices (also used for hotplug later)
    let udev_backend = UdevBackend::new(&seat_name)?;
    let primary_gpu_path = udev::primary_gpu(&seat_name).ok().flatten();
    if let Some(ref p) = primary_gpu_path {
        tracing::info!("System primary GPU: {}", p.display());
    }

    // Build ordered candidate list: primary GPU first, then all others.
    // On hybrid graphics (iGPU + dGPU), the "primary" GPU may not have
    // the display outputs, so we fall back to other devices.
    let gpu_paths: Vec<PathBuf> = {
        let mut paths = Vec::new();
        if let Some(ref p) = primary_gpu_path {
            paths.push(p.clone());
        }
        for (_dev_id, path) in udev_backend.device_list() {
            let p = path.to_path_buf();
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        paths
    };
    tracing::info!("GPU candidates: {gpu_paths:?}");

    if gpu_paths.is_empty() {
        return Err("No GPUs found".into());
    }

    // 3. Try each GPU until one has connected displays
    let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;

    let (mut drm, drm_notifier, gbm, renderer, render_formats, render_node, device_fd) = 'found: {
        for path in &gpu_paths {
            let node = match DrmNode::from_path(path) {
                Ok(n) => n,
                Err(e) => {
                    tracing::debug!("{}: not a DRM node ({e}), skipping", path.display());
                    continue;
                }
            };
            if node.ty() != NodeType::Primary {
                tracing::debug!("{}: not a primary node, skipping", path.display());
                continue;
            }

            let fd = match session.open(path, open_flags) {
                Ok(fd) => fd,
                Err(e) => {
                    tracing::warn!("{}: failed to open ({e})", path.display());
                    continue;
                }
            };
            let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

            // true = release existing CRTCs for a clean modeset (avoids conflicts
            // with previous session's DRM state)
            let (drm, drm_notifier) = match DrmDevice::new(device_fd.clone(), true) {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!("{}: failed to create DRM device ({e})", path.display());
                    continue;
                }
            };

            if !gpu_has_connected_displays(&drm) {
                tracing::info!(
                    "{}: no connected displays, trying next GPU",
                    path.display()
                );
                continue;
            }

            let gbm = match GbmDevice::new(device_fd.clone()) {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!("{}: failed to create GBM device ({e})", path.display());
                    continue;
                }
            };

            let egl_display = match unsafe { EGLDisplay::new(gbm.clone()) } {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("{}: failed to create EGL display ({e})", path.display());
                    continue;
                }
            };
            let egl_context = match EGLContext::new(&egl_display) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("{}: failed to create EGL context ({e})", path.display());
                    continue;
                }
            };
            let render_formats: Vec<Format> = egl_context
                .dmabuf_render_formats()
                .iter()
                .copied()
                .filter(|f| {
                    // Intel CCS modifiers increase display link bandwidth, which can
                    // prevent high-res/high-refresh modes from working (e.g. ultrawides
                    // that need DSC). Filter them out — the GPU falls back to
                    // uncompressed framebuffers with no visual difference.
                    let is_ccs = matches!(
                        f.modifier,
                        Modifier::I915_y_tiled_ccs
                            | Modifier::I915_y_tiled_gen12_rc_ccs
                            | Modifier::I915_y_tiled_gen12_mc_ccs
                            // Yf_TILED_CCS
                            | Modifier::Unrecognized(0x100000000000005)
                            // Y_TILED_GEN12_RC_CCS_CC
                            | Modifier::Unrecognized(0x100000000000008)
                            // 4_TILED_DG2_RC_CCS
                            | Modifier::Unrecognized(0x10000000000000a)
                            // 4_TILED_DG2_MC_CCS
                            | Modifier::Unrecognized(0x10000000000000b)
                            // 4_TILED_DG2_RC_CCS_CC
                            | Modifier::Unrecognized(0x10000000000000c)
                    );
                    !is_ccs
                })
                .collect();
            let renderer =
                match unsafe { smithay::backend::renderer::gles::GlesRenderer::new(egl_context) } {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(
                            "{}: failed to create GLES renderer ({e})",
                            path.display()
                        );
                        continue;
                    }
                };

            let render_node = node
                .node_with_type(NodeType::Render)
                .and_then(|n| n.ok())
                .unwrap_or(node);

            tracing::info!("Using GPU: {}", path.display());
            break 'found (drm, drm_notifier, gbm, renderer, render_formats, render_node, device_fd);
        }
        return Err("No GPU with connected displays found (are you running from a TTY?)".into());
    };

    // 4. Store renderer on state + create DMA-BUF global
    data.backend = Some(Backend::Udev(Box::new(renderer)));
    let formats = data.backend.as_mut().unwrap().renderer().dmabuf_formats();
    let default_feedback = DmabufFeedbackBuilder::new(render_node.dev_id(), formats)
        .build()
        .expect("failed to build dmabuf feedback");
    let dmabuf_global = data
        .dmabuf_state
        .create_global_with_default_feedback::<DriftWm>(
            &data.display_handle,
            &default_feedback,
        );
    data.dmabuf_global = Some(dmabuf_global);
    if supports_syncobj_eventfd(&device_fd) {
        data.drm_syncobj_state =
            Some(smithay::wayland::drm_syncobj::DrmSyncobjState::new::<DriftWm>(
                &data.display_handle,
                device_fd.clone(),
            ));
    } else {
        tracing::info!(
            "DRM device lacks syncobj eventfd support: disabling linux-drm-syncobj-v1 advertisement and falling back to implicit synchronization"
        );
        data.drm_syncobj_state = None;
    }

    // 5. Set up libinput
    let libinput_session = LibinputSessionInterface::from(session.clone());
    let mut libinput = Libinput::new_with_udev(libinput_session);
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|_| "Failed to assign libinput seat")?;
    let libinput_backend = LibinputInputBackend::new(libinput.clone());

    event_loop.handle().insert_source(libinput_backend, |mut event, _, data| {
        use smithay::backend::input::InputEvent;
        match &mut event {
            InputEvent::DeviceAdded { device } => {
                data.configure_libinput_device(device);
                data.input_devices.push(device.clone());
            }
            InputEvent::DeviceRemoved { device } => {
                data.input_devices.retain(|d| d != device);
            }
            _ => {}
        }
        data.process_input_event(event);
    })?;

    // Store session on state so keyboard handler can call change_vt()
    data.session = Some(session);

    // 6. Scan connectors and set up outputs
    log_drm_connectors(&drm);

    let mut drm_scanner = DrmScanner::new();
    let scan_result = drm_scanner.scan_connectors(&drm)?;
    let mut device_surfaces: HashMap<crtc::Handle, SurfaceData> = HashMap::new();
    let saved_output_state = crate::state::read_all_per_output_state();

    for event in scan_result {
        match event {
            DrmScanEvent::Connected { connector, crtc: Some(crtc) } => {
                tracing::info!(
                    "Connector connected: {}-{} (CRTC {:?})",
                    connector_type_name(&connector),
                    connector.interface_id(),
                    crtc,
                );
                let dh = data.display_handle.clone();
                if let Some(surface_data) = create_surface(
                    &mut drm,
                    &gbm,
                    &render_formats,
                    &connector,
                    crtc,
                    &dh,
                    data,
                    &saved_output_state,
                ) {
                    device_surfaces.insert(crtc, surface_data);
                }
            }
            DrmScanEvent::Connected { connector, crtc: None } => {
                tracing::warn!(
                    "Connector {}-{} has no available CRTC",
                    connector_type_name(&connector),
                    connector.interface_id()
                );
            }
            DrmScanEvent::Disconnected { connector, crtc } => {
                tracing::debug!(
                    "Connector {}-{} disconnected (CRTC {:?})",
                    connector_type_name(&connector),
                    connector.interface_id(),
                    crtc,
                );
            }
            _ => {}
        }
    }

    if device_surfaces.is_empty() {
        return Err("Display connected but failed to create DRM surfaces".into());
    }

    // 7. Compile background shader / load tile (shared with winit)
    // Uses first surface's mode for initial background element size (resized per-frame anyway)
    {
        let mut backend = data.backend.take().unwrap();
        data.render.shadow_shader = crate::render::compile_shadow_shader(backend.renderer());
        data.render.corner_clip_shader = crate::render::compile_corner_clip_shader(backend.renderer());
        let (blur_down, blur_up, blur_mask) = crate::render::compile_blur_shaders(backend.renderer());
        data.render.blur_down_shader = blur_down;
        data.render.blur_up_shader = blur_up;
        data.render.blur_mask_shader = blur_mask;
        data.backend = Some(backend);
    }

    // 8. Build shared device state (Rc<RefCell<>> for safe sharing across calloop closures)
    let device = Rc::new(RefCell::new(DeviceData {
        drm,
        gbm,
        drm_scanner,
        surfaces: device_surfaces,
        render_formats,
        libinput,
    }));

    // 9. Register DRM event source (VBlank handler)
    let device_for_drm = Rc::clone(&device);
    event_loop.handle().insert_source(drm_notifier, move |event, _meta, data: &mut DriftWm| {
        let mut dev = device_for_drm.borrow_mut();
        match event {
            DrmEvent::VBlank(crtc) => {
                let Some(surface) = dev.surfaces.get_mut(&crtc) else {
                    return;
                };
                if let Err(e) = surface.compositor.frame_submitted() {
                    tracing::warn!("frame_submitted error: {e:?}");
                }
                data.frames_pending.remove(&crtc);
                // Real VBlank beat any estimated-VBlank timer we might have armed.
                if let Some(token) = data.estimated_vblank_timers.remove(&crtc) {
                    data.loop_handle.remove(token);
                }
                if data.redraws_needed.contains(&crtc) {
                    render_frame(data, &mut surface.compositor, &surface.output, crtc);
                }
            }
            DrmEvent::Error(err) => {
                tracing::error!("DRM error: {err}");
            }
        }
    })?;

    // 10. Register session notifier (VT switching)
    let device_for_session = Rc::clone(&device);
    event_loop.handle().insert_source(session_notifier, move |event, _, data: &mut DriftWm| {
        let mut dev = device_for_session.borrow_mut();
        match event {
            SessionEvent::PauseSession => {
                tracing::info!("Session paused (VT switch away)");
                dev.libinput.suspend();
                dev.drm.pause();
                for (_, token) in data.estimated_vblank_timers.drain() {
                    data.loop_handle.remove(token);
                }
            }
            SessionEvent::ActivateSession => {
                tracing::info!("Session resumed (VT switch back)");
                if dev.libinput.resume().is_err() {
                    tracing::warn!("Failed to resume libinput");
                }
                if let Err(e) = dev.drm.activate(false) {
                    tracing::error!("Failed to activate DRM: {e}");
                    return;
                }
                // VBlanks for pre-switch frames never arrive
                data.frames_pending.clear();
                for (_, token) in data.estimated_vblank_timers.drain() {
                    data.loop_handle.remove(token);
                }
                for (&crtc, surface) in dev.surfaces.iter_mut() {
                    if let Err(e) = surface.compositor.reset_state() {
                        tracing::warn!("Failed to reset DRM surface state: {e}");
                    }
                    let _ = surface.compositor.frame_submitted();
                    render_frame(data, &mut surface.compositor, &surface.output, crtc);
                }
            }
        }
    })?;

    // 11. Register udev backend for hotplug
    let device_for_hotplug = Rc::clone(&device);
    let udev_dispatcher = Dispatcher::new(udev_backend, move |event: UdevEvent, _, data: &mut DriftWm| {
        let mut dev = device_for_hotplug.borrow_mut();
        match event {
            UdevEvent::Changed { device_id } => {
                tracing::debug!("Udev device changed: {device_id:?}");
                let DeviceData {
                    ref mut drm_scanner,
                    ref mut drm,
                    ref gbm,
                    ref render_formats,
                    ref mut surfaces,
                    ..
                } = *dev;
                if let Ok(scan_result) = drm_scanner.scan_connectors(&*drm) {
                    for scan_event in scan_result {
                        match scan_event {
                            DrmScanEvent::Connected { connector, crtc: Some(crtc) } => {
                                if surfaces.contains_key(&crtc) {
                                    continue;
                                }
                                tracing::info!(
                                    "Hotplug: {}-{} connected",
                                    connector_type_name(&connector),
                                    connector.interface_id()
                                );
                                // Replace any virtual placeholder outputs. The unmap-to-
                                // create_surface sequence is synchronous within this
                                // connector handler, so active_output() is never None.
                                if !data.disconnected_outputs.is_empty() {
                                    let virtual_outputs: Vec<_> = data.space.outputs()
                                        .filter(|o| data.disconnected_outputs.contains(&o.name()))
                                        .cloned()
                                        .collect();
                                    for old in &virtual_outputs {
                                        data.space.unmap_output(old);
                                        data.render.cached_bg_elements.remove(&old.name());
                                        data.remove_capture_state(&old.name());
                                    }
                                    data.disconnected_outputs.clear();
                                    data.focused_output = None;
                                }
                                let saved = crate::state::read_all_per_output_state();
                                let dh = data.display_handle.clone();
                                if let Some(sd) = create_surface(
                                    drm,
                                    gbm,
                                    render_formats,
                                    &connector,
                                    crtc,
                                    &dh,
                                    data,
                                    &saved,
                                ) {
                                    surfaces.insert(crtc, sd);
                                    data.active_crtcs.insert(crtc);
                                    let surface = surfaces.get_mut(&crtc).unwrap();
                                    // Notify existing toplevels about the new output
                                    driftwm::protocols::foreign_toplevel::send_output_enter_all(
                                        &mut data.foreign_toplevel_state,
                                        &surface.output,
                                    );
                                    render_frame(data, &mut surface.compositor, &surface.output, crtc);
                                }
                            }
                            DrmScanEvent::Disconnected { crtc: Some(crtc), .. } => {
                                tracing::info!("Hotplug: CRTC {crtc:?} disconnected");
                                if let Some(surface) = surfaces.remove(&crtc) {
                                    driftwm::protocols::foreign_toplevel::send_output_leave_all(
                                        &mut data.foreign_toplevel_state,
                                        &surface.output,
                                    );

                                    // Stop capture/screencopy sessions for this output
                                    data.image_copy_capture_state.remove_output(&surface.output);
                                    data.screencopy_state.remove_output(&surface.output);

                                    // Close layer surfaces on this output so clients
                                    // (swayosd, waybar, etc.) can recreate on remaining outputs
                                    for layer in smithay::desktop::layer_map_for_output(&surface.output).layers() {
                                        layer.layer_surface().send_close();
                                    }

                                    if surfaces.is_empty() {
                                        // Last output disconnected — keep it in the Space as
                                        // a virtual placeholder so active_output() always
                                        // returns Some. Input events (USB keyboard/mouse) may
                                        // still arrive; the virtual output retains its old
                                        // mode/size so position_transformed() works correctly.
                                        tracing::warn!(
                                            "Last output disconnected — keeping virtual output '{}'",
                                            surface.output.name()
                                        );
                                        data.disconnected_outputs.insert(surface.output.name());
                                        data.exit_fullscreen_on(&surface.output);
                                        data.render.cached_bg_elements.remove(&surface.output.name());
                                        data.remove_capture_state(&surface.output.name());
                                        data.lock_surfaces.remove(&surface.output);
                                    } else {
                                        data.space.unmap_output(&surface.output);

                                        // Cancel any active pointer grab — grabs store an Output
                                        // clone and would operate on stale state after disconnect.
                                        if let Some(pointer) = data.seat.get_pointer() {
                                            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                            pointer.unset_grab(data, serial, 0);
                                        }

                                        // Clean up focused_output if it was on the disconnected output
                                        if data.focused_output.as_ref().is_some_and(|fo| fo == &surface.output) {
                                            data.focused_output = data.space.outputs().next().cloned();
                                            if let Some(ref new_out) = data.focused_output {
                                                let (cam, zoom, size) = {
                                                    let os = crate::state::output_state(new_out);
                                                    let sz = crate::state::output_logical_size(new_out);
                                                    (os.camera, os.zoom, sz)
                                                };
                                                let center = smithay::utils::Point::from((
                                                    cam.x + size.w as f64 / (2.0 * zoom),
                                                    cam.y + size.h as f64 / (2.0 * zoom),
                                                ));
                                                data.warp_pointer(center);
                                            }
                                        }

                                        // Clean up gesture state if gesture was on the disconnected output
                                        if data.gesture_output.as_ref().is_some_and(|go| go == &surface.output) {
                                            data.gesture_output = None;
                                            data.gesture_state = None;
                                        }

                                        // Clean up per-output resources
                                        data.render.cached_bg_elements.remove(&surface.output.name());
                                        data.remove_capture_state(&surface.output.name());
                                        data.fullscreen.remove(&surface.output);
                                        data.lock_surfaces.remove(&surface.output);
                                    }
                                }
                                data.active_crtcs.remove(&crtc);
                                data.frames_pending.remove(&crtc);
                                data.redraws_needed.remove(&crtc);
                                if let Some(token) = data.estimated_vblank_timers.remove(&crtc) {
                                    data.loop_handle.remove(token);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                // Notify output management clients after hotplug changes
                let head_state = collect_output_state_from_surfaces(surfaces, drm);
                driftwm::protocols::output_management::notify_changes::<DriftWm>(
                    &mut data.output_management_state,
                    head_state,
                );
            }
            UdevEvent::Added { device_id: _, path } => {
                tracing::info!("Udev device added: {path:?} (ignoring — single GPU)");
            }
            UdevEvent::Removed { device_id } => {
                tracing::info!("Udev device removed: {device_id:?}");
            }
        }
    });
    event_loop.handle().register_dispatcher(udev_dispatcher)?;

    // 12. Seed active_crtcs and queue initial render
    {
        let mut dev = device.borrow_mut();
        for (&crtc, surface) in dev.surfaces.iter_mut() {
            data.active_crtcs.insert(crtc);
            render_frame(data, &mut surface.compositor, &surface.output, crtc);
        }
        // 13. Notify output management clients of initial state
        let head_state = collect_output_state_from_surfaces(&dev.surfaces, &dev.drm);
        driftwm::protocols::output_management::notify_changes::<DriftWm>(
            &mut data.output_management_state,
            head_state,
        );
    }

    Ok(UdevDevice(device))
}

/// Quick check: does this DRM device have any connector in Connected state?
fn gpu_has_connected_displays(drm: &DrmDevice) -> bool {
    use smithay::reexports::drm::control::Device as ControlDevice;
    let Ok(res) = ControlDevice::resource_handles(drm) else {
        return false;
    };
    res.connectors().iter().any(|&handle| {
        ControlDevice::get_connector(drm, handle, true)
            .is_ok_and(|c| c.state() == connector::State::Connected)
    })
}

/// Log all connectors and their states for the selected GPU.
fn log_drm_connectors(drm: &DrmDevice) {
    use smithay::reexports::drm::control::Device as ControlDevice;
    let Ok(res) = ControlDevice::resource_handles(drm) else {
        return;
    };
    tracing::info!(
        "DRM resources: {} connectors, {} CRTCs, {} encoders",
        res.connectors().len(),
        res.crtcs().len(),
        res.encoders().len(),
    );
    for &handle in res.connectors() {
        if let Ok(info) = ControlDevice::get_connector(drm, handle, true) {
            tracing::info!(
                "  connector {}-{}: state={:?}, modes={}",
                connector_type_name(&info),
                info.interface_id(),
                info.state(),
                info.modes().len(),
            );
        }
    }
}

/// Pick the best mode for a connector: prefer MODE_TYPE_PREFERRED,
/// fall back to highest resolution (w*h), then highest refresh.
fn pick_preferred_mode(modes: &[control::Mode]) -> Option<control::Mode> {
    if modes.is_empty() {
        return None;
    }
    if let Some(preferred) = modes.iter().find(|m| {
        m.mode_type().contains(control::ModeTypeFlags::PREFERRED)
    }) {
        return Some(*preferred);
    }
    modes.iter().max_by_key(|m| {
        let (w, h) = m.size();
        (w as u64 * h as u64, m.vrefresh() as u64)
    }).copied()
}

/// Select a mode based on output config, falling back to preferred.
pub(crate) fn pick_mode_for_config(
    modes: &[control::Mode],
    config: &ConfigOutputMode,
) -> Option<control::Mode> {
    match config {
        ConfigOutputMode::Preferred => pick_preferred_mode(modes),
        ConfigOutputMode::Size(w, h) => {
            let matched = modes
                .iter()
                .filter(|m| m.size() == (*w as u16, *h as u16))
                .max_by_key(|m| m.vrefresh() as u64);
            if matched.is_none() {
                tracing::warn!("No mode matching {w}x{h}, falling back to preferred");
            }
            matched.copied().or_else(|| pick_preferred_mode(modes))
        }
        ConfigOutputMode::SizeRefresh(w, h, hz) => {
            let matched = modes.iter().find(|m| {
                m.size() == (*w as u16, *h as u16) && m.vrefresh() == *hz
            });
            if matched.is_none() {
                tracing::warn!("No mode matching {w}x{h}@{hz}Hz, falling back to preferred");
            }
            matched.copied().or_else(|| pick_preferred_mode(modes))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn create_surface(
    drm: &mut DrmDevice,
    gbm: &GbmDevice<DrmDeviceFd>,
    render_formats: &[Format],
    connector: &connector::Info,
    crtc: crtc::Handle,
    dh: &smithay::reexports::wayland_server::DisplayHandle,
    state: &mut DriftWm,
    saved_output_state: &std::collections::HashMap<String, (smithay::utils::Point<f64, smithay::utils::Logical>, f64)>,
) -> Option<SurfaceData> {
    let connector_name = format!(
        "{}-{}",
        connector_type_name(connector),
        connector.interface_id()
    );

    let output_cfg = state.config.output_config(&connector_name);

    let config_mode = output_cfg
        .map(|c| &c.mode)
        .unwrap_or(&ConfigOutputMode::Preferred);
    let mode = pick_mode_for_config(connector.modes(), config_mode)?;
    tracing::info!(
        "Output {connector_name}: mode {}x{}@{}Hz",
        mode.size().0,
        mode.size().1,
        mode.vrefresh()
    );

    let drm_surface = match drm.create_surface(crtc, mode, &[connector.handle()]) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("FAILED: drm.create_surface: {e}");
            return None;
        }
    };

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let edid = smithay_drm_extras::display_info::for_connector(drm, connector.handle());
    let make = edid
        .as_ref()
        .and_then(|i| i.make())
        .unwrap_or_else(|| "Unknown".to_string());
    let model = edid
        .as_ref()
        .and_then(|i| i.model())
        .unwrap_or_else(|| connector_name.clone());
    let serial_number = edid
        .as_ref()
        .and_then(|i| i.serial())
        .unwrap_or_default();
    let output = Output::new(
        connector_name.clone(),
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: convert_subpixel(connector.subpixel()),
            make: make.clone(),
            model: model.clone(),
            serial_number: serial_number.clone(),
        },
    );

    let output_mode = Mode {
        size: (mode.size().0 as i32, mode.size().1 as i32).into(),
        refresh: (mode.vrefresh() * 1000) as i32,
    };
    let scale_val = output_cfg
        .and_then(|c| c.scale)
        .unwrap_or_else(|| {
            tracing::info!(
                "No [[outputs]] entry for '{}' — defaulting to scale 1.0. \
                 Add an [[outputs]] section to config.toml to set a custom scale.",
                connector_name,
            );
            1.0
        });
    let scale = smithay::output::Scale::Fractional(scale_val);
    let transform = output_cfg
        .and_then(|c| c.transform)
        .unwrap_or(Transform::Normal);
    // Compute layout position from config
    let layout_position: smithay::utils::Point<i32, smithay::utils::Logical> = match output_cfg.map(|c| &c.position) {
        Some(OutputPosition::Fixed(x, y)) => {
            tracing::info!("Output {connector_name}: layout position ({x}, {y}) from config");
            (*x, *y).into()
        }
        _ => {
            // Auto: place left-to-right by connection order
            let auto_x: i32 = state.space.outputs().map(|o| {
                crate::state::output_logical_size(o).w
            }).sum();
            tracing::info!("Output {connector_name}: auto layout position ({auto_x}, 0)");
            (auto_x, 0).into()
        }
    };
    output.change_current_state(Some(output_mode), Some(transform), Some(scale), Some(layout_position));
    output.set_preferred(output_mode);
    output.create_global::<DriftWm>(dh);

    let allocator = GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
    let compositor = match DrmCompositor::new(
        &output,
        drm_surface,
        None,
        allocator.clone(),
        GbmFramebufferExporter::new(gbm.clone(), None.into()),
        SUPPORTED_COLOR_FORMATS.iter().copied(),
        render_formats.iter().copied(),
        drm.cursor_size(),
        Some(gbm.clone()),
    ) {
        Ok(c) => c,
        Err(e) => {
            // DrmCompositor::new consumes the surface on error — recreate it.
            // Retry with Modifier::Invalid (implicit) only, which is the most
            // compatible option (lets the driver pick the layout).
            tracing::warn!(
                "DrmCompositor failed ({e:?}), retrying with implicit modifier"
            );
            let _ = std::fs::write("/tmp/driftwm-drm-error.txt", format!("{e:?}"));

            let fallback_surface = match drm.create_surface(crtc, mode, &[connector.handle()]) {
                Ok(s) => s,
                Err(e2) => {
                    tracing::error!("Failed to recreate DRM surface: {e2}");
                    return None;
                }
            };
            let fallback_formats: Vec<Format> = render_formats
                .iter()
                .copied()
                .filter(|f| f.modifier == Modifier::Invalid)
                .collect();

            match DrmCompositor::new(
                &output,
                fallback_surface,
                None,
                allocator,
                GbmFramebufferExporter::new(gbm.clone(), None.into()),
                SUPPORTED_COLOR_FORMATS.iter().copied(),
                fallback_formats,
                drm.cursor_size(),
                Some(gbm.clone()),
            ) {
                Ok(c) => c,
                Err(e2) => {
                    tracing::error!("DrmCompositor failed even with implicit modifier: {e2:?}");
                    let _ = std::fs::write(
                        "/tmp/driftwm-drm-error.txt",
                        format!("First: {e:?}\nFallback: {e2:?}"),
                    );
                    return None;
                }
            }
        }
    };

    // Each new output gets its own camera centered on its viewport
    let logical_size = transform
        .transform_size(output_mode.size)
        .to_f64()
        .to_logical(scale_val)
        .to_i32_ceil::<i32>();
    let camera = smithay::utils::Point::from((
        -(logical_size.w as f64) / 2.0,
        -(logical_size.h as f64) / 2.0,
    ));

    init_output_state(&output, camera, state.config.friction, layout_position);

    // Restore per-output camera/zoom from state file if available
    if let Some(&(saved_cam, saved_zoom)) = saved_output_state.get(&connector_name) {
        let mut os = crate::state::output_state(&output);
        os.camera = saved_cam;
        os.zoom = saved_zoom;
        tracing::info!("Output {connector_name}: restored camera ({:.0}, {:.0}) zoom {:.3}", saved_cam.x, saved_cam.y, saved_zoom);
    }

    // Set focused_output to the first output created
    if state.focused_output.is_none() {
        state.focused_output = Some(output.clone());
        // Center pointer on first output
        let size = crate::state::output_logical_size(&output);
        let (cam, zoom) = {
            let os = crate::state::output_state(&output);
            (os.camera, os.zoom)
        };
        let center = smithay::utils::Point::from((
            cam.x + size.w as f64 / (2.0 * zoom),
            cam.y + size.h as f64 / (2.0 * zoom),
        ));
        state.warp_pointer(center);
    }

    // Use potentially-restored camera for output mapping
    let effective_camera = crate::state::output_state(&output).camera;
    state
        .space
        .map_output(&output, effective_camera.to_i32_round());

    Some(SurfaceData {
        compositor,
        output,
        connector: connector.handle(),
        make,
        model,
        serial_number,
    })
}

/// Render a single frame and queue it to the DRM compositor.
fn render_frame(
    data: &mut DriftWm,
    compositor: &mut GbmDrmCompositor,
    output: &Output,
    crtc: crtc::Handle,
) {
    data.redraws_needed.remove(&crtc);

    // Flush Wayland clients
    data.display_handle.flush_clients().ok();

    // Read per-output state for this frame
    let (cur_camera, cur_zoom, last_cam, last_zoom) = {
        let os = crate::state::output_state(output);
        (os.camera, os.zoom, os.last_rendered_camera, os.last_rendered_zoom)
    };

    // Update background element
    let (camera_moved, zoom_changed) =
        crate::render::update_background_element(data, output, cur_camera, cur_zoom, last_cam, last_zoom);

    // Force full redraw when viewport shifts — DrmCompositor's damage tracker
    // doesn't know all elements moved, so without this we get partial-update artifacts.
    if camera_moved || zoom_changed {
        compositor.reset_buffer_ages();
    }

    // Take renderer out to split borrow from state
    let mut backend = data.backend.take().unwrap();
    let renderer = backend.renderer();

    // Build cursor + compose frame
    let cursor_alpha = if data.active_output().as_ref() == Some(output) {
        1.0
    } else {
        data.config.inactive_cursor_opacity as f32
    };
    let cursor_elements = crate::render::build_cursor_elements(data, renderer, cur_camera, cur_zoom, output.current_scale().fractional_scale(), cursor_alpha);
    let renderer = backend.renderer();
    let elements = crate::render::compose_frame(data, renderer, output, cursor_elements);

    // Primary plane scanout (any format) + cursor plane. Overlay planes left off —
    // they cause hard-to-diagnose flicker on some hardware (niri does the same).
    let frame_flags = if data.config.backend.disable_direct_scanout {
        FrameFlags::empty()
    } else {
        FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY | FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT
    };

    // Render via DRM compositor (latency-sensitive — do first)
    let renderer = backend.renderer();
    let render_result = compositor.render_frame::<_, OutputRenderElements>(
        renderer,
        &elements,
        [0.0f32, 0.0, 0.0, 1.0],
        frame_flags,
    );

    // Opt-in fence wait for NVIDIA/explicit-sync hardware that page-flips
    // before the GPU has actually finished rendering.
    if data.config.backend.wait_for_frame_completion
        && let Ok(ref rr) = render_result
        && rr.needs_sync()
        && let PrimaryPlaneElement::Swapchain(ref element) = rr.primary_element
    {
        let _ = element.sync.wait();
    }

    match render_result {
        Ok(_render_result) => match compositor.queue_frame(()) {
            Ok(()) => {
                data.frames_pending.insert(crtc);
            }
            Err(FrameError::EmptyFrame) => {
                // No page flip → no VBlank. Arm a wake if animations still need ticking.
                if data.has_active_animations() || data.render.background_is_animated {
                    queue_estimated_vblank_timer(data, output, crtc);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to queue frame: {e:?}");
            }
        },
        Err(e) => {
            tracing::warn!("Render frame error: {e:?}");
        }
    }

    // Fulfill capture requests after main render
    let renderer = backend.renderer();
    crate::render::render_screencopy(data, renderer, output, &elements);

    let renderer = backend.renderer();
    crate::render::render_capture_frames(data, renderer, output, &elements);

    // Put backend back
    data.backend = Some(backend);

    // Record camera+zoom for next-frame change detection
    {
        let mut os = crate::state::output_state(output);
        os.last_rendered_camera = os.camera;
        os.last_rendered_zoom = os.zoom;
    }
    data.write_state_file_if_dirty();

    // Post-render
    crate::render::post_render(data, output);
    data.display_handle.flush_clients().ok();
}

/// Wake the VBlank-driven loop at ~one refresh period when queue_frame returned
/// EmptyFrame, so ongoing animations keep ticking. Idempotent per CRTC.
fn queue_estimated_vblank_timer(
    data: &mut DriftWm,
    output: &Output,
    crtc: crtc::Handle,
) {
    if data.estimated_vblank_timers.contains_key(&crtc) {
        return;
    }
    // Clamp refresh mHz before the cast: negative i32 would wrap to a huge u64 and
    // produce a near-zero-duration timer, spinning the loop.
    let duration = output
        .current_mode()
        .map(|m| m.refresh.max(1_000) as u64)
        .map(|mhz| Duration::from_nanos(1_000_000_000_000 / mhz))
        .unwrap_or_else(|| Duration::from_micros(16_667));

    let timer = Timer::from_duration(duration);
    match data.loop_handle.insert_source(timer, move |_, _, data: &mut DriftWm| {
        data.estimated_vblank_timers.remove(&crtc);
        TimeoutAction::Drop
    }) {
        Ok(tok) => {
            data.estimated_vblank_timers.insert(crtc, tok);
        }
        Err(e) => tracing::warn!("Failed to insert estimated VBlank timer: {e:?}"),
    }
}

use driftwm::protocols::output_management::{OutputHeadState, ModeInfo};

fn collect_output_state_from_surfaces(
    surfaces: &HashMap<crtc::Handle, SurfaceData>,
    drm: &DrmDevice,
) -> HashMap<String, OutputHeadState> {
    use smithay::reexports::drm::control::Device as ControlDevice;
    let mut result = HashMap::new();
    for surface in surfaces.values() {
        let output = &surface.output;
        let name = output.name();
        let mode = output.current_mode().unwrap();
        let transform = output.current_transform();
        let scale = output.current_scale().fractional_scale();
        let layout_pos = crate::state::output_state(output).layout_position;

        let modes: Vec<ModeInfo> = match ControlDevice::get_connector(drm, surface.connector, false) {
            Ok(info) => info
                .modes()
                .iter()
                .map(|m| ModeInfo {
                    width: m.size().0 as i32,
                    height: m.size().1 as i32,
                    refresh: (m.vrefresh() as i32) * 1000,
                    preferred: m.mode_type().contains(control::ModeTypeFlags::PREFERRED),
                })
                .collect(),
            Err(_) => vec![],
        };

        let current_mode_index = modes.iter().position(|m| {
            m.width == mode.size.w && m.height == mode.size.h && m.refresh == mode.refresh
        });

        let phys = output.physical_properties().size;
        result.insert(
            name.clone(),
            OutputHeadState {
                name,
                description: format!("{} {} ({})", surface.make, surface.model, output.name()),
                make: surface.make.clone(),
                model: surface.model.clone(),
                serial_number: surface.serial_number.clone(),
                physical_size: (phys.w, phys.h),
                modes,
                current_mode_index,
                position: (layout_pos.x, layout_pos.y),
                transform,
                scale,
            },
        );
    }
    result
}

fn convert_subpixel(sp: connector::SubPixel) -> Subpixel {
    match sp {
        connector::SubPixel::Unknown => Subpixel::Unknown,
        connector::SubPixel::HorizontalRgb => Subpixel::HorizontalRgb,
        connector::SubPixel::HorizontalBgr => Subpixel::HorizontalBgr,
        connector::SubPixel::VerticalRgb => Subpixel::VerticalRgb,
        connector::SubPixel::VerticalBgr => Subpixel::VerticalBgr,
        connector::SubPixel::None => Subpixel::None,
        _ => Subpixel::Unknown,
    }
}

fn connector_type_name(connector: &connector::Info) -> &'static str {
    match connector.interface() {
        connector::Interface::DVII => "DVI-I",
        connector::Interface::DVID => "DVI-D",
        connector::Interface::DVIA => "DVI-A",
        connector::Interface::SVideo => "S-Video",
        connector::Interface::DisplayPort => "DP",
        connector::Interface::HDMIA => "HDMI-A",
        connector::Interface::HDMIB => "HDMI-B",
        connector::Interface::EmbeddedDisplayPort => "eDP",
        connector::Interface::VGA => "VGA",
        _ => "Unknown",
    }
}
