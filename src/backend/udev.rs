use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::{
    backend::{
        allocator::{
            Format, Fourcc, Modifier,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmEvent, DrmNode, NodeType,
            compositor::{DrmCompositor, FrameError, FrameFlags, PrimaryPlaneElement},
            exporter::gbm::GbmFramebufferExporter,
        },
        egl::{EGLContext, EGLDevice, EGLDisplay, context::ContextPriority},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            ImportDma, RendererSuper,
            gles::GlesRenderer,
            multigpu::{GpuManager, MultiFrame, MultiRenderer, gbm::GbmGlesBackend},
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{self, UdevBackend, UdevEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            Dispatcher, EventLoop, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{self, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
    },
    utils::{DeviceFd, Transform},
    wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder},
};
use smithay::reexports::wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags;

use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::backend::Backend;
use crate::backend::cvt;
use crate::backend::gamma::{GammaProps, set_gamma_for_crtc_legacy};
use crate::state::{DriftWm, init_output_state};
use driftwm::config::{OutputMode as ConfigOutputMode, OutputPosition};
use smithay::wayland::seat::WaylandFocus;

const SUPPORTED_COLOR_FORMATS: &[Fourcc] = &[
    Fourcc::Xrgb8888,
    Fourcc::Xbgr8888,
    Fourcc::Argb8888,
    Fourcc::Abgr8888,
];

type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    smithay::desktop::utils::OutputPresentationFeedback,
    DrmDeviceFd,
>;

// Multi-GPU rendering: render on the primary GPU, scan out (with an implicit
// PRIME copy) on outputs that live on other GPUs. Both type params are the same
// GBM-GLES backend; the render and target nodes differ at call time.
pub type MultiGpuRenderer<'render> = MultiRenderer<
    'render,
    'render,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

pub type MultiGpuFrame<'render, 'frame, 'buffer> = MultiFrame<
    'render,
    'render,
    'frame,
    'buffer,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

pub type MultiGpuRendererError<'render> = <MultiGpuRenderer<'render> as RendererSuper>::Error;

/// The udev backend's renderer state stored on `Backend::Udev`. Holds the
/// multi-GPU manager (one GLES renderer per DRM render node) and the primary
/// render node. `gpu_manager.single_renderer(&primary_render_node)` yields a
/// `MultiGpuRenderer` for same-GPU work; for an output on another GPU,
/// `gpu_manager.renderer(primary, target, fmt)` adds the implicit PRIME copy.
pub struct UdevRenderer {
    pub gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    pub primary_render_node: DrmNode,
}

struct DeviceData {
    /// This device's KMS (primary) node — the key in `DriftWm::udev_devices`,
    /// and the allocation target advertised in scanout dmabuf-feedback tranches.
    kms_node: DrmNode,
    /// This device's DRM render node (resolved via EGL; the KMS node itself on
    /// split-DRM systems without one). Outputs here scan out buffers rendered
    /// on the primary GPU — when the nodes differ, via an implicit PRIME copy.
    render_node: DrmNode,
    drm: DrmDevice,
    gbm: GbmDevice<DrmDeviceFd>,
    drm_scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    render_formats: Vec<Format>,
    /// Calloop token for this device's DRM (VBlank) event source; removed
    /// when the GPU is unplugged.
    drm_token: Option<RegistrationToken>,
}

struct SurfaceData {
    compositor: GbmDrmCompositor,
    output: Output,
    connector: connector::Handle,
    make: String,
    model: String,
    serial_number: String,
    global: GlobalId,
    /// Atomic GAMMA_LUT/GAMMA_LUT_SIZE property handles. `None` if the driver
    /// doesn't expose them; in that case we fall back to legacy `set_gamma`.
    gamma_props: Option<GammaProps>,
    /// Gamma ramp queued while the session is inactive (VT switched away).
    /// Re-applied on session resume. `Some(Some(ramp))` = set to ramp,
    /// `Some(None)` = reset to identity, `None` = nothing pending.
    pending_gamma_change: Option<Option<Vec<u16>>>,
    /// Per-surface dmabuf feedback sent to clients shown on this output.
    /// `None` if building it failed — clients fall back to the default global.
    dmabuf_feedback: Option<SurfaceDmabufFeedback>,
}

/// Per-surface dmabuf feedback. `render` steers clients toward the primary
/// render node; `scanout` adds scanout-flagged tranches targeting this
/// output's KMS device so a fullscreen client allocates buffers its
/// DrmCompositor can promote to a plane. Which one a surface receives is
/// decided per frame from its render-element state (see
/// `render::send_dmabuf_feedbacks`).
pub struct SurfaceDmabufFeedback {
    pub render: DmabufFeedback,
    pub scanout: DmabufFeedback,
}

fn surface_dmabuf_feedback(
    compositor: &GbmDrmCompositor,
    primary_formats: FormatSet,
    primary_render_node: DrmNode,
    surface_render_node: DrmNode,
    surface_kms_node: DrmNode,
) -> Result<SurfaceDmabufFeedback, std::io::Error> {
    let surface = compositor.surface();
    let planes = surface.planes();

    let primary_plane_formats = surface.plane_info().formats.clone();
    let primary_or_overlay_plane_formats = primary_plane_formats
        .iter()
        .chain(planes.overlay.iter().flat_map(|p| p.formats.iter()))
        .copied()
        .collect::<FormatSet>();

    // Limit scanout tranches to formats we can also render from, so a buffer
    // that fails the scanout test still has a composite fallback path.
    let mut primary_scanout_formats = primary_plane_formats
        .intersection(&primary_formats)
        .copied()
        .collect::<Vec<_>>();
    let mut primary_or_overlay_scanout_formats = primary_or_overlay_plane_formats
        .intersection(&primary_formats)
        .copied()
        .collect::<Vec<_>>();

    // Cross-GPU scanout is only reliable with Linear buffers: iGPU+dGPU pairs
    // can share non-Linear modifiers on paper yet scan out glitched frames
    // (same workaround as niri).
    if surface_render_node != primary_render_node {
        primary_scanout_formats.retain(|f| f.modifier == Modifier::Linear);
        primary_or_overlay_scanout_formats.retain(|f| f.modifier == Modifier::Linear);
    }

    tracing::info!(
        "dmabuf feedback for {surface_kms_node}: {} plane formats, {} render-node formats, \
         scanout tranches {} + {}",
        primary_plane_formats.iter().count(),
        primary_formats.iter().count(),
        primary_scanout_formats.len(),
        primary_or_overlay_scanout_formats.len(),
    );

    let builder = DmabufFeedbackBuilder::new(primary_render_node.dev_id(), primary_formats);

    // Prefer primary-plane-only formats over primary-or-overlay: overlay
    // planes are disabled in render_frame, so this raises the chance a
    // client's buffer lands in the tranche that can actually scan out.
    let scanout = builder
        .clone()
        .add_preference_tranche(
            surface_kms_node.dev_id(),
            Some(TrancheFlags::Scanout),
            primary_scanout_formats,
        )
        .add_preference_tranche(
            surface_kms_node.dev_id(),
            Some(TrancheFlags::Scanout),
            primary_or_overlay_scanout_formats,
        )
        .build()?;

    // On the primary node the render path can scan out too — reuse the
    // scanout feedback to avoid advertising duplicate tranches.
    let render = if surface_render_node == primary_render_node {
        scanout.clone()
    } else {
        builder.build()?
    };

    Ok(SurfaceDmabufFeedback { render, scanout })
}

/// Opaque handle to udev backend device data, stored in
/// `DriftWm::udev_devices` keyed by KMS node. Rc-cloneable so the render loop
/// and gamma-control handler can each grab an independent `RefCell` borrow
/// without re-routing through DriftWm.
#[derive(Clone)]
pub(crate) struct UdevDevice(Rc<RefCell<DeviceData>>);

/// Apply (or clear, with `None`) a gamma ramp on `surface` via whichever
/// path the CRTC supports — atomic GAMMA_LUT first, legacy ioctl fallback.
fn apply_gamma(
    surface: &mut SurfaceData,
    drm: &DrmDevice,
    crtc: crtc::Handle,
    ramp: Option<&[u16]>,
) -> Option<()> {
    if let Some(gp) = &mut surface.gamma_props {
        gp.set_gamma(drm, ramp)
    } else {
        set_gamma_for_crtc_legacy(drm, crtc, ramp)
    }
}

impl UdevDevice {
    /// Look up the per-output gamma LUT size. Prefers atomic GAMMA_LUT_SIZE;
    /// falls back to the CRTC's legacy `gamma_length`. Returns `None` if the
    /// CRTC reports size 0 (e.g. Apple DCP on Asahi, virtual outputs without
    /// gamma support) so the protocol cleanly fails the control rather than
    /// advertising a 0-entry LUT.
    pub(crate) fn get_gamma_size(&self, output: &Output) -> Option<u32> {
        use smithay::reexports::drm::control::Device as _;
        let dev = self.0.borrow();
        let (crtc, surface) = dev.surfaces.iter().find(|(_, s)| s.output == *output)?;
        let size = if let Some(gp) = &surface.gamma_props {
            gp.gamma_size(&dev.drm)?
        } else {
            dev.drm.get_crtc(*crtc).ok()?.gamma_length()
        };
        (size != 0).then_some(size)
    }

    /// Apply a gamma ramp (or reset to identity if `None`). Atomic path if
    /// the driver exposes GAMMA_LUT; legacy ioctl otherwise. If the session
    /// is inactive (VT switched away), the ramp is queued on the surface
    /// and re-applied on resume.
    pub(crate) fn set_gamma(&self, output: &Output, ramp: Option<Vec<u16>>) -> Option<()> {
        let mut dev = self.0.borrow_mut();
        let DeviceData { drm, surfaces, .. } = &mut *dev;
        let (crtc, surface) = surfaces.iter_mut().find(|(_, s)| s.output == *output)?;

        if !drm.is_active() {
            surface.pending_gamma_change = Some(ramp);
            return Some(());
        }

        apply_gamma(surface, drm, *crtc, ramp.as_deref())
    }
}

/// Tick animations once for all outputs, mark dirty CRTCs, then render.
///
/// Clones each device's `Rc` handle out of `data.udev_devices` first so a
/// borrow stays independent of mutations on `data`. Global per-frame work
/// (DPMS drain, mode changes, foreign-toplevel + output-management refresh)
/// runs once and is routed to the owning device; dirty-marking and rendering
/// iterate every device.
pub(crate) fn render_if_needed(data: &mut DriftWm) {
    // Fast path: nothing needs attention — skip all work when idle
    let any_chunked_pending = data
        .render
        .cached_tile_chunks
        .values()
        .any(|c| c.has_pending_loads())
        || data
            .render
            .cached_shader_chunks
            .values()
            .any(|c| c.has_pending_bakes());
    if data.redraws_needed.is_empty()
        && !data.has_active_animations()
        && !data.render.background_is_animated
        && !data.output_config_dirty
        && data.pending_dpms.is_empty()
        && !any_chunked_pending
        && !data.pending_pointer_resync
    {
        return;
    }

    // Free capture textures left by finished screenshot/screencast clients
    // (kept warm while one renders into them). Only fires on render-active
    // cycles, so a fully-idle stop frees on next activity — memory, not battery.
    data.render
        .evict_idle_capture_state(data.start_time.elapsed());

    // Clone the device handles out so each borrow is independent of `data`.
    let devices: Vec<UdevDevice> = data.udev_devices.values().cloned().collect();
    if devices.is_empty() {
        return;
    }

    // 1. Tick animations once for all outputs (before device borrows)
    data.tick_all_animations();

    // Emit the one coalesced pointer motion for this frame, after animations.
    data.flush_pointer_resync();

    // 2. Drain pending DPMS transitions before animation marking so DPMS-off
    //    outputs don't get re-dirtied below. Each output lives on exactly one
    //    device, so find the surface across all devices.
    if !data.pending_dpms.is_empty() {
        let pending: Vec<(Output, bool)> = data.pending_dpms.drain().collect();
        for (output, on) in &pending {
            for device in &devices {
                let mut dev = device.0.borrow_mut();
                let Some((&crtc, surface)) =
                    dev.surfaces.iter_mut().find(|(_, s)| s.output == *output)
                else {
                    continue;
                };
                if *on {
                    data.redraws_needed.insert(output.clone());
                } else {
                    if let Err(e) = surface.compositor.clear() {
                        tracing::warn!(
                            "DPMS off: compositor.clear failed for '{}': {e:?}",
                            output.name()
                        );
                    }
                    data.redraws_needed.remove(output);
                    data.frames_pending.remove(&crtc);
                    if let Some(token) = data.estimated_vblank_timers.remove(&crtc) {
                        data.loop_handle.remove(token);
                    }
                }
                break;
            }
        }
        // Broadcast mode events for client-initiated changes (already sent
        // inline) plus anything else that drifted; idempotent.
        driftwm::protocols::output_power::OutputPowerState::refresh(data);
    }

    // 3. Mark outputs dirty for per-output animations / pending chunk uploads,
    //    across every active device.
    for device in &devices {
        let dev = device.0.borrow();
        if !dev.drm.is_active() {
            continue;
        }
        for surface in dev.surfaces.values() {
            if data.dpms_off_outputs.contains(&surface.output) {
                continue;
            }
            if data.output_has_active_animations(&surface.output) {
                data.redraws_needed.insert(surface.output.clone());
            }
            // Chunked-bg with tiles still to upload: keep firing frames until the
            // visible set fully resolves. Otherwise the loop idles after pan
            // stops and blurry chunks stay covered by the fallback plane until
            // unrelated damage (cursor, animation, client commit) wakes us.
            if let Some(cache) = data.render.cached_tile_chunks.get(&surface.output.name())
                && cache.has_pending_loads()
            {
                data.redraws_needed.insert(surface.output.clone());
            }
            // Same for chunked shader-bake: refine sharp chunks after pan stops.
            if let Some(cache) = data.render.cached_shader_chunks.get(&surface.output.name())
                && cache.has_pending_bakes()
            {
                data.redraws_needed.insert(surface.output.clone());
            }
        }
    }

    // Global animations (key repeat, cursor) → every output.
    if data.held_action.is_some()
        || data.cursor.exec_cursor_show_at.is_some()
        || data.cursor.exec_cursor_deadline.is_some()
        || data.cursor_is_animated()
    {
        data.mark_all_dirty();
    } else if data.render.background_is_animated {
        // Fullscreen outputs skip the background entirely, so an animated bg
        // gives them nothing to redraw — marking them just burns battery.
        let dirty: Vec<_> = data
            .active_outputs
            .iter()
            .filter(|o| !data.is_output_fullscreen(o))
            .cloned()
            .collect();
        data.redraws_needed.extend(dirty);
    }

    // 4. Foreign toplevel refresh (once per frame, not per-output)
    crate::render::refresh_foreign_toplevels(data);

    // 4a. Drain queued mode changes before re-notifying clients so the
    // re-broadcast reflects the new mode state. Mode changes come from
    // wlr-output-management Apply or config reload. Each device claims the
    // entries for outputs it owns and hands the rest back; anything left
    // after every device had a look targets a gone output.
    if !data.pending_mode_changes.is_empty() {
        let mut pending = std::mem::take(&mut data.pending_mode_changes);
        for device in &devices {
            if pending.is_empty() {
                break;
            }
            let mut dev = device.0.borrow_mut();
            let DeviceData { drm, surfaces, .. } = &mut *dev;
            pending = apply_pending_mode_changes(drm, surfaces, data, pending);
        }
        for (name, _) in pending {
            tracing::warn!("Mode change for '{name}' dropped: output no longer present");
        }
    }

    // 4b. Re-notify output management clients after apply_output_config,
    //     aggregating heads across every device.
    if data.output_config_dirty {
        data.output_config_dirty = false;
        let head_state = collect_all_head_states(data);
        driftwm::protocols::output_management::notify_changes::<DriftWm>(
            &mut data.output_management_state,
            head_state,
        );
    }

    // 5. Render outputs that need it, per device.
    for device in &devices {
        let mut dev = device.0.borrow_mut();
        if !dev.drm.is_active() {
            continue;
        }
        let render_node = dev.render_node;
        for (&crtc, surface) in dev.surfaces.iter_mut() {
            if data.dpms_off_outputs.contains(&surface.output) {
                data.redraws_needed.remove(&surface.output);
                continue;
            }
            // An armed estimated-VBlank timer counts as waiting, like frames_pending:
            // re-rendering before either resolves spins render_frame past refresh rate.
            if data.redraws_needed.contains(&surface.output)
                && !data.frames_pending.contains(&crtc)
                && !data.estimated_vblank_timers.contains_key(&crtc)
            {
                render_frame(data, surface, crtc, render_node);
            }
        }
    }
}

pub fn init_udev(
    event_loop: &mut EventLoop<'static, DriftWm>,
    data: &mut DriftWm,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Create libseat session
    let (mut session, session_notifier) = LibSeatSession::new()
        .map_err(|e| format!("Failed to create session (are you running from a TTY?): {e}"))?;
    let seat_name = session.seat();
    tracing::info!("Session created on seat: {seat_name}");
    tracing::info!(
        "Backend config: wait_for_frame_completion={}, disable_direct_scanout={}, disable_hardware_cursor={}",
        data.config.backend.wait_for_frame_completion,
        data.config.backend.disable_direct_scanout,
        data.config.backend.disable_hardware_cursor,
    );

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

    // 3. Open every usable GPU. The first that opens becomes the primary
    // render GPU (candidate order puts the system primary first); the others'
    // outputs scan out primary-rendered buffers via an implicit PRIME copy.

    // Multi-GPU manager: one GLES renderer per render node, created lazily as
    // nodes are added. The primary node's renderer is used for same-GPU work;
    // outputs on other GPUs go through a cross-GPU MultiRenderer (PRIME copy).
    let mut gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>> =
        GpuManager::new(GbmGlesBackend::with_context_priority(ContextPriority::High))
            .map_err(|e| format!("Failed to create GPU manager: {e}"))?;

    let mut opened: Vec<OpenedGpu> = Vec::new();
    for path in &gpu_paths {
        if let Some(gpu) = open_gpu(&mut session, &mut gpu_manager, path) {
            tracing::info!(
                "Using GPU: {} (render node {})",
                path.display(),
                gpu.render_node,
            );
            opened.push(gpu);
        }
    }
    let Some(primary) = opened.first() else {
        return Err("No usable GPU found (are you running from a TTY?)".into());
    };
    let primary_render_node = primary.render_node;
    let primary_render_formats = primary.render_formats.clone();

    // 4. Store renderer on state + create DMA-BUF global
    data.backend = Some(Backend::Udev(Box::new(UdevRenderer {
        gpu_manager,
        primary_render_node,
    })));
    let formats = data
        .backend
        .as_mut()
        .unwrap()
        .with_renderer(|r| r.dmabuf_formats());
    data.render_device = Some(primary_render_node.dev_id());
    // Capture clients allocate buffers we render INTO, so advertise the
    // render-target set (already CCS-filtered above) — not the wider
    // import set, which can include formats we can't bind as a target.
    data.render_dmabuf_formats = Some(primary_render_formats.iter().copied().collect());
    let default_feedback = DmabufFeedbackBuilder::new(primary_render_node.dev_id(), formats)
        .build()
        .expect("failed to build dmabuf feedback");
    let dmabuf_global = data
        .dmabuf_state
        .create_global_with_default_feedback::<DriftWm>(&data.display_handle, &default_feedback);
    data.dmabuf_global = Some(dmabuf_global);

    // Compile background/effect shaders on the primary renderer before any
    // frame renders (attach_gpu below queues the first frames).
    {
        let mut backend = data.backend.take().unwrap();
        backend.with_renderer(|r| {
            data.render.shadow_shader = crate::render::compile_shadow_shader(r);
            data.render.border_shader = crate::render::compile_border_shader(r);
            data.render.corner_clip_shader = crate::render::compile_corner_clip_shader(r);
            let (blur_down, blur_up, blur_mask) = crate::render::compile_blur_shaders(r);
            data.render.blur_down_shader = blur_down;
            data.render.blur_up_shader = blur_up;
            data.render.blur_mask_shader = blur_mask;
        });
        data.backend = Some(backend);
    }

    // 5. Set up libinput
    let libinput_session = LibinputSessionInterface::from(session.clone());
    let mut libinput = Libinput::new_with_udev(libinput_session);
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|_| "Failed to assign libinput seat")?;
    let libinput_backend = LibinputInputBackend::new(libinput.clone());

    event_loop
        .handle()
        .insert_source(libinput_backend, |mut event, _, data| {
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

    // 6. Register session notifier (VT switching). Pauses/resumes every DRM
    // device; libinput is seat-wide, so the one handle moves into the closure.
    event_loop
        .handle()
        .insert_source(session_notifier, move |event, _, data: &mut DriftWm| {
            match event {
                SessionEvent::PauseSession => {
                    tracing::info!("Session paused (VT switch away)");
                    libinput.suspend();
                    for device in data.udev_devices.values() {
                        device.0.borrow_mut().drm.pause();
                    }
                    for (_, token) in data.estimated_vblank_timers.drain() {
                        data.loop_handle.remove(token);
                    }
                    // Releases for held keys / cycle modifiers may not be delivered
                    // when the session is paused.
                    data.suppressed_keys.clear();
                    data.cycle_state = None;
                    data.tap.reset();
                }
                SessionEvent::ActivateSession => {
                    tracing::info!("Session resumed (VT switch back)");
                    if libinput.resume().is_err() {
                        tracing::warn!("Failed to resume libinput");
                    }
                    // VBlanks for pre-switch frames never arrive
                    data.frames_pending.clear();
                    for (_, token) in data.estimated_vblank_timers.drain() {
                        data.loop_handle.remove(token);
                    }
                    // VT switch implicitly wakes the screen. Clear DPMS-off so
                    // the render loop below actually paints; the daemon will
                    // re-request off after idle if still applicable.
                    data.dpms_off_outputs.clear();
                    data.pending_dpms.clear();
                    driftwm::protocols::output_power::OutputPowerState::refresh(data);
                    let devices: Vec<UdevDevice> = data.udev_devices.values().cloned().collect();
                    for device in &devices {
                        let mut dev = device.0.borrow_mut();
                        if let Err(e) = dev.drm.activate(false) {
                            tracing::error!("Failed to activate DRM: {e}");
                            continue;
                        }
                        let render_node = dev.render_node;
                        let DeviceData { drm, surfaces, .. } = &mut *dev;
                        for (&crtc, surface) in surfaces.iter_mut() {
                            if let Err(e) = surface.compositor.reset_state() {
                                tracing::warn!("Failed to reset DRM surface state: {e}");
                            }
                            let _ = surface.compositor.frame_submitted();
                            if let Some(ramp) = surface.pending_gamma_change.take() {
                                if apply_gamma(surface, drm, crtc, ramp.as_deref()).is_none() {
                                    tracing::warn!(
                                        "failed to re-apply gamma on session resume for crtc {crtc:?}"
                                    );
                                }
                            } else if let Some(gp) = &mut surface.gamma_props
                                && gp.has_previous_blob()
                            {
                                // VT switch clears CRTC gamma to default. Re-apply
                                // the last-set blob so a tint set before the switch
                                // doesn't silently vanish until the client re-polls.
                                // Legacy path has no equivalent — kernel doesn't
                                // retain the ramp and we don't shadow it.
                                if gp.restore_gamma(drm).is_none() {
                                    tracing::warn!(
                                        "failed to restore gamma on session resume for crtc {crtc:?}"
                                    );
                                }
                            }
                            render_frame(data, surface, crtc, render_node);
                        }
                    }
                }
            }
        })?;

    // 7. Register udev backend for hotplug (connectors AND whole GPUs)
    let udev_dispatcher = Dispatcher::new(
        udev_backend,
        move |event: UdevEvent, _, data: &mut DriftWm| match event {
            UdevEvent::Changed { device_id } => {
                let Ok(node) = DrmNode::from_dev_id(device_id) else {
                    return;
                };
                let Some(device) = data.udev_devices.get(&node).cloned() else {
                    tracing::debug!("udev change for unknown device {device_id:?}");
                    return;
                };
                tracing::debug!("Udev device changed: {device_id:?}");
                scan_device_connectors(data, &device);
            }
            UdevEvent::Added { device_id, path } => {
                let Ok(node) = DrmNode::from_dev_id(device_id) else {
                    return;
                };
                if data.udev_devices.contains_key(&node) {
                    return;
                }
                tracing::info!("Udev device added: {}", path.display());
                gpu_added(data, &path);
            }
            UdevEvent::Removed { device_id } => {
                let Ok(node) = DrmNode::from_dev_id(device_id) else {
                    return;
                };
                gpu_removed(data, node);
            }
        },
    );
    event_loop.handle().register_dispatcher(udev_dispatcher)?;

    // 8. Attach every opened GPU: registers its VBlank source, scans its
    // connectors, creates outputs and queues their first frames.
    for gpu in opened {
        attach_gpu(data, gpu);
    }

    let total_surfaces: usize = data
        .udev_devices
        .values()
        .map(|d| d.0.borrow().surfaces.len())
        .sum();
    if total_surfaces == 0 {
        return Err("No GPU with connected displays found (are you running from a TTY?)".into());
    }

    Ok(())
}

/// A DRM device opened and EGL-probed, with its render node registered on the
/// GPU manager, but not yet attached to the event loop or scanned for outputs.
struct OpenedGpu {
    node: DrmNode,
    drm: DrmDevice,
    drm_notifier: DrmDeviceNotifier,
    gbm: GbmDevice<DrmDeviceFd>,
    render_formats: Vec<Format>,
    render_node: DrmNode,
}

/// Open one DRM device and register its render node with the GPU manager.
/// Returns `None` (with a log line) on any failure so callers can skip to the
/// next candidate.
fn open_gpu(
    session: &mut LibSeatSession,
    gpu_manager: &mut GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    path: &Path,
) -> Option<OpenedGpu> {
    let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;

    let node = match DrmNode::from_path(path) {
        Ok(n) => n,
        Err(e) => {
            tracing::debug!("{}: not a DRM node ({e}), skipping", path.display());
            return None;
        }
    };
    if node.ty() != NodeType::Primary {
        tracing::debug!("{}: not a primary node, skipping", path.display());
        return None;
    }

    let fd = match session.open(path, open_flags) {
        Ok(fd) => fd,
        Err(e) => {
            tracing::warn!("{}: failed to open ({e})", path.display());
            return None;
        }
    };
    let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    // true = release existing CRTCs for a clean modeset (avoids conflicts
    // with previous session's DRM state)
    let (drm, drm_notifier) = match DrmDevice::new(device_fd.clone(), true) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!("{}: failed to create DRM device ({e})", path.display());
            return None;
        }
    };

    let gbm = match GbmDevice::new(device_fd) {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("{}: failed to create GBM device ({e})", path.display());
            return None;
        }
    };

    let egl_display = match unsafe { EGLDisplay::new(gbm.clone()) } {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("{}: failed to create EGL display ({e})", path.display());
            return None;
        }
    };
    if EGLDevice::device_for_display(&egl_display).is_ok_and(|d| d.is_software()) {
        tracing::warn!("{}: software EGL device, skipping", path.display());
        return None;
    }
    // High priority lets the compositor's composite preempt a
    // GPU-saturating client (shader compile, screen-share encode) instead
    // of queuing behind it. EGL_IMG_context_priority is best-effort:
    // smithay falls back to default priority if the extension is absent, and
    // some drivers (notably NVIDIA) may only partially honor it.
    let egl_context = match EGLContext::new_with_priority(&egl_display, ContextPriority::High) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("{}: failed to create EGL context ({e})", path.display());
            return None;
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
    // Ask EGL/Mesa for the actual rendering device — on split-DRM
    // systems the KMS node we opened has no render node, but Mesa
    // routes rendering through the right GPU under the hood. We need
    // to advertise that GPU's render node to clients (`zwp_linux_dmabuf_v1`
    // feedback, xdph-wlr) so they don't crash trying to use the
    // display-only node.
    let render_node = EGLDevice::device_for_display(&egl_display)
        .ok()
        .and_then(|d| d.try_get_render_node().ok().flatten())
        .or_else(|| node.node_with_type(NodeType::Render).and_then(|n| n.ok()))
        .unwrap_or_else(|| {
            tracing::warn!(
                "could not resolve a DRM render node; falling back to KMS node {node:?} \
                 — capture clients may misbehave"
            );
            node
        });

    // Drop the probe context before handing the GBM device to the GPU
    // manager, which creates its own EGL display + GLES renderer for the
    // render node (avoids two high-priority contexts on the same device).
    // add_node is a no-op if another KMS device already registered this
    // render node (split-DRM boards routing through one render GPU).
    drop(egl_context);
    if let Err(e) = gpu_manager.as_mut().add_node(render_node, gbm.clone()) {
        tracing::warn!(
            "{}: failed to add render node to GPU manager ({e})",
            path.display()
        );
        return None;
    }

    Some(OpenedGpu {
        node,
        drm,
        drm_notifier,
        gbm,
        render_formats,
        render_node,
    })
}

