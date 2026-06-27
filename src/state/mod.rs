mod animation;
mod cluster_snapshot;
mod cursor;
mod errors;
pub mod fit;
mod focus;
mod fullscreen;
mod init;
mod navigation;
pub mod persistence;
mod reload;
mod render_cache;
pub use cluster_snapshot::ClusterResizeSnapshot;
pub use cursor::{CursorFrames, CursorState};
pub use errors::ErrorSource;
pub use focus::FocusTarget;
pub use persistence::{read_all_per_output_state, remove_state_file};
pub use render_cache::{BorderCacheEntry, RenderCache, ShadowCacheEntry};

use smithay::{
    desktop::{PopupGrab, PopupManager, PopupUngrabStrategy, Space, Window},
    input::{Seat, SeatState},
    output::Output,
    reexports::{
        calloop::{LoopHandle, LoopSignal},
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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesTexture;
use smithay::utils::Physical;
use smithay::wayland::background_effect::BackgroundEffectState;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::idle_inhibit::IdleInhibitManagerState;
use smithay::wayland::idle_notify::IdleNotifierState;
use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState;
use smithay::wayland::pointer_constraints::PointerConstraintsState;
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::security_context::SecurityContextState;
use smithay::wayland::selection::ext_data_control::DataControlState as ExtDataControlState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::selection::wlr_data_control::DataControlState;
use smithay::wayland::session_lock::{LockSurface, SessionLockManagerState, SessionLocker};
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;
use smithay::wayland::xdg_activation::XdgActivationState;
use smithay::wayland::xdg_foreign::XdgForeignState;

use smithay::backend::session::libseat::LibSeatSession;
use smithay::wayland::seat::WaylandFocus;

use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::drm::control::crtc;

use crate::backend::Backend;
use crate::input::gestures::GestureState;
use crate::input::keyboard::TapTracker;
use driftwm::canvas::MomentumState;
use driftwm::config::Config;
use driftwm::window_ext::WindowExt;

/// Min visible fraction of the focused window for auto-placement to anchor a
/// new window to its cluster. Lower than the navigation/activation thresholds:
/// even a small sliver of the cluster on-screen is a stronger signal than the
/// alternative (dropping the new window in the middle of an unrelated region).
const AUTO_PLACE_CLUSTER_THRESHOLD: f64 = 0.33;

/// A layer surface pinned to a canvas position instead of being anchored
/// via LayerMap. Created when a layer's namespace matches a rule with `position`.
pub struct CanvasLayer {
    pub surface: smithay::desktop::LayerSurface,
    /// Rule position (Y-up, window-centered) — converted to canvas coords after first commit.
    pub rule_position: (i32, i32),
    /// Internal canvas position (Y-down, top-left). None until first commit reveals size.
    pub position: Option<Point<i32, Logical>>,
    pub namespace: String,
}

/// Per-output screencopy state reused across frames so the damage tracker's
/// age increments and smithay re-renders only damaged regions.
pub struct CaptureOutputState {
    pub damage_tracker: OutputDamageTracker,
    pub offscreen_texture: Option<(GlesTexture, Size<i32, Physical>)>,
    pub age: usize,
    /// Reset age when cursor inclusion changes between frames.
    pub last_paint_cursors: bool,
    /// Time (since `start_time`) this state was last rendered into; idle
    /// entries are evicted so a finished capture's texture doesn't linger
    /// until output disconnect.
    pub last_used: std::time::Duration,
    /// Last frame time submitted to a continuous capture client, for
    /// `max_capture_fps` rate-limiting.
    pub last_submit: Option<std::time::Duration>,
}

/// Buffered middle-click from a 3-finger tap. Held for DOUBLE_TAP_WINDOW_MS
/// to see if a 3-finger swipe follows (→ move window); on timeout the click
/// is forwarded to the client (paste).
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

#[inline]
pub(crate) fn log_err(context: &str, result: Result<impl Sized, impl std::fmt::Display>) {
    if let Err(e) = result {
        tracing::error!("{context}: {e}");
    }
}

/// Spawn a shell command with SIGCHLD reset to default and sigmask cleared.
/// The compositor sets SIG_IGN on SIGCHLD for zombie reaping, but children
/// inherit this — breaking GLib's waitpid()-based subprocess management
/// (swaync-client hangs because GSpawnSync gets ECHILD).
/// We also block SIGINT/SIGTERM/SIGHUP via pthread_sigmask for our own
/// shutdown handling, and that mask is inherited too — clear it so apps
/// with their own signal handlers still see those signals normally.
///
/// `env` is layered on top of inherited env (toolkit defaults + user `[env]` +
/// XCURSOR_*); driftwm never mutates its own process env at runtime, so this
/// is the only way config-defined env vars reach children.
pub fn spawn_command(cmd: &str, env: &HashMap<String, String>) {
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new("sh");
    child.args(["-c", cmd]).envs(env);
    unsafe {
        child.pre_exec(|| {
            libc::signal(libc::SIGCHLD, libc::SIG_DFL);
            crate::signals::unblock_all()?;
            Ok(())
        });
    }
    log_err("spawn command", child.spawn());
}

/// Saved viewport state for HomeToggle return, plus the optional fullscreen window to restore.
#[derive(Clone)]
pub struct HomeReturn {
    pub camera: Point<f64, Logical>,
    pub zoom: f64,
    pub fullscreen_window: Option<Window>,
}

/// Pre-fullscreen geometry + viewport, restored on exit.
pub struct FullscreenState {
    pub window: Window,
    pub saved_location: Point<i32, Logical>,
    pub saved_camera: Point<f64, Logical>,
    pub saved_zoom: f64,
    pub saved_size: Size<i32, Logical>,
    /// If the window was screen-pinned when it entered fullscreen, its pin
    /// state, re-inserted on exit so fullscreen is a transparent round-trip
    /// (pinned → fullscreen → pinned) rather than a permanent unpin.
    pub saved_pinned: Option<PinnedState>,
}

pub struct PendingRecenter {
    pub target_center: Point<f64, Logical>,
    pub pre_exit_size: Size<i32, Logical>,
}

/// Active drag-and-drop icon. `offset` accumulates `wl_surface.attach` deltas
/// so the icon stays anchored to the client's grab point (e.g. a Firefox tab
/// dragged from its mid-point doesn't snap to top-left of the cursor).
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

#[derive(Clone, Debug)]
pub struct PendingMode {
    pub intent: ModeIntent,
    pub retry_count: u8,
}

/// What mode the user (config or wlr-output-management client) asked for.
/// Resolved to a concrete `drm::control::Mode` in the udev backend.
#[derive(Clone, Debug)]
pub enum ModeIntent {
    /// Index into the connector's EDID-advertised modes list. Sent by
    /// wlr-output-management `SetMode` after the protocol layer chose a
    /// specific `ZwlrOutputModeV1`.
    EdidIndex(usize),
    /// Custom WxH@refresh_mHz. Tried as an exact EDID match first; if not
    /// found, a CVT modeline is synthesized.
    Custom { w: i32, h: i32, refresh_mhz: i32 },
    /// "Whatever the connector says is preferred." Reserved for the
    /// reload-restores-preferred case; deferred in the v1 reload path
    /// (we don't currently re-modeset when rule reverts to Preferred).
    #[allow(dead_code)]
    Preferred,
}

/// Per-output viewport state, stored on each `Output` via `UserDataMap`
/// (wrapped in `Mutex` since `UserDataMap` requires `Sync`). !Send fields
/// and non-Copy ownership types (fullscreen, lock_surface) stay on DriftWm.
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
    /// Physical arrangement in layout space: (0,0) for single output,
    /// from config for multi-monitor.
    pub layout_position: Point<i32, Logical>,
    pub home_return: Option<HomeReturn>,
}

pub fn init_output_state(
    output: &Output,
    camera: Point<f64, Logical>,
    drift: f64,
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
            momentum: MomentumState::new(drift),
            panning: false,
            edge_pan_velocity: None,
            last_rendered_camera: Point::from((f64::NAN, f64::NAN)),
            last_frame_instant: Instant::now(),
            layout_position,
            home_return: None,
        })
    });
}

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

