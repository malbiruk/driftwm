//! The [`DriftWm`] struct — every field of compositor state — together with the
//! ancillary types those fields are built from and the whole-state routines
//! that have to enumerate them ([`DriftWm::debug_counters`],
//! [`DriftWm::verify_stage_invariants`]).
//!
//! Behaviour does not belong here. An `impl DriftWm` method goes beside the
//! subsystem it serves — `output.rs`, `keyboard_focus.rs`, `redraw.rs`,
//! `window_lifecycle.rs`, … — leaving this file the state declarations and the
//! routines that must name every field. Nothing said so before, and the file
//! grew past 3,000 lines of methods that each had a better home.

mod activation;
mod cluster_snapshot;
mod cursor;
mod edge_pan;
mod errors;
pub mod fill;
pub mod fit;
mod focus;
mod fullscreen;
mod init;
mod keyboard_focus;
mod layers;
mod membership;
mod navigation;
mod output;
pub mod persistence;
mod pinned;
pub(crate) use pinned::clamp_pin_frame;
mod placement;
mod recenter;
mod redraw;
mod reload;
mod render_cache;
mod resize;
pub(crate) mod session_lock;
mod session_store;
mod stage_window;
mod suspended;
mod viewport;
mod viewport_animation;
pub(crate) mod window_animation;
mod window_animation_driver;
mod window_frame;
mod window_lifecycle;
pub use cluster_snapshot::{ClusterMember, ClusterResizeSnapshot};
pub use cursor::{CursorFrames, CursorState};
use edge_pan::EdgePanDelay;
pub use errors::ErrorSource;
pub use focus::{FocusIntent, FocusTarget};
pub use layers::CanvasLayer;
pub(crate) use navigation::CLICK_NAVIGATE_SLOP;
pub use navigation::NavZoom;
pub use persistence::{read_all_per_output_state, remove_state_file};
pub use render_cache::{BorderCacheEntry, RenderCache, ShadowCacheEntry};
pub(crate) use resize::{owes_a_configured_size, resize_constraints};
pub use session_store::{CameraSeed, SessionStore};
pub use stage_window::{StageWindow, SuspendedId, SuspendedWindow};
pub use suspended::{
    AdoptOrigin, DeferredAdopt, PendingRelaunch, RelaunchMarker, RevealCause, SuspendMark,
    UnmapSnapshot,
};
pub(crate) use window_frame::{configured_window_size, frame_loc_for_center, visual_frame_center};

// Kept out of the group above so fixture scenarios can name the interval a
// window mutation debounces on without widening it for production.
#[cfg(test)]
pub(crate) use session_store::WRITE_DEBOUNCE;