/// Wire an opened GPU into the compositor: register its VBlank event source,
/// store it in `udev_devices`, and scan its connectors (creating outputs and
/// queuing their first frames).
fn attach_gpu(data: &mut DriftWm, gpu: OpenedGpu) {
    let OpenedGpu {
        node,
        drm,
        drm_notifier,
        gbm,
        render_formats,
        render_node,
    } = gpu;

    log_drm_connectors(&drm);

    let device = UdevDevice(Rc::new(RefCell::new(DeviceData {
        kms_node: node,
        render_node,
        drm,
        gbm,
        drm_scanner: DrmScanner::new(),
        surfaces: HashMap::new(),
        render_formats,
        drm_token: None,
    })));

    let device_for_drm = device.clone();
    let token =
        data.loop_handle
            .insert_source(drm_notifier, move |event, meta, data: &mut DriftWm| {
                let mut dev = device_for_drm.0.borrow_mut();
                let render_node = dev.render_node;
                match event {
                    DrmEvent::VBlank(crtc) => {
                        let Some(surface) = dev.surfaces.get_mut(&crtc) else {
                            return;
                        };
                        match surface.compositor.frame_submitted() {
                            Ok(Some(mut feedback)) => {
                                deliver_presentation(&mut feedback, &surface.output, meta.as_ref());
                            }
                            Ok(None) => {}
                            Err(e) => tracing::warn!("frame_submitted error: {e:?}"),
                        }
                        data.frames_pending.remove(&crtc);
                        // Real VBlank beat any estimated-VBlank timer we might have armed.
                        if let Some(token) = data.estimated_vblank_timers.remove(&crtc) {
                            data.loop_handle.remove(token);
                        }
                        if data.redraws_needed.contains(&surface.output) {
                            render_frame(data, surface, crtc, render_node);
                        }
                    }
                    DrmEvent::Error(err) => {
                        tracing::error!("DRM error: {err}");
                    }
                }
            });
    match token {
        Ok(token) => device.0.borrow_mut().drm_token = Some(token),
        Err(e) => tracing::error!("Failed to register DRM event source for {node}: {e}"),
    }

    data.udev_devices.insert(node, device.clone());
    scan_device_connectors(data, &device);
}