pub fn output_state(output: &Output) -> MutexGuard<'_, OutputState> {
    output
        .user_data()
        .get::<Mutex<OutputState>>()
        .expect("OutputState not initialized on output")
        .lock()
        .expect("OutputState mutex poisoned")
}

/// An output's current viewport as a canvas rect: `screen = (canvas − camera) ·
/// zoom`, so it spans `size / zoom` canvas units from the camera. Single source
/// of truth for the bare-`screenshot` region and the capture wallpaper anchor,
/// which must agree or the wallpaper crop misaligns.
pub fn output_viewport_rect(output: &Output) -> Rectangle<i32, Logical> {
    let (camera, zoom) = {
        let os = output_state(output);
        (os.camera, os.zoom)
    };
    let size = output_logical_size(output);
    Rectangle::new(
        camera.to_i32_round(),
        Size::<i32, Logical>::from((
            (size.w as f64 / zoom).round() as i32,
            (size.h as f64 / zoom).round() as i32,
        )),
    )
}

/// Runtime state for a window pinned to one output's screen space (the
/// `pinned_to_screen` rule or the `pin-to-screen` toggle). The window stays in
/// `space` for focus/decorations/popups; rendering, hit-testing, and capture
/// route through this instead of the camera transform. Source of truth for
/// "is this window pinned" — `DriftWm::is_pinned`.
pub struct PinnedState {
    pub output: Output,
    /// Output-relative top-left, Y-down (internal convention), pre-zoom.
    pub screen_pos: Point<i32, Logical>,
}

/// An active xdg-popup grab and the toplevel/layer surface it is rooted on.
/// Kept so focus changes can tear the grab down explicitly: smithay leaves the
/// grab attached to the keyboard after the popup is gone, so without this the
/// keyboard would stay pinned to `root` while navigation moves the camera.
pub struct PopupGrabState {
    pub root: WlSurface,
    pub grab: PopupGrab<DriftWm>,
    /// False = pointer-only grab, for a root that takes no keyboard focus. Gates
    /// the focus-change teardown: focus can never reach such a root, so tearing
    /// down there would just dismiss the popup.
    pub has_keyboard_grab: bool,
}

/// Central compositor state.
pub struct DriftWm {
    pub start_time: Instant,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, DriftWm>,
    pub loop_signal: LoopSignal,

    pub space: Space<Window>,
    pub popups: PopupManager,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<DriftWm>,
    pub data_device_state: DataDeviceState,

    pub seat: Seat<DriftWm>,

    pub cursor: CursorState,
    pub dnd_icon: Option<DndIcon>,

    pub backend: Option<Backend>,
    // -- global: IPC server --
    pub ipc_server: Option<crate::ipc::IpcServer>,
    // -- global: SSD decorations --
    pub decorations: HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        crate::decorations::WindowDecoration,
    >,
    pub pending_ssd: HashSet<smithay::reexports::wayland_server::backend::ObjectId>,
    /// Windows pinned to an output's screen space. Same lifetime as
    /// `decorations` (keyed by surface `ObjectId`, cleaned on destroy).
    pub pinned: HashMap<smithay::reexports::wayland_server::backend::ObjectId, PinnedState>,
    /// Supersample factor for SSD decoration buffers: `ceil` of the largest
    /// output scale. One buffer rendered at this density serves every output
    /// (downscaling stays crisp; only upscaling blurs).
    pub decoration_scale: i32,
    pub render: RenderCache,

    pub dmabuf_state: DmabufState,
    pub dmabuf_global: Option<DmabufGlobal>,
    /// DRM render-node `dev_t` and DMA-BUF formats. `None` on winit (nested
    /// compositor has no direct DRM device). Used by ext-image-copy-capture.
    pub render_device: Option<u64>,
    pub render_dmabuf_formats: Option<smithay::backend::allocator::format::FormatSet>,
    #[allow(dead_code)]
    pub cursor_shape_state: CursorShapeManagerState,
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    #[allow(dead_code)]
    pub fractional_scale_state: FractionalScaleManagerState,
    pub xdg_activation_state: XdgActivationState,
    pub primary_selection_state: PrimarySelectionState,
    pub data_control_state: DataControlState,
    pub ext_data_control_state: ExtDataControlState,
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
    /// Surfaces holding zwp-idle-inhibit-v1 inhibitors. Only those actively
    /// scanning out count, so a hidden browser tab playing video can't keep
    /// the screen awake.
    pub idle_inhibiting_surfaces: HashSet<WlSurface>,
    pub idle_notifier_state: IdleNotifierState<DriftWm>,
    #[allow(dead_code)]
    pub presentation_state: PresentationState,
    #[allow(dead_code)]
    pub decoration_state: XdgDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub foreign_toplevel_state: driftwm::protocols::foreign_toplevel::ForeignToplevelManagerState,
    pub foreign_toplevel_list_state:
        smithay::wayland::foreign_toplevel_list::ForeignToplevelListState,
    pub screencopy_state: driftwm::protocols::screencopy::ScreencopyManagerState,
    pub output_management_state: driftwm::protocols::output_management::OutputManagementState,
    pub output_power_state: driftwm::protocols::output_power::OutputPowerState,
    /// Outputs currently in DPMS off; render loop skips these.
    pub dpms_off_outputs: HashSet<Output>,
    /// Client-requested DPMS transitions awaiting the udev render loop —
    /// can't touch DrmCompositor from wayland dispatch (it lives behind
    /// Rc<RefCell<>> in calloop closures).
    pub pending_dpms: HashMap<Output, bool>,
    pub pending_screencopies: Vec<driftwm::protocols::screencopy::Screencopy>,
    #[allow(dead_code)]
    pub image_capture_source_state: smithay::wayland::image_capture_source::ImageCaptureSourceState,
    pub output_capture_source_state:
        smithay::wayland::image_capture_source::OutputCaptureSourceState,
    pub toplevel_capture_source_state:
        smithay::wayland::image_capture_source::ToplevelCaptureSourceState,
    pub image_copy_capture_state: driftwm::protocols::image_copy_capture::ImageCopyCaptureState,
    pub pending_captures: Vec<driftwm::protocols::image_copy_capture::PendingCapture>,
    pub xdg_foreign_state: XdgForeignState,
    #[allow(dead_code)]
    pub background_effect_state: BackgroundEffectState,
    pub session_lock_manager_state: SessionLockManagerState,
    pub gamma_control_manager_state: driftwm::protocols::gamma_control::GammaControlManagerState,
    pub session_lock: SessionLock,
    pub lock_surfaces: HashMap<Output, LockSurface>,

    pub pointer_over_layer: bool,
    pub canvas_layers: Vec<CanvasLayer>,

    pub config: Config,

    pub pending_center: HashSet<WlSurface>,
    pub pending_size: HashSet<WlSurface>,
    /// Surfaces that requested set_maximized / set_fullscreen before their
    /// first sized commit. Deferred until after first-commit positioning so
    /// `restore_size` / `saved_size` capture the client's preferred geometry.
    pub pending_fit: HashSet<WlSurface>,
    pub pending_fullscreen: HashSet<WlSurface>,
    /// Keyboard focus snapshot captured at `new_toplevel` time, keyed by the
    /// new surface. `Some(None)` means user had no focus (e.g. clicked empty
    /// canvas); missing entry means snapshot was already consumed.
    pub auto_anchor_snapshot: HashMap<WlSurface, Option<Window>>,
    /// After unfit, re-center around `target_center` once geometry actually
    /// shrinks from `pre_exit_size`. Waiting avoids firing while the client
    /// (Chromium) still reports the fit-era size.
    pub pending_recenter:
        HashMap<smithay::reexports::wayland_server::backend::ObjectId, PendingRecenter>,
    /// Last "settled" snap rect per window, captured at initial map and
    /// move/resize grab end. Used as authoritative rect in
    /// `toplevel_destroyed` — protects against clients (foot) that
    /// shrink/reposition their buffer during destroy.
    pub stable_snap_rects: HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        driftwm::layout::snap::SnapRect,
    >,

