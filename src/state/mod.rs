mod animation;
mod cursor;
pub mod fit;
mod fullscreen;
mod navigation;
mod render_cache;
pub use cursor::{CursorFrames, CursorState};
pub use render_cache::RenderCache;

use smithay::{
    desktop::{PopupManager, Space, Window},
    input::{
        Seat, SeatState,
        keyboard::{ModifiersState, XkbConfig},
    },
    output::Output,
    reexports::{
        calloop::{LoopHandle, LoopSignal},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point, Rectangle, Size},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        cursor_shape::CursorShapeManagerState,
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::xdg::XdgShellState,
        shm::ShmState,
    },
};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesTexture;
use smithay::utils::Physical;
use smithay::wayland::content_type::ContentTypeState;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::idle_inhibit::IdleInhibitManagerState;
use smithay::wayland::idle_notify::IdleNotifierState;
use smithay::wayland::input_method::InputMethodManagerState;
use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState;
use smithay::wayland::pointer_constraints::PointerConstraintsState;
use smithay::wayland::pointer_gestures::PointerGesturesState;
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::security_context::SecurityContextState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::selection::wlr_data_control::DataControlState;
use smithay::wayland::session_lock::{LockSurface, SessionLockManagerState, SessionLocker};
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;
use smithay::wayland::xdg_activation::XdgActivationState;
use smithay::wayland::xdg_foreign::XdgForeignState;

use smithay::backend::session::libseat::LibSeatSession;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::X11Surface;
use smithay::xwayland::xwm::X11Wm;

use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::drm::control::crtc;

use crate::backend::Backend;
use crate::input::gestures::GestureState;
use driftwm::canvas::MomentumState;
use driftwm::config::Config;
use driftwm::window_ext::WindowExt;

/// A layer surface placed at a fixed canvas position (instead of screen-anchored via LayerMap).
/// Created when a layer surface's namespace matches a window rule with `position`.
pub struct CanvasLayer {
    pub surface: smithay::desktop::LayerSurface,
    /// Rule position (Y-up, window-centered) — converted to canvas coords after first commit.
    pub rule_position: (i32, i32),
    /// Internal canvas position (Y-down, top-left). None until first commit reveals size.
    pub position: Option<Point<i32, Logical>>,
    pub namespace: String,
}

/// Persistent per-output state for screen recording capture, reused across frames
/// so the damage tracker's age increments and smithay only re-renders damaged regions.
pub struct CaptureOutputState {
    pub damage_tracker: OutputDamageTracker,
    /// Reused offscreen texture for SHM captures (avoids allocation per frame).
    pub offscreen_texture: Option<(GlesTexture, Size<i32, Physical>)>,
    pub age: usize,
    /// Reset age when cursor inclusion changes between frames.
    pub last_paint_cursors: bool,
}

/// Buffered middle-click from a 3-finger tap. Held for DOUBLE_TAP_WINDOW_MS
/// to see if a 3-finger swipe follows (→ move window). If the timer fires
/// without a swipe, the click is forwarded to the client (paste).
pub struct PendingMiddleClick {
    pub press_time: u32,
    pub release_time: Option<u32>,
    pub timer_token: RegistrationToken,
}

/// Session lock state machine: Unlocked → Pending → Locked → Unlocked.
pub enum SessionLock {
    Unlocked,
    /// Lock requested; screen goes black until lock surface commits.
    Pending(SessionLocker),
    /// Lock confirmed; rendering only the lock surface.
    Locked,
}

pub use crate::focus::FocusTarget;

/// Log an error result with context, discarding the Ok value.
#[inline]
pub(crate) fn log_err(context: &str, result: Result<impl Sized, impl std::fmt::Display>) {
    if let Err(e) = result {
        tracing::error!("{context}: {e}");
    }
}

/// Spawn a shell command with SIGCHLD reset to default.
/// The compositor sets SIG_IGN on SIGCHLD for zombie reaping, but children
/// inherit this — breaking GLib's waitpid()-based subprocess management
/// (swaync-client hangs because GSpawnSync gets ECHILD).
pub fn spawn_command(cmd: &str) {
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new("sh");
    child.args(["-c", cmd]);
    unsafe {
        child.pre_exec(|| {
            libc::signal(libc::SIGCHLD, libc::SIG_DFL);
            Ok(())
        });
    }
    log_err("spawn command", child.spawn());
}

/// Saved viewport state for HomeToggle return — includes optional fullscreen window.
#[derive(Clone)]
pub struct HomeReturn {
    pub camera: Point<f64, Logical>,
    pub zoom: f64,
    pub fullscreen_window: Option<Window>,
}

/// Saved state for a fullscreen window — restored on exit.
pub struct FullscreenState {
    pub window: Window,
    pub saved_location: Point<i32, Logical>,
    pub saved_camera: Point<f64, Logical>,
    pub saved_zoom: f64,
    pub saved_size: Size<i32, Logical>,
}

pub struct PendingRecenter {
    pub target_center: Point<f64, Logical>,
    pub pre_exit_size: Size<i32, Logical>,
}

/// Per-output viewport state, stored on each `Output` via `UserDataMap`.
/// Wrapped in `Mutex` since `UserDataMap` requires `Sync`.
/// Fields that are !Send (PixelShaderElement) stay on DriftWm.
/// Fields with non-Copy ownership types (fullscreen, lock_surface)
/// stay on DriftWm for Phase 1 — moved here when multi-output needs them.
#[derive(Clone)]
pub struct OutputState {
    pub camera: Point<f64, Logical>,
    pub zoom: f64,
    pub zoom_target: Option<f64>,
    pub zoom_animation_center: Option<Point<f64, Logical>>,
    pub last_rendered_zoom: f64,
    pub overview_return: Option<(Point<f64, Logical>, f64)>,
    pub camera_target: Option<Point<f64, Logical>>,
    pub last_scroll_pan: Option<Instant>,
    pub momentum: MomentumState,
    pub panning: bool,
    pub edge_pan_velocity: Option<Point<f64, Logical>>,
    pub last_rendered_camera: Point<f64, Logical>,
    pub last_frame_instant: Instant,
    /// Physical arrangement position in layout space.
    /// (0,0) for single output; from config for multi-monitor.
    pub layout_position: Point<i32, Logical>,
    /// Saved home position for HomeToggle (per-output).
    pub home_return: Option<HomeReturn>,
}

/// Initialize per-output state on a newly created output.
pub fn init_output_state(
    output: &Output,
    camera: Point<f64, Logical>,
    friction: f64,
    layout_position: Point<i32, Logical>,
) {
    if output.user_data().get::<Mutex<OutputState>>().is_some() {
        tracing::warn!("OutputState already initialized for output, skipping");
        return;
    }
    output.user_data().insert_if_missing_threadsafe(|| {
        Mutex::new(OutputState {
            camera,
            zoom: 1.0,
            zoom_target: None,
            zoom_animation_center: None,
            last_rendered_zoom: f64::NAN,
            overview_return: None,
            camera_target: None,
            last_scroll_pan: None,
            momentum: MomentumState::new(friction),
            panning: false,
            edge_pan_velocity: None,
            last_rendered_camera: Point::from((f64::NAN, f64::NAN)),
            last_frame_instant: Instant::now(),
            layout_position,
            home_return: None,
        })
    });
}

/// Screen-space center of an output's usable area (for per-output animation paths).
pub fn usable_center_for_output(output: &Output) -> Point<f64, Logical> {
    let map = smithay::desktop::layer_map_for_output(output);
    let zone = map.non_exclusive_zone();
    Point::from((
        zone.loc.x as f64 + zone.size.w as f64 / 2.0,
        zone.loc.y as f64 + zone.size.h as f64 / 2.0,
    ))
}

/// Logical output size accounting for scale and transform (90°/270° swap width/height).
pub fn output_logical_size(output: &Output) -> Size<i32, Logical> {
    let scale = output.current_scale().fractional_scale();
    output
        .current_mode()
        .map(|m| {
            output
                .current_transform()
                .transform_size(m.size)
                .to_f64()
                .to_logical(scale)
                .to_i32_ceil()
        })
        .unwrap_or((1, 1).into())
}

/// Get a lock on an output's per-output state.
pub fn output_state(output: &Output) -> MutexGuard<'_, OutputState> {
    output
        .user_data()
        .get::<Mutex<OutputState>>()
        .expect("OutputState not initialized on output")
        .lock()
        .expect("OutputState mutex poisoned")
}

/// Central compositor state.
pub struct DriftWm {
    // -- global: infrastructure --
    pub start_time: Instant,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, DriftWm>,
    pub loop_signal: LoopSignal,

    // -- global: desktop --
    pub space: Space<Window>,
    pub popups: PopupManager,

    // -- global: protocol state --
    pub compositor_state: CompositorState,
    pub drm_syncobj_state: Option<smithay::wayland::drm_syncobj::DrmSyncobjState>,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<DriftWm>,
    pub data_device_state: DataDeviceState,

    // -- global: input --
    pub seat: Seat<DriftWm>,

    // -- global: cursor --
    pub cursor: CursorState,

    // -- global: backend --
    pub backend: Option<Backend>,
    // -- global: SSD decorations --
    pub decorations: HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        crate::decorations::WindowDecoration,
    >,
    pub pending_ssd: HashSet<smithay::reexports::wayland_server::backend::ObjectId>,
    // -- global: render state (shaders, blur, backgrounds, captures) --
    pub render: RenderCache,

    // -- global: protocol state (held for smithay delegate macros) --
    pub dmabuf_state: DmabufState,
    pub dmabuf_global: Option<DmabufGlobal>,
    #[allow(dead_code)]
    pub cursor_shape_state: CursorShapeManagerState,
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    #[allow(dead_code)]
    pub fractional_scale_state: FractionalScaleManagerState,
    pub xdg_activation_state: XdgActivationState,
    pub primary_selection_state: PrimarySelectionState,
    pub data_control_state: DataControlState,
    #[allow(dead_code)]
    pub pointer_constraints_state: PointerConstraintsState,
    #[allow(dead_code)]
    pub relative_pointer_state: RelativePointerManagerState,
    #[allow(dead_code)]
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    #[allow(dead_code)]
    pub virtual_keyboard_state: VirtualKeyboardManagerState,
    #[allow(dead_code)]
    pub security_context_state: SecurityContextState,
    #[allow(dead_code)]
    pub idle_inhibit_state: IdleInhibitManagerState,
    pub idle_notifier_state: IdleNotifierState<DriftWm>,
    #[allow(dead_code)]
    pub presentation_state: PresentationState,
    #[allow(dead_code)]
    pub decoration_state: XdgDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub foreign_toplevel_state: driftwm::protocols::foreign_toplevel::ForeignToplevelManagerState,
    pub screencopy_state: driftwm::protocols::screencopy::ScreencopyManagerState,
    pub output_management_state: driftwm::protocols::output_management::OutputManagementState,
    pub pending_screencopies: Vec<driftwm::protocols::screencopy::Screencopy>,
    #[allow(dead_code)]
    pub image_capture_source_state:
        driftwm::protocols::image_capture_source::ImageCaptureSourceState,
    pub image_copy_capture_state: driftwm::protocols::image_copy_capture::ImageCopyCaptureState,
    pub pending_captures: Vec<driftwm::protocols::image_copy_capture::PendingCapture>,
    pub xdg_foreign_state: XdgForeignState,
    pub session_lock_manager_state: SessionLockManagerState,
    pub session_lock: SessionLock,
    // -- per-output: lock surface (one per output in multi-monitor) --
    pub lock_surfaces: HashMap<Output, LockSurface>,