/// Rescan a device's connectors, creating surfaces for newly connected ones
/// and tearing down disconnected ones. Shared between initial attach and
/// udev change events.
fn scan_device_connectors(data: &mut DriftWm, device: &UdevDevice) {
    {
        let mut dev = device.0.borrow_mut();
        let DeviceData {
            kms_node,
            render_node,
            ref mut drm_scanner,
            ref mut drm,
            ref gbm,
            ref render_formats,
            ref mut surfaces,
            ..
        } = *dev;
        let scan_result = match drm_scanner.scan_connectors(&*drm) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Failed to scan connectors: {e}");
                return;
            }
        };
        for scan_event in scan_result {
            match scan_event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    if surfaces.contains_key(&crtc) {
                        continue;
                    }
                    tracing::info!(
                        "Connector connected: {}-{} (CRTC {:?})",
                        connector_type_name(&connector),
                        connector.interface_id(),
                        crtc,
                    );
                    // Replace any virtual placeholder outputs. The unmap-to-
                    // create_surface sequence is synchronous within this
                    // connector handler, so active_output() is never None.
                    if !data.disconnected_outputs.is_empty() {
                        let virtual_outputs: Vec<_> = data
                            .space
                            .outputs()
                            .filter(|o| data.disconnected_outputs.contains(&o.name()))
                            .cloned()
                            .collect();
                        for old in &virtual_outputs {
                            data.space.unmap_output(old);
                            data.render.remove_output(&old.name());
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
                        render_node,
                        kms_node,
                        &connector,
                        crtc,
                        &dh,
                        data,
                        &saved,
                    ) {
                        surfaces.insert(crtc, sd);
                        data.active_outputs.insert(surfaces[&crtc].output.clone());
                        // Pin any windows orphaned by the virtual-output swap to
                        // the freshly connected monitor.
                        let new_output = surfaces[&crtc].output.clone();
                        data.reassign_orphaned_pinned(&new_output);
                        let surface = surfaces.get_mut(&crtc).unwrap();
                        // Notify existing toplevels about the new output
                        driftwm::protocols::foreign_toplevel::send_output_enter_all(
                            &mut data.foreign_toplevel_state,
                            &surface.output,
                        );
                        render_frame(data, surface, crtc, render_node);
                    }
                }
                DrmScanEvent::Connected {
                    connector,
                    crtc: None,
                } => {
                    tracing::warn!(
                        "Connector {}-{} has no available CRTC",
                        connector_type_name(&connector),
                        connector.interface_id()
                    );
                }
                DrmScanEvent::Disconnected {
                    crtc: Some(crtc), ..
                } => {
                    tracing::info!("Hotplug: CRTC {crtc:?} disconnected");
                    if let Some(surface) = surfaces.remove(&crtc) {
                        // "Last output" spans all devices: another GPU's
                        // monitor keeps the canvas alive.
                        let is_last = surfaces.is_empty()
                            && data
                                .udev_devices
                                .values()
                                .filter(|d| !Rc::ptr_eq(&d.0, &device.0))
                                .all(|d| d.0.borrow().surfaces.is_empty());
                        teardown_output(data, surface, is_last);
                    }
                    data.frames_pending.remove(&crtc);
                    if let Some(token) = data.estimated_vblank_timers.remove(&crtc) {
                        data.loop_handle.remove(token);
                    }
                }
                _ => {}
            }
        }
    }
    // Notify output management clients after connector changes; heads span
    // every device, so aggregate across all of them.
    let head_state = collect_all_head_states(data);
    driftwm::protocols::output_management::notify_changes::<DriftWm>(
        &mut data.output_management_state,
        head_state,
    );
}

