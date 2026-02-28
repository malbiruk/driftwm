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
            compositor::{DrmCompositor, FrameFlags},
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
        calloop::{
            Dispatcher, EventLoop,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{self, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
    },
    utils::{DeviceFd, Transform},
};

use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::render::OutputRenderElements;
use crate::backend::Backend;
use crate::state::{CalloopData, DriftWm, log_err};

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
}

pub fn init_udev(
    event_loop: &mut EventLoop<'static, CalloopData>,
    data: &mut CalloopData,
) -> Result<(), Box<dyn std::error::Error>> {
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

    let (mut drm, drm_notifier, gbm, renderer, render_formats) = 'found: {
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

            let gbm = match GbmDevice::new(device_fd) {
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

            tracing::info!("Using GPU: {}", path.display());
            break 'found (drm, drm_notifier, gbm, renderer, render_formats);
        }
        return Err("No GPU with connected displays found (are you running from a TTY?)".into());
    };

    // 4. Store renderer on state + create DMA-BUF global
    data.state.backend = Some(Backend::Udev(Box::new(renderer)));
    let formats = data.state.backend.as_mut().unwrap().renderer().dmabuf_formats();
    let dmabuf_global = data
        .state
        .dmabuf_state
        .create_global::<DriftWm>(&data.display.handle(), formats);
    data.state.dmabuf_global = Some(dmabuf_global);

    // 5. Set up libinput
    let libinput_session = LibinputSessionInterface::from(session.clone());
    let mut libinput = Libinput::new_with_udev(libinput_session);
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|_| "Failed to assign libinput seat")?;
    let libinput_backend = LibinputInputBackend::new(libinput.clone());

    event_loop.handle().insert_source(libinput_backend, |mut event, _, data| {
        use smithay::backend::input::InputEvent;
        if let InputEvent::DeviceAdded { device } = &mut event {
            data.state.configure_libinput_device(device);
        }
        data.state.process_input_event(event);
    })?;

    // Store session on state so keyboard handler can call change_vt()
    data.state.session = Some(session);

    // 6. Scan connectors and set up outputs
    log_drm_connectors(&drm);

    let mut drm_scanner = DrmScanner::new();
    let scan_result = drm_scanner.scan_connectors(&drm)?;
    let mut device_surfaces: HashMap<crtc::Handle, SurfaceData> = HashMap::new();

    for event in scan_result {
        match event {
            DrmScanEvent::Connected { connector, crtc: Some(crtc) } => {
                tracing::info!(
                    "Connector connected: {}-{} (CRTC {:?})",
                    connector_type_name(&connector),
                    connector.interface_id(),
                    crtc,
                );
                if let Some(surface_data) = create_surface(
                    &mut drm,
                    &gbm,
                    &render_formats,
                    &connector,
                    crtc,
                    &data.display.handle(),
                    &mut data.state,
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
        }
    }

    if device_surfaces.is_empty() {
        return Err("Display connected but failed to create DRM surfaces".into());
    }

    // 7. Compile background shader / load tile (shared with winit)
    let initial_size = device_surfaces
        .values()
        .next()
        .and_then(|s| s.output.current_mode())
        .map(|m| m.size.to_logical(1))
        .unwrap_or((1920, 1080).into());
    {
        let mut backend = data.state.backend.take().unwrap();
        crate::render::init_background(&mut data.state, backend.renderer(), initial_size);
        data.state.backend = Some(backend);
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
    event_loop.handle().insert_source(drm_notifier, move |event, _meta, data: &mut CalloopData| {
        let mut dev = device_for_drm.borrow_mut();
        match event {
            DrmEvent::VBlank(crtc) => {
                let Some(surface) = dev.surfaces.get_mut(&crtc) else {
                    return;
                };
                if let Err(e) = surface.compositor.frame_submitted() {
                    tracing::warn!("frame_submitted error: {e:?}");
                }
                if data.state.redraw_needed {
                    render_frame(data, &mut surface.compositor, &surface.output);
                }
            }
            DrmEvent::Error(err) => {
                tracing::error!("DRM error: {err}");
            }
        }
    })?;

    // 10. Register session notifier (VT switching)
    let device_for_session = Rc::clone(&device);
    event_loop.handle().insert_source(session_notifier, move |event, _, data: &mut CalloopData| {
        let mut dev = device_for_session.borrow_mut();
        match event {
            SessionEvent::PauseSession => {
                tracing::info!("Session paused (VT switch away)");
                dev.libinput.suspend();
                dev.drm.pause();
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
                for surface in dev.surfaces.values_mut() {
                    // reset_state on DrmCompositor (not the raw DrmSurface) so
                    // reset_pending=true forces full damage on next render.
                    if let Err(e) = surface.compositor.reset_state() {
                        tracing::warn!("Failed to reset DRM surface state: {e}");
                    }
                    // Clear stale "frame pending" state from before the VT switch —
                    // the VBlank for the last queued frame was never delivered.
                    let _ = surface.compositor.frame_submitted();
                    render_frame(data, &mut surface.compositor, &surface.output);
                }
            }
        }
    })?;

    // 11. Register udev backend for hotplug
    let device_for_hotplug = Rc::clone(&device);
    let udev_dispatcher = Dispatcher::new(udev_backend, move |event: UdevEvent, _, data: &mut CalloopData| {
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
                                if let Some(sd) = create_surface(
                                    drm,
                                    gbm,
                                    render_formats,
                                    &connector,
                                    crtc,
                                    &data.display.handle(),
                                    &mut data.state,
                                ) {
                                    surfaces.insert(crtc, sd);
                                    let surface = surfaces.get_mut(&crtc).unwrap();
                                    render_frame(data, &mut surface.compositor, &surface.output);
                                }
                            }
                            DrmScanEvent::Disconnected { crtc: Some(crtc), .. } => {
                                tracing::info!("Hotplug: CRTC {crtc:?} disconnected");
                                if let Some(surface) = surfaces.remove(&crtc) {
                                    data.state.space.unmap_output(&surface.output);
                                }
                            }
                            _ => {}
                        }
                    }
                }
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

    // 12. Render kickstart timer — checks for pending damage and restarts
    // the VBlank chain when it has stopped (idle → new damage).
    let device_for_timer = Rc::clone(&device);
    let timer = Timer::from_duration(Duration::from_millis(16));
    event_loop.handle().insert_source(timer, move |_, _, data: &mut CalloopData| {
        if data.state.redraw_needed {
            let mut dev = device_for_timer.borrow_mut();
            for surface in dev.surfaces.values_mut() {
                render_frame(data, &mut surface.compositor, &surface.output);
            }
        }
        TimeoutAction::ToDuration(Duration::from_millis(16))
    })?;

    // 13. Queue initial render for all surfaces
    {
        let mut dev = device.borrow_mut();
        for surface in dev.surfaces.values_mut() {
            render_frame(data, &mut surface.compositor, &surface.output);
        }
    }

    Ok(())
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
fn pick_mode(connector: &connector::Info) -> Option<control::Mode> {
    let modes = connector.modes();
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

fn create_surface(
    drm: &mut DrmDevice,
    gbm: &GbmDevice<DrmDeviceFd>,
    render_formats: &[Format],
    connector: &connector::Info,
    crtc: crtc::Handle,
    dh: &smithay::reexports::wayland_server::DisplayHandle,
    state: &mut DriftWm,
) -> Option<SurfaceData> {
    let mode = pick_mode(connector)?;
    tracing::info!(
        "Using mode: {}x{}@{}Hz",
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

    let connector_name = format!(
        "{}-{}",
        connector_type_name(connector),
        connector.interface_id()
    );

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        connector_name,
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: convert_subpixel(connector.subpixel()),
            make: "driftwm".to_string(),
            model: "udev".to_string(),
        },
    );

    let output_mode = Mode {
        size: (mode.size().0 as i32, mode.size().1 as i32).into(),
        refresh: (mode.vrefresh() * 1000) as i32,
    };
    let scale = smithay::output::Scale::Fractional(state.config.output_scale);
    output.change_current_state(Some(output_mode), Some(Transform::Normal), Some(scale), None);
    output.set_preferred(output_mode);
    output.create_global::<DriftWm>(dh);

    let allocator = GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
    let compositor = match DrmCompositor::new(
        &output,
        drm_surface,
        None,
        allocator.clone(),
        GbmFramebufferExporter::new(gbm.clone(), None),
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
                GbmFramebufferExporter::new(gbm.clone(), None),
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

    if state.space.outputs().next().is_none() {
        let logical_size = output_mode.size.to_logical(1);
        state.camera = smithay::utils::Point::from((
            -(logical_size.w as f64) / 2.0,
            -(logical_size.h as f64) / 2.0,
        ));
    }
    state
        .space
        .map_output(&output, state.camera.to_i32_round());

    Some(SurfaceData { compositor, output })
}

/// Render a single frame and queue it to the DRM compositor.
fn render_frame(
    data: &mut CalloopData,
    compositor: &mut GbmDrmCompositor,
    output: &Output,
) {
    // Clear the flag — re-set below if animations need more frames.
    data.state.redraw_needed = false;

    let now = std::time::Instant::now();
    let dt = (now - data.state.last_frame_instant).min(std::time::Duration::from_millis(33));
    data.state.last_frame_instant = now;
    data.state.frame_counter = data.state.frame_counter.wrapping_add(1);

    // Animations
    data.state.apply_key_repeat();
    data.state.apply_scroll_momentum();
    data.state.apply_edge_pan();
    data.state.apply_camera_animation(dt);
    data.state.apply_zoom_animation(dt);

    // Keep rendering while animations are in progress
    if data.state.has_active_animations() {
        data.state.redraw_needed = true;
    }

    // Dispatch Wayland clients
    log_err("dispatch_clients", data.display.dispatch_clients(&mut data.state));
    log_err("flush_clients", data.display.flush_clients());

    // Update background element
    crate::render::update_background_element(&mut data.state, output);

    // Take renderer out to split borrow from state
    let mut backend = data.state.backend.take().unwrap();
    let renderer = backend.renderer();

    // Build cursor + compose frame
    let cursor_elements = crate::render::build_cursor_elements(&mut data.state, renderer);
    let renderer = backend.renderer();
    let elements = crate::render::compose_frame(&mut data.state, renderer, output, cursor_elements);

    // Render via DRM compositor
    let renderer = backend.renderer();
    match compositor.render_frame::<_, OutputRenderElements>(
        renderer,
        &elements,
        [0.0f32, 0.0, 0.0, 1.0],
        FrameFlags::empty(),
    ) {
        Ok(_render_result) => {
            if let Err(e) = compositor.queue_frame(()) {
                tracing::warn!("Failed to queue frame: {e:?}");
            }
        }
        Err(e) => {
            tracing::warn!("Render frame error: {e:?}");
        }
    }

    // Put backend back
    data.state.backend = Some(backend);

    // Record camera+zoom for next-frame change detection
    data.state.last_rendered_camera = data.state.camera;
    data.state.last_rendered_zoom = data.state.zoom;
    data.state.write_state_file_if_dirty();

    // Post-render
    crate::render::post_render(&mut data.state, output);
    log_err("flush_clients", data.display.flush_clients());
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