    // -- global: pointer/layer state --
    pub pointer_over_layer: bool,
    pub canvas_layers: Vec<CanvasLayer>,

    // -- global: config --
    pub config: Config,

    // -- global: window management --
    pub pending_center: HashSet<WlSurface>,
    pub pending_size: HashSet<WlSurface>,
    /// After unfit, re-center the window around `target_center` once its
    /// reported geometry actually changes from `pre_exit_size` — handles
    /// clients (Chromium) whose post-unfit size is smaller than what we
    /// configured. Waiting for the size change avoids firing while the
    /// client is still reporting the fit-era geometry.
    pub pending_recenter:
        HashMap<smithay::reexports::wayland_server::backend::ObjectId, PendingRecenter>,

    // -- global: focus/navigation --
    pub focus_history: Vec<Window>,
    pub cycle_state: Option<usize>,

    // -- global: key repeat --
    pub held_action: Option<(u32, driftwm::config::Action, Instant)>,

    // -- per-output: fullscreen (keyed by output, since FullscreenState has Window) --
    pub fullscreen: HashMap<Output, FullscreenState>,

    // -- global: gesture state --
    pub gesture_state: Option<GestureState>,
    pub pending_middle_click: Option<PendingMiddleClick>,

    // -- global: momentum launch timer --
    pub momentum_timer: Option<RegistrationToken>,

    // -- global: session --
    pub session: Option<LibSeatSession>,
    pub input_devices: Vec<smithay::reexports::input::Device>,

    // -- global: state file persistence --
    pub state_file_cameras: HashMap<String, (Point<f64, Logical>, f64)>,
    pub state_file_last_write: Instant,
    /// Active XKB layout name (e.g. "English (US)"), updated on key events.
    pub active_layout: String,
    pub state_file_layout: String,
    pub state_file_window_count: usize,
    pub state_file_layer_count: usize,

    // -- global: autostart --
    pub autostart: Vec<String>,

    // -- global: udev/DRM --
    pub active_crtcs: HashSet<crtc::Handle>,
    pub redraws_needed: HashSet<crtc::Handle>,
    pub frames_pending: HashSet<crtc::Handle>,
    /// One-shot timers armed when queue_frame returned EmptyFrame so the loop
    /// still wakes at ~refresh rate to advance animations (e.g. xcursor frames).
    pub estimated_vblank_timers: HashMap<crtc::Handle, RegistrationToken>,

    // -- global: config hot-reload --
    pub config_file_mtime: Option<std::time::SystemTime>,

    // -- global: multi-monitor --
    /// Global animation tick timestamp — used for dt computation in tick_all_animations().
    /// Separate from per-output last_frame_instant to avoid double-ticking when multiple
    /// outputs render in one iteration.
    pub last_animation_tick: Instant,
    /// The output the pointer is currently on (for input routing).
    pub focused_output: Option<Output>,
    /// The output a gesture started on (pinned for duration of gesture).
    pub gesture_output: Option<Output>,
    /// Fullscreen window that was exited by a gesture (saved before execute_action sees it).
    pub gesture_exited_fullscreen: Option<Window>,
    /// Output names kept as virtual placeholders when all physical outputs disconnect.
    /// Prevents `active_output().unwrap()` panics by keeping the output in the Space.
    pub disconnected_outputs: HashSet<String>,
    /// Set when output config was applied via wlr-output-management; render loop
    /// should re-collect output state and notify clients.
    pub output_config_dirty: bool,

    // -- global: XWayland --
    pub xwayland_shell_state: XWaylandShellState,
    pub x11_wm: Option<X11Wm>,
    /// Override-redirect X11 windows (menus, tooltips) — rendered manually, not in Space.
    pub x11_override_redirect: Vec<X11Surface>,
    pub x11_display: Option<u32>,
    /// XWayland client handle, stored for reconnect/cleanup.
    pub xwayland_client: Option<smithay::reexports::wayland_server::Client>,
    /// Last X11 surface that received `set_activated(true)`. Used by
    /// `focus_changed` to deactivate the previously-focused X11 window
    /// regardless of which code path issued the focus change.
    pub last_x11_focused: Option<X11Surface>,
    /// Last X11 surface raised in XWayland's internal stack on hover.
    /// XWayland routes pointer events (motion/enter/leave) using its X
    /// server stacking, not the compositor's visual stacking. Without
    /// raising on hover, hover indicators appear on the wrong X11 client
    /// when overlapping windows on the canvas.
    pub last_x11_hover_raised: Option<X11Surface>,
    /// Canvas position corresponding to X11 root (0, 0) for OR-only X11
    /// apps (e.g. jgmenu) where no managed X11 window in `space` provides
    /// an anchor. Pinned to the cursor when the first OR maps and held
    /// stable for the rest of the OR session so submenus keep their
    /// relative offsets to the parent menu. Cleared when the last OR
    /// unmaps.
    pub or_root_anchor: Option<Point<i32, Logical>>,

    // -- global: SSD title bar double-click --
    pub last_titlebar_click: Option<(
        Instant,
        smithay::reexports::wayland_server::backend::ObjectId,
    )>,
}

/// Per-client state stored by wayland-server for each connected client.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// Clients connected via a security-context listener are denied access to
    /// privileged protocols (screencopy, foreign-toplevel, virtual keyboard,
    /// etc.). See SecurityContextHandler.
    pub is_restricted: bool,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Restricted clients (those connecting through a security-context listener)
/// are denied access to privileged protocols. Clients without a ClientState
/// (e.g. XWayland's internal client, whose setup callback doesn't attach
/// one) are treated as unrestricted since they are spawned by the
/// compositor itself.
pub(crate) fn client_is_unrestricted(
    client: &smithay::reexports::wayland_server::Client,
) -> bool {
    client
        .get_data::<ClientState>()
        .is_none_or(|d| !d.is_restricted)
}

impl DriftWm {
    pub fn new(
        dh: DisplayHandle,
        loop_handle: LoopHandle<'static, DriftWm>,
        loop_signal: LoopSignal,
    ) -> Self {
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(
            &dh,
            [
                xdg_toplevel::WmCapabilities::Fullscreen,
                xdg_toplevel::WmCapabilities::Maximize,
            ],
        );
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);