    pub focus_history: Vec<Window>,
    pub cycle_state: Option<usize>,

    /// Window-level keyboard-focus intent. The actual keyboard focus is
    /// derived from this plus any higher-priority owner (session lock,
    /// exclusive / on-demand layer surface) in `update_keyboard_focus`.
    pub window_focus: Option<FocusTarget>,
    /// Layer surface granted keyboard focus on click via `OnDemand`
    /// interactivity. Cleared when a window takes focus or it unmaps.
    pub on_demand_layer: Option<WlSurface>,
    /// The active popup keyboard/pointer grab, if any. See [`PopupGrabState`].
    pub popup_grab: Option<PopupGrabState>,

    pub held_action: Option<(u32, driftwm::config::Action, Instant)>,

    pub tap: TapTracker,
    /// Action queued by a completed tap chord, run after the closure forwards
    /// the modifier events so the focused client still sees them.
    pub pending_tap_action: Option<driftwm::config::Action>,

    /// Keycodes whose press was intercepted by a binding. Their releases must
    /// also be intercepted, otherwise the focused client receives a "release
    /// without press" — games / Discord / state-tracking apps break, and
    /// launchers leak the trigger key into the previously focused window.
    pub suppressed_keys: HashSet<u32>,

    pub fullscreen: HashMap<Output, FullscreenState>,

    pub gesture_state: Option<GestureState>,
    pub pending_middle_click: Option<PendingMiddleClick>,

    pub momentum_timer: Option<RegistrationToken>,

    pub session: Option<LibSeatSession>,
    pub input_devices: Vec<smithay::reexports::input::Device>,

    pub state_file_cameras: HashMap<String, (Point<f64, Logical>, f64)>,
    pub state_file_last_write: Instant,
    /// Active XKB layout name (e.g. "English (US)"), updated on key events.
    pub active_layout: String,
    pub state_file_layout: String,
    pub state_file_windows: Vec<crate::ipc::protocol::WindowInfo>,
    pub state_file_layer_count: usize,

    pub autostart: Vec<String>,

    /// Outputs whose CRTC is currently active. Universe for [`Self::mark_all_dirty`].
    pub active_outputs: HashSet<Output>,
    pub redraws_needed: HashSet<Output>,
    pub frames_pending: HashSet<crtc::Handle>,
    /// One-shot timers armed when queue_frame returned EmptyFrame so the loop
    /// still wakes at ~refresh rate to advance animations (e.g. xcursor frames).
    pub estimated_vblank_timers: HashMap<crtc::Handle, RegistrationToken>,

    pub config_file_mtime: Option<std::time::SystemTime>,

    /// Global animation tick timestamp — separate from per-output
    /// last_frame_instant to avoid double-ticking when multiple outputs
    /// render in one iteration.
    pub last_animation_tick: Instant,
    /// A deferred pointer resync is pending. Flushed once per rendered frame so
    /// a 90-140 Hz pan/momentum stream doesn't re-render a hover-reactive client
    /// per event. See [`DriftWm::warp_pointer`].
    pub pending_pointer_resync: bool,
    /// wl_surface commits since the last rendered frame. Tracy diagnostic
    /// counter (plotted as `frame.commits`); sampled and reset on every
    /// render_frame, so it's only meaningful on a single-output profiling
    /// session — with multiple outputs the count splits across them.
    pub commits_since_render: u32,
    /// Output the pointer is on (for input routing).
    pub focused_output: Option<Output>,
    /// Output a gesture started on (pinned for the gesture's duration).
    pub gesture_output: Option<Output>,
    /// Fullscreen window exited by a gesture (saved before execute_action runs).
    pub gesture_exited_fullscreen: Option<Window>,
    /// Virtual output placeholders kept when all physical outputs disconnect,
    /// so `active_output().unwrap()` doesn't panic.
    pub disconnected_outputs: HashSet<String>,
    /// Set when output config was applied via wlr-output-management; render
    /// loop re-collects output state and notifies clients.
    pub output_config_dirty: bool,
    /// Mode-change requests from wlr-output-management Apply or config reload.
    /// Drained by the udev render loop, which resolves each intent to a
    /// concrete `control::Mode`. Keyed by output name; backend resolves CRTCs.
    pub pending_mode_changes: HashMap<String, PendingMode>,

    pub satellite: Option<crate::xwayland::Satellite>,

    /// Udev backend handle (Rc — cloneable). Single owner here; render loop
    /// and protocols (gamma_control) borrow via `udev_device.as_ref()`.
    /// `None` when the winit backend is in use.
    pub udev_device: Option<crate::backend::udev::UdevDevice>,

    pub last_titlebar_click: Option<(
        Instant,
        smithay::reexports::wayland_server::backend::ObjectId,
    )>,

    /// Compositor-generated errors shown in the on-screen error bar, keyed by
    /// source. Empty = no bar. Use [`Self::set_error`]/[`Self::clear_error`].
    pub errors: BTreeMap<ErrorSource, String>,

    /// Cursor edge-pan: when true, the viewport pans while the bare cursor
    /// touches a screen edge. Toggled by
    /// [`Action::ToggleCursorPan`](driftwm::config::Action::ToggleCursorPan).
    pub cursor_edge_pan: bool,
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// True for clients connected via a security-context listener; denied
    /// privileged protocols (screencopy, foreign-toplevel, virtual keyboard).
    pub is_restricted: bool,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

pub(crate) fn client_is_unrestricted(client: &smithay::reexports::wayland_server::Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_none_or(|d| !d.is_restricted)
}

impl DriftWm {
    /// Drop dead inhibitors and tell idle-notifier whether any *visible*
    /// inhibitor surface is scanning out. Hidden inhibitors don't count —
    /// otherwise a backgrounded browser tab would keep the screen awake.
    pub fn refresh_idle_inhibit(&mut self) {
        use smithay::desktop::utils::surface_primary_scanout_output;
        use smithay::wayland::compositor::with_states;

        self.idle_inhibiting_surfaces.retain(|s| s.is_alive());

        let is_inhibited = self.idle_inhibiting_surfaces.iter().any(|surface| {
            with_states(surface, |states| {
                surface_primary_scanout_output(surface, states).is_some()
            })
        });
        self.idle_notifier_state.set_is_inhibited(is_inhibited);
    }

    /// Push any `below` windows to the bottom of the z-order.
    /// Called after every `raise_element()` to maintain stacking.
    pub fn enforce_below_windows(&mut self) {
        self.render.blur_geometry_generation += 1;
        // Space stores elements with last = topmost, and raise_element appends.
        // Raise non-below windows in reverse to keep their relative stacking
        // while ensuring they sit above any below windows.
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

        for fs in self.fullscreen.values() {
            self.space.raise_element(&fs.window, false);
        }
    }

    /// Raise `window`, then its child windows, so a child/modal dialog stays
    /// directly above its own parent without jumping over unrelated windows
    /// that sit higher in the stack.
    pub fn raise_with_children(&mut self, window: &Window) {
        let stack: Vec<Window> = self.space.elements().cloned().collect();
        let order = subtree_raise_order(&stack, window, |child, parent| {
            parent
                .wl_surface()
                .is_some_and(|s| child.parent_surface().as_ref() == Some(&*s))
        });
        for w in &order {
            self.space.raise_element(w, true);
        }
    }