/// A whole GPU appeared at runtime (eGPU dock, driver rebind): open it, add
/// its render node to the GPU manager, and light up its outputs.
fn gpu_added(data: &mut DriftWm, path: &Path) {
    let Some(Backend::Udev(udev)) = data.backend.as_mut() else {
        return;
    };
    let Some(session) = data.session.as_mut() else {
        return;
    };
    let Some(gpu) = open_gpu(session, &mut udev.gpu_manager, path) else {
        return;
    };
    attach_gpu(data, gpu);
}

/// A whole GPU disappeared: tear down its outputs, drop its event source and
/// (unless shared or primary) its renderer.
fn gpu_removed(data: &mut DriftWm, node: DrmNode) {
    let Some(device) = data.udev_devices.remove(&node) else {
        tracing::debug!("udev removal for unknown device {node}");
        return;
    };
    tracing::info!("GPU removed: {node}");

    let (surfaces, token, render_node) = {
        let mut dev = device.0.borrow_mut();
        let surfaces: Vec<(crtc::Handle, SurfaceData)> = dev.surfaces.drain().collect();
        (surfaces, dev.drm_token.take(), dev.render_node)
    };
    if let Some(token) = token {
        data.loop_handle.remove(token);
    }

    let surviving: usize = data
        .udev_devices
        .values()
        .map(|d| d.0.borrow().surfaces.len())
        .sum();
    let count = surfaces.len();
    for (i, (crtc, surface)) in surfaces.into_iter().enumerate() {
        data.frames_pending.remove(&crtc);
        if let Some(t) = data.estimated_vblank_timers.remove(&crtc) {
            data.loop_handle.remove(t);
        }
        teardown_output(data, surface, surviving == 0 && i + 1 == count);
    }

    if let Some(Backend::Udev(udev)) = data.backend.as_mut() {
        if render_node == udev.primary_render_node {
            // Surviving outputs render on this node; nothing sane to do but
            // keep it and hope only the KMS side went away.
            if !data.udev_devices.is_empty() {
                tracing::error!(
                    "Primary render GPU {render_node} removed with outputs remaining — \
                     rendering may fail until restart"
                );
            }
        } else if !data
            .udev_devices
            .values()
            .any(|d| d.0.borrow().render_node == render_node)
        {
            udev.gpu_manager.as_mut().remove_node(&render_node);
        }
    }

    let head_state = collect_all_head_states(data);
    driftwm::protocols::output_management::notify_changes::<DriftWm>(
        &mut data.output_management_state,
        head_state,
    );
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
    if let Some(preferred) = modes
        .iter()
        .find(|m| m.mode_type().contains(control::ModeTypeFlags::PREFERRED))
    {
        return Some(*preferred);
    }
    modes
        .iter()
        .max_by_key(|m| {
            let (w, h) = m.size();
            (w as u64 * h as u64, m.vrefresh() as u64)
        })
        .copied()
}