        let cursor_shape_state = CursorShapeManagerState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&dh);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        SinglePixelBufferState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        let data_control_state = DataControlState::new::<Self, _>(
            &dh,
            Some(&primary_selection_state),
            client_is_unrestricted,
        );
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&dh);
        let _pointer_gestures_state = PointerGesturesState::new::<Self>(&dh);
        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(&dh);
        TextInputManagerState::new::<Self>(&dh);
        InputMethodManagerState::new::<Self, _>(&dh, client_is_unrestricted);
        let security_context_state =
            SecurityContextState::new::<Self, _>(&dh, client_is_unrestricted);
        let virtual_keyboard_state =
            VirtualKeyboardManagerState::new::<Self, _>(&dh, client_is_unrestricted);
        let idle_inhibit_state = IdleInhibitManagerState::new::<Self>(&dh);
        let idle_notifier_state = IdleNotifierState::new(&dh, loop_handle.clone());
        let presentation_state = PresentationState::new::<Self>(&dh, 1); // CLOCK_MONOTONIC
        let decoration_state = XdgDecorationState::new::<Self>(&dh);
        let layer_shell_state =
            WlrLayerShellState::new_with_filter::<Self, _>(&dh, client_is_unrestricted);
        let foreign_toplevel_state =
            driftwm::protocols::foreign_toplevel::ForeignToplevelManagerState::new::<Self, _>(
                &dh,
                client_is_unrestricted,
            );
        let screencopy_state =
            driftwm::protocols::screencopy::ScreencopyManagerState::new::<Self, _>(
                &dh,
                client_is_unrestricted,
            );
        let image_capture_source_state =
            driftwm::protocols::image_capture_source::ImageCaptureSourceState::new::<Self, _>(
                &dh,
                client_is_unrestricted,
            );
        let image_copy_capture_state =
            driftwm::protocols::image_copy_capture::ImageCopyCaptureState::new::<Self, _>(
                &dh,
                client_is_unrestricted,
            );
        let output_management_state =
            driftwm::protocols::output_management::OutputManagementState::new::<Self, _>(
                &dh,
                client_is_unrestricted,
            );
        let session_lock_manager_state =
            SessionLockManagerState::new::<Self, _>(&dh, client_is_unrestricted);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        let xdg_foreign_state = XdgForeignState::new::<Self>(&dh);
        ContentTypeState::new::<Self>(&dh);
        {
            use smithay::wayland::shell::xdg::dialog::XdgDialogState;
            XdgDialogState::new::<Self>(&dh);
        }
        {
            use smithay::wayland::xwayland_keyboard_grab::XWaylandKeyboardGrabState;
            XWaylandKeyboardGrabState::new::<Self>(&dh);
        }

        let config = Config::load();

        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "seat-0");
        let kb = &config.keyboard_layout;
        let xkb = XkbConfig {
            layout: &kb.layout,
            variant: &kb.variant,
            options: if kb.options.is_empty() {
                None
            } else {
                Some(kb.options.clone())
            },
            model: &kb.model,
            ..Default::default()
        };
        let keyboard = seat
            .add_keyboard(xkb, config.repeat_delay, config.repeat_rate)
            .expect("Failed to add keyboard");
        keyboard.set_modifier_state(ModifiersState {
            num_lock: config.num_lock,
            caps_lock: config.caps_lock,
            ..Default::default()
        });
        seat.add_pointer();

        let autostart = config.autostart.clone();
        Self {
            start_time: Instant::now(),
            display_handle: dh,
            loop_handle,
            loop_signal,
            space: Space::default(),
            popups: PopupManager::default(),
            compositor_state,
            drm_syncobj_state: None,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            seat,
            cursor: CursorState::new(),
            backend: None,
            decorations: HashMap::new(),
            pending_ssd: HashSet::new(),
            render: RenderCache::new(),
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            cursor_shape_state,
            viewporter_state,
            fractional_scale_state,
            xdg_activation_state,
            primary_selection_state,
            data_control_state,
            pointer_constraints_state,
            relative_pointer_state,
            keyboard_shortcuts_inhibit_state,
            virtual_keyboard_state,
            security_context_state,
            idle_inhibit_state,
            idle_notifier_state,
            presentation_state,
            decoration_state,
            layer_shell_state,
            foreign_toplevel_state,
            screencopy_state,
            output_management_state,
            pending_screencopies: Vec::new(),
            image_capture_source_state,
            image_copy_capture_state,
            pending_captures: Vec::new(),
            xdg_foreign_state,
            session_lock_manager_state,
            session_lock: SessionLock::Unlocked,
            lock_surfaces: HashMap::new(),
            pointer_over_layer: false,
            canvas_layers: Vec::new(),
            config,
            pending_center: HashSet::new(),
            pending_size: HashSet::new(),
            pending_recenter: HashMap::new(),
            focus_history: Vec::new(),
            cycle_state: None,
            held_action: None,
            gesture_state: None,
            pending_middle_click: None,
            momentum_timer: None,
            fullscreen: HashMap::new(),
            session: None,
            input_devices: Vec::new(),
            state_file_cameras: HashMap::new(),
            state_file_last_write: Instant::now(),
            active_layout: String::new(),
            state_file_layout: String::new(),
            state_file_window_count: 0,
            state_file_layer_count: 0,
            autostart,
            active_crtcs: HashSet::new(),
            redraws_needed: HashSet::new(),
            frames_pending: HashSet::new(),
            estimated_vblank_timers: HashMap::new(),
            config_file_mtime: None,
            last_animation_tick: Instant::now(),
            focused_output: None,
            gesture_output: None,
            gesture_exited_fullscreen: None,
            disconnected_outputs: HashSet::new(),
            output_config_dirty: false,
            xwayland_shell_state,
            x11_wm: None,
            x11_override_redirect: Vec::new(),
            x11_display: None,
            xwayland_client: None,
            last_x11_focused: None,
            last_x11_hover_raised: None,
            or_root_anchor: None,
            last_titlebar_click: None,
        }
    }

    /// Push any `below` windows to the bottom of the z-order.
    /// Called after every `raise_element()` to maintain stacking.
    pub fn enforce_below_windows(&mut self) {
        self.render.blur_geometry_generation += 1;
        // Space stores elements in a vec where last = topmost.
        // raise_element pushes to the end (top). So we raise all
        // non-below windows in reverse order to preserve their relative
        // stacking while ensuring they sit above any below windows.
        let non_below: Vec<_> = self
            .space
            .elements()
            .filter(|w| {
                !w.wl_surface()
                    .and_then(|s| driftwm::config::applied_rule(&s))
                    .is_some_and(|r| r.widget)
            })
            .cloned()
            .collect();

        for w in non_below {
            self.space.raise_element(&w, false);
        }

        // Parent-child stacking: raise children after their parents so
        // they always appear on top. Works naturally for nested hierarchies.
        let parented: Vec<Window> = self
            .space
            .elements()
            .filter(|w| w.parent_surface().is_some())
            .cloned()
            .collect();
        for child in parented {
            self.space.raise_element(&child, false);
        }

        for fs in self.fullscreen.values() {
            self.space.raise_element(&fs.window, false);
        }

        self.update_x11_stacking_order();
    }

    /// Find the Window in space whose wl_surface matches the given one.
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(surface))
            .cloned()
    }

    /// Get the innermost modal child of a window (for focus redirect).
    /// Recursively chases modal chains (e.g. file picker → overwrite confirm).
    /// Capped at 10 iterations to guard against circular parents.
    pub fn topmost_modal_child(&self, window: &Window) -> Option<Window> {
        let parent_surface = window.wl_surface()?;
        let child = self
            .space
            .elements()
            .rfind(|w| w.parent_surface().as_ref() == Some(&*parent_surface) && w.is_modal())
            .cloned()?;
        self.topmost_modal_child_inner(&child, 9).or(Some(child))
    }

    fn topmost_modal_child_inner(&self, window: &Window, depth: u8) -> Option<Window> {
        if depth == 0 {
            return None;
        }
        let parent_surface = window.wl_surface()?;
        let child = self
            .space
            .elements()
            .rfind(|w| w.parent_surface().as_ref() == Some(&*parent_surface) && w.is_modal())
            .cloned()?;
        self.topmost_modal_child_inner(&child, depth - 1)
            .or(Some(child))
    }

    /// Raise a window and set keyboard focus, with modal focus redirect.
    /// If the window has a modal child, focus goes to that child instead.
    /// X11 `_NET_WM_STATE_FOCUSED` is synced via `SeatHandler::focus_changed`,
    /// which fires from `keyboard.set_focus` regardless of caller.
    pub fn raise_and_focus(&mut self, window: &Window, serial: smithay::utils::Serial) {
        self.space.raise_element(window, true);
        self.enforce_below_windows();

        let focus_surface = self
            .topmost_modal_child(window)
            .or(Some(window.clone()))
            .and_then(|w| w.wl_surface().map(|s| FocusTarget(s.into_owned())));

        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, focus_surface, serial);
    }

    /// Update `_NET_WM_STATE_FOCUSED` on X11 windows when keyboard focus moves.
    /// Driven from `SeatHandler::focus_changed`, so every focus transition is
    /// covered regardless of which call site invoked `keyboard.set_focus`.
    pub fn sync_x11_activated(&mut self, new_focus: Option<&WlSurface>) {
        let new_x11 = new_focus.and_then(|s| self.find_x11_surface_by_wl(s));

        if let Some(prev) = self.last_x11_focused.take()
            && Some(&prev) != new_x11.as_ref()
            && !prev.is_override_redirect()
        {
            let _ = prev.set_activated(false);
        }

        if let Some(x11) = new_x11
            && !x11.is_override_redirect()
        {
            let _ = x11.set_activated(true);
            self.last_x11_focused = Some(x11);
        }
    }

    /// Find a mapped window wrapping the given X11 surface.
    pub fn find_x11_window(&self, x11: &X11Surface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.x11_surface() == Some(x11))
            .cloned()
    }

    /// Sync an X11 toplevel's X11-root position to its compositor screen
    /// position (`canvas_loc - camera`). Without this, X11 windows larger
    /// than the X11 root (= wl_output bounding box) get pointer events
    /// clamped to root edges, producing dead zones in the surface periphery.
    /// XWayland computes the absolute X11 pointer position as
    /// `drawable_x + event_x` and the X server clamps to root bounds; making
    /// `drawable_x = window_screen_x` cancels out the clamp at zoom = 1.0.
    /// No-op for OR surfaces, unmapped windows, or unchanged positions.
    pub fn sync_x11_position(&self, window: &Window) {
        let Some(x11) = window.x11_surface() else { return };
        if x11.is_override_redirect() {
            return;
        }
        let Some(canvas_loc) = self.space.element_location(window) else { return };
        let Some(output) = self.active_output() else { return };
        let camera = output_state(&output).camera;
        let new_loc: Point<i32, Logical> = (
            canvas_loc.x - camera.x.round() as i32,
            canvas_loc.y - camera.y.round() as i32,
        )
            .into();
        let geo = x11.geometry();
        if geo.loc == new_loc {
            return;
        }
        let _ = x11.configure(Rectangle::new(new_loc, geo.size));
    }

    /// Sync X11 root positions for every X11 toplevel in `space`. Call after
    /// the camera moves so visible X11 windows keep cursor events unclamped.
    pub fn sync_all_x11_positions(&self) {
        let windows: Vec<Window> = self
            .space
            .elements()
            .filter(|w| w.x11_surface().is_some())
            .cloned()
            .collect();
        for w in &windows {
            self.sync_x11_position(w);
        }
    }

    /// Re-anchor an X11 window's X11-root position so the cursor's surface
    /// coord lands near the root center, only when the cursor would
    /// otherwise be clamped (its computed X11 absolute coord is outside
    /// root bounds with a margin). Complements `sync_x11_position` for
    /// zoom levels where cursor canvas reach exceeds the X11 root size.
    /// Called from pointer-focus dispatch with the live cursor canvas pos.
    pub fn nudge_x11_root_for_cursor(
        &self,
        wl_surface: &WlSurface,
        cursor_canvas: Point<f64, Logical>,
    ) {
        let Some(x11) = self.find_x11_surface_by_wl(wl_surface) else { return };
        if x11.is_override_redirect() {
            return;
        }
        let Some(window) = self.find_x11_window(&x11) else { return };
        let Some(canvas_loc) = self.space.element_location(&window) else { return };
        let Some(output) = self.active_output() else { return };
        let root_size = output_logical_size(&output);
        let surface_x = cursor_canvas.x - canvas_loc.x as f64;
        let surface_y = cursor_canvas.y - canvas_loc.y as f64;
        let geo = x11.geometry();
        let cursor_root_x = (geo.loc.x as f64 + surface_x).round() as i32;
        let cursor_root_y = (geo.loc.y as f64 + surface_y).round() as i32;
        // Margin keeps a buffer before clamping kicks in, so we don't
        // re-configure on every pixel of motion near the edges.
        const MARGIN: i32 = 64;
        let in_bounds = cursor_root_x >= MARGIN
            && cursor_root_x < root_size.w - MARGIN
            && cursor_root_y >= MARGIN
            && cursor_root_y < root_size.h - MARGIN;
        if in_bounds {
            return;
        }
        let new_loc: Point<i32, Logical> = (
            root_size.w / 2 - surface_x.round() as i32,
            root_size.h / 2 - surface_y.round() as i32,
        )
            .into();
        if new_loc == geo.loc {
            return;
        }
        let _ = x11.configure(Rectangle::new(new_loc, geo.size));
    }

    /// Raise the X11 window owning `wl_surface` to the top of XWayland's
    /// internal stack — *only* on the X11 side, never touches compositor
    /// visual stacking. Called from pointer-focus dispatch so hover events
    /// route to the visually-topmost X11 client when overlapping windows
    /// share canvas coords. No-op if the surface isn't an X11 window or is
    /// already top, and skipped for override-redirect surfaces.
    pub fn raise_x11_for_hover(&mut self, wl_surface: &WlSurface) {
        let Some(x11) = self.find_x11_surface_by_wl(wl_surface) else { return };
        if x11.is_override_redirect() {
            return;
        }
        if self.last_x11_hover_raised.as_ref() == Some(&x11) {
            return;
        }
        let Some(xwm) = self.x11_wm.as_mut() else { return };
        if let Err(err) = xwm.raise_window(&x11) {
            tracing::warn!(?err, "Failed to raise X11 window for hover");
            return;
        }
        self.last_x11_hover_raised = Some(x11);
    }

    /// Push the compositor's current bottom→top window order into XWayland
    /// so the X server's stacking matches the visual stacking. Required for
    /// correct pointer-event routing between overlapping X11 clients.
    /// Skips the call for ≤1 X11 windows — nothing to reorder, and the
    /// underlying `grab_server` round-trip can delay input events.
    pub fn update_x11_stacking_order(&mut self) {
        let Some(xwm) = self.x11_wm.as_mut() else { return };
        let order: Vec<X11Surface> = self
            .space
            .elements()
            .filter_map(|w| w.x11_surface().cloned())
            .collect();
        if order.len() < 2 {
            return;
        }
        if let Err(err) = xwm.update_stacking_order_upwards(order.iter()) {
            tracing::warn!(?err, "Failed to sync X11 stacking order");
        }
    }

    /// Briefly redirect keyboard focus to `or_wl`, deliver Esc press+release,
    /// then restore the prior focus. Used for X11 popup-menu dismissal where
    /// the popup app (jgmenu, dmenu) has an X11 KeyboardGrab and dismisses
    /// on Esc — but doesn't install a Wayland-side xwayland-keyboard-grab,
    /// so Esc only reaches it when its wl_surface holds keyboard focus.
    pub fn synth_esc_to_or(&mut self, or_wl: WlSurface) {
        use smithay::backend::input::KeyState;
        use smithay::input::keyboard::{FilterResult, Keycode};
        let keyboard = self.seat.get_keyboard().unwrap();
        let prev = keyboard.current_focus();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, Some(FocusTarget(or_wl)), serial);

        let time = self.start_time.elapsed().as_millis() as u32;
        let esc = Keycode::new(9); // evdev KEY_ESC (1) + 8
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.input::<(), _>(self, esc, KeyState::Pressed,
            smithay::utils::SERIAL_COUNTER.next_serial(), time,
            |_, _, _| FilterResult::Forward);
        keyboard.input::<(), _>(self, esc, KeyState::Released,
            smithay::utils::SERIAL_COUNTER.next_serial(), time,
            |_, _, _| FilterResult::Forward);

        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, prev, smithay::utils::SERIAL_COUNTER.next_serial());
    }

    /// Find the X11Surface whose underlying wl_surface matches the given one.
    /// Searches both managed windows (in `space`) and override-redirect popups
    /// — the latter matters for the xwayland-keyboard-grab protocol, which
    /// hands us an OR's wl_surface when the X11 client calls XGrabKeyboard.
    pub fn find_x11_surface_by_wl(&self, wl: &WlSurface) -> Option<X11Surface> {
        self.space
            .elements()
            .filter_map(|w| w.x11_surface().cloned())
            .find(|x11| x11.wl_surface().as_ref() == Some(wl))
            .or_else(|| {
                self.x11_override_redirect
                    .iter()
                    .find(|x11| x11.wl_surface().as_ref() == Some(wl))
                    .cloned()
            })
    }

    /// Compute the canvas position of an override-redirect X11 surface.
    /// OR windows use absolute X11 root coords; we map them relative to
    /// their parent's canvas position, or center them if no parent exists.
    pub fn or_canvas_position(&self, or_surface: &X11Surface) -> Point<i32, Logical> {
        let or_geo = or_surface.geometry();

        if let Some(parent_id) = or_surface.is_transient_for() {
            // Search managed windows in Space for parent
            let parent_in_space = self
                .space
                .elements()
                .find(|w| w.x11_surface().is_some_and(|x| x.window_id() == parent_id));
            if let Some(parent_win) = parent_in_space {
                let parent_canvas = self.space.element_location(parent_win).unwrap_or_default();
                let parent_x11_loc = parent_win.x11_surface().unwrap().geometry().loc;
                return parent_canvas + (or_geo.loc - parent_x11_loc);
            }

            // Search other OR windows (nested menus) with depth limit
            fn find_or_parent(
                or_list: &[X11Surface],
                space: &smithay::desktop::Space<smithay::desktop::Window>,
                target_id: u32,
                depth: u32,
            ) -> Option<Point<i32, Logical>> {
                if depth == 0 {
                    return None;
                }
                let parent_or = or_list.iter().find(|w| w.window_id() == target_id)?;
                let parent_geo = parent_or.geometry();
                if let Some(grandparent_id) = parent_or.is_transient_for() {
                    // Check Space first
                    let gp_in_space = space.elements().find(|w| {
                        w.x11_surface()
                            .is_some_and(|x| x.window_id() == grandparent_id)
                    });
                    if let Some(gp_win) = gp_in_space {
                        let gp_canvas = space.element_location(gp_win).unwrap_or_default();
                        let gp_x11_loc = gp_win.x11_surface().unwrap().geometry().loc;
                        return Some(gp_canvas + (parent_geo.loc - gp_x11_loc));
                    }
                    // Recurse into OR list
                    let gp_canvas = find_or_parent(or_list, space, grandparent_id, depth - 1)?;
                    return Some(
                        gp_canvas
                            + (parent_geo.loc
                                - or_list
                                    .iter()
                                    .find(|w| w.window_id() == grandparent_id)
                                    .map(|w| w.geometry().loc)
                                    .unwrap_or_default()),
                    );
                }
                None
            }

            if let Some(parent_canvas) =
                find_or_parent(&self.x11_override_redirect, &self.space, parent_id, 10)
            {
                let parent_or = self
                    .x11_override_redirect
                    .iter()
                    .find(|w| w.window_id() == parent_id);
                let parent_x11_loc = parent_or.map(|w| w.geometry().loc).unwrap_or_default();
                return parent_canvas + (or_geo.loc - parent_x11_loc);
            }
        }

        // No transient_for: decide between cursor-pinned and managed-window
        // anchoring by whether the OR's WM_CLASS matches any managed X11
        // window. No match → OR-only app (jgmenu / dmenu-style), use the
        // cursor-pinned `or_root_anchor`. Match → managed-app OR (Steam
        // hover menus etc.), use `pick_anchor` with that window as anchor.
        // All X11 clients share one wl_client so class is the best signal.
        let or_class = or_surface.class();
        let has_class_match = !or_class.is_empty()
            && self.space.elements().any(|w| {
                w.x11_surface().is_some_and(|x| x.class() == or_class)
            });

        if !has_class_match
            && let Some(anchor) = self.or_root_anchor
        {
            return anchor + or_geo.loc;
        }

        // Managed-app unparented OR, or fallback when no cursor anchor
        // is pinned yet. Prefer the X11 window raised on hover (most
        // likely the creator), then last-focused, then topmost.
        let pick_anchor = |target: Option<&X11Surface>| {
            self.space.elements().rev().find_map(|w| {
                let x11 = w.x11_surface()?;
                if let Some(t) = target
                    && x11 != t
                {
                    return None;
                }
                let canvas_loc = self.space.element_location(w)?;
                Some((canvas_loc, x11.geometry().loc))
            })
        };
        let anchor = pick_anchor(self.last_x11_hover_raised.as_ref())
            .or_else(|| pick_anchor(self.last_x11_focused.as_ref()))
            .or_else(|| pick_anchor(None));
        if let Some((anchor_canvas, anchor_x11)) = anchor {
            return anchor_canvas + (or_geo.loc - anchor_x11);
        }

        // Last resort: cursor-pinned anchor, regardless of class-match.
        if let Some(anchor) = self.or_root_anchor {
            return anchor + or_geo.loc;
        }

        // No X11 windows at all: center in viewport
        self.active_output()
            .and_then(|o| self.space.output_geometry(&o))
            .map(|viewport| {
                let cam = self.camera();
                let z = self.zoom();
                Point::from((
                    (cam.x + viewport.size.w as f64 / (2.0 * z)) as i32 - or_geo.size.w / 2,
                    (cam.y + viewport.size.h as f64 / (2.0 * z)) as i32 - or_geo.size.h / 2,
                ))
            })
            .unwrap_or_default()
    }

    /// Mark all active outputs as needing a redraw.
    pub fn mark_all_dirty(&mut self) {
        self.redraws_needed.extend(self.active_crtcs.iter());
    }

    pub fn remove_capture_state(&mut self, output_name: &str) {
        self.render.remove_capture_state(output_name);
    }

    pub fn cursor_is_animated(&self) -> bool {
        self.cursor.is_animated()
    }

    /// True if a specific output has per-output animations in progress.
    pub fn output_has_active_animations(&self, output: &Output) -> bool {
        let os = output_state(output);
        os.camera_target.is_some()
            || os.zoom_target.is_some()
            || os.edge_pan_velocity.is_some()
            || os.momentum.velocity.x != 0.0
            || os.momentum.velocity.y != 0.0
    }

    /// True if any animation is still in progress and needs continued rendering.
    pub fn has_active_animations(&self) -> bool {
        self.space
            .outputs()
            .any(|o| self.output_has_active_animations(o))
            || self.held_action.is_some()
            || self.cursor.exec_cursor_show_at.is_some()
            || self.cursor.exec_cursor_deadline.is_some()
            || self.cursor.is_animated()
    }

    /// Forward a buffered middle-click press+release to the client.
    pub fn flush_middle_click(&mut self, press_time: u32, release_time: Option<u32>) {
        let pointer = self.seat.get_pointer().unwrap();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        pointer.button(
            self,
            &smithay::input::pointer::ButtonEvent {
                button: driftwm::config::BTN_MIDDLE,
                state: smithay::backend::input::ButtonState::Pressed,
                serial,
                time: press_time,
            },
        );
        pointer.frame(self);
        if let Some(rt) = release_time {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            pointer.button(
                self,
                &smithay::input::pointer::ButtonEvent {
                    button: driftwm::config::BTN_MIDDLE,
                    state: smithay::backend::input::ButtonState::Released,
                    serial,
                    time: rt,
                },
            );
            pointer.frame(self);
        }
    }

    /// Flush the pending middle-click (called by calloop timer when no swipe followed).
    pub fn flush_pending_middle_click(&mut self) {
        let Some(pending) = self.pending_middle_click.take() else {
            return;
        };
        self.flush_middle_click(pending.press_time, pending.release_time);
    }

    /// The output the pointer is currently on.
    /// Returns `focused_output` with fallback to first output.
    pub fn active_output(&self) -> Option<Output> {
        self.focused_output
            .clone()
            .or_else(|| self.space.outputs().next().cloned())
    }

    /// Get the fullscreen state for the active output (if any).
    pub fn active_fullscreen(&self) -> Option<&FullscreenState> {
        self.active_output().and_then(|o| self.fullscreen.get(&o))
    }

    /// Check if the active output is in fullscreen mode.
    pub fn is_fullscreen(&self) -> bool {
        self.active_output()
            .is_some_and(|o| self.fullscreen.contains_key(&o))
    }

    /// Check if a specific output is in fullscreen mode.
    pub fn is_output_fullscreen(&self, output: &Output) -> bool {
        self.fullscreen.contains_key(output)
    }

    /// Find the output whose viewport contains (or is nearest to) a window's center.
    /// Falls back to active output if the window isn't visible on any output.
    pub fn output_for_window(&self, window: &smithay::desktop::Window) -> Option<Output> {
        let loc = self.space.element_location(window)?;
        let geo = window.geometry();
        let center: Point<f64, Logical> = Point::from((
            loc.x as f64 + geo.size.w as f64 / 2.0,
            loc.y as f64 + geo.size.h as f64 / 2.0,
        ));
        // Find which output's visible canvas rect contains the window center.
        let found = self
            .space
            .outputs()
            .find(|output| {
                let os = output_state(output);
                let size = output_logical_size(output);
                let visible =
                    driftwm::canvas::visible_canvas_rect(os.camera.to_i32_round(), size, os.zoom);
                drop(os);
                visible.contains(Point::from((center.x as i32, center.y as i32)))
            })
            .cloned();
        found.or_else(|| self.active_output())
    }

    /// Find the nearest output in the given direction from `from`.
    pub fn output_in_direction(
        &self,
        from: &Output,
        dir: &driftwm::config::Direction,
    ) -> Option<Output> {
        let from_center: Point<f64, Logical> = {
            let os = output_state(from);
            let size = output_logical_size(from);
            Point::from((
                os.layout_position.x as f64 + size.w as f64 / 2.0,
                os.layout_position.y as f64 + size.h as f64 / 2.0,
            ))
        };
        let (dx, dy) = dir.to_unit_vec();

        self.space
            .outputs()
            .filter(|o| *o != from)
            .filter_map(|o| {
                let os = output_state(o);
                let size = output_logical_size(o);
                let center: Point<f64, Logical> = Point::from((
                    os.layout_position.x as f64 + size.w as f64 / 2.0,
                    os.layout_position.y as f64 + size.h as f64 / 2.0,
                ));
                drop(os);
                let to_x = center.x - from_center.x;
                let to_y = center.y - from_center.y;
                let dist = (to_x * to_x + to_y * to_y).sqrt();
                if dist < 1.0 {
                    return None;
                }
                // Check alignment with direction (dot product > 0.5 = within ~60°)
                let dot = (to_x * dx + to_y * dy) / dist;
                if dot > 0.5 {
                    Some((o.clone(), dist))
                } else {
                    None
                }
            })
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .map(|(o, _)| o)
    }

    /// Find which output's layout rectangle contains `pos` in layout space.
    /// Uses `layout_position` + output mode size (NOT `space.output_geometry()`).
    pub fn output_at_layout_pos(&self, pos: Point<f64, Logical>) -> Option<Output> {
        self.space
            .outputs()
            .find(|output| {
                let os = output_state(output);
                let lp = os.layout_position;
                drop(os);
                let size = output_logical_size(output);
                pos.x >= lp.x as f64
                    && pos.x < (lp.x + size.w) as f64
                    && pos.y >= lp.y as f64
                    && pos.y < (lp.y + size.h) as f64
            })
            .cloned()
    }

    /// Convert canvas position to layout position via an output's camera/zoom.
    /// layout_pos = (canvas - camera) * zoom + layout_position
    #[cfg(test)]
    pub fn canvas_to_layout_pos(
        canvas_pos: Point<f64, Logical>,
        os: &OutputState,
    ) -> Point<f64, Logical> {
        let screen = driftwm::canvas::canvas_to_screen(
            driftwm::canvas::CanvasPos(canvas_pos),
            os.camera,
            os.zoom,
        )
        .0;
        Point::from((
            screen.x + os.layout_position.x as f64,
            screen.y + os.layout_position.y as f64,
        ))
    }

    /// Convert layout position to canvas position via an output's camera/zoom.
    /// canvas = (layout_pos - layout_position) / zoom + camera
    #[cfg(test)]
    pub fn layout_to_canvas_pos(
        layout_pos: Point<f64, Logical>,
        os: &OutputState,
    ) -> Point<f64, Logical> {
        let screen = Point::from((
            layout_pos.x - os.layout_position.x as f64,
            layout_pos.y - os.layout_position.y as f64,
        ));
        driftwm::canvas::screen_to_canvas(driftwm::canvas::ScreenPos(screen), os.camera, os.zoom).0
    }

    /// Batch-access per-output state under a single mutex lock.
    pub fn with_output_state<R>(&mut self, f: impl FnOnce(&mut OutputState) -> R) -> R {
        let output = self.active_output().unwrap();
        let mut guard = output_state(&output);
        f(&mut guard)
    }

    // -- Per-output field accessors (delegate to active output's OutputState) --

    pub fn camera(&self) -> Point<f64, Logical> {
        output_state(&self.active_output().unwrap()).camera
    }
    pub fn set_camera(&mut self, val: Point<f64, Logical>) {
        output_state(&self.active_output().unwrap()).camera = val;
    }
    pub fn zoom(&self) -> f64 {
        output_state(&self.active_output().unwrap()).zoom
    }
    pub fn set_zoom(&mut self, val: f64) {
        output_state(&self.active_output().unwrap()).zoom = val;
    }
    pub fn zoom_target(&self) -> Option<f64> {
        output_state(&self.active_output().unwrap()).zoom_target
    }
    pub fn set_zoom_target(&mut self, val: Option<f64>) {
        output_state(&self.active_output().unwrap()).zoom_target = val;
    }
    pub fn zoom_animation_center(&self) -> Option<Point<f64, Logical>> {
        output_state(&self.active_output().unwrap()).zoom_animation_center
    }
    pub fn set_zoom_animation_center(&mut self, val: Option<Point<f64, Logical>>) {
        output_state(&self.active_output().unwrap()).zoom_animation_center = val;
    }
    pub fn overview_return(&self) -> Option<(Point<f64, Logical>, f64)> {
        output_state(&self.active_output().unwrap()).overview_return
    }
    pub fn set_overview_return(&mut self, val: Option<(Point<f64, Logical>, f64)>) {
        output_state(&self.active_output().unwrap()).overview_return = val;
    }
    pub fn camera_target(&self) -> Option<Point<f64, Logical>> {
        output_state(&self.active_output().unwrap()).camera_target
    }
    pub fn set_camera_target(&mut self, val: Option<Point<f64, Logical>>) {
        output_state(&self.active_output().unwrap()).camera_target = val;
    }
    pub fn last_scroll_pan(&self) -> Option<Instant> {
        output_state(&self.active_output().unwrap()).last_scroll_pan
    }
    pub fn set_last_scroll_pan(&mut self, val: Option<Instant>) {
        output_state(&self.active_output().unwrap()).last_scroll_pan = val;
    }
    pub fn panning(&self) -> bool {
        output_state(&self.active_output().unwrap()).panning
    }
    pub fn set_panning(&mut self, val: bool) {
        output_state(&self.active_output().unwrap()).panning = val;
    }
    pub fn edge_pan_velocity(&self) -> Option<Point<f64, Logical>> {
        output_state(&self.active_output().unwrap()).edge_pan_velocity
    }
    pub fn last_frame_instant(&self) -> Instant {
        output_state(&self.active_output().unwrap()).last_frame_instant
    }
    pub fn set_last_frame_instant(&mut self, val: Instant) {
        output_state(&self.active_output().unwrap()).last_frame_instant = val;
    }

    /// Sync each output's position to its camera, so render_output
    /// automatically applies the canvas→screen transform.
    pub fn update_output_from_camera(&mut self) {
        let mut changed = false;
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            let cam = output_state(&output).camera.to_i32_round();
            if self.space.output_geometry(&output).map(|g| g.loc) != Some(cam) {
                changed = true;
            }
            self.space.map_output(&output, cam);
        }
        if changed {
            self.render.blur_camera_generation += 1;
            // Re-anchor X11 root positions so cursor events stay unclamped
            // for windows larger than the X11 root after the camera moves.
            self.sync_all_x11_positions();
        }
    }

    /// Logical viewport size of the active (pointer-focused) output.
    pub fn get_viewport_size(&self) -> Size<i32, Logical> {
        self.active_output()
            .map(|o| output_logical_size(&o))
            .unwrap_or((1, 1).into())
    }

    /// Viewport area minus layer-shell exclusive zones (panels, bars).
    pub fn get_usable_area(&self) -> Rectangle<i32, Logical> {
        self.active_output()
            .map(|o| {
                let map = smithay::desktop::layer_map_for_output(&o);
                map.non_exclusive_zone()
            })
            .unwrap_or_else(|| Rectangle::new((0, 0).into(), (1, 1).into()))
    }

    /// Screen-space center of the usable area (accounts for panel exclusive zones).
    /// Without panels, equals (viewport.w/2, viewport.h/2).
    pub fn usable_center_screen(&self) -> Point<f64, Logical> {
        let usable = self.get_usable_area();
        Point::from((
            usable.loc.x as f64 + usable.size.w as f64 / 2.0,
            usable.loc.y as f64 + usable.size.h as f64 / 2.0,
        ))
    }

    /// Center of the active output's viewport expressed in canvas coordinates.
    pub fn viewport_center_canvas(&self) -> Point<f64, Logical> {
        let vc = self.usable_center_screen();
        let camera = self.camera();
        let zoom = self.zoom();
        Point::from((camera.x + vc.x / zoom, camera.y + vc.y / zoom))
    }

    /// Currently keyboard-focused window, if any.
    /// Does not filter widgets — pair with `.filter(|w| !w.is_widget())` when needed.
    pub fn focused_window(&self) -> Option<Window> {
        let keyboard = self.seat.get_keyboard()?;
        let focus = keyboard.current_focus()?;
        self.space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(&focus.0))
            .cloned()
    }

    /// SSD title bar height for a window (0 for CSD/borderless).
    pub fn window_ssd_bar(&self, window: &Window) -> i32 {
        window
            .wl_surface()
            .filter(|s| self.decorations.contains_key(&s.id()))
            .map_or(0, |_| driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT)
    }

    /// Visual center of a window, accounting for SSD title bar above content.
    pub fn window_visual_center(&self, window: &Window) -> Option<Point<f64, Logical>> {
        let loc = self.space.element_location(window)?;
        let size = window.geometry().size;
        let bar = self.window_ssd_bar(window) as f64;
        Some(Point::from((
            loc.x as f64 + size.w as f64 / 2.0,
            loc.y as f64 - bar + (size.h as f64 + bar) / 2.0,
        )))
    }

    /// Offset a spawn position so it doesn't overlap an existing window.
    /// Walks in diagonal steps (title bar height) until no window is within a few pixels.
    pub fn cascade_position(&self, mut pos: (i32, i32), skip: &Window) -> (i32, i32) {
        let step = driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT;
        loop {
            let dominated = self.space.elements().any(|w| {
                w != skip
                    && self
                        .space
                        .element_location(w)
                        .is_some_and(|loc| (loc.x - pos.0).abs() <= 2 && (loc.y - pos.1).abs() <= 2)
            });
            if !dominated {
                break pos;
            }
            pos.0 += step;
            pos.1 += step;
        }
    }

    /// Write viewport center + zoom to `$XDG_RUNTIME_DIR/driftwm/state` if changed.
    /// Atomic: writes to .tmp then renames.
    pub fn write_state_file_if_dirty(&mut self) {
        // Check if any output's camera/zoom changed (not just active output)
        let layout_dirty = self.state_file_layout != self.active_layout;
        let mut any_output_dirty = false;
        for output in self.space.outputs() {
            let os = output_state(output);
            let name = output.name();
            let (cam, z) = (os.camera, os.zoom);
            drop(os);
            if let Some(&(cached_cam, cached_z)) = self.state_file_cameras.get(&name) {
                if (cam.x - cached_cam.x).abs() >= 0.5
                    || (cam.y - cached_cam.y).abs() >= 0.5
                    || (z - cached_z).abs() >= 0.001
                {
                    any_output_dirty = true;
                    break;
                }
            } else {
                any_output_dirty = true;
                break;
            }
        }
        let window_count = self.space.elements().count();
        let layer_count: usize = self
            .space
            .outputs()
            .map(|o| smithay::desktop::layer_map_for_output(o).layers().count())
            .sum();
        let windows_dirty = window_count != self.state_file_window_count
            || layer_count != self.state_file_layer_count;

        if !layout_dirty && !any_output_dirty && !windows_dirty {
            return;
        }
        // Throttle writes to ~10/sec max (100ms between writes)
        if self.state_file_last_write.elapsed() < std::time::Duration::from_millis(100) {
            return;
        }
        // Update cached state
        self.state_file_window_count = window_count;
        self.state_file_layer_count = layer_count;
        for output in self.space.outputs() {
            let os = output_state(output);
            self.state_file_cameras
                .insert(output.name(), (os.camera, os.zoom));
        }
        self.state_file_layout = self.active_layout.clone();
        self.state_file_last_write = Instant::now();

        // Convert active output's camera to viewport center in canvas coords.
        // Negate Y so positive = above origin (user-facing Y-up convention).
        let cam = self.camera();
        let z = self.zoom();
        let vp = self.get_viewport_size();
        let cx = cam.x + vp.w as f64 / (2.0 * z);
        let cy = -(cam.y + vp.h as f64 / (2.0 * z));

        let Some(dir) = state_file_dir() else { return };
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("state");
        let tmp = dir.join("state.tmp");
        let mut content = format!(
            "x={cx:.0}\ny={cy:.0}\nzoom={z:.3}\nlayout={}\n",
            self.active_layout
        );

        {
            let home_return = output_state(&self.active_output().unwrap())
                .home_return
                .clone();
            if let Some(ref ret) = home_return {
                let sz = ret.zoom;
                let sx = ret.camera.x + vp.w as f64 / (2.0 * sz);
                let sy = -(ret.camera.y + vp.h as f64 / (2.0 * sz));
                content += &format!("saved_x={sx:.0}\nsaved_y={sy:.0}\nsaved_zoom={sz:.3}\n");
            }
        }

        // Window list: app_id of each toplevel (focused window first)
        // Falls back to X11 class for XWayland windows.
        let focused_surface = self.seat.get_keyboard().and_then(|kb| kb.current_focus());
        let mut app_ids: Vec<String> = Vec::new();
        for window in self.space.elements() {
            let Some(surface) = window.wl_surface() else {
                continue;
            };
            let mut app_id = smithay::wayland::compositor::with_states(&surface, |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .and_then(|d| d.lock().ok())
                    .and_then(|guard| guard.app_id.clone())
            })
            .unwrap_or_default();
            if app_id.is_empty()
                && let Some(x11) = window.x11_surface()
            {
                app_id = x11.class();
            }
            if !app_id.is_empty() {
                let is_focused = focused_surface.as_ref().is_some_and(|f| f.0 == *surface);
                if is_focused {
                    app_ids.insert(0, app_id);
                } else {
                    app_ids.push(app_id);
                }
            }
        }
        if !app_ids.is_empty() {
            content += &format!("windows={}\n", app_ids.join(","));
        }

        // Layer shell surfaces (waybar, notifications, etc.)
        let mut layers: Vec<String> = Vec::new();
        for output in self.space.outputs() {
            let layer_map = smithay::desktop::layer_map_for_output(output);
            for layer in layer_map.layers() {
                let ns = layer.namespace().to_string();
                if !ns.is_empty() && !layers.contains(&ns) {
                    layers.push(ns);
                }
            }
        }
        if !layers.is_empty() {
            content += &format!("layers={}\n", layers.join(","));
        }

        // Per-output camera/zoom state
        for output in self.space.outputs() {
            let os = output_state(output);
            let name = output.name();
            content += &format!(
                "outputs.{name}.camera_x={:.1}\noutputs.{name}.camera_y={:.1}\noutputs.{name}.zoom={:.3}\n",
                os.camera.x, os.camera.y, os.zoom
            );
        }

        if std::fs::write(&tmp, content).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Hot-reload config from disk. On parse failure, logs an error and keeps the old config.
    pub fn reload_config(&mut self) {
        let config_path = driftwm::config::config_path();
        let contents = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    "Config reload: failed to read {}: {e}",
                    config_path.display()
                );
                return;
            }
        };
        let mut new_config = match driftwm::config::Config::from_toml(&contents) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Config reload: parse error: {e}");
                return;
            }
        };

        // Hot-reload keyboard layout
        if new_config.keyboard_layout != self.config.keyboard_layout {
            let kb = &new_config.keyboard_layout;
            let xkb = XkbConfig {
                layout: &kb.layout,
                variant: &kb.variant,
                options: if kb.options.is_empty() {
                    None
                } else {
                    Some(kb.options.clone())
                },
                model: &kb.model,
                ..Default::default()
            };
            let keyboard = self.seat.get_keyboard().unwrap();
            let num_lock = keyboard.modifier_state().num_lock;
            if let Err(err) = keyboard.set_xkb_config(self, xkb) {
                tracing::warn!("Config reload: error updating keyboard layout: {err:?}");
                new_config.keyboard_layout = self.config.keyboard_layout.clone();
            } else {
                tracing::info!("Config reload: keyboard layout updated");
                let mut mods = keyboard.modifier_state();
                if mods.num_lock != num_lock {
                    mods.num_lock = num_lock;
                    keyboard.set_modifier_state(mods);
                }
            }
        }
        if new_config.autostart != self.config.autostart {
            tracing::info!("Config reload: autostart changes only apply at startup");
        }

        // Keyboard repeat rate/delay
        if new_config.repeat_rate != self.config.repeat_rate
            || new_config.repeat_delay != self.config.repeat_delay
        {
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.change_repeat_info(new_config.repeat_rate, new_config.repeat_delay);
        }

        // Momentum friction — apply to all outputs
        if new_config.friction != self.config.friction {
            for output in self.space.outputs() {
                output_state(output).momentum.friction = new_config.friction;
            }
        }

        // Background shader/tile — always clear cached state so that editing
        // the shader file on disk takes effect after `touch`ing the config.
        self.render.background_shader = None;
        self.render.cached_bg_elements.clear();
        self.render.tile_shader = None;
        self.render.cached_tile_bg.clear();

        // Cursor theme/size — validate theme before committing
        let theme_changed = new_config.cursor_theme != self.config.cursor_theme;
        let size_changed = new_config.cursor_size != self.config.cursor_size;
        if theme_changed || size_changed {
            let theme_ok = if theme_changed {
                if let Some(ref theme_name) = new_config.cursor_theme {
                    let theme = xcursor::CursorTheme::load(theme_name);
                    if theme.load_icon("default").is_some() {
                        unsafe { std::env::set_var("XCURSOR_THEME", theme_name) };
                        true
                    } else {
                        tracing::warn!(
                            "Cursor theme '{theme_name}' not found, keeping current theme"
                        );
                        new_config.cursor_theme = self.config.cursor_theme.clone();
                        false
                    }
                } else {
                    unsafe { std::env::remove_var("XCURSOR_THEME") };
                    true
                }
            } else {
                false
            };

            if size_changed {
                if let Some(size) = new_config.cursor_size {
                    unsafe { std::env::set_var("XCURSOR_SIZE", size.to_string()) };
                } else {
                    unsafe { std::env::remove_var("XCURSOR_SIZE") };
                }
            }

            if theme_ok || size_changed {
                self.cursor.cursor_buffers.clear();
            }
        }

        // Trackpad settings — reconfigure all connected devices
        if new_config.trackpad != self.config.trackpad {
            self.config.trackpad = new_config.trackpad.clone();
            let devices = self.input_devices.clone();
            for mut device in devices {
                self.configure_libinput_device(&mut device);
            }
            tracing::info!("Config reload: trackpad settings applied to all devices");
        }

        // Env vars — diff old vs new, apply changes
        for (key, value) in &new_config.env {
            if self.config.env.get(key) != Some(value) {
                tracing::info!("Config reload: env {key}={value}");
                unsafe { std::env::set_var(key, value) };
            }
        }
        for key in self.config.env.keys() {
            if !new_config.env.contains_key(key) {
                tracing::info!("Config reload: env unset {key}");
                unsafe { std::env::remove_var(key) };
            }
        }

        self.config = new_config;
        self.mark_all_dirty();
        tracing::info!("Config reloaded");
    }

    pub fn load_xcursor(&mut self, name: &str) -> Option<&CursorFrames> {
        self.cursor.load_xcursor(name)
    }

    /// Build snap target rectangles for all windows except `primary` and
    /// anything in `cluster_excludes` (used during a multi-window cluster
    /// drag to stop members from snapping against each other). Widgets are
    /// always skipped.
    pub fn snap_targets(
        &self,
        primary: &WlSurface,
        cluster_excludes: &HashSet<WlSurface>,
    ) -> (Vec<driftwm::snap::SnapRect>, i32) {
        snap_targets_impl(&self.space, &self.decorations, primary, cluster_excludes)
    }

    /// Every non-widget window in the space with its `SnapRect`. Used to
    /// feed `cluster::cluster_of` at drag start. The surface is discarded —
    /// `Window` identity is what the BFS needs.
    pub fn all_windows_with_snap_rects(&self) -> Vec<(Window, driftwm::snap::SnapRect)> {
        self.space
            .elements()
            .filter_map(|w| {
                window_snap_rect(&self.space, &self.decorations, w)
                    .map(|(_, rect)| (w.clone(), rect))
            })
            .collect()
    }

    /// Snapshot the focused window's cluster for a move drag.
    ///
    /// Returns both the member offsets (from the primary's canvas position)
    /// AND the exclude set of member surfaces. Both are frozen at drag start:
    /// cluster membership doesn't change mid-drag and offsets are invariant
    /// over motion, snap, and cross-output teleport.
    //
    // smithay's `Window` wraps `Arc<WindowInner>` which contains an
    // `AtomicF64` (scale), so clippy flags it as an interior-mutable hash
    // key. Here that's a false positive: `Window: Hash + Eq` are both
    // implemented on the `Arc` pointer identity, which is stable under any
    // interior mutation of `WindowInner`. Safe to use as a HashSet member.
    #[allow(clippy::mutable_key_type)]
    pub fn cluster_snapshot_for_drag(
        &self,
        window: &Window,
        primary_pos: Point<i32, Logical>,
    ) -> ClusterDragSnapshot {
        let rects = self.all_windows_with_snap_rects();
        let component = crate::cluster::cluster_of(window, &rects, self.config.snap_gap);

        let mut members = Vec::new();
        let mut surfaces = HashSet::new();
        for m in component {
            if &m == window {
                continue;
            }
            let Some(pos) = self.space.element_location(&m) else {
                continue;
            };
            let offset = pos - primary_pos;
            if let Some(s) = m.wl_surface() {
                surfaces.insert(s.into_owned());
            }
            members.push((m, offset));
        }
        (members, surfaces)
    }

    /// Snapshot the focused window's cluster for a resize drag.
    ///
    /// Three-step classification, per active resize edge:
    ///
    /// 1. **Seed** with the primary's direct neighbors on that edge
    ///    (first hop only — `adjacent_side(primary, m) == Some(side)`).
    /// 2. **BFS expand** from the seed through the full cluster graph
    ///    (any adjacency direction), adding any member whose rect is
    ///    *downstream* of the primary's edge. For a right drag, downstream
    ///    means `m.x_low >= primary.x_high` — i.e. the member sits on or
    ///    past the primary's right edge and can be pushed by it.
    /// 3. Members not in any static shift set are still stored (with
    ///    `axis_x = None`, `axis_y = None`) so that the motion-time
    ///    cascade can detect and resolve overlap between shifted and
    ///    non-shifted members.
    ///
    /// Steps 1+2 are static (computed once at grab start). Step 3 runs per
    /// motion tick inside `ResizeSurfaceGrab::motion`.
    //
    // Same Arc-identity rationale as `cluster_snapshot_for_drag`: smithay's
    // `Window` is hashed by Arc pointer and is stable under interior mutation.
    #[allow(clippy::mutable_key_type)]
    pub fn cluster_snapshot_for_resize(
        &self,
        window: &Window,
        edges: xdg_toplevel::ResizeEdge,
    ) -> ClusterResizeSnapshot {
        use crate::cluster::{Side, adjacent_side, cluster_of};
        use crate::grabs::{has_bottom, has_left, has_right, has_top};
        use driftwm::snap::SnapRect;

        let rects = self.all_windows_with_snap_rects();
        let gap = self.config.snap_gap;
        let full = cluster_of(window, &rects, gap);

        // Primary's SnapRect for the downstream filter. Bail with an empty
        // snapshot if the primary somehow has no rect (widget, unmapped).
        let Some(primary_rect) = rects
            .iter()
            .find(|(w, _)| w == window)
            .map(|(_, r)| *r)
        else {
            return ClusterResizeSnapshot::empty();
        };

        // Rect lookup for BFS inner loop. Keys are cloned Arcs — cheap.
        let rect_of: HashMap<Window, SnapRect> =
            rects.iter().map(|(w, r)| (w.clone(), *r)).collect();

        // Compute the static shift set for a single side.
        let compute_shift_set = |side: Side| -> HashSet<Window> {
            // Step 1: direct neighbors of primary on this side.
            let mut set: HashSet<Window> = HashSet::new();
            let mut queue: std::collections::VecDeque<Window> =
                std::collections::VecDeque::new();
            for (w, r) in &rects {
                if w == window {
                    continue;
                }
                if adjacent_side(&primary_rect, r, gap) == Some(side) {
                    set.insert(w.clone());
                    queue.push_back(w.clone());
                }
            }

            // Step 2: BFS through full cluster graph, adding members whose
            // rect is downstream of the primary's `side` edge.
            let is_downstream = |r: &SnapRect| -> bool {
                match side {
                    Side::Right => r.x_low >= primary_rect.x_high,
                    Side::Left => r.x_high <= primary_rect.x_low,
                    Side::Bottom => r.y_low >= primary_rect.y_high,
                    Side::Top => r.y_high <= primary_rect.y_low,
                }
            };

            while let Some(current) = queue.pop_front() {
                let Some(cur_rect) = rect_of.get(&current).copied() else {
                    continue;
                };
                for (w, r) in &rects {
                    if w == window || set.contains(w) || !full.contains(w) {
                        continue;
                    }
                    if adjacent_side(&cur_rect, r, gap).is_none() {
                        continue;
                    }
                    if !is_downstream(r) {
                        continue;
                    }
                    set.insert(w.clone());
                    queue.push_back(w.clone());
                }
            }

            set
        };

        let right_set = if has_right(edges) {
            compute_shift_set(Side::Right)
        } else {
            HashSet::new()
        };
        let left_set = if has_left(edges) {
            compute_shift_set(Side::Left)
        } else {
            HashSet::new()
        };
        let bottom_set = if has_bottom(edges) {
            compute_shift_set(Side::Bottom)
        } else {
            HashSet::new()
        };
        let top_set = if has_top(edges) {
            compute_shift_set(Side::Top)
        } else {
            HashSet::new()
        };

        // Exclude set = union of static shift sets only. Stationary cluster
        // members stay as valid snap targets so the primary can same-edge
        // or opposite-edge snap against them.
        let mut exclude: HashSet<WlSurface> = HashSet::new();
        for set in [&right_set, &left_set, &bottom_set, &top_set] {
            for w in set {
                if let Some(s) = w.wl_surface() {
                    exclude.insert(s.into_owned());
                }
            }
        }

        // Build the member list from the FULL cluster. Members with no
        // static shift on either axis still ride along so the cascade can
        // reach them.
        let mut members = Vec::new();
        for m in full {
            if &m == window {
                continue;
            }
            let Some(initial_pos) = self.space.element_location(&m) else {
                continue;
            };
            let Some(initial_rect) = rect_of.get(&m).copied() else {
                continue;
            };
            let axis_x = if right_set.contains(&m) {
                Some(Side::Right)
            } else if left_set.contains(&m) {
                Some(Side::Left)
            } else {
                None
            };
            let axis_y = if bottom_set.contains(&m) {
                Some(Side::Bottom)
            } else if top_set.contains(&m) {
                Some(Side::Top)
            } else {
                None
            };
            members.push(ClusterResizeMember {
                window: m,
                initial_pos,
                initial_rect,
                axis_x,
                axis_y,
            });
        }

        ClusterResizeSnapshot {
            members,
            exclude,
            bonds: Vec::new(),
            bonds_set: HashSet::new(),
            primary_rect,
            resize_edges: edges as u32,
        }
    }
}