    /// Drop every per-surface map/cache entry keyed by `surface`. Shared by the
    /// normal and crash shutdown paths so the two can't drift apart and leak.
    /// Pure removal — focus/fullscreen recovery stays at the call sites. Safe on
    /// non-toplevel surfaces: the extra lookups just miss.
    pub fn cleanup_surface_state(&mut self, surface: &WlSurface) {
        let id = surface.id();
        self.decorations.remove(&id);
        self.pinned.remove(&id);
        self.pending_ssd.remove(&id);
        self.pending_recenter.remove(&id);
        self.stable_snap_rects.remove(&id);
        self.pending_center.remove(surface);
        self.pending_size.remove(surface);
        self.pending_fit.remove(surface);
        self.pending_fullscreen.remove(surface);
        self.render.blur_cache.remove(&id);
        self.render.shadow_cache.remove(&id);
        self.render.border_cache.remove(&id);
        // capture_state keys this surface's texture/damage tracker under "cap-tl:".
        self.render
            .capture_state
            .remove(&format!("cap-tl:{:?}", id));
        self.image_copy_capture_state.remove_toplevel(surface);
        self.auto_anchor_snapshot.remove(surface);
        // Drop snapshots pointing at the destroyed surface as their anchor.
        // Keep `None`-anchor entries (user had no focus — unrelated).
        self.auto_anchor_snapshot
            .retain(|_, anchor| match anchor.as_ref() {
                None => true,
                Some(w) => w.wl_surface().is_some_and(|s| &*s != surface),
            });
    }

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(surface))
            .cloned()
    }

    /// Innermost modal descendant for focus redirect. Chases modal chains
    /// (e.g. file picker → overwrite confirm); capped at 10 to guard against
    /// circular parents.
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

    /// Raise a window and focus it (or its innermost modal child).
    pub fn raise_and_focus(&mut self, window: &Window, serial: smithay::utils::Serial) {
        self.raise_with_children(window);
        self.enforce_below_windows();

        let focus_surface = self
            .topmost_modal_child(window)
            .or(Some(window.clone()))
            .and_then(|w| w.wl_surface().map(|s| FocusTarget(s.into_owned())));

        self.set_window_focus(focus_surface, serial);
    }

    /// Record a window-level keyboard-focus intent and recompute the actual
    /// focus. Higher-priority owners (an exclusive / on-demand layer surface)
    /// still win — this is what keeps a launcher focused while the pointer
    /// moves over a window underneath it.
    pub fn set_window_focus(
        &mut self,
        target: Option<FocusTarget>,
        serial: smithay::utils::Serial,
    ) {
        self.window_focus = target;
        // An explicit window focus supersedes any on-demand layer focus.
        self.on_demand_layer = None;
        self.update_keyboard_focus(serial);
    }

    /// Derive and apply the authoritative keyboard focus from the current
    /// state, in priority order: session lock (handled imperatively, so we
    /// bail) → exclusive layer → on-demand layer → focused window.
    pub fn update_keyboard_focus(&mut self, serial: smithay::utils::Serial) {
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            return;
        }

        let target = self
            .exclusive_layer_focus()
            .or_else(|| self.on_demand_layer_focus())
            .or_else(|| self.focused_window_target());

        // Focus left the grab root: tear the stale grab down ourselves (see PopupGrabState).
        let leaving_grab_root = self
            .popup_grab
            .as_ref()
            .is_some_and(|g| g.has_keyboard_grab && target.as_ref().map(|t| &t.0) != Some(&g.root));
        if leaving_grab_root && let Some(mut g) = self.popup_grab.take() {
            g.grab.ungrab(PopupUngrabStrategy::All);
            let time = self.start_time.elapsed().as_millis() as u32;
            self.seat.get_keyboard().unwrap().unset_grab(self);
            self.seat
                .get_pointer()
                .unwrap()
                .unset_grab(self, serial, time);
        }

        // Focus staying on the grab root: a live grab keeps ownership (it rejects
        // the change), and an ended-but-still-attached grab releases on this
        // set_focus. Route through the keyboard directly, skipping the per-window
        // layout swap the live grab would otherwise trigger spuriously.
        let keyboard = self.seat.get_keyboard().unwrap();
        if keyboard.is_grabbed() {
            keyboard.set_focus(self, target, serial);
            return;
        }

        self.set_keyboard_focus(target, serial);
    }

    /// The window the keyboard falls back to when no layer owns focus. Prefers
    /// the live `window_focus` intent; if that window died while a layer or
    /// lock held focus, recovers via the most-recent live history entry rather
    /// than focusing nothing. A deliberate `None` (e.g. click on empty canvas)
    /// stays `None`.
    fn focused_window_target(&self) -> Option<FocusTarget> {
        use smithay::utils::IsAlive;
        match &self.window_focus {
            Some(t) if t.0.alive() => Some(t.clone()),
            Some(_) => self
                .focus_history
                .iter()
                .find(|w| w.alive())
                .and_then(|w| w.wl_surface().map(|s| FocusTarget(s.into_owned()))),
            None => None,
        }
    }

    /// First mapped layer surface (across outputs and canvas layers) that
    /// requests `Exclusive` keyboard interactivity, in z-priority order.
    fn exclusive_layer_focus(&self) -> Option<FocusTarget> {
        use smithay::utils::IsAlive;
        use smithay::wayland::shell::wlr_layer::{KeyboardInteractivity, Layer};

        for cl in &self.canvas_layers {
            let s = cl.surface.wl_surface();
            if s.alive()
                && cl.surface.cached_state().keyboard_interactivity
                    == KeyboardInteractivity::Exclusive
            {
                return Some(FocusTarget(s.clone()));
            }
        }
        for output in self.space.outputs() {
            let map = smithay::desktop::layer_map_for_output(output);
            for layer in [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background] {
                for surface in map.layers_on(layer) {
                    let s = surface.wl_surface();
                    if s.alive()
                        && surface.cached_state().keyboard_interactivity
                            == KeyboardInteractivity::Exclusive
                    {
                        return Some(FocusTarget(s.clone()));
                    }
                }
            }
        }
        None
    }

    /// The tracked on-demand layer surface, if it's still mapped and still
    /// requests `OnDemand` interactivity.
    fn on_demand_layer_focus(&self) -> Option<FocusTarget> {
        use smithay::utils::IsAlive;
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        let surface = self.on_demand_layer.as_ref()?;
        if !surface.alive() {
            return None;
        }
        (self.layer_interactivity(surface) == Some(KeyboardInteractivity::OnDemand))
            .then(|| FocusTarget(surface.clone()))
    }

    pub(crate) fn layer_interactivity(
        &self,
        surface: &WlSurface,
    ) -> Option<smithay::wayland::shell::wlr_layer::KeyboardInteractivity> {
        for cl in &self.canvas_layers {
            if cl.surface.wl_surface() == surface {
                return Some(cl.surface.cached_state().keyboard_interactivity);
            }
        }
        for output in self.space.outputs() {
            let map = smithay::desktop::layer_map_for_output(output);
            for l in map.layers() {
                if l.wl_surface() == surface {
                    return Some(l.cached_state().keyboard_interactivity);
                }
            }
        }
        None
    }

    /// On a click over a layer surface, grant it keyboard focus if it requests
    /// `OnDemand`. A click elsewhere (passed `None` or a non-on-demand layer)
    /// clears any existing on-demand focus.
    pub fn focus_layer_if_on_demand(
        &mut self,
        surface: Option<WlSurface>,
        serial: smithay::utils::Serial,
    ) {
        use smithay::wayland::compositor::get_parent;
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        // The pointer's focus may be a subsurface; resolve to the root surface
        // that the layer is keyed by.
        let surface = surface.map(|mut s| {
            while let Some(parent) = get_parent(&s) {
                s = parent;
            }
            s
        });

        if let Some(surface) = surface
            && self.layer_interactivity(&surface) == Some(KeyboardInteractivity::OnDemand)
        {
            if self.on_demand_layer.as_ref() != Some(&surface) {
                self.on_demand_layer = Some(surface);
                self.update_keyboard_focus(serial);
            }
            return;
        }

        if self.on_demand_layer.take().is_some() {
            self.update_keyboard_focus(serial);
        }
    }

    /// Single point where keyboard focus is applied.
    pub fn set_keyboard_focus(
        &mut self,
        target: Option<FocusTarget>,
        serial: smithay::utils::Serial,
    ) {
        let keyboard = self.seat.get_keyboard().unwrap();

        if self.config.remember_layout_per_window {
            let old = keyboard.current_focus();
            let focus_changing = old.as_ref().map(|f| &f.0) != target.as_ref().map(|f| &f.0);
            if focus_changing {
                self.remember_window_layout(&keyboard, old.as_ref(), target.as_ref());
            }
        }

        keyboard.set_focus(self, target, serial);
    }

    /// Save the active layout on the outgoing window, restore the incoming one's.
    /// Unfocuses before swapping so the outgoing client never sees the layout change.
    fn remember_window_layout(
        &mut self,
        keyboard: &smithay::input::keyboard::KeyboardHandle<Self>,
        old: Option<&FocusTarget>,
        new: Option<&FocusTarget>,
    ) {
        use smithay::input::keyboard::Layout;
        use smithay::utils::IsAlive;
        use smithay::wayland::compositor::with_states;
        use std::cell::Cell;

        let current =
            keyboard.with_xkb_state(self, |ctx| ctx.xkb().lock().unwrap().active_layout());

        if let Some(old) = old
            && old.0.alive()
        {
            with_states(&old.0, |states| {
                states
                    .data_map
                    .get_or_insert::<Cell<Layout>, _>(Cell::default)
                    .set(current)
            });
        }

        let Some(new) = new else { return };
        let saved = with_states(&new.0, |states| {
            states
                .data_map
                .get_or_insert::<Cell<Layout>, _>(Cell::default)
                .get()
        });

        let layout_count =
            keyboard.with_xkb_state(self, |ctx| ctx.xkb().lock().unwrap().layouts().count());
        if saved == current || saved.0 as usize >= layout_count {
            return;
        }

        keyboard.set_focus(self, None, smithay::utils::SERIAL_COUNTER.next_serial());
        let name = keyboard.with_xkb_state(self, |mut ctx| {
            ctx.set_layout(saved);
            let xkb = ctx.xkb().lock().unwrap();
            xkb.layout_name(xkb.active_layout()).to_owned()
        });
        self.active_layout = name;
    }

    pub fn mark_all_dirty(&mut self) {
        self.redraws_needed.clone_from(&self.active_outputs);
    }

    /// Mark every output displaying `surface` (or its root toplevel / hosting
    /// layer / lock output) as needing a redraw. Falls back to
    /// [`Self::mark_all_dirty`] when the surface can't be resolved — covers
    /// DnD icons, orphan popups, and pre-mapping toplevels.
    pub fn mark_dirty_for_surface(&mut self, surface: &WlSurface) {
        use smithay::desktop::{WindowSurfaceType, layer_map_for_output};
        use smithay::wayland::compositor::get_parent;

        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        let outputs: Vec<Output> = self.space.outputs().cloned().collect();

        if let Some(window) = self
            .space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(&root))
            .cloned()
            && let Some(win_bbox) = self.space.element_bbox(&window)
        {
            // Use zoom-aware visible canvas rect rather than
            // `Space::outputs_for_element`: the latter is built on the cached
            // mode-sized output geometry, which undercounts at zoom < 1.
            // bbox (not geometry) ensures popups extending past the toplevel
            // still damage the right outputs — matches smithay's refresh semantics.
            // Inflate by SSD chrome (shadow + title bar) so a window whose body
            // is off-screen but whose shadow/title-bar sliver still shows marks
            // that output. A resolved window always returns: when visible on no
            // output it marks nothing, rather than falling through to the
            // `mark_all_dirty` path and redrawing every output for it.
            let margin = self.config.decorations.title_bar_height
                + driftwm::config::DecorationConfig::SHADOW_RADIUS.ceil() as i32;
            let mut chrome_bbox = win_bbox;
            chrome_bbox.loc.x -= margin;
            chrome_bbox.loc.y -= margin;
            chrome_bbox.size.w += 2 * margin;
            chrome_bbox.size.h += 2 * margin;
            for output in &outputs {
                let (cam, zoom) = {
                    let os = output_state(output);
                    (os.camera.to_i32_round(), os.zoom)
                };
                let viewport = output_logical_size(output);
                let visible = driftwm::canvas::visible_canvas_rect(cam, viewport, zoom);
                if visible.overlaps(chrome_bbox) {
                    self.redraws_needed.insert(output.clone());
                }
            }
            return;
        }

        // Canvas-positioned layer widgets aren't in any LayerMap; resolve them
        // against each output's visible canvas rect like windows, so a widget
        // commit redraws only the outputs showing it, not every output.
        let widget_bbox = self
            .canvas_layers
            .iter()
            .find(|cl| cl.surface.wl_surface() == &root)
            .and_then(|cl| {
                let pos = cl.position?;
                let mut bbox = cl.surface.bbox();
                bbox.loc += pos;
                Some(bbox)
            });
        if let Some(widget_bbox) = widget_bbox {
            for output in &outputs {
                let (cam, zoom) = {
                    let os = output_state(output);
                    (os.camera.to_i32_round(), os.zoom)
                };
                let viewport = output_logical_size(output);
                let visible = driftwm::canvas::visible_canvas_rect(cam, viewport, zoom);
                if visible.overlaps(widget_bbox) {
                    self.redraws_needed.insert(output.clone());
                }
            }
            return;
        }

        for output in &outputs {
            let hit = layer_map_for_output(output)
                .layer_for_surface(&root, WindowSurfaceType::ALL)
                .is_some();
            if hit {
                self.redraws_needed.insert(output.clone());
                return;
            }
        }

        if let Some(output) = self
            .lock_surfaces
            .iter()
            .find(|(_, ls)| ls.wl_surface() == &root)
            .map(|(o, _)| o.clone())
        {
            self.redraws_needed.insert(output);
            return;
        }

        self.mark_all_dirty();
    }

    pub fn cursor_is_animated(&self) -> bool {
        self.cursor.is_animated()
    }

    pub fn output_has_active_animations(&self, output: &Output) -> bool {
        let os = output_state(output);
        os.camera_target.is_some()
            || os.zoom_target.is_some()
            || os.edge_pan_velocity.is_some()
            || os.momentum.velocity.x != 0.0
            || os.momentum.velocity.y != 0.0
    }

    pub fn has_active_animations(&self) -> bool {
        self.space
            .outputs()
            .any(|o| self.output_has_active_animations(o))
            || self.held_action.is_some()
            || self.cursor.exec_cursor_show_at.is_some()
            || self.cursor.exec_cursor_deadline.is_some()
            || self.cursor.is_animated()
    }

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

    /// Called by the calloop timer when no swipe followed the 3-finger tap.
    pub fn flush_pending_middle_click(&mut self) {
        let Some(pending) = self.pending_middle_click.take() else {
            return;
        };
        self.flush_middle_click(pending.press_time, pending.release_time);
    }

    /// The output the pointer is currently on; falls back to the first output.
    pub fn active_output(&self) -> Option<Output> {
        self.focused_output
            .clone()
            .or_else(|| self.space.outputs().next().cloned())
    }

    pub fn active_fullscreen(&self) -> Option<&FullscreenState> {
        self.active_output().and_then(|o| self.fullscreen.get(&o))
    }

    pub fn is_fullscreen(&self) -> bool {
        self.active_output()
            .is_some_and(|o| self.fullscreen.contains_key(&o))
    }

    pub fn is_output_fullscreen(&self, output: &Output) -> bool {
        self.fullscreen.contains_key(output)
    }

    /// Output whose viewport contains the window's center, or the active
    /// output if the window isn't visible on any.
    pub fn output_for_window(&self, window: &smithay::desktop::Window) -> Option<Output> {
        // A pinned window is fixed to one output regardless of canvas geometry.
        if let Some(p) = window.wl_surface().and_then(|s| self.pinned.get(&s.id())) {
            return Some(p.output.clone());
        }
        let loc = self.space.element_location(window)?;
        let geo = window.geometry();
        let center: Point<f64, Logical> = Point::from((
            loc.x as f64 + geo.size.w as f64 / 2.0,
            loc.y as f64 + geo.size.h as f64 / 2.0,
        ));
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

    /// True if `window` is pinned to an output's screen space.
    pub fn is_pinned(&self, window: &Window) -> bool {
        window
            .wl_surface()
            .is_some_and(|s| self.pinned.contains_key(&s.id()))
    }

    /// True if `window` is a real canvas window — neither a widget (wallpaper
    /// layer, immovable) nor screen-pinned. The eligibility test for canvas
    /// operations: navigation, centering, fitting, snapping, zoom-to-fit, etc.
    pub fn is_canvas_window(&self, window: &Window) -> bool {
        !window.is_widget() && !self.is_pinned(window)
    }

    /// Effective render transform for `window` in one pass: the pre-zoom,
    /// output-relative logical origin of its surface tree (geometry top-left
    /// minus `geometry().loc`) and the scale to render at. The single
    /// canvas↔screen chokepoint — every render/capture consumer routes through
    /// it so a pinned window is decided once, not re-inlined per site.
    ///
    /// - Normal window: `loc - geom_loc - camera`, scaled by `zoom`.
    /// - Pinned window on its output: `screen_pos - geom_loc`, scale `1.0`
    ///   (identity — no camera, no zoom).
    /// - Pinned window on any other output: `None` (don't render here).
    /// - `output = None` (off-screen canvas capture): pinned → `None` by
    ///   construction, so captures never include screen-pinned windows.
    pub fn window_render_transform(
        &self,
        window: &Window,
        output: Option<&Output>,
        camera: Point<f64, Logical>,
        zoom: f64,
    ) -> Option<(Point<f64, Logical>, f64)> {
        let loc = self.space.element_location(window)?;
        let geom_loc = window.geometry().loc;
        // A fullscreen window is visible only on its own output. For any other
        // output — and for the off-screen capture pass (`output == None`) — it
        // must not render: it keeps a real canvas coord at its output's
        // camera origin, so another monitor's camera would otherwise pan over
        // it. On its own output it falls through to the canvas branch below,
        // which yields (0,0) at zoom 1 thanks to the camera-park.
        if let Some(fs_output) = window
            .wl_surface()
            .and_then(|s| self.find_fullscreen_output_for_surface(&s))
            && output != Some(&fs_output)
        {
            return None;
        }
        let pinned = window.wl_surface().and_then(|s| {
            self.pinned
                .get(&s.id())
                .map(|p| (p.output.clone(), p.screen_pos))
        });
        match pinned {
            Some((pin_output, screen_pos)) => match output {
                Some(o) if *o == pin_output => Some((
                    Point::from((
                        screen_pos.x as f64 - geom_loc.x as f64,
                        screen_pos.y as f64 - geom_loc.y as f64,
                    )),
                    1.0,
                )),
                _ => None,
            },
            None => Some((
                Point::from((
                    loc.x as f64 - geom_loc.x as f64 - camera.x,
                    loc.y as f64 - geom_loc.y as f64 - camera.y,
                )),
                zoom,
            )),
        }
    }

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
                // dot > 0.5 ≈ alignment within ~60° of `dir`.
                let dot = (to_x * dx + to_y * dy) / dist;
                if dot > 0.5 {
                    Some((o.clone(), dist))
                } else {
                    None
                }
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(o, _)| o)
    }

    /// Output whose layout rectangle contains `pos`. Uses `layout_position` +
    /// mode size (NOT `space.output_geometry()`, which is zoom-cached).
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

    /// layout_pos = (canvas - camera) * zoom + layout_position.
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

    /// canvas = (layout_pos - layout_position) / zoom + camera.
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

    /// Batch-access per-output state under a single mutex lock. Returns
    /// `None` (skipping `f`) when there's no active output — per-output state
    /// has no meaning then. Value-returning callers should provide a fallback
    /// (e.g. `unwrap_or(1.0)` for zoom).
    pub fn with_output_state<R>(&mut self, f: impl FnOnce(&mut OutputState) -> R) -> Option<R> {
        let output = self.active_output()?;
        let mut guard = output_state(&output);
        Some(f(&mut guard))
    }

    // Per-output field accessors. Getters fall back to a sensible default
    // when no output exists; setters silently no-op. Hotplug/lid-close races
    // briefly leave the compositor with zero outputs — must not panic then.
    pub fn camera(&self) -> Point<f64, Logical> {
        self.active_output()
            .map(|o| output_state(&o).camera)
            .unwrap_or_default()
    }
    pub fn set_camera(&mut self, val: Point<f64, Logical>) {
        if let Some(o) = self.active_output() {
            // The fullscreen window is pinned to this output's camera-origin at
            // zoom 1; moving the camera would slide it off (0,0) and re-expose
            // it to other cameras. Every nav path already exits fullscreen
            // first, so a set_camera here means one slipped through — refuse it.
            // (`enter_fullscreen` snaps via output_state directly to bypass this.)
            if self.fullscreen.contains_key(&o) {
                return;
            }
            output_state(&o).camera = val;
        }
    }
    pub fn zoom(&self) -> f64 {
        // 1.0 default (not 0.0) avoids divide-by-zero in `step / zoom` callers.
        self.active_output()
            .map(|o| output_state(&o).zoom)
            .unwrap_or(1.0)
    }
    pub fn set_zoom(&mut self, val: f64) {
        if let Some(o) = self.active_output() {
            output_state(&o).zoom = val;
        }
    }
    pub fn zoom_target(&self) -> Option<f64> {
        self.active_output()
            .and_then(|o| output_state(&o).zoom_target)
    }
    pub fn set_zoom_target(&mut self, val: Option<f64>) {
        if let Some(o) = self.active_output() {
            output_state(&o).zoom_target = val;
        }
    }
    pub fn zoom_animation_center(&self) -> Option<Point<f64, Logical>> {
        self.active_output()
            .and_then(|o| output_state(&o).zoom_animation_center)
    }
    pub fn set_zoom_animation_center(&mut self, val: Option<Point<f64, Logical>>) {
        if let Some(o) = self.active_output() {
            output_state(&o).zoom_animation_center = val;
        }
    }
    pub fn overview_return(&self) -> Option<(Point<f64, Logical>, f64)> {
        self.active_output()
            .and_then(|o| output_state(&o).overview_return)
    }
    pub fn set_overview_return(&mut self, val: Option<(Point<f64, Logical>, f64)>) {
        if let Some(o) = self.active_output() {
            output_state(&o).overview_return = val;
        }
    }
    pub fn camera_target(&self) -> Option<Point<f64, Logical>> {
        self.active_output()
            .and_then(|o| output_state(&o).camera_target)
    }
    pub fn set_camera_target(&mut self, val: Option<Point<f64, Logical>>) {
        if let Some(o) = self.active_output() {
            output_state(&o).camera_target = val;
        }
    }
    pub fn last_scroll_pan(&self) -> Option<Instant> {
        self.active_output()
            .and_then(|o| output_state(&o).last_scroll_pan)
    }
    pub fn set_last_scroll_pan(&mut self, val: Option<Instant>) {
        if let Some(o) = self.active_output() {
            output_state(&o).last_scroll_pan = val;
        }
    }
    pub fn panning(&self) -> bool {
        self.active_output()
            .is_some_and(|o| output_state(&o).panning)
    }
    pub fn set_panning(&mut self, val: bool) {
        if let Some(o) = self.active_output() {
            output_state(&o).panning = val;
        }
    }
    pub fn edge_pan_velocity(&self) -> Option<Point<f64, Logical>> {
        self.active_output()
            .and_then(|o| output_state(&o).edge_pan_velocity)
    }
    pub fn last_frame_instant(&self) -> Instant {
        self.active_output()
            .map(|o| output_state(&o).last_frame_instant)
            .unwrap_or_else(Instant::now)
    }
    pub fn set_last_frame_instant(&mut self, val: Instant) {
        if let Some(o) = self.active_output() {
            output_state(&o).last_frame_instant = val;
        }
    }

    /// Sync each output's position to its camera so render_output
    /// applies the canvas→screen transform.
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
            self.sync_pinned_locs();
        }
    }

    /// Re-anchor each pinned window's `Space` location to the canvas point its
    /// fixed `screen_pos` currently maps to. Without this the loc freezes at
    /// placement and `Space::refresh` drifts it off its output as the camera
    /// pans — triggering spurious `output_leave` and the visibility culls, which
    /// would freeze the pinned window at 0 FPS. Re-mapped bottom-to-top in the
    /// current z-order because `Space::map_element` raises the element to the
    /// top of its z-class, so order-preserving re-map keeps multi-pinned
    /// stacking and focus-raise intact. Only the canvas loc is touched —
    /// rendering and hit-testing still read `screen_pos`.
    fn sync_pinned_locs(&mut self) {
        if self.pinned.is_empty() {
            return;
        }
        let pinned_windows: Vec<Window> = self
            .space
            .elements()
            .filter(|w| self.is_pinned(w))
            .cloned()
            .collect();
        for window in pinned_windows {
            let Some(id) = window.wl_surface().map(|s| s.id()) else {
                continue;
            };
            let Some(p) = self.pinned.get(&id) else {
                continue;
            };
            let screen_pos = p.screen_pos;
            let (camera, zoom) = {
                let os = output_state(&p.output);
                (os.camera, os.zoom)
            };
            let canvas = driftwm::canvas::screen_to_canvas(
                driftwm::canvas::ScreenPos(screen_pos.to_f64()),
                camera,
                zoom,
            )
            .0
            .to_i32_round();
            self.space.map_element(window, canvas, false);
        }
    }

    /// Reassign every pinned window whose output is no longer a live space
    /// output (it was unplugged) to `to`, clamping `screen_pos` into the new
    /// output's bounds. Covers both the multi-output unplug (output already
    /// unmapped) and the last-output reconnection (virtual placeholder swapped
    /// for the new monitor).
    pub fn reassign_orphaned_pinned(&mut self, to: &Output) {
        if self.pinned.is_empty() {
            return;
        }
        // Few outputs; a Vec + linear scan avoids the mutable-key-type lint
        // (Output wraps an Arc with interior mutability).
        let live: Vec<Output> = self.space.outputs().cloned().collect();
        let to_size = output_logical_size(to);
        let ids: Vec<_> = self
            .pinned
            .iter()
            .filter(|(_, p)| !live.contains(&p.output))
            .map(|(id, _)| id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        for id in ids {
            let win_size = self
                .space
                .elements()
                .find(|w| w.wl_surface().is_some_and(|s| s.id() == id))
                .map(|w| w.geometry().size)
                .unwrap_or_default();
            if let Some(p) = self.pinned.get_mut(&id) {
                p.output = to.clone();
                p.screen_pos.x = p.screen_pos.x.clamp(0, (to_size.w - win_size.w).max(0));
                p.screen_pos.y = p.screen_pos.y.clamp(0, (to_size.h - win_size.h).max(0));
            }
        }
        // Re-anchor the Space loc to the new output now — `sync_pinned_locs`
        // only fires on camera changes, which a hotplug doesn't guarantee, so
        // without this the reassigned window keeps its stale (off the new
        // output) canvas loc and gets culled until the next pan.
        self.sync_pinned_locs();
    }

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

    /// Screen-space center of the usable area (= viewport center when no panels exist).
    pub fn usable_center_screen(&self) -> Point<f64, Logical> {
        let usable = self.get_usable_area();
        Point::from((
            usable.loc.x as f64 + usable.size.w as f64 / 2.0,
            usable.loc.y as f64 + usable.size.h as f64 / 2.0,
        ))
    }

    pub fn viewport_center_canvas(&self) -> Point<f64, Logical> {
        let vc = self.usable_center_screen();
        let camera = self.camera();
        let zoom = self.zoom();
        Point::from((camera.x + vc.x / zoom, camera.y + vc.y / zoom))
    }

    /// Keyboard-focused window. Does not filter widgets — pair with
    /// `.filter(|w| !w.is_widget())` if needed.
    pub fn focused_window(&self) -> Option<Window> {
        let keyboard = self.seat.get_keyboard()?;
        let focus = keyboard.current_focus()?;
        self.space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(&focus.0))
            .cloned()
    }

    pub fn window_ssd_bar(&self, window: &Window) -> i32 {
        window
            .wl_surface()
            .filter(|s| self.decorations.contains_key(&s.id()))
            .map_or(0, |_| self.config.decorations.title_bar_height)
    }

    /// Recompute `decoration_scale` from current outputs. Call after output
    /// add/remove/scale change so SSD buffers re-render at the right density.
    pub fn recompute_decoration_scale(&mut self) {
        let max_scale = self
            .space
            .outputs()
            .map(|o| o.current_scale().fractional_scale())
            .fold(1.0_f64, f64::max);
        self.decoration_scale = max_scale.ceil() as i32;
    }

    /// Per-window border width, resolving rule override against
    /// `[decorations] border_width`. Returns 0 when the effective decoration
    /// mode is `None` (hard veto — per-window overrides ignored).
    pub fn window_border_width(&self, surface: &WlSurface) -> i32 {
        let applied = driftwm::config::applied_rule(surface);
        let mode = driftwm::config::effective_decoration_mode(
            applied.as_ref().and_then(|r| r.decoration.as_ref()),
            &self.config.decorations.default_mode,
        );
        driftwm::config::effective_border_width(applied.as_ref(), mode, &self.config.decorations)
    }

    /// Visual center accounting for SSD title bar above content.
    pub fn window_visual_center(&self, window: &Window) -> Option<Point<f64, Logical>> {
        let loc = self.space.element_location(window)?;
        let size = window.geometry().size;
        let bar = self.window_ssd_bar(window) as f64;
        Some(Point::from((
            loc.x as f64 + size.w as f64 / 2.0,
            loc.y as f64 - bar + (size.h as f64 + bar) / 2.0,
        )))
    }

    /// True if at least `threshold` of the window's area is inside the viewport.
    pub fn window_visible_at_least(&self, window: &Window, threshold: f64) -> bool {
        let Some(loc) = self.space.element_location(window) else {
            return false;
        };
        driftwm::canvas::visible_fraction(
            loc,
            window.geometry().size,
            self.camera(),
            self.get_viewport_size(),
            self.zoom(),
        ) >= threshold
    }

    /// Spawn pos for `placement = "cursor"`: center the visual frame
    /// (titlebar + content) on the cursor, clamped to the active output's
    /// usable rect. `bar` is SSD title-bar height (0 for CSD/minimal).
    pub fn cursor_placement_pos(
        &self,
        window_size: Size<i32, Logical>,
        bar: i32,
    ) -> Option<(i32, i32)> {
        self.active_output()?;

        let pointer = self.seat.get_pointer()?;
        let cursor = pointer.current_location();

        // usable area is screen-local; convert to canvas coords.
        let usable = self.get_usable_area();
        let zoom = self.zoom();
        let camera = self.camera();
        let cx_min = camera.x + usable.loc.x as f64 / zoom;
        let cy_min = camera.y + usable.loc.y as f64 / zoom;
        let cx_max = camera.x + (usable.loc.x + usable.size.w) as f64 / zoom;
        let cy_max = camera.y + (usable.loc.y + usable.size.h) as f64 / zoom;

        // Target: visual frame center on cursor. Frame spans [loc.y - bar, loc.y + h],
        // so frame center = loc.y + (h - bar)/2  →  loc.y = cursor.y - h/2 + bar/2.
        let bar_f = bar as f64;
        let raw_x = cursor.x - window_size.w as f64 / 2.0;
        let raw_y = cursor.y - window_size.h as f64 / 2.0 + bar_f / 2.0;

        // Clamp so the frame stays fully inside the usable canvas rect.
        // For oversized windows, .max() keeps the upper bound >= lower bound
        // (the top sticks at the usable edge; the bottom overflows).
        let max_x = (cx_max - window_size.w as f64).max(cx_min);
        let max_y = (cy_max - window_size.h as f64).max(cy_min + bar_f);
        let x = raw_x.clamp(cx_min, max_x);
        let y = raw_y.clamp(cy_min + bar_f, max_y);

        Some((x.round() as i32, y.round() as i32))
    }

    /// Spawn pos for `placement = "auto"`: snap-place adjacent to the focused
    /// window's cluster. Returns content top-left (shifted down by `bar` so
    /// the visual frame snaps to the neighbor). `None` on no eligible focus
    /// or no valid placement; caller falls back to center.
    ///
    /// `new_window` is excluded from anchor search and obstacle list. Without
    /// the skip we'd anchor the new window against itself, since by the time
    /// this runs `new_window` is already at the viewport center and front of
    /// `focus_history`.
    pub fn auto_placement_pos(
        &self,
        new_window: &Window,
        new_size: Size<i32, Logical>,
        bar: i32,
    ) -> Option<(i32, i32)> {
        // Anchor = keyboard focus at `new_toplevel` time, snapshotted before
        // focus was reassigned to the new surface. `None` (or absent) means
        // no anchor and caller falls back to center.
        let new_surface = new_window.wl_surface()?.into_owned();
        let focused = self.auto_anchor_snapshot.get(&new_surface)?.as_ref()?;
        let widget = focused
            .wl_surface()
            .and_then(|s| driftwm::config::applied_rule(&s))
            .is_some_and(|r| r.widget);
        let is_fs = self.fullscreen.values().any(|fs| &fs.window == focused);
        if widget || is_fs || self.is_pinned(focused) {
            return None;
        }

        // Only anchor when enough of the focused window is visible that the
        // user is plausibly working on its cluster; otherwise they intend a
        // fresh cluster and caller falls back to center.
        if !self.window_visible_at_least(focused, AUTO_PLACE_CLUSTER_THRESHOLD) {
            return None;
        }

        // Widgets sit visually below windows (wallpaper-like) — neither
        // anchors nor obstacles for auto placement.
        let mut rects: Vec<driftwm::layout::auto_placement::Rect> = Vec::new();
        let mut eligible: HashSet<usize> = HashSet::new();
        let mut focused_idx: Option<usize> = None;
        for w in self.space.elements() {
            if w == new_window {
                continue;
            }
            let widget = w
                .wl_surface()
                .and_then(|s| driftwm::config::applied_rule(&s))
                .is_some_and(|r| r.widget);
            let is_fs = self.fullscreen.values().any(|fs| &fs.window == w);
            if widget || is_fs || self.is_pinned(w) {
                continue;
            }
            let Some(loc) = self.space.element_location(w) else {
                continue;
            };
            let size = w.geometry().size;
            let b = self.window_ssd_bar(w);
            let bw = w.wl_surface().map_or(0, |s| self.window_border_width(&s)) as f64;
            let idx = rects.len();
            rects.push(driftwm::layout::auto_placement::Rect {
                x: loc.x as f64 - bw,
                y: (loc.y - b) as f64 - bw,
                w: size.w as f64 + 2.0 * bw,
                h: (size.h + b) as f64 + 2.0 * bw,
            });
            eligible.insert(idx);
            if w == focused {
                focused_idx = Some(idx);
            }
        }
        let focused_idx = focused_idx?;

        let new_bw = self.window_border_width(&new_surface) as f64;
        let new_w_f = new_size.w as f64 + 2.0 * new_bw;
        let new_h_f = (new_size.h + bar) as f64 + 2.0 * new_bw;

        let camera = self.camera();
        let zoom = self.zoom();
        let vc_screen = self.usable_center_screen();
        let vc = (camera.x + vc_screen.x / zoom, camera.y + vc_screen.y / zoom);

        let pos = driftwm::layout::auto_placement::place_auto(
            &rects,
            focused_idx,
            &eligible,
            new_w_f,
            new_h_f,
            vc,
            self.config.snap_gap,
        )?;

        // place_auto returns frame top-left (outside border, above title bar);
        // shift inward to content top-left.
        let bw_i = new_bw as i32;
        Some((
            pos.0.round() as i32 + bw_i,
            pos.1.round() as i32 + bw_i + bar,
        ))
    }

    /// Walk a spawn position in title-bar-sized diagonal steps until it
    /// doesn't sit on top of an existing window.
    pub fn cascade_position(&self, mut pos: (i32, i32), skip: &Window) -> (i32, i32) {
        let step = self.config.decorations.title_bar_height;
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

    pub fn load_xcursor(&mut self, name: &str) -> Option<&CursorFrames> {
        let theme = self.config.cursor_theme.as_deref().unwrap_or("default");
        let size = self.config.cursor_size.unwrap_or(24);
        self.cursor.load_xcursor(name, theme, size)
    }
}