/// Where a chosen mode came from. `SynthesizedCvt` modes haven't been
/// validated by the kernel yet — callers should be prepared to retry with
/// `pick_preferred_mode` if the atomic-test fails.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModeSource {
    Edid,
    SynthesizedCvt,
}

/// Select a mode based on output config, falling back to preferred.
/// For `SizeRefresh` rules that don't match any EDID-advertised mode,
/// synthesize a CVT modeline — this lets users drive CRTs above their
/// EDID-reported refresh range.
pub(crate) fn pick_mode_for_config(
    modes: &[control::Mode],
    config: &ConfigOutputMode,
    connector_name: &str,
) -> Option<(control::Mode, ModeSource)> {
    match config {
        ConfigOutputMode::Preferred => pick_preferred_mode(modes).map(|m| (m, ModeSource::Edid)),
        ConfigOutputMode::Size(w, h) => {
            let matched = modes
                .iter()
                .filter(|m| m.size() == (*w as u16, *h as u16))
                .max_by_key(|m| m.vrefresh() as u64);
            if matched.is_none() {
                tracing::warn!("No mode matching {w}x{h}, falling back to preferred");
            }
            matched
                .copied()
                .map(|m| (m, ModeSource::Edid))
                .or_else(|| pick_preferred_mode(modes).map(|m| (m, ModeSource::Edid)))
        }
        ConfigOutputMode::SizeRefresh(w, h, hz) => {
            if let Some(m) = modes
                .iter()
                .find(|m| m.size() == (*w as u16, *h as u16) && m.vrefresh() == *hz)
            {
                return Some((*m, ModeSource::Edid));
            }
            tracing::warn!(
                "Output {connector_name}: mode {w}x{h}@{hz}Hz not in EDID, synthesizing CVT modeline"
            );
            match cvt::synth_cvt(*w as u16, *h as u16, *hz) {
                Ok(raw) => Some((control::Mode::from(raw), ModeSource::SynthesizedCvt)),
                Err(e) => {
                    tracing::error!(
                        "Output {connector_name}: CVT synthesis failed ({e}), falling back to preferred"
                    );
                    pick_preferred_mode(modes).map(|m| (m, ModeSource::Edid))
                }
            }
        }
    }
}

/// Resolve a queued `ModeIntent` to a concrete `control::Mode` for the given
/// connector. `Custom` first looks for an exact EDID match; only synthesizes
/// CVT if nothing matches.
fn resolve_pending_mode(
    intent: &crate::state::ModeIntent,
    connector: &connector::Info,
    connector_name: &str,
) -> Option<control::Mode> {
    match intent {
        crate::state::ModeIntent::EdidIndex(idx) => connector.modes().get(*idx).copied(),
        crate::state::ModeIntent::Custom { w, h, refresh_mhz } => {
            let hz = (*refresh_mhz / 1000) as u32;
            if let Some(m) = connector
                .modes()
                .iter()
                .find(|m| m.size() == (*w as u16, *h as u16) && m.vrefresh() == hz)
            {
                return Some(*m);
            }
            match cvt::synth_cvt(*w as u16, *h as u16, hz) {
                Ok(raw) => Some(control::Mode::from(raw)),
                Err(e) => {
                    tracing::error!(
                        "Output {connector_name}: CVT synthesis failed ({e}) for {w}x{h}@{hz}Hz"
                    );
                    None
                }
            }
        }
        crate::state::ModeIntent::Preferred => pick_preferred_mode(connector.modes()),
    }
}