/// A cluster member (other than the primary) captured at resize grab start.
///
/// All cluster members are recorded, not just statically-shifted ones: the
/// motion-time cascade in `ResizeSurfaceGrab::motion` needs access to
/// non-shifted members' initial rects so it can detect post-shift overlap and
/// pull them into the shift set.
///
/// `axis_x` / `axis_y` are `Some` when this member is part of the *static*
/// shift set (steps 1+2 of the snapshot algorithm): direct neighbors of the
/// primary's resized edge, plus cluster members "downstream" of that edge
/// reachable via BFS through the full cluster graph. `None` means the member
/// stays stationary unless cascade picks it up.
pub struct ClusterResizeMember {
    pub window: Window,
    pub initial_pos: Point<i32, Logical>,
    pub initial_rect: driftwm::snap::SnapRect,
    pub axis_x: Option<crate::cluster::Side>,
    pub axis_y: Option<crate::cluster::Side>,
}

/// Frozen-at-grab-start cluster snapshot for `ResizeSurfaceGrab`.
///
/// `members` holds every cluster member except the primary. `exclude` is the
/// *static* shift set only (not the full cluster): stationary cluster
/// members remain valid `snap_targets`, so e.g. `A.right` can same-edge-snap
/// to the right edge of a cluster neighbor that isn't moving this frame.
pub struct ClusterResizeSnapshot {
    pub members: Vec<ClusterResizeMember>,
    pub exclude: HashSet<WlSurface>,
    /// Sticky bonds formed during the push cascade. Once `(m, n)` is bonded,
    /// `n` tracks `m`'s leading edge ± gap unconditionally on every
    /// subsequent frame — including drag reversal and past `n`'s initial
    /// position. Bonds persist until the grab ends (snapshot is dropped).
    /// Stored as a `Vec` to preserve insertion order, which determines
    /// cascade evaluation order for transitive chains (A→B before B→C).
    pub bonds: Vec<(usize, usize)>,
    /// Parallel set for O(1) bond dedup. Mirrors `bonds` contents.
    bonds_set: HashSet<(usize, usize)>,
    /// Primary window's rect frozen at grab start, used to compute
    /// primary-push encroachment in `resolve_cluster_shifts` phase 2.5.
    pub primary_rect: driftwm::snap::SnapRect,
    /// Which edges are active for this resize (raw u32 bitmask: top=1,
    /// bottom=2, left=4, right=8). Combined with `primary_rect` and the
    /// current deltas, this lets `compute_shifts` reconstruct the primary's
    /// current rect each frame without storing extra state.
    pub resize_edges: u32,
}