use smithay::{
    desktop::{PopupGrab, PopupManager, Space, Window},
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
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::Instant;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesTexture;
use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;
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
use driftwm::config::{Config, HotCorner};
use driftwm::window_ext::WindowExt;

/// Min visible fraction an element needs before auto-placement will anchor a
/// new window to its cluster — of the focused window, and of every candidate
/// the fallback picker considers. Lower than the navigation/activation thresholds:
/// even a small sliver of the cluster on-screen is a stronger signal than the
/// alternative (dropping the new window in the middle of an unrelated region).
const AUTO_PLACE_CLUSTER_THRESHOLD: f64 = 0.33;

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

/// A click armed for auto-navigate: a normal window was pressed with
/// `auto_navigate_on_click` enabled. On release — if the pointer barely moved
/// and the window is still clipped — the camera pans to it like activation does.
pub struct PendingClickNavigate {
    pub window: Window,
    /// Press position in screen space, so the click/drag slop is measured in
    /// physical pixels regardless of zoom.
    pub press_screen_pos: Point<f64, Logical>,
    pub button: u32,
    /// Active output at press time. `canvas_to_screen` and the pan both follow
    /// the active output, so a release on a different output would compare
    /// incompatible screen coords — resolve drops the pending instead.
    pub output: Output,
    /// Wait out the double-click window before panning. Only the SSD title bar
    /// hosts the compositor's own double-click (fit), so its pan must defer —
    /// otherwise release #1 slides the window out from under click #2. Content
    /// clicks pan immediately: protecting a *client's* double-click isn't the
    /// compositor's job, and a deferral there would put a dead beat on every
    /// click.
    pub defer: bool,
}

/// What a pick-mode press landed on: a canvas client window or a suspended
/// stand-in. Below `zoom_interact_min` both are one uniform click/drag target.
#[derive(Clone)]
pub enum PickTarget {
    Client(Window),
    Suspended(SuspendedId),
}

/// A left click armed in pick mode (below `zoom_interact_min`). On release
/// within the slop it centers the target; a drag past the slop promotes to a
/// move (and cancels this). Mirrors `PendingClickNavigate` — press screen
/// coords and output make the slop comparison zoom- and output-safe.
pub struct PendingPick {
    pub target: PickTarget,
    pub press_screen_pos: Point<f64, Logical>,
    pub button: u32,
    pub output: Output,
}

/// Session lock state machine: Unlocked → Pending → Locked → Unlocked.
///
/// `Pending` waits for all lock surfaces to commit their first buffer, with a
/// 1-second deadline after which we force-enter `Locked` regardless. Locking an
/// unlocked session leaves the desktop up for that wait so there is no blank
/// flash; `keep_lock_frames` covers the cases where that would leak unlocked
/// content instead (see its own doc).
/// `Locked` renders lock frames and defers the `locked` protocol event until
/// every output has presented one.
pub enum SessionLock {
    Unlocked,
    /// Lock granted; the lock surfaces have yet to commit.
    Pending {
        locker: SessionLocker,
        /// Outputs whose lock surface has committed its first buffer.
        ready_outputs: HashSet<Output>,
        /// Keep painting lock frames for the wait instead of the desktop.
        /// Set wherever letting the desktop through would put unlocked content
        /// on a screen that must not show it: a takeover of a lock already in
        /// place, a `Pending` no deadline bounds, and a lock whose wait a
        /// DPMS-off panel overlaps.
        keep_lock_frames: bool,
        /// Deadline after which `Locked` is forced even if not all surfaces
        /// mapped; see `PENDING_LOCK_DEADLINE`.
        deadline_token: Option<RegistrationToken>,
    },
    /// Rendering only the lock surface. Carries the client's lock object partly
    /// so a later lock request can tell a live locker (which it must not
    /// displace) from one whose client died (which it may).
    ///
    /// Entering this state is *not* the same as telling the client the session
    /// is locked: the protocol forbids the `locked` event until a lock frame has
    /// been presented on every output, so the locker is held here until the
    /// outputs report in.
    Locked {
        lock: ExtSessionLockV1,
        /// Held until every output owing a lock frame has presented one;
        /// consuming it sends `locked`. `None` once sent.
        pending_confirmation: Option<SessionLocker>,
        /// Outputs that still owe a presented lock frame.
        awaiting_present: HashSet<Output>,
    },
}

impl SessionLock {
    /// Whether the session is locked at all — pending and confirmed alike. Both
    /// blank the screen, so every input/navigation gate wants this, not a
    /// specific variant.
    pub fn is_locked(&self) -> bool {
        !matches!(self, SessionLock::Unlocked)
    }

    /// Whether the compositor is currently painting a lock frame rather than the
    /// desktop. Deliberately distinct from [`Self::is_locked`], which gates
    /// input: the two answer different questions.
    ///
    /// The renderer and the lock-frame bookkeeping must both read *this* — a
    /// disagreement about whether a given frame was a lock frame would make the
    /// confirmation meaningless. The two remaining render-path readers of
    /// `is_locked` (`src/render/cursor.rs` and the ghost-cursor alpha in
    /// `src/backend/udev.rs`) deliberately stay on it: they ask which coordinate
    /// space the pointer is in, which is settled when `Pending` is entered, not
    /// what the frame paints.
    pub fn renders_lock_frame(&self) -> bool {
        matches!(
            self,
            SessionLock::Locked { .. }
                | SessionLock::Pending {
                    keep_lock_frames: true,
                    ..
                }
        )
    }

    /// The lock object of the client that owns the session, pending or
    /// confirmed. Identifies the incumbent: whether it is still alive, and which
    /// client may put surfaces on the lock.
    pub fn incumbent(&self) -> Option<&ExtSessionLockV1> {
        match self {
            SessionLock::Unlocked => None,
            SessionLock::Pending { locker, .. } => Some(locker.ext_session_lock()),
            SessionLock::Locked { lock, .. } => Some(lock),
        }
    }
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

/// Pre-fullscreen viewport, restored on exit — the third saved-viewport slot
/// on [`OutputState`] alongside `home_return` and `overview_return`. The
/// membership and geometry half (window, saved location/size) lives on the
/// stage, which is the sole authority for "what is fullscreen where"; this is
/// only the restore payload.
#[derive(Clone)]
pub struct FullscreenReturn {
    pub camera: Point<f64, Logical>,
    pub zoom: f64,
    /// If the window was screen-pinned when it entered fullscreen, its pin
    /// site, re-inserted on exit so fullscreen is a transparent round-trip
    /// (pinned → fullscreen → pinned) rather than a permanent unpin.
    pub pinned: Option<driftwm::stage::PinnedSite>,
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

/// What mode the user (config or wlr-output-management client) asked for.
/// Resolved to a concrete `drm::control::Mode` in the udev backend.
#[derive(Clone, Debug, PartialEq)]
pub enum ModeIntent {
    /// Index into the connector's EDID-advertised modes list. Sent by
    /// wlr-output-management `SetMode` after the protocol layer chose a
    /// specific `ZwlrOutputModeV1`.
    EdidIndex(usize),
    /// Custom WxH@refresh_mHz. Tried as an exact EDID match first; if not
    /// found, a CVT modeline is synthesized.
    Custom { w: i32, h: i32, refresh_mhz: i32 },
    /// The connector's preferred mode. Queued by config reload when an
    /// output's rule is (or reverts to) "preferred"; the backend resolves it
    /// against the connector and skips the modeset when it's already active.
    Preferred,
    /// The highest-resolution mode (then highest refresh). Queued by config
    /// reload for a "max" rule; resolved against the connector, and the
    /// modeset is skipped when it's already active.
    Max,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoomAnimationAnchor {
    pub canvas: Point<f64, Logical>,
    pub screen: Point<f64, Logical>,
}

/// A view move that belongs to a window transition and must not start before the
/// window does. Fit pans the camera to centre the window it is resizing; starting
/// that while the window is still frozen on its pre-fit picture reads as two
/// motions instead of one.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingView {
    /// The output whose viewport this moves — resolved when the move was staged,
    /// not when it lands, since the pointer (and so the active output) can move
    /// while the window is frozen.
    pub output: String,
    pub camera: Point<f64, Logical>,
    pub zoom: f64,
    pub anchor: ZoomAnimationAnchor,
    /// The viewport as the staging action left it. Anything that moves the camera
    /// in the meantime — a pan gesture, momentum, a navigation action — takes
    /// ownership of the view, and this move is dropped rather than yanking the
    /// canvas back a third of a second later.
    pub staged_camera: Point<f64, Logical>,
    pub staged_zoom: f64,
}

/// Per-output viewport state, stored on each `Output` via `UserDataMap`
/// (wrapped in `Mutex` since `UserDataMap` requires `Sync`). !Send fields
/// and non-Copy ownership types (fullscreen, lock_surface) stay on DriftWm.
#[derive(Clone)]
pub struct OutputState {
    pub camera: Point<f64, Logical>,
    pub zoom: f64,
    pub zoom_target: Option<f64>,
    pub zoom_animation_anchor: Option<ZoomAnimationAnchor>,
    pub last_rendered_zoom: f64,
    pub overview_return: Option<(Point<f64, Logical>, f64)>,
    pub camera_target: Option<Point<f64, Logical>>,
    pub last_scroll_pan: Option<Instant>,
    pub momentum: MomentumState,
    pub panning: bool,
    pub edge_pan_velocity: Option<Point<f64, Logical>>,
    /// Cursor/finger position in this output's screen-local coordinates for
    /// the current edge-pan request. Stored with the velocity so latency can
    /// be scoped to the exact shared segment of a partially adjacent output.
    pub edge_pan_screen_pos: Option<Point<f64, Logical>>,
    /// Monitor-facing velocity components currently waiting for the edge-pan
    /// latency. Kept per-output so the render tick can finish the wait even
    /// when no new pointer motion arrives.
    pub edge_pan_delay: Option<EdgePanDelay>,
    pub last_rendered_camera: Point<f64, Logical>,
    pub last_frame_instant: Instant,
    /// Physical arrangement in layout space: (0,0) for single output,
    /// from config for multi-monitor.
    pub layout_position: Point<i32, Logical>,
    pub home_return: Option<HomeReturn>,
    pub fullscreen_return: Option<FullscreenReturn>,
    /// This output's active bookmark: the visible bookmark nearest its usable
    /// center, with hysteresis. Recomputed per frame by the ext-workspace
    /// refresh; the focused output's value is what the protocol and IPC report.
    pub active_bookmark: Option<String>,
    /// The backend, not the config, owns this output's mode, scale and
    /// transform — set for the nested output, whose host window drives size
    /// and scale and whose transform is the renderer's Y-flip compensation.
    /// Config reload skips those three here and applies position only.
    pub backend_owned_mode: bool,
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
            zoom_animation_anchor: None,
            last_rendered_zoom: f64::NAN,
            overview_return: None,
            camera_target: None,
            last_scroll_pan: None,
            momentum: MomentumState::new(drift),
            panning: false,
            edge_pan_velocity: None,
            edge_pan_screen_pos: None,
            edge_pan_delay: None,
            last_rendered_camera: Point::from((f64::NAN, f64::NAN)),
            last_frame_instant: Instant::now(),
            layout_position,
            home_return: None,
            fullscreen_return: None,
            active_bookmark: None,
            backend_owned_mode: false,
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

/// Render-space position of a canvas window: stage position minus the client's
/// geometry offset, relative to the camera. Single home of this formula —
/// `window_render_transform`'s canvas branch and the isolated-capture bypass
/// both use it, so they can't diverge.
pub fn canvas_render_loc(
    loc: Point<i32, Logical>,
    geom_loc: Point<i32, Logical>,
    camera: Point<f64, Logical>,
) -> Point<f64, Logical> {
    Point::from((
        loc.x as f64 - geom_loc.x as f64 - camera.x,
        loc.y as f64 - geom_loc.y as f64 - camera.y,
    ))
}

pub fn output_state(output: &Output) -> MutexGuard<'_, OutputState> {
    let mutex = output
        .user_data()
        .get::<Mutex<OutputState>>()
        .expect("OutputState not initialized on output");
    // Only the main thread locks this, so contention can only be a re-entrant
    // lock — fail loudly instead of freezing the session on a deadlock.
    match mutex.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => panic!("output_state locked re-entrantly"),
        Err(TryLockError::Poisoned(err)) => panic!("OutputState mutex poisoned: {err}"),
    }
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

    /// Sole source of truth for the window list, z-order, positions, focus
    /// history / MRU cycle, fullscreen membership, and fit state. Mutate
    /// through [`Self::map_window`] / [`Self::raise_window`] /
    /// [`Self::unmap_window`] and the stage-backed methods.
    pub stage: driftwm::stage::Stage<StageWindow>,
    /// Output registry only (`map_output` / `outputs` / `output_geometry`);
    /// holds no window elements. Per-window output membership
    /// (`wl_surface.enter`/`leave`) is driven by [`Self::refresh_window_outputs`],
    /// not by this. A clippy lint bans every `Space` element method.
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
    /// Subscribed IPC connections receiving pushed state events. Pruned when a
    /// push write finds the connection gone, and on connection teardown.
    pub ipc_subscribers: Vec<crate::ipc::Subscriber>,
    /// Hash of the last broadcast event, to skip re-sending an identical
    /// snapshot when a dirty tick didn't change what subscribers see.
    pub ipc_last_event_hash: Option<u64>,
    // -- global: SSD decorations --
    pub decorations:
        HashMap<crate::decorations::DecorationKey, crate::decorations::WindowDecoration>,
    pub pending_ssd: HashSet<smithay::reexports::wayland_server::backend::ObjectId>,
    /// Supersample factor for SSD decoration buffers: `ceil` of the largest
    /// output scale. One buffer rendered at this density serves every output
    /// (downscaling stays crisp; only upscaling blurs).
    pub decoration_scale: i32,
    pub render: RenderCache,
    /// Per-window open/close/move/resize/fullscreen animation bookkeeping,
    /// keyed by stable `ElementId`. Render-only; the stage stays authoritative.
    pub(crate) window_animations: window_animation::WindowAnimations,
    /// Flattened textures of closed windows, faded out after teardown.
    pub(crate) closing_snapshots: Vec<crate::render::ClosingSnapshot>,
    /// Departing stand-in chrome fading out — over the window that adopted its
    /// slot, or in place when the stand-in was dismissed.
    pub(crate) standin_fades: Vec<crate::render::StandInFade>,
    /// Content textures captured at unmap/teardown, keyed by root surface id,
    /// consumed when the close animation flattens.
    pub(crate) close_pixels: std::collections::HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        crate::render::ClosePixels,
    >,
    /// Content captured while a compositor resize is frozen, consumed when the
    /// client's redraw releases the freeze.
    pub(crate) resize_captures: crate::render::ResizeCaptures,
    /// The old content of a resized window, fading out over the new content for
    /// the length of its geometry leg.
    pub(crate) resize_crossfades:
        HashMap<driftwm::stage::ElementId, crate::render::ResizeCrossfade>,

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
    /// Per-virtual-keyboard xkb mirror for resolving compositor bindings on
    /// OSK input (see `protocols::virtual_keyboard`).
    pub virtual_kb_bindings: driftwm::protocols::virtual_keyboard::VirtualKeyboardBindings,
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
    pub ext_workspace_state: driftwm::protocols::ext_workspace::ExtWorkspaceManagerState,
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
    #[allow(dead_code)]
    pub tablet_state: smithay::wayland::tablet_manager::TabletManagerState,
    pub gamma_control_manager_state: driftwm::protocols::gamma_control::GammaControlManagerState,
    pub session_lock: SessionLock,
    pub lock_surfaces: HashMap<Output, LockSurface>,

    pub pointer_over_layer: bool,
    /// Last pointer hit-test landed on a screen-space target — a wlr layer or a
    /// screen-pinned window (both live in screen coords, unlike canvas windows).
    pub pointer_over_screen_space: bool,
    pub canvas_layers: Vec<CanvasLayer>,

    pub config: Config,

    pub pending_center: HashSet<WlSurface>,
    pub pending_size: HashSet<WlSurface>,
    /// Surfaces that requested set_maximized / set_fullscreen before their
    /// first sized commit. Deferred until after first-commit positioning so
    /// `restore_size` / `saved_size` capture the client's preferred geometry.
    pub pending_fit: HashSet<WlSurface>,
    /// Surfaces that requested fullscreen before their first sized commit,
    /// mapped to the client-requested output (if any). Resolved against window
    /// rules + active output when the deferred fullscreen fires on first commit.
    pub pending_fullscreen: HashMap<WlSurface, Option<Output>>,
    /// Focused-element snapshot captured at `new_toplevel` time, keyed by the
    /// new surface: the live window or suspended stand-in the user was working
    /// with, so a new window auto-places beside it. `Some(None)` means user had
    /// no focus (e.g. clicked empty canvas); missing entry means the snapshot
    /// was already consumed.
    pub auto_anchor_snapshot: HashMap<WlSurface, Option<StageWindow>>,
    /// Set only by a deliberate click on empty canvas, cleared by every focus
    /// write. Lets auto placement tell "the user asked for a blank slate" (stay
    /// centered) apart from "the anchor merely isn't usable" (fall back to the
    /// nearest element in view).
    pub suppress_auto_anchor: bool,
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
    /// Adopted windows still owing the stable snap rect their adopt would have
    /// written, holding the size the adopt configured. Seeding at adopt time
    /// asserts a footprint the client has not committed: one that acks the
    /// configure before it redraws keeps committing its pre-adopt (larger) size,
    /// which `reflow_grown_snapped_window` reads as a grow past the settled
    /// footprint and answers by relocating the window beside a neighbor. The
    /// entry is consumed by the first commit that carries the adopt size; a
    /// client that never commits it keeps its window out of the reflow (and out
    /// of `markless_suspend_rect`'s shrink protection) until unmap clears it.
    pub(crate) pending_adopt_settle:
        HashMap<smithay::reexports::wayland_server::backend::ObjectId, Size<i32, Logical>>,
    /// Sizes a non-interactive resize (`msg resize`, `grow-window` /
    /// `shrink-window`) has asked a client for and is still waiting to see
    /// answered, keyed by surface id. The next request measures against the
    /// entry instead of against geometry the client may not have repainted yet,
    /// and a step's entry also carries the placement it promised. Overwritten by
    /// the next request on the same surface, and dropped with the surface; a step
    /// that turns out to be a no-op leaves the standing entry alone.
    pub(crate) pending_resizes:
        HashMap<smithay::reexports::wayland_server::backend::ObjectId, resize::PendingResize>,

    /// Windows whose close was requested via `suspend-window`: their next
    /// `toplevel_destroyed` converts into a suspended window. Keyed by surface
    /// id; each carries the trigger-time identity + geometry and a deadline
    /// (a refused close lets the mark lapse). Swept on the per-frame tick.
    pub suspend_marks: HashMap<smithay::reexports::wayland_server::backend::ObjectId, SuspendMark>,
    /// Windows whose close was requested by the compositor (close-window,
    /// `msg close`, taskbar close): their destroy stays a real close even when
    /// `suspend_on_close` is on. Deadline mirrors the suspend marks so a refused
    /// close can't real-close a crash days later.
    pub real_close_marks:
        HashMap<smithay::reexports::wayland_server::backend::ObjectId, std::time::Instant>,
    /// Markless-conversion inputs captured when a mapped toplevel unmaps, so a
    /// client that unmaps before destroying (which resets its xdg role and wipes
    /// app_id / title / geometry) still converts under `suspend_on_close`. Keyed
    /// by surface id; consumed by the destroy and dropped on remap. See
    /// [`UnmapSnapshot`].
    pub unmap_snapshots:
        HashMap<smithay::reexports::wayland_server::backend::ObjectId, UnmapSnapshot>,
    /// Resolved desktop-entry database for identity + relaunch. Warmed on a
    /// background thread at startup (delivered by ping); a suspend before the
    /// warm lands builds it synchronously. `None` until either populates it.
    pub desktop_entry_cache: Option<driftwm::desktop_entry::DesktopEntryCache>,
    /// Monotonic source of per-process suspended-window ids. Durable session
    /// keys are layered on top later (session restore).
    pub next_suspended_id: u64,
    /// In-flight relaunches, keyed by the suspended window being relaunched
    /// (the suspended window itself holds no pending state). Drives the
    /// "launching…" label ([`Self::is_suspended_launching`]) and both match
    /// signals; see [`PendingRelaunch`].
    pub pending_relaunches: BTreeMap<SuspendedId, PendingRelaunch>,
    /// A relaunched surface that presented its activation token before its
    /// first-commit placement, awaiting adoption into the suspended window it
    /// names. Purged with the surface if the client dies before mapping.
    pub pending_adoptions: HashMap<WlSurface, SuspendedId>,
    /// Adoptions a live interactive grab held back, in deferral order. The
    /// window keeps whatever placement it was given and moves into the
    /// stand-in's slot once the grab lets go; see
    /// [`DriftWm::flush_deferred_adoptions`] for what lands and what doesn't.
    /// A first-commit entry does more than postpone: it is also what keeps its
    /// window off the screen and out of every canvas relation for the duration
    /// ([`DriftWm::reveal_deferred_adopt`] hands all of that back), so an entry
    /// that outlives its purpose is an invisible window.
    pub(crate) deferred_adoptions: Vec<DeferredAdopt>,
    /// Durable session store (session restore): the `session.json` path, dirty
    /// timer, carried-forward entries, and fresh-boot camera seed.
    pub session_store: SessionStore,
    /// The bookmark registry: named canvas points (Y-up, window-center
    /// convention). Seeded from `[navigation.bookmarks]` at startup, then the
    /// live source of truth — set-bookmark, IPC, and reload mutate it.
    pub bookmarks: BTreeMap<String, [f64; 2]>,

    /// Window-level keyboard-focus intent. The actual keyboard focus is
    /// derived from this plus any higher-priority owner (session lock,
    /// exclusive / on-demand layer surface) in `update_keyboard_focus`. A
    /// `Suspended` intent derives to no seat keyboard focus.
    pub window_focus: Option<FocusIntent>,
    /// Layer surface granted keyboard focus on click via `OnDemand`
    /// interactivity. Cleared when a window takes focus or it unmaps.
    pub on_demand_layer: Option<WlSurface>,
    /// The active popup keyboard/pointer grab, if any. See [`PopupGrabState`].
    pub popup_grab: Option<PopupGrabState>,
    /// Stage elements under an active interactive `MoveGrab`, tracked so the
    /// relaunch adopt path and the animation start path can tell whether *this*
    /// element is being dragged right now — a plain "any grab active" check
    /// would wrongly block them while something else is being moved. Stand-ins
    /// are drag targets too, hence [`ClusterMember`] rather than `Window`. A
    /// multiset (not an `Option`) because a pointer move and a touch move can
    /// run on different elements at once; grabs push on install and remove on
    /// unset.
    pub interactive_move: Vec<ClusterMember>,
    /// View moves a live grab held back, keyed by the output each was staged
    /// for. Landing one warps the pointer into the grab, so it waits — and the
    /// animation entry that carried it is gone by the time it is held back, so
    /// this is the only thing left that can hand it over once the grab lets go.
    pub(crate) deferred_views: HashMap<String, PendingView>,

    pub held_action: Option<(u32, driftwm::config::Action, Instant)>,

    /// Fractional wheel-notch credit for wheel-up/wheel-down bindings.
    /// High-resolution wheels emit sub-notch v120 deltas; they accumulate
    /// here and the bound action fires once per whole notch. Direction
    /// flips discard the residual.
    pub wheel_notch_accum: f64,

    pub tap: TapTracker,
    /// Action queued by a completed tap chord, run after the closure forwards
    /// the modifier events so the focused client still sees them.
    pub pending_tap_action: Option<driftwm::config::Action>,

    /// Keycodes whose press was intercepted by a binding. Their releases must
    /// also be intercepted, otherwise the focused client receives a "release
    /// without press" — games / Discord / state-tracking apps break, and
    /// launchers leak the trigger key into the previously focused window.
    pub suppressed_keys: HashSet<u32>,

    /// Mouse buttons currently held down. Cleared on VT switch and session
    /// pause alongside `suppressed_keys`.
    pub held_buttons: HashSet<u32>,

    /// Buttons whose press pick mode swallowed (below `zoom_interact_min`), so
    /// their release is swallowed too rather than forwarded to a client that
    /// never saw the press. Drained in `track_held_button` (not on the release
    /// path) so a release missed while locked or after an output drop can't
    /// leave a stale entry that suppresses a later real release.
    pub pick_swallowed_buttons: HashSet<u32>,

    pub gesture_state: Option<GestureState>,
    pub pending_middle_click: Option<PendingMiddleClick>,

    pub momentum_timer: Option<RegistrationToken>,
    /// When a pan burst's auto-launch is due, and the output it pans. Moves with
    /// every pan event while `momentum_timer` stays inserted for the whole burst
    /// — the timer reschedules itself onto this deadline instead of being torn
    /// down and re-registered per event. Held by name so a launch pending on an
    /// output that disconnects mid-burst simply resolves to nothing.
    pub momentum_deadline: Option<(Instant, String)>,

    pub session: Option<LibSeatSession>,
    pub input_devices: Vec<smithay::reexports::input::Device>,

    /// Runtime trackpad send-events override set by
    /// [`Action::SetTrackpad`](driftwm::config::Action::SetTrackpad). `None`
    /// follows `[input.trackpad]`. Config is a seed: reload clears this only
    /// when the resolved mode changed, so an unrelated edit can't silently
    /// re-enable a trackpad the user turned off.
    pub trackpad_send_events: Option<driftwm::config::SendEvents>,

    pub state_file_cameras: HashMap<String, (Point<f64, Logical>, f64)>,
    pub state_file_last_write: Instant,
    /// Active XKB layout name (e.g. "English (US)"), updated on key events.
    pub active_layout: String,
    pub state_file_layout: String,
    pub state_file_windows: Vec<crate::ipc::protocol::WindowInfo>,
    pub state_file_layer_count: usize,
    /// Sorted `(id, output, screen_pos, size)` of screen-pinned windows and
    /// `(output, id, app_id)` of fullscreen windows. Both are excluded from the
    /// canvas window list, so they need their own change detection to keep the
    /// state file's per-output sections from going stale.
    pub state_file_pinned: Vec<(u64, String, [i32; 2], [i32; 2])>,
    pub state_file_fullscreen: Vec<(String, u64, String)>,
    /// Sorted `(namespace, position, size)` of canvas-positioned layers, for
    /// the same staleness detection as the pinned/fullscreen signatures.
    pub state_file_canvas_layers: Vec<(String, [i32; 2], [i32; 2])>,
    /// Name of the active output at the last state-file write. The file's
    /// top-level camera and the snapshot's `active` flags follow the active
    /// output, so switching outputs must dirty them even when no camera moved.
    pub state_file_active_output: Option<String>,
    /// Set by the per-frame ext-workspace refresh when any output's active
    /// bookmark flipped. Broadcast-only (like a title change): forces a
    /// subscription push without marking the state file dirty, since an
    /// incumbent can flip with the camera still (set-bookmark under the current
    /// viewport, delete of the active bookmark).
    pub active_bookmark_dirty: bool,

    pub autostart: Vec<String>,

    /// Outputs whose CRTC is currently active. Universe for [`Self::mark_all_dirty`].
    pub active_outputs: HashSet<Output>,
    pub redraws_needed: HashSet<Output>,
    pub frames_pending: HashSet<crtc::Handle>,
    /// One-shot timers armed when queue_frame returned EmptyFrame so the loop
    /// still wakes at ~refresh rate to advance animations (e.g. xcursor frames).
    pub estimated_vblank_timers: HashMap<crtc::Handle, RegistrationToken>,
    /// Consecutive render-fence timeouts per CRTC, driving the udev backend's
    /// escalating wait budget. Per-CRTC because one wedged output among several
    /// must not have its budget reset by its healthy neighbours in the same
    /// render pass. No entry is a fence that came back on its last frame.
    pub fence_failures: HashMap<crtc::Handle, u32>,
    /// Backstop for a lock confirmation that never gets its frames presented —
    /// see `LOCK_CONFIRM_TIMEOUT`.
    pub lock_confirm_timer: Option<RegistrationToken>,
    /// CRTCs whose in-flight (queued, not yet flipped) frame was composed as a
    /// lock frame.
    pub lock_frame_queued: HashSet<crtc::Handle>,
    /// CRTCs whose currently scanned-out frame was composed as a lock frame.
    pub lock_frame_on_screen: HashSet<crtc::Handle>,

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
    /// A fullscreen window that input-layer code exited ahead of dispatching an
    /// action, so `execute_action` can still treat it as the was-fullscreen
    /// window. Set by the touch tier-crossing exit, consumed by
    /// `execute_action`, and cleared on touch-grab teardown so it can't leak
    /// into a later action.
    pub pre_exited_fullscreen: Option<Window>,
    /// Set while an Alt-Tab cycle step is navigating, so the focus change it
    /// causes is recognized as cycle-initiated: `focus_changed` neither commits
    /// the session nor promotes. Any other focus change during a session commits.
    pub cycle_navigating: bool,
    /// Virtual output placeholders kept when all physical outputs disconnect,
    /// so `active_output().unwrap()` doesn't panic.
    pub disconnected_outputs: HashSet<String>,
    /// Set when output config was applied via wlr-output-management; render
    /// loop re-collects output state and notifies clients.
    pub output_config_dirty: bool,
    /// Mode-change requests from wlr-output-management Apply or config reload.
    /// Drained by the udev render loop, which resolves each intent to a
    /// concrete `control::Mode`. Keyed by output name; backend resolves CRTCs.
    pub pending_mode_changes: HashMap<String, ModeIntent>,

    pub satellite: Option<crate::xwayland::Satellite>,

    /// Udev backend handle (Rc — cloneable). Single owner here; render loop
    /// and protocols (gamma_control) borrow via `udev_device.as_ref()`.
    /// `None` when the winit backend is in use.
    pub udev_device: Option<crate::backend::udev::UdevDevice>,

    pub last_titlebar_click: Option<(
        Instant,
        smithay::reexports::wayland_server::backend::ObjectId,
    )>,

    /// Corner the pointer currently occupies, and the output it's on. Latched
    /// even when fullscreen/dragging suppresses the action; cleared only when
    /// the pointer leaves the corner (or that output).
    pub hot_corner_latch: Option<(Output, HotCorner)>,

    /// Click armed for auto-navigate on release (see `auto_navigate_on_click`).
    pub pending_click_navigate: Option<PendingClickNavigate>,

    /// Left click armed in pick mode (see `PendingPick`, `arm_pick`).
    pub pending_pick: Option<PendingPick>,

    /// Timer for the deferred click-navigate pan. The pan waits out the
    /// double-click window so a second click can cancel it; a fresh press clears
    /// this via `cancel_click_navigate`.
    pub click_navigate_timer: Option<RegistrationToken>,

    /// Compositor-generated errors shown in the on-screen error bar, keyed by
    /// source. Empty = no bar. Use [`Self::set_error`]/[`Self::clear_error`].
    pub errors: BTreeMap<ErrorSource, String>,

    /// Cursor edge-pan: when true, the viewport pans while the bare cursor
    /// touches a screen edge. Toggled by
    /// [`Action::ToggleCursorPan`](driftwm::config::Action::ToggleCursorPan).
    pub cursor_edge_pan: bool,
    pub touch_state: crate::input::touch::TouchState,
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

    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        // A protocol error reaches the client as a bare EOF whenever we posted
        // it on an already-destroyed object, so this is the only place the
        // interface and message survive at all. Worth a warn: a toolkit bug
        // report otherwise arrives as an unattributable "broken pipe".
        if let DisconnectReason::ProtocolError(err) = reason {
            tracing::warn!(
                "client {client_id:?} disconnected: {}@{} code={} — {}",
                err.object_interface,
                err.object_id,
                err.code,
                err.message
            );
        }
    }
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

    /// Take every output's viewport out of flight — camera target, zoom target,
    /// zoom anchor and momentum — and record `target` as under a fresh
    /// interactive move grab. Called at grab install (not first motion) so a
    /// press-and-hold with no motion is still guarded. Only the bookkeeping half
    /// is balanced by `disarm_interactive_move` on grab unset; the cancel is a
    /// one-way snapshot of the moment the grab took over.
    pub fn arm_interactive_move<T: Clone + Into<ClusterMember>>(&mut self, target: &T) {
        // The grab measures its delta against a frozen canvas anchor, and a
        // camera tick warps the pointer synchronously into it — so a flight the
        // grab did not cause would drag the window from a motionless mouse.
        // Stopping momentum drops the pan samples behind it too, which is what
        // we want: a drag decided mid-swipe must not inherit that swipe's fling.
        self.cancel_animations_everywhere();
        self.interactive_move.push(target.clone().into());
    }

    /// Drop one `target` entry armed by `arm_interactive_move`. Removes a single
    /// occurrence so overlapping pointer/touch moves stay balanced. Mirroring
    /// the arm's cancel, the last one out hands back the view moves the grab
    /// held off.
    pub fn disarm_interactive_move<T: Clone + Into<ClusterMember>>(&mut self, target: &T) {
        let target = target.clone().into();
        if let Some(i) = self.interactive_move.iter().position(|m| *m == target) {
            self.interactive_move.remove(i);
        }
        self.flush_deferred_views();
        self.schedule_deferred_adoptions();
    }

    /// Whether `element` is the target of a live move grab. Membership only —
    /// [`Self::element_under_interactive_grab`] is the wider question, since a
    /// client resize is witnessed by the surface rather than by this list.
    pub(crate) fn element_under_interactive_move(&self, element: &StageWindow) -> bool {
        self.interactive_move
            .contains(&ClusterMember::from_element(element))
    }

    pub fn cursor_is_animated(&self) -> bool {
        self.cursor.is_animated()
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

    /// Bounding box of a mapped window in canvas coordinates: `window.bbox_with_popups()`
    /// (which includes popup/subsurface overhang) placed at the stage position.
    /// Mirrors `Space::element_bbox`.
    pub(crate) fn window_bbox_with_popups(
        &self,
        window: &Window,
    ) -> Option<smithay::utils::Rectangle<i32, Logical>> {
        let pos = self.stage.position_of(window)?;
        let mut bbox = window.bbox_with_popups();
        bbox.loc += pos - window.geometry().loc;
        Some(bbox)
    }

    /// True if `window` is pinned to an output's screen space.
    pub fn is_pinned<Q>(&self, window: &Q) -> bool
    where
        StageWindow: PartialEq<Q>,
    {
        self.stage.is_pinned(window)
    }

    /// True if `window` is currently fullscreen on some output.
    pub fn is_window_fullscreen<Q>(&self, window: &Q) -> bool
    where
        StageWindow: PartialEq<Q>,
    {
        self.stage.is_fullscreen(window)
    }

    /// The fullscreen window on `output`, if any.
    pub fn fullscreen_window_on(&self, output: &Output) -> Option<Window> {
        self.stage
            .fullscreen_on(&output.name())
            .and_then(|fs| fs.window.client())
            .cloned()
    }

    /// The fullscreen window on the active output, if any.
    pub fn active_fullscreen_window(&self) -> Option<Window> {
        self.active_output()
            .and_then(|o| self.fullscreen_window_on(&o))
    }

    /// True if `window` is a real canvas window — not a widget (wallpaper
    /// layer, immovable), screen-pinned, fullscreen, or held back for a
    /// deferred adopt. The eligibility test for canvas operations: navigation,
    /// centering, fitting, snapping, zoom-to-fit, etc. A fullscreen window fills
    /// its own output and is parked at that output's camera origin, so it must
    /// never join another output's snap/cluster/fit geometry; a window awaiting
    /// its adopt is not drawn at all, so nothing may aim the camera or a
    /// placement at it.
    pub fn is_canvas_window<Q>(&self, window: &Q) -> bool
    where
        Q: WindowExt + WaylandFocus,
        StageWindow: PartialEq<Q>,
    {
        !window.is_widget()
            && !self.is_pinned(window)
            && !self.is_window_fullscreen(window)
            && !self.hidden_by_deferred_adopt(window)
    }

    pub fn load_xcursor(&mut self, name: &str) -> Option<&CursorFrames> {
        let theme = self.config.cursor_theme.as_deref().unwrap_or("default");
        let size = self.config.cursor_size.unwrap_or(24);
        self.cursor.load_xcursor(name, theme, size)
    }
}

impl DriftWm {
    /// Cull dead windows and refresh output membership even on idle
    /// (no-render) turns, or a client that died without a clean unmap lingers
    /// in the read model until the next damage-driven render. Shared by the
    /// main loop and the test server pump so the two can't drift apart.
    pub fn refresh_and_flush_clients(&mut self) {
        self.stage.retain_alive();
        // Prune animation entries whose window left the stage — covers crash
        // paths (no `unmap_window`) and lets the fixture baseline drain without
        // a tick source.
        let stage = &self.stage;
        self.window_animations
            .retain_ids(|id| stage.window_by_id(id).is_some());
        // Same sweep for the crossfade halves. It covers teardown only: the id
        // survives `Stage::replace`, so conversion and adoption drop theirs at
        // the replace itself.
        self.resize_captures
            .retain_ids(|id| stage.window_by_id(id).is_some());
        self.resize_crossfades
            .retain(|id, _| stage.window_by_id(*id).is_some());
        self.refresh_window_outputs();
        self.popups.cleanup();
        self.display_handle.flush_clients().ok();
    }

    /// Sizes of every unbounded, window/surface/client/output-name-keyed
    /// collection, keyed by field name. Exposed over IPC (`debug-counters`) and
    /// snapshotted by the test fixture at teardown, so a collection that grows
    /// without draining becomes an assertable regression instead of an invisible
    /// slow leak. Bounded maps and ones keyed by live hardware handles
    /// (DPMS sets, vblank timers) are deliberately omitted so they don't add
    /// baseline noise; the keys are implementation detail, not a stable
    /// interface.
    pub fn debug_counters(&self) -> BTreeMap<String, usize> {
        [
            ("stage_entries", self.stage.windows().len()),
            ("stage_focus_history", self.stage.focus_history().len()),
            ("stage_fullscreen", self.stage.fullscreen_entries().count()),
            ("stage_pinned", self.stage.pinned_windows().count()),
            ("decorations", self.decorations.len()),
            ("pending_ssd", self.pending_ssd.len()),
            ("pending_center", self.pending_center.len()),
            ("pending_size", self.pending_size.len()),
            ("pending_fit", self.pending_fit.len()),
            ("pending_fullscreen", self.pending_fullscreen.len()),
            ("auto_anchor_snapshot", self.auto_anchor_snapshot.len()),
            ("pending_recenter", self.pending_recenter.len()),
            ("stable_snap_rects", self.stable_snap_rects.len()),
            ("pending_adopt_settle", self.pending_adopt_settle.len()),
            ("pending_resizes", self.pending_resizes.len()),
            ("suspend_marks", self.suspend_marks.len()),
            ("real_close_marks", self.real_close_marks.len()),
            ("window_animations", self.window_animations.len()),
            ("closing_snapshots", self.closing_snapshots.len()),
            ("standin_fades", self.standin_fades.len()),
            ("close_pixels", self.close_pixels.len()),
            ("resize_captures", self.resize_captures.len()),
            ("resize_crossfades", self.resize_crossfades.len()),
            ("unmap_snapshots", self.unmap_snapshots.len()),
            ("pending_relaunches", self.pending_relaunches.len()),
            ("pending_adoptions", self.pending_adoptions.len()),
            ("deferred_adoptions", self.deferred_adoptions.len()),
            (
                "idle_inhibiting_surfaces",
                self.idle_inhibiting_surfaces.len(),
            ),
            ("canvas_layers", self.canvas_layers.len()),
            ("lock_surfaces", self.lock_surfaces.len()),
            ("pending_screencopies", self.pending_screencopies.len()),
            ("pending_captures", self.pending_captures.len()),
            ("ipc_subscribers", self.ipc_subscribers.len()),
            (
                "virtual_kb_bindings",
                self.virtual_kb_bindings.keyboard_count(),
            ),
            ("state_file_cameras", self.state_file_cameras.len()),
            ("disconnected_outputs", self.disconnected_outputs.len()),
            ("pending_mode_changes", self.pending_mode_changes.len()),
            ("blur_cache", self.render.blur_cache.len()),
            ("shared_blur", self.render.shared_blur.len()),
            (
                "blur_scratch",
                self.render.blur_scratch.values().map(|p| p.len()).sum(),
            ),
            ("shadow_cache", self.render.shadow_cache.len()),
            ("border_cache", self.render.border_cache.len()),
            ("cached_bg", self.render.cached_bg.len()),
            ("capture_state", self.render.capture_state.len()),
            ("cached_tile_chunks", self.render.cached_tile_chunks.len()),
            (
                "cached_shader_chunks",
                self.render.cached_shader_chunks.len(),
            ),
            // Both stay at 0 for a fullscreen output that conceals its canvas:
            // pending work there keeps the udev scheduler marking it dirty every
            // vblank.
            (
                "bg_chunk_loads_in_flight",
                self.render
                    .cached_tile_chunks
                    .values()
                    .map(|c| c.in_flight_len())
                    .sum(),
            ),
            (
                "shader_chunk_caches_pending",
                self.render
                    .cached_shader_chunks
                    .values()
                    .filter(|c| c.has_pending_bakes())
                    .count(),
            ),
            ("cached_error_bar", self.render.cached_error_bar.len()),
            ("cached_outlines", self.render.cached_outlines.len()),
            (
                "background_last_animate",
                self.render.background_last_animate.len(),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// Debug-only end-of-tick check: the stage's own structural invariants
    /// hold; the two halves of the fullscreen split (stage membership and
    /// per-output viewport-return) cover the same outputs; and every SSD
    /// decoration entry belongs to a live window on the stage. A panic here is
    /// a routing bug — some mutation bypassed the stage wrappers.
    #[cfg(debug_assertions)]
    pub fn verify_stage_invariants(&self) {
        self.stage.verify_invariants();

        for output in self.space.outputs() {
            assert_eq!(
                self.stage.fullscreen_on(&output.name()).is_some(),
                output_state(output).fullscreen_return.is_some(),
                "fullscreen membership and viewport-return presence diverged on {}",
                output.name()
            );
        }
        for (name, _) in self.stage.fullscreen_entries() {
            assert!(
                self.output_by_name(name).is_some(),
                "stage fullscreen entry for {name} outlived its output"
            );
        }
        for (_, site) in self.stage.pinned_windows() {
            assert!(
                self.output_by_name(&site.output).is_some(),
                "pin references dead output {}",
                site.output
            );
        }
        for output in self.space.outputs() {
            if let Some(site) = output_state(output)
                .fullscreen_return
                .as_ref()
                .and_then(|ret| ret.pinned.as_ref())
            {
                assert!(
                    self.output_by_name(&site.output).is_some(),
                    "pin suspended by fullscreen on {} references dead output {}",
                    output.name(),
                    site.output
                );
            }
        }

        for key in self.decorations.keys() {
            let present = match key {
                crate::decorations::DecorationKey::Surface(id) => self
                    .stage
                    .windows()
                    .any(|w| w.wl_surface().is_some_and(|s| s.id() == *id)),
                crate::decorations::DecorationKey::Suspended(sid) => self
                    .stage
                    .windows()
                    .any(|w| w.suspended().is_some_and(|s| s.id == *sid)),
            };
            assert!(present, "decoration entry for a window not on the stage");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_state_relock_panics_instead_of_deadlocking() {
        let output = Output::new(
            "relock-test".to_string(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".to_string(),
                model: "test".to_string(),
                serial_number: "0".to_string(),
            },
        );
        init_output_state(&output, Point::from((0.0, 0.0)), 0.96, Point::from((0, 0)));
        let _guard = output_state(&output);
        let relock = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(output_state(&output));
        }));
        assert!(relock.is_err(), "re-entrant lock must panic, not deadlock");
    }
}