#[allow(clippy::too_many_arguments)]
fn create_surface(
    drm: &mut DrmDevice,
    gbm: &GbmDevice<DrmDeviceFd>,
    render_formats: &[Format],
    render_node: DrmNode,
    kms_node: DrmNode,
    connector: &connector::Info,
    crtc: crtc::Handle,
    dh: &smithay::reexports::wayland_server::DisplayHandle,
    state: &mut DriftWm,
    saved_output_state: &std::collections::HashMap<
        String,
        (smithay::utils::Point<f64, smithay::utils::Logical>, f64),
    >,
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
    let (mode, mode_source) =
        pick_mode_for_config(connector.modes(), config_mode, &connector_name)?;
    tracing::info!(
        "Output {connector_name}: mode {}x{}@{}Hz ({:?})",
        mode.size().0,
        mode.size().1,
        mode.vrefresh(),
        mode_source,
    );

    let (drm_surface, mode) = match drm.create_surface(crtc, mode, &[connector.handle()]) {
        Ok(s) => (s, mode),
        Err(e) if mode_source == ModeSource::SynthesizedCvt => {
            tracing::error!(
                "Output {connector_name}: synthesized CVT mode rejected by kernel ({e}), falling back to preferred"
            );
            let fallback = pick_preferred_mode(connector.modes())?;
            match drm.create_surface(crtc, fallback, &[connector.handle()]) {
                Ok(s) => (s, fallback),
                Err(e2) => {
                    tracing::error!("FAILED: drm.create_surface (preferred fallback): {e2}");
                    return None;
                }
            }
        }
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
    let serial_number = edid.as_ref().and_then(|i| i.serial()).unwrap_or_default();
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
    let scale_val = output_cfg.and_then(|c| c.scale).unwrap_or_else(|| {
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
    let layout_position: smithay::utils::Point<i32, smithay::utils::Logical> =
        match output_cfg.map(|c| &c.position) {
            Some(OutputPosition::Fixed(x, y)) => {
                tracing::info!("Output {connector_name}: layout position ({x}, {y}) from config");
                (*x, *y).into()
            }
            _ => {
                // Auto: place left-to-right by connection order
                let auto_x: i32 = state
                    .space
                    .outputs()
                    .map(|o| crate::state::output_logical_size(o).w)
                    .sum();
                tracing::info!("Output {connector_name}: auto layout position ({auto_x}, 0)");
                (auto_x, 0).into()
            }
        };
    output.change_current_state(
        Some(output_mode),
        Some(transform),
        Some(scale),
        Some(layout_position),
    );
    output.set_preferred(output_mode);
    let global = output.create_global::<DriftWm>(dh);

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    // The exporter's import filter must name this device's render node:
    // client buffers allocated there can go to a KMS plane directly
    // (`NodeFilter::None` would veto direct scanout entirely).
    let compositor = match DrmCompositor::new(
        &output,
        drm_surface,
        None,
        allocator.clone(),
        GbmFramebufferExporter::new(gbm.clone(), render_node.into()),
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
            tracing::warn!("DrmCompositor failed ({e:?}), retrying with implicit modifier");
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
                GbmFramebufferExporter::new(gbm.clone(), render_node.into()),
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

    init_output_state(&output, camera, state.config.drift, layout_position);

    // Restore per-output camera/zoom from state file if available
    if let Some(&(saved_cam, saved_zoom)) = saved_output_state.get(&connector_name) {
        let mut os = crate::state::output_state(&output);
        os.camera = saved_cam;
        os.zoom = saved_zoom;
        tracing::info!(
            "Output {connector_name}: restored camera ({:.0}, {:.0}) zoom {:.3}",
            saved_cam.x,
            saved_cam.y,
            saved_zoom
        );
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
    state.recompute_decoration_scale();

    let gamma_props = GammaProps::new(drm, crtc);
    if gamma_props.is_none() {
        tracing::info!(
            "GAMMA_LUT atomic property unavailable on CRTC {crtc:?} — falling back to legacy \
             drmModeCrtcSetGamma ioctl. Driver may not expose GAMMA_LUT/GAMMA_LUT_SIZE properties."
        );
    }

    let mut dmabuf_feedback = None;
    if let Some(crate::backend::Backend::Udev(udev)) = state.backend.as_mut() {
        let primary_render_node = udev.primary_render_node;
        if let Ok(renderer) = udev.gpu_manager.single_renderer(&primary_render_node) {
            let primary_formats = renderer.dmabuf_formats();
            drop(renderer);
            match surface_dmabuf_feedback(
                &compositor,
                primary_formats,
                primary_render_node,
                render_node,
                kms_node,
            ) {
                Ok(f) => dmabuf_feedback = Some(f),
                Err(e) => {
                    tracing::warn!("Failed to build dmabuf feedback for {connector_name}: {e}");
                }
            }
        }
    }

    Some(SurfaceData {
        compositor,
        output,
        connector: connector.handle(),
        make,
        model,
        serial_number,
        global,
        gamma_props,
        pending_gamma_change: None,
        dmabuf_feedback,
    })
}

/// Tear down a `wl_output` global. Disables it now so clients see the
/// removal event, then queues a delayed `remove_global` so any in-flight
/// bind requests don't hit a freed global and get protocol-killed.
fn remove_output_global(data: &mut DriftWm, global: GlobalId) {
    data.display_handle
        .disable_global::<DriftWm>(global.clone());
    let dh = data.display_handle.clone();
    let timer = Timer::from_duration(Duration::from_secs(10));
    if let Err(e) = data
        .loop_handle
        .insert_source(timer, move |_, _, _: &mut DriftWm| {
            dh.remove_global::<DriftWm>(global.clone());
            TimeoutAction::Drop
        })
    {
        tracing::warn!("Failed to schedule wl_output global removal: {e:?}");
    }
}

/// Drop everything bound to a disconnected output.
///
/// Runs whether the output is the last surviving one or not. The "last output"
/// path keeps the [`Output`] in the [`Space`] as a virtual placeholder (so
/// `active_output()` stays `Some` while a USB monitor is replugged) but still
/// needs the grab/gesture/focus cleanup — otherwise a move grab pinned to the
/// dying output keeps mutating its stale per-output state on every cursor event.
fn teardown_output(data: &mut DriftWm, surface: SurfaceData, is_last: bool) {
    let SurfaceData { output, global, .. } = surface;

    driftwm::protocols::foreign_toplevel::send_output_leave_all(
        &mut data.foreign_toplevel_state,
        &output,
    );
    data.image_copy_capture_state.remove_output(&output);
    data.screencopy_state.remove_output(&output);
    data.gamma_control_manager_state.output_removed(&output);

    // Fail + drop pending captures that can no longer render — a stranded entry
    // hangs the client and leaks its buffer fd. Toplevel captures drain on any
    // output's render path, but when this was the *last* output no CRTC remains
    // to run them (the virtual placeholder is never rendered), so they're dead.
    // Screencopy's Drop sends failed() itself; ext-image-copy frames must be
    // failed explicitly.
    data.pending_screencopies.retain(|s| s.output() != &output);
    {
        use driftwm::protocols::image_copy_capture::PendingCaptureKind;
        use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason;
        let mut i = 0;
        while i < data.pending_captures.len() {
            let dead = match &data.pending_captures[i].kind {
                PendingCaptureKind::Output(o) => o == &output,
                PendingCaptureKind::Toplevel(_) => is_last,
            };
            if dead {
                data.pending_captures
                    .swap_remove(i)
                    .frame
                    .failed(FailureReason::Unknown);
            } else {
                i += 1;
            }
        }
    }

    // Disable the wl_output global before any further state mutation so clients
    // (wf-recorder, swayosd, etc.) see the removal first.
    remove_output_global(data, global);

    // Close layer surfaces hosted on this output. They'll re-anchor against
    // remaining outputs on their next configure round-trip.
    for layer in smithay::desktop::layer_map_for_output(&output).layers() {
        layer.layer_surface().send_close();
    }

    // Grabs (move/resize/pan/navigate) clone the Output and keep mutating its
    // per-output state on every motion. Cancel before the output goes away.
    if let Some(pointer) = data.seat.get_pointer() {
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        pointer.unset_grab(data, serial, 0);
    }
    if data.gesture_output.as_ref().is_some_and(|go| go == &output) {
        data.gesture_output = None;
        data.gesture_state = None;
    }

    data.exit_fullscreen_on(&output);
    data.render.remove_output(&output.name());
    data.lock_surfaces.remove(&output);
    data.active_outputs.remove(&output);
    data.redraws_needed.remove(&output);

    if is_last {
        // Keep the Output mapped as a virtual placeholder so active_output()
        // and other queries stay Some while no monitor is attached. The DRM
        // surface and wl_output global are already gone, so it's purely an
        // input-routing/coordinate-system anchor.
        tracing::warn!(
            "Last output disconnected — keeping virtual output '{}'",
            output.name()
        );
        data.disconnected_outputs.insert(output.name());
    } else {
        data.space.unmap_output(&output);
        // Reassign screen-pinned windows on the gone output to a survivor.
        let pin_target = data.space.outputs().next().cloned();
        if let Some(target) = pin_target {
            data.reassign_orphaned_pinned(&target);
        }
        data.recompute_decoration_scale();
        data.fullscreen.remove(&output);
        data.dpms_off_outputs.remove(&output);
        data.pending_dpms.remove(&output);

        if data.focused_output.as_ref().is_some_and(|fo| fo == &output) {
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
    }
}

/// Render a single frame and queue it to the DRM compositor. `render_node` is
/// the owning device's render node, deciding same-GPU vs cross-GPU rendering.
fn render_frame(
    data: &mut DriftWm,
    surface: &mut SurfaceData,
    crtc: crtc::Handle,
    render_node: DrmNode,
) {
    #[cfg(feature = "profile-with-tracy")]
    let _span = tracy_client::span!("udev::render_frame");

    let SurfaceData {
        compositor,
        output,
        dmabuf_feedback,
        ..
    } = surface;
    let output = &*output;

    #[cfg(feature = "profile-with-tracy")]
    {
        static COMMITS_PLOT: std::sync::OnceLock<tracy_client::PlotName> =
            std::sync::OnceLock::new();
        let commits = COMMITS_PLOT
            .get_or_init(|| tracy_client::PlotName::new_leak("frame.commits".to_string()));
        if let Some(client) = tracy_client::Client::running() {
            client.plot(*commits, data.commits_since_render as f64);
        }
    }
    data.commits_since_render = 0;

    data.redraws_needed.remove(output);

    // Flush Wayland clients
    data.display_handle.flush_clients().ok();

    // Read per-output state for this frame
    let (cur_camera, cur_zoom, last_cam, last_zoom) = {
        let os = crate::state::output_state(output);
        (
            os.camera,
            os.zoom,
            os.last_rendered_camera,
            os.last_rendered_zoom,
        )
    };

    // Update background element
    let (camera_moved, zoom_changed) = crate::render::update_background_element(
        data, output, cur_camera, cur_zoom, last_cam, last_zoom,
    );

    // Force full redraw when viewport shifts — DrmCompositor's damage tracker
    // doesn't know all elements moved, so without this we get partial-update artifacts.
    if camera_moved || zoom_changed {
        compositor.reset_buffer_ages();
    }

    // Force full redraw when animated background is visible through transparent windows.
    // smithay's buffer-age optimisation skips recompositing windows whose surface content
    // didn't change — but transparent windows show the background through them, so when
    // the background shader advances a frame the stale composited result is reused and
    // the background appears "frozen" inside those windows.
    // Fix: reset buffer ages so every pixel is redrawn from scratch this frame.
    if data.render.background_is_animated {
        let has_transparent = data.space.elements().any(|w| {
            w.wl_surface()
                .as_deref()
                .and_then(driftwm::config::applied_rule)
                .and_then(|r| r.opacity)
                .is_some_and(|o| o < 1.0)
        });
        if has_transparent {
            compositor.reset_buffer_ages();
        }
    }

    // Take the backend out to split the borrow from state, then grab a
    // MultiRenderer for the whole frame. Same GPU: plain single_renderer.
    // Output on another GPU: render on the primary, copy to the scanout GPU
    // in the compositor's framebuffer format (implicit PRIME).
    let mut backend = data.backend.take().unwrap();
    let Backend::Udev(udev) = &mut backend else {
        data.backend = Some(backend);
        return;
    };
    let renderer = if render_node == udev.primary_render_node {
        udev.gpu_manager.single_renderer(&udev.primary_render_node)
    } else {
        udev.gpu_manager
            .renderer(&udev.primary_render_node, &render_node, compositor.format())
    };
    let mut renderer = match renderer {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Failed to acquire renderer for {render_node}: {e:?}");
            data.backend = Some(backend);
            queue_estimated_vblank_timer(data, output, crtc);
            return;
        }
    };

    // Build cursor + compose frame
    let cursor_alpha = if data.active_output().as_ref() == Some(output) {
        1.0
    } else if data.is_output_fullscreen(output) || data.is_fullscreen() {
        // The ghost cursor shows where the pointer sits on the shared canvas,
        // which only applies between canvas viewports. A fullscreen output is
        // not one — don't ghost the pointer onto a fullscreen output's window,
        // nor project a fullscreen output's pointer onto other monitors.
        0.0
    } else {
        data.config.inactive_cursor_opacity as f32
    };
    #[cfg(feature = "profile-with-tracy")]
    let _cursor_span = tracy_client::span!("udev::build_cursor_elements");
    let cursor_elements = crate::render::build_cursor_elements(
        data,
        &mut renderer,
        cur_camera,
        cur_zoom,
        output.current_scale().fractional_scale(),
        cursor_alpha,
    );
    #[cfg(feature = "profile-with-tracy")]
    drop(_cursor_span);
    let elements = crate::render::compose_frame(data, &mut renderer, output, cursor_elements);

    // Overlay planes are left off — they cause hard-to-diagnose flicker on some
    // hardware. disable_hardware_cursor composites the cursor into the frame instead
    // of using the KMS cursor plane: a workaround for NVIDIA, where a system-memory
    // cursor buffer can't be scanned out (stutter/tearing), while keeping direct
    // scanout for fullscreen apps.
    //
    // Also skip cursor plane scanout when the cursor is dimmed: smithay's cursor plane
    // cache is keyed by element id + commit and ignores alpha, so a 1.0 → <1.0 change
    // reuses the previously-drawn opaque buffer. GPU compositing reapplies alpha.
    let mut frame_flags = FrameFlags::empty();
    if !data.config.backend.disable_direct_scanout {
        frame_flags |= FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY;
    }
    if cursor_alpha >= 1.0 && !data.config.backend.disable_hardware_cursor {
        frame_flags |= FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT;
    }

    // Render via DRM compositor (latency-sensitive — do first)
    #[cfg(feature = "profile-with-tracy")]
    let _composite_span = tracy_client::span!("udev::compositor_render_frame");
    let render_result = compositor.render_frame(
        &mut renderer,
        &elements,
        [0.0f32, 0.0, 0.0, 1.0],
        frame_flags,
    );
    #[cfg(feature = "profile-with-tracy")]
    drop(_composite_span);

    // CPU-wait on the GPU fence when KMS can't gate the flip on it
    // (typical on NVIDIA — EGL fence isn't exportable as IN_FENCE_FD).
    // Config flag forces the wait even when smithay says it's not needed.
    if let Ok(ref rr) = render_result
        && (rr.needs_sync() || data.config.backend.wait_for_frame_completion)
        && let PrimaryPlaneElement::Swapchain(ref element) = rr.primary_element
    {
        tracing::debug!(
            "Fence wait: needs_sync={}, force={}",
            rr.needs_sync(),
            data.config.backend.wait_for_frame_completion,
        );
        let _ = element.sync.wait();
    }

    match render_result {
        Ok(render_result) => {
            crate::render::update_primary_scanout_output(data, output, &render_result.states);
            if let Some(surface_feedback) = dmabuf_feedback.as_ref() {
                crate::render::send_dmabuf_feedbacks(
                    data,
                    output,
                    surface_feedback,
                    &render_result.states,
                );
            }
            let feedback =
                crate::render::take_presentation_feedback(data, output, &render_result.states);
            let queue_result = {
                #[cfg(feature = "profile-with-tracy")]
                let _span = tracy_client::span!("udev::queue_frame");
                compositor.queue_frame(feedback)
            };
            match queue_result {
                Ok(()) => {
                    data.frames_pending.insert(crtc);
                }
                Err(FrameError::EmptyFrame) => {
                    // No page flip - no real VBlank to wake us. Always arm the
                    // estimated timer so the render gate paces re-renders to the refresh
                    // period; otherwise a dirty-but-unchanged output spins render_frame.
                    queue_estimated_vblank_timer(data, output, crtc);
                }
                Err(e) => {
                    tracing::warn!("Failed to queue frame: {e:?}");
                    queue_estimated_vblank_timer(data, output, crtc);
                }
            }
        }
        Err(e) => {
            tracing::warn!("Render frame error: {e:?}");
            queue_estimated_vblank_timer(data, output, crtc);
        }
    }

    // Fulfill capture requests after main render
    #[cfg(feature = "profile-with-tracy")]
    let _captures_span = tracy_client::span!("udev::captures");
    crate::render::render_screencopy(data, &mut renderer, output, &elements);
    crate::render::render_capture_frames(data, &mut renderer, output, &elements);
    crate::render::render_toplevel_captures(data, &mut renderer);
    #[cfg(feature = "profile-with-tracy")]
    drop(_captures_span);

    // Drop the renderer (borrows the backend's GPU manager) before putting the
    // backend back on state.
    drop(renderer);
    data.backend = Some(backend);

    // Record camera+zoom for next-frame change detection
    {
        let mut os = crate::state::output_state(output);
        os.last_rendered_camera = os.camera;
        os.last_rendered_zoom = os.zoom;
    }
    data.write_state_file_if_dirty();

    // Post-render
    #[cfg(feature = "profile-with-tracy")]
    let _post_span = tracy_client::span!("udev::post_render");
    crate::render::post_render(data, output);
    data.display_handle.flush_clients().ok();
    #[cfg(feature = "profile-with-tracy")]
    drop(_post_span);

    #[cfg(feature = "profile-with-tracy")]
    {
        drop(_span);
        tracy_client::Client::running().map(|c| c.frame_mark());
    }
}

/// Forward a page-flip's timing to all clients waiting on `wp_presentation`.
/// `meta` carries the kernel timestamp + sequence; if it's missing (rare on
/// some drivers) we discard rather than fabricate, per protocol guidance.
fn deliver_presentation(
    feedback: &mut smithay::desktop::utils::OutputPresentationFeedback,
    output: &Output,
    meta: Option<&smithay::backend::drm::DrmEventMetadata>,
) {
    use smithay::backend::drm::DrmEventTime as DrmTime;
    use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
    use smithay::wayland::presentation::Refresh;

    let Some(meta) = meta else {
        feedback.discarded();
        return;
    };

    let refresh_picos = output
        .current_mode()
        .map(|m| (1_000_000_000_000u64 / (m.refresh.max(1) as u64)) as u32)
        .unwrap_or(0);
    let refresh = Refresh::Fixed(Duration::from_nanos(refresh_picos as u64));

    let flags = wp_presentation_feedback::Kind::Vsync
        | wp_presentation_feedback::Kind::HwClock
        | wp_presentation_feedback::Kind::HwCompletion;

    match meta.time {
        DrmTime::Monotonic(time) => {
            feedback.presented::<_, smithay::utils::Monotonic>(
                time,
                refresh,
                meta.sequence as u64,
                flags,
            );
        }
        DrmTime::Realtime(_) => {
            // We advertised CLOCK_MONOTONIC; a realtime stamp from the kernel
            // can't be reported safely against that clock id.
            feedback.discarded();
        }
    }
}

/// Wake the VBlank-driven loop at ~one refresh period when queue_frame returned
/// EmptyFrame, so ongoing animations keep ticking. Idempotent per CRTC.
fn queue_estimated_vblank_timer(data: &mut DriftWm, output: &Output, crtc: crtc::Handle) {
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
    match data
        .loop_handle
        .insert_source(timer, move |_, _, data: &mut DriftWm| {
            data.estimated_vblank_timers.remove(&crtc);
            TimeoutAction::Drop
        }) {
        Ok(tok) => {
            data.estimated_vblank_timers.insert(crtc, tok);
        }
        Err(e) => tracing::warn!("Failed to insert estimated VBlank timer: {e:?}"),
    }
}

use driftwm::protocols::output_management::{ModeInfo, OutputHeadState};

/// Apply queued mode changes for outputs on this device via
/// `DrmCompositor::use_mode`; entries for other devices' outputs are returned
/// unclaimed. Entries for outputs with a frame in flight are deferred back
/// into `data.pending_mode_changes` (bounded retries) so we don't modeset on
/// top of an in-progress page flip.
fn apply_pending_mode_changes(
    drm: &DrmDevice,
    surfaces: &mut HashMap<crtc::Handle, SurfaceData>,
    data: &mut DriftWm,
    pending: HashMap<String, crate::state::PendingMode>,
) -> HashMap<String, crate::state::PendingMode> {
    use smithay::reexports::drm::control::Device as ControlDevice;
    const MAX_RETRIES: u8 = 3;

    let mut unclaimed = HashMap::new();
    for (name, mut pm) in pending {
        let Some((crtc, surface)) = surfaces.iter_mut().find(|(_, s)| s.output.name() == name)
        else {
            unclaimed.insert(name, pm);
            continue;
        };

        // Defer if a page flip is in flight on this CRTC — modesetting on top
        // of pending frames is undefined behavior.
        if data.frames_pending.contains(crtc) {
            if pm.retry_count >= MAX_RETRIES {
                tracing::error!(
                    "Mode change for '{name}' dropped after {MAX_RETRIES} deferrals (frames stuck pending)"
                );
                continue;
            }
            pm.retry_count += 1;
            data.pending_mode_changes.insert(name, pm);
            continue;
        }

        let connector = match ControlDevice::get_connector(drm, surface.connector, false) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Mode change for '{name}': get_connector failed: {e}");
                continue;
            }
        };

        let Some(mode) = resolve_pending_mode(&pm.intent, &connector, &name) else {
            tracing::error!(
                "Mode change for '{name}': could not resolve intent {:?}",
                pm.intent
            );
            continue;
        };

        match surface.compositor.use_mode(mode) {
            Ok(_) => {
                let new_smithay_mode = Mode {
                    size: (mode.size().0 as i32, mode.size().1 as i32).into(),
                    refresh: (mode.vrefresh() * 1000) as i32,
                };
                surface
                    .output
                    .change_current_state(Some(new_smithay_mode), None, None, None);
                surface.output.set_preferred(new_smithay_mode);
                // Re-anchor layer surfaces (waybar/mako/swaync) to the new
                // output dimensions. Without this they keep their old
                // geometry until the client re-anchors itself.
                {
                    let mut map = smithay::desktop::layer_map_for_output(&surface.output);
                    map.arrange();
                }
                // Resize fullscreen window (if any) to the new viewport.
                let new_size =
                    smithay::utils::Size::from((mode.size().0 as i32, mode.size().1 as i32));
                data.resize_fullscreen_for_output(&surface.output, new_size);
                data.render.remove_output(&name);
                data.redraws_needed.insert(surface.output.clone());
                data.output_config_dirty = true;
                tracing::info!(
                    "Mode change applied to '{name}': {}x{}@{}Hz",
                    mode.size().0,
                    mode.size().1,
                    mode.vrefresh(),
                );
            }
            Err(e) => {
                tracing::error!("Mode change rejected by kernel for '{name}': {e:?}");
                // Re-broadcast so clients see the state didn't actually move.
                data.output_config_dirty = true;
            }
        }
    }
    unclaimed
}

/// Aggregate wlr-output-management head state across every DRM device.
fn collect_all_head_states(data: &DriftWm) -> HashMap<String, OutputHeadState> {
    let mut head_state = HashMap::new();
    for device in data.udev_devices.values() {
        let dev = device.0.borrow();
        head_state.extend(collect_output_state_from_surfaces(&dev.surfaces, &dev.drm));
    }
    head_state
}

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

        let mut modes: Vec<ModeInfo> =
            match ControlDevice::get_connector(drm, surface.connector, false) {
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

        // If the active mode is a CVT-synthesized one (not in the EDID list),
        // append it so `wlr-randr` can show it as current. Without this the
        // user runs `wlr-randr --custom-mode ...`, sees the display change,
        // and then sees the old mode list with nothing marked current — looks
        // broken.
        let mut current_mode_index = modes.iter().position(|m| {
            m.width == mode.size.w && m.height == mode.size.h && m.refresh == mode.refresh
        });
        if current_mode_index.is_none() {
            modes.push(ModeInfo {
                width: mode.size.w,
                height: mode.size.h,
                refresh: mode.refresh,
                preferred: false,
            });
            current_mode_index = Some(modes.len() - 1);
        }

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