impl ClusterResizeSnapshot {
    /// Empty snapshot for single-window resize (no cluster members).
    pub fn empty() -> Self {
        Self {
            members: Vec::new(),
            exclude: HashSet::new(),
            bonds: Vec::new(),
            bonds_set: HashSet::new(),
            primary_rect: driftwm::snap::SnapRect {
                x_low: 0.0,
                x_high: 0.0,
                y_low: 0.0,
                y_high: 0.0,
            },
            resize_edges: 0,
        }
    }

    /// Compute shifts, reposition every affected cluster member, and
    /// re-map the primary to the tail of `Space::elements` so it stays
    /// on top of its own cluster. One call replaces the duplicated
    /// `compute_shifts` + `map_element` loop + primary re-map block that
    /// was previously inlined in both `ResizeSurfaceGrab::motion` and
    /// the gesture resize path.
    pub fn apply_member_shifts(
        &mut self,
        space: &mut Space<Window>,
        primary: &Window,
        initial_size: Size<i32, Logical>,
        new_w: i32,
        new_h: i32,
        gap: f64,
    ) {
        use smithay::utils::IsAlive;
        if self.members.is_empty() {
            return;
        }
        let width_delta = new_w - initial_size.w;
        let height_delta = new_h - initial_size.h;
        let shifts = self.compute_shifts(width_delta, height_delta, gap);

        for (i, (dx, dy)) in &shifts {
            let m = &self.members[*i];
            if !m.window.alive() {
                continue;
            }
            let new_pos = m.initial_pos + Point::from((*dx, *dy));
            space.map_element(m.window.clone(), new_pos, false);
        }

        if !shifts.is_empty()
            && let Some(cur) = space.element_location(primary)
        {
            space.map_element(primary.clone(), cur, false);
        }
    }