/// Order in which to raise `target` and its descendants so each child ends up
/// directly above its own parent: `target` first, then descendants breadth-first,
/// leaving unrelated windows below the subtree untouched. `is_child(a, b)` reports
/// whether `a`'s parent is `b`. Already-visited windows are skipped, so cyclic
/// parent links still terminate.
fn subtree_raise_order<T>(stack: &[T], target: &T, is_child: impl Fn(&T, &T) -> bool) -> Vec<T>
where
    T: Clone + PartialEq,
{
    let mut order = vec![target.clone()];
    let mut i = 0;
    while i < order.len() {
        let parent = order[i].clone();
        for w in stack {
            if !order.contains(w) && is_child(w, &parent) {
                order.push(w.clone());
            }
        }
        i += 1;
    }
    order
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

    #[derive(Clone, PartialEq, Debug)]
    struct StackWin {
        id: u32,
        parent: Option<u32>,
    }

    fn win(id: u32, parent: Option<u32>) -> StackWin {
        StackWin { id, parent }
    }

    fn raise(stack: &[StackWin], target: u32) -> Vec<u32> {
        let target = stack.iter().find(|w| w.id == target).unwrap();
        subtree_raise_order(stack, target, |c, p| c.parent == Some(p.id))
            .iter()
            .map(|w| w.id)
            .collect()
    }

    #[test]
    fn raise_lifts_only_own_children() {
        // 0 has child 1; 2 is unrelated.
        let stack = [win(0, None), win(1, Some(0)), win(2, None)];
        assert_eq!(raise(&stack, 0), vec![0, 1]);
        assert_eq!(raise(&stack, 2), vec![2]);
    }

    #[test]
    fn raise_follows_nested_modal_chain() {
        // 0 -> 1 -> 2 (dialog of a dialog), plus unrelated 3.
        let stack = [win(0, None), win(1, Some(0)), win(2, Some(1)), win(3, None)];
        assert_eq!(raise(&stack, 0), vec![0, 1, 2]);
    }

    #[test]
    fn raise_terminates_on_cyclic_parents() {
        // 0 and 1 claim each other as parent; must not loop forever.
        let stack = [win(0, Some(1)), win(1, Some(0))];
        assert_eq!(raise(&stack, 0), vec![0, 1]);
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
