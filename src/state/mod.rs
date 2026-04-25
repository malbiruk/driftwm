mod animation;
mod cluster_snapshot;
mod cursor;
pub mod fit;
mod fullscreen;
mod init;
mod navigation;
mod persistence;
mod reload;
mod render_cache;
mod xwayland;
pub use cluster_snapshot::ClusterResizeSnapshot;
pub(crate) use cluster_snapshot::snap_targets_impl;
pub use cursor::{CursorFrames, CursorState};
pub use persistence::{read_all_per_output_state, remove_state_file};
pub use render_cache::RenderCache;

use smithay::{
    desktop::{PopupManager, Space, Window},
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
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesTexture;
use smithay::utils::Physical;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::idle_inhibit::IdleInhibitManagerState;
use smithay::wayland::idle_notify::IdleNotifierState;
use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState;
use smithay::wayland::pointer_constraints::PointerConstraintsState;
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::security_context::SecurityContextState;
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
    #[allow(dead_code)]
    pub hyprland_toplevel_export_state: driftwm::protocols::hyprland_toplevel_export::HyprlandToplevelExportState,
    pub pending_hyprland_exports: Vec<driftwm::protocols::hyprland_toplevel_export::PendingToplevelExport>,
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

    pub fn load_xcursor(&mut self, name: &str) -> Option<&CursorFrames> {
        self.cursor.load_xcursor(name)
    }
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