    /// Compute the per-member translation vector for one motion tick.
    ///
    /// Wraps `cluster::resolve_cluster_shifts`, passing the accumulated
    /// bonds and merging any newly-formed bonds back. Takes `&mut self`
    /// so bonds persist across frames.
    pub fn compute_shifts(
        &mut self,
        width_delta: i32,
        height_delta: i32,
        gap: f64,
    ) -> HashMap<usize, (i32, i32)> {
        use crate::cluster::{ResizeClassification, resolve_cluster_shifts};
        use smithay::utils::IsAlive;

        // Dead windows get a degenerate rect that can't produce overlap
        // in the push cascade, preventing ghost-rect collisions from a
        // member that was unmapped mid-drag.
        const DEAD_RECT: driftwm::snap::SnapRect = driftwm::snap::SnapRect {
            x_low: f64::MAX / 2.0,
            x_high: f64::MAX / 2.0,
            y_low: f64::MAX / 2.0,
            y_high: f64::MAX / 2.0,
        };

        let classifications: Vec<ResizeClassification> = self
            .members
            .iter()
            .map(|m| {
                let alive = m.window.alive();
                ResizeClassification {
                    axis_x: if alive { m.axis_x } else { None },
                    axis_y: if alive { m.axis_y } else { None },
                    initial_rect: if alive { m.initial_rect } else { DEAD_RECT },
                }
            })
            .collect();

        // Reconstruct primary's current rect from the frozen initial rect and
        // the active edges + current deltas, so phase 2.5 can push members.
        let mut p_cur = self.primary_rect;
        if self.resize_edges & 8 != 0 { p_cur.x_high += width_delta as f64; }
        if self.resize_edges & 4 != 0 { p_cur.x_low -= width_delta as f64; }
        if self.resize_edges & 2 != 0 { p_cur.y_high += height_delta as f64; }
        if self.resize_edges & 1 != 0 { p_cur.y_low -= height_delta as f64; }
        let primary = Some((self.primary_rect, p_cur));

        let (shifts, new_bonds) =
            resolve_cluster_shifts(&classifications, width_delta, height_delta, gap, &self.bonds, primary);
        for bond in new_bonds {
            if self.bonds_set.insert(bond) {
                self.bonds.push(bond);
            }
        }
        shifts
    }
}

/// `(cluster_members, cluster_member_surfaces)` — snapshot for a move drag.
/// Members carry their canvas-offset-from-primary; surfaces are the exclude
/// set for `snap_targets` during the drag.
pub type ClusterDragSnapshot = (
    Vec<(Window, Point<i32, Logical>)>,
    HashSet<WlSurface>,
);

/// Free-function form of `DriftWm::snap_targets` for callers that already hold
/// a mutable borrow on another `DriftWm` field (e.g. `gesture_state`) and so
/// cannot call the `&self` method. Rust's borrow checker accepts disjoint field
/// borrows when they're passed as separate references.
pub(crate) fn snap_targets_impl(
    space: &Space<Window>,
    decorations: &HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        crate::decorations::WindowDecoration,
    >,
    primary: &WlSurface,
    cluster_excludes: &HashSet<WlSurface>,
) -> (Vec<driftwm::snap::SnapRect>, i32) {
    let self_bar = if decorations.contains_key(&primary.id()) {
        driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT
    } else {
        0
    };
    let mut others = Vec::new();
    for w in space.elements() {
        let Some((surface, rect)) = window_snap_rect(space, decorations, w) else {
            continue;
        };
        if surface == *primary || cluster_excludes.contains(&surface) {
            continue;
        }
        others.push(rect);
    }
    (others, self_bar)
}

/// Compute the snap rectangle for a single window, returning its surface
/// alongside (needed by `snap_targets_impl` for exclusion checks). Returns
/// `None` for widgets, unmapped windows, or anything without a `wl_surface`.
/// Y-low includes the title-bar height for CSD-decorated windows.
fn window_snap_rect(
    space: &Space<Window>,
    decorations: &HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        crate::decorations::WindowDecoration,
    >,
    w: &Window,
) -> Option<(WlSurface, driftwm::snap::SnapRect)> {
    let surface = w.wl_surface()?.into_owned();
    if driftwm::config::applied_rule(&surface).is_some_and(|r| r.widget) {
        return None;
    }
    let loc = space.element_location(w)?;
    let size = w.geometry().size;
    let bar = if decorations.contains_key(&surface.id()) {
        driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT
    } else {
        0
    };
    Some((
        surface,
        driftwm::snap::SnapRect {
            x_low: loc.x as f64,
            x_high: loc.x as f64 + size.w as f64,
            y_low: loc.y as f64 - bar as f64,
            y_high: loc.y as f64 + size.h as f64,
        },
    ))
}

fn state_file_dir() -> Option<std::path::PathBuf> {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(|d| std::path::PathBuf::from(d).join("driftwm"))
}

/// Remove the state file on compositor exit.
pub fn remove_state_file() {
    if let Some(dir) = state_file_dir() {
        let _ = std::fs::remove_file(dir.join("state"));
        let _ = std::fs::remove_file(dir.join("state.tmp"));
    }
}

/// Read all per-output camera/zoom entries from the state file.
/// Returns a map from output name to `(camera, zoom)`.
pub fn read_all_per_output_state() -> HashMap<String, (Point<f64, Logical>, f64)> {
    let mut result = HashMap::new();
    let Some(dir) = state_file_dir() else {
        return result;
    };
    let Ok(content) = std::fs::read_to_string(dir.join("state")) else {
        return result;
    };

    // Parse lines like "outputs.eDP-1.camera_x=123.4"
    type Partial = (Option<f64>, Option<f64>, Option<f64>);
    let mut entries: HashMap<String, Partial> = HashMap::new();
    for line in content.lines() {
        let Some(rest) = line.strip_prefix("outputs.") else {
            continue;
        };
        // rest = "eDP-1.camera_x=123.4"
        let Some((name_and_key, val_str)) = rest.split_once('=') else {
            continue;
        };
        let Ok(val) = val_str.parse::<f64>() else {
            continue;
        };
        if let Some(name) = name_and_key.strip_suffix(".camera_x") {
            entries.entry(name.to_string()).or_default().0 = Some(val);
        } else if let Some(name) = name_and_key.strip_suffix(".camera_y") {
            entries.entry(name.to_string()).or_default().1 = Some(val);
        } else if let Some(name) = name_and_key.strip_suffix(".zoom") {
            entries.entry(name.to_string()).or_default().2 = Some(val);
        }
    }
    for (name, (cx, cy, z)) in entries {
        if let (Some(x), Some(y), Some(zoom)) = (cx, cy, z) {
            result.insert(name, (Point::from((x, y)), zoom));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use driftwm::canvas::MomentumState;

    fn mock_output_state(
        camera: (f64, f64),
        zoom: f64,
        layout_position: (i32, i32),
    ) -> OutputState {
        OutputState {
            camera: Point::from(camera),
            zoom,
            zoom_target: None,
            zoom_animation_center: None,
            last_rendered_zoom: zoom,
            overview_return: None,
            camera_target: None,
            last_scroll_pan: None,
            momentum: MomentumState::new(0.96),
            panning: false,
            edge_pan_velocity: None,
            last_rendered_camera: Point::from(camera),
            last_frame_instant: Instant::now(),
            layout_position: Point::from(layout_position),
            home_return: None,
        }
    }

    #[test]
    fn canvas_to_layout_round_trip_zoom_1() {
        let os = mock_output_state((100.0, 200.0), 1.0, (0, 0));
        let canvas = Point::from((150.0, 250.0));
        let layout = DriftWm::canvas_to_layout_pos(canvas, &os);
        let back = DriftWm::layout_to_canvas_pos(layout, &os);
        assert!((back.x - canvas.x).abs() < 0.001);
        assert!((back.y - canvas.y).abs() < 0.001);
    }

    #[test]
    fn canvas_to_layout_round_trip_with_zoom() {
        let os = mock_output_state((50.0, 75.0), 2.0, (1920, 0));
        let canvas = Point::from((80.0, 100.0));
        let layout = DriftWm::canvas_to_layout_pos(canvas, &os);
        let back = DriftWm::layout_to_canvas_pos(layout, &os);
        assert!((back.x - canvas.x).abs() < 0.001);
        assert!((back.y - canvas.y).abs() < 0.001);
    }

    #[test]
    fn canvas_to_layout_known_values() {
        // camera=(100,200), zoom=2, layout_position=(1920,0)
        // screen = (canvas - camera) * zoom = (50-100)*2 = -100, (50-200)*2 = -300
        // layout = screen + layout_position = -100+1920 = 1820, -300+0 = -300
        let os = mock_output_state((100.0, 200.0), 2.0, (1920, 0));
        let canvas = Point::from((50.0, 50.0));
        let layout = DriftWm::canvas_to_layout_pos(canvas, &os);
        assert!((layout.x - 1820.0).abs() < 0.001);
        assert!((layout.y - (-300.0)).abs() < 0.001);
    }

    #[test]
    fn layout_to_canvas_known_values() {
        // layout=(1920,0), layout_position=(1920,0), zoom=1, camera=(500,300)
        // screen = layout - layout_position = (0, 0)
        // canvas = screen / zoom + camera = 0 + 500 = 500, 0 + 300 = 300
        let os = mock_output_state((500.0, 300.0), 1.0, (1920, 0));
        let layout = Point::from((1920.0, 0.0));
        let canvas = DriftWm::layout_to_canvas_pos(layout, &os);
        assert!((canvas.x - 500.0).abs() < 0.001);
        assert!((canvas.y - 300.0).abs() < 0.001);
    }

    #[test]
    fn round_trip_two_outputs_different_cameras() {
        let os_a = mock_output_state((0.0, 0.0), 1.0, (0, 0));
        let os_b = mock_output_state((500.0, 200.0), 0.5, (1920, 0));

        let canvas = Point::from((600.0, 300.0));
        // Through output A
        let layout_a = DriftWm::canvas_to_layout_pos(canvas, &os_a);
        let back_a = DriftWm::layout_to_canvas_pos(layout_a, &os_a);
        assert!((back_a.x - canvas.x).abs() < 0.001);
        assert!((back_a.y - canvas.y).abs() < 0.001);

        // Through output B
        let layout_b = DriftWm::canvas_to_layout_pos(canvas, &os_b);
        let back_b = DriftWm::layout_to_canvas_pos(layout_b, &os_b);
        assert!((back_b.x - canvas.x).abs() < 0.001);
        assert!((back_b.y - canvas.y).abs() < 0.001);
    }
}
