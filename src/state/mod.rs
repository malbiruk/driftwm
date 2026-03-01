mod animation;
mod fullscreen;
mod navigation;

use smithay::{
    desktop::{PopupManager, Space, Window},
    input::{Seat, SeatState, keyboard::XkbConfig, pointer::CursorImageStatus},
    reexports::{
        calloop::{LoopHandle, LoopSignal},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point, Size},
    wayland::output::OutputManagerState,
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        cursor_shape::CursorShapeManagerState,
        selection::data_device::DataDeviceState,
        shell::xdg::XdgShellState,
        shm::ShmState,
    },
};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use smithay::backend::allocator::Fourcc;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::idle_inhibit::IdleInhibitManagerState;
use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState;
use smithay::wayland::pointer_constraints::PointerConstraintsState;
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::selection::wlr_data_control::DataControlState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::xdg_activation::XdgActivationState;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::gles::{GlesPixelProgram, element::PixelShaderElement};
use smithay::utils::Transform;

use smithay::backend::session::libseat::LibSeatSession;

use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::drm::control::crtc;

use crate::backend::Backend;
use crate::input::gestures::GestureState;
use driftwm::canvas::MomentumState;
use driftwm::config::Config;

/// Buffered middle-click from a 3-finger tap. Held for DOUBLE_TAP_WINDOW_MS
/// to see if a 3-finger swipe follows (→ move window). If the timer fires
/// without a swipe, the click is forwarded to the client (paste).
pub struct PendingMiddleClick {
    pub press_time: u32,
    pub release_time: Option<u32>,
    pub timer_token: RegistrationToken,
}

pub use crate::focus::FocusTarget;

/// Log an error result with context, discarding the Ok value.
#[inline]
pub fn log_err(context: &str, result: Result<impl Sized, impl std::fmt::Display>) {
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

/// Wrapper held by the calloop event loop — gives callbacks access
/// to both compositor state and the Wayland display.
pub struct CalloopData {
    pub state: DriftWm,
    pub display: Display<DriftWm>,
}

/// Saved state for a fullscreen window — restored on exit.
pub struct FullscreenState {
    pub window: Window,
    pub saved_location: Point<i32, Logical>,
    pub saved_camera: Point<f64, Logical>,
    pub saved_zoom: f64,
}

/// Central compositor state.
pub struct DriftWm {
    pub start_time: Instant,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, CalloopData>,
    pub loop_signal: LoopSignal,

    // Desktop
    pub space: Space<Window>,
    pub popups: PopupManager,

    // Protocol state
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<DriftWm>,
    pub data_device_state: DataDeviceState,

    // Input
    pub seat: Seat<DriftWm>,

    // Viewport / camera / zoom
    pub camera: Point<f64, Logical>,
    pub zoom: f64,
    /// Zoom animation target. When Some, zoom lerps toward this value each frame.
    pub zoom_target: Option<f64>,
    /// Last rendered zoom — for shader/damage change detection.
    pub last_rendered_zoom: f64,
    /// Saved (camera, zoom) for ZoomToFit toggle-back.
    pub overview_return: Option<(Point<f64, Logical>, f64)>,
    /// Timestamp of the last scroll-pan event. Used to keep panning sticky
    /// within a scroll gesture (150ms window) even if a window slides under.
    pub last_scroll_pan: Option<Instant>,
    /// Scroll momentum: velocity, friction, frame-based skip.
    pub momentum: MomentumState,
    /// Monotonic frame counter, incremented each render tick.
    pub frame_counter: u64,
    /// True while a PanGrab is active. Suppresses momentum ticks so
    /// they don't interfere with the grab's camera tracking.
    pub panning: bool,

    /// Auto-pan velocity when dragging a window to viewport edge.
    /// Set by MoveSurfaceGrab, cleared when grab ends or cursor leaves edge zone.
    pub edge_pan_velocity: Option<Point<f64, Logical>>,

    // Cursor
    pub cursor_status: CursorImageStatus,
    /// True while a compositor grab (pan/resize) owns the cursor icon.
    /// Blocks client cursor updates in `cursor_image()`.
    pub grab_cursor: bool,
    pub cursor_buffers: HashMap<String, (MemoryRenderBuffer, Point<i32, Logical>)>,

    // Backend (moved here so protocol handlers can access the renderer)
    pub backend: Option<Backend>,
    /// Compiled background shader program (compiled once at startup).
    pub background_shader: Option<GlesPixelProgram>,
    /// Cached shader background element (stable Id for damage tracking).
    pub cached_bg_element: Option<PixelShaderElement>,
    /// Camera position at last render — used to detect movement and update uniforms.
    pub last_rendered_camera: Point<f64, Logical>,
    /// Pre-loaded tile image for tiled background (loaded once at startup).
    /// Buffer is (w+1)×(h+1) with the last col/row duplicated for 1px overlap.
    /// Stores (buffer, original_width, original_height).
    pub background_tile: Option<(MemoryRenderBuffer, i32, i32)>,

    // Protocols
    pub dmabuf_state: DmabufState,
    pub dmabuf_global: Option<DmabufGlobal>,
    pub cursor_shape_state: CursorShapeManagerState,
    pub viewporter_state: ViewporterState,
    pub fractional_scale_state: FractionalScaleManagerState,
    pub xdg_activation_state: XdgActivationState,
    pub primary_selection_state: PrimarySelectionState,
    pub data_control_state: DataControlState,
    pub pointer_constraints_state: PointerConstraintsState,
    pub relative_pointer_state: RelativePointerManagerState,
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    pub idle_inhibit_state: IdleInhibitManagerState,
    pub presentation_state: PresentationState,
    pub decoration_state: XdgDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub foreign_toplevel_state: driftwm::protocols::foreign_toplevel::ForeignToplevelManagerState,

    /// True when pointer focus is a layer surface (screen-fixed, not canvas-relative).
    /// Guards synthetic pointer adjustments in camera/zoom animations.
    pub pointer_over_layer: bool,

    // Keybindings and settings
    pub config: Config,

    /// Surfaces awaiting their first buffer commit, to be centered once size is known.
    pub pending_center: HashSet<WlSurface>,

    // Window navigation
    /// Camera animation target. When Some, camera lerps toward this point each frame.
    pub camera_target: Option<Point<f64, Logical>>,
    /// Timestamp of the last rendered frame, for delta-time computation.
    pub last_frame_instant: Instant,
    /// MRU focus history: index 0 = most recently focused.
    pub focus_history: Vec<Window>,
    /// Active Alt-Tab cycling index into focus_history. None when not cycling.
    pub cycle_state: Option<usize>,
    /// Saved (camera, zoom) to return to when toggling home a second time.
    pub home_return: Option<(Point<f64, Logical>, f64)>,

    // Key repeat for compositor bindings (smithay's repeat only applies to
    // client-forwarded keys, not intercepted compositor actions).
    /// Currently held repeatable action: (keycode, action, next_fire_time).
    pub held_action: Option<(u32, driftwm::config::Action, Instant)>,

    /// Active fullscreen window state. When Some, viewport is locked.
    pub fullscreen: Option<FullscreenState>,

    /// Active gesture state. Set at Begin, cleared at End/Cancel.
    pub gesture_state: Option<GestureState>,

    /// Buffered middle-click waiting for a possible 3-finger swipe.
    pub pending_middle_click: Option<PendingMiddleClick>,

    /// Libseat session for VT switching (udev backend only).
    pub session: Option<LibSeatSession>,

    /// Last camera/zoom written to the state file (for dirty detection).
    pub state_file_camera: Point<f64, Logical>,
    pub state_file_zoom: f64,
    /// Throttle: last time the state file was actually written.
    pub state_file_last_write: Instant,

    /// Active XKB layout name (e.g. "English (US)"), updated on key events.
    pub active_layout: String,
    /// Last layout written to the state file (for dirty detection).
    pub state_file_layout: String,

    /// Commands to spawn after WAYLAND_DISPLAY is set.
    pub autostart: Vec<String>,

    /// Active DRM CRTCs (set by udev backend init/hotplug).
    pub active_crtcs: HashSet<crtc::Handle>,
    /// CRTCs that need a new frame rendered.
    pub redraws_needed: HashSet<crtc::Handle>,
    /// CRTCs with a frame queued to DRM, awaiting VBlank.
    pub frames_pending: HashSet<crtc::Handle>,

    /// Config file mtime — for polling-based hot-reload.
    pub config_file_mtime: Option<std::time::SystemTime>,
}

/// Per-client state stored by wayland-server for each connected client.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

impl DriftWm {
    pub fn new(
        dh: DisplayHandle,
        loop_handle: LoopHandle<'static, CalloopData>,
        loop_signal: LoopSignal,
    ) -> Self {
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(
            &dh,
            [xdg_toplevel::WmCapabilities::Fullscreen],
        );
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);

        let cursor_shape_state = CursorShapeManagerState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&dh);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        let data_control_state =
            DataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), |_| true);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&dh);
        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(&dh);
        let idle_inhibit_state = IdleInhibitManagerState::new::<Self>(&dh);
        let presentation_state = PresentationState::new::<Self>(&dh, 1); // CLOCK_MONOTONIC
        let decoration_state = XdgDecorationState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let foreign_toplevel_state =
            driftwm::protocols::foreign_toplevel::ForeignToplevelManagerState::new::<Self, _>(&dh, |_| true);

        let config = Config::load();

        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "seat-0");
        let kb = &config.keyboard_layout;
        let xkb = XkbConfig {
            layout: &kb.layout,
            variant: &kb.variant,
            options: if kb.options.is_empty() { None } else { Some(kb.options.clone()) },
            model: &kb.model,
            ..Default::default()
        };
        seat.add_keyboard(xkb, config.repeat_delay, config.repeat_rate)
            .expect("Failed to add keyboard");
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
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            seat,
            camera: Point::from((0.0, 0.0)),
            zoom: 1.0,
            zoom_target: None,
            last_rendered_zoom: f64::NAN,
            overview_return: None,
            last_scroll_pan: None,
            momentum: MomentumState::new(config.friction),
            frame_counter: 0,
            panning: false,
            edge_pan_velocity: None,
            cursor_status: CursorImageStatus::default_named(),
            grab_cursor: false,
            cursor_buffers: HashMap::new(),
            backend: None,
            background_shader: None,
            cached_bg_element: None,
            last_rendered_camera: Point::from((f64::NAN, f64::NAN)),
            background_tile: None,
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
            idle_inhibit_state,
            presentation_state,
            decoration_state,
            layer_shell_state,
            foreign_toplevel_state,
            pointer_over_layer: false,
            config,
            pending_center: HashSet::new(),
            camera_target: None,
            last_frame_instant: Instant::now(),
            focus_history: Vec::new(),
            cycle_state: None,
            home_return: None,
            held_action: None,
            gesture_state: None,
            pending_middle_click: None,
            fullscreen: None,
            session: None,
            state_file_camera: Point::from((f64::NAN, f64::NAN)),
            state_file_zoom: f64::NAN,
            state_file_last_write: Instant::now(),
            active_layout: String::new(),
            state_file_layout: String::new(),
            autostart,
            active_crtcs: HashSet::new(),
            redraws_needed: HashSet::new(),
            frames_pending: HashSet::new(),
            config_file_mtime: None,
        }
    }

    /// Push any `below` windows to the bottom of the z-order.
    /// Called after every `raise_element()` to maintain stacking.
    pub fn enforce_below_windows(&mut self) {
        // Space stores elements in a vec where last = topmost.
        // raise_element pushes to the end (top). So we raise all
        // non-below windows in reverse order to preserve their relative
        // stacking while ensuring they sit above any below windows.
        let non_below: Vec<_> = self
            .space
            .elements()
            .filter(|w| {
                !driftwm::config::applied_rule(w.toplevel().unwrap().wl_surface())
                    .is_some_and(|r| r.widget)
            })
            .cloned()
            .collect();

        for w in non_below {
            self.space.raise_element(&w, false);
        }

        if let Some(ref fs) = self.fullscreen {
            self.space.raise_element(&fs.window, false);
        }
    }

    /// Mark all active outputs as needing a redraw.
    pub fn mark_all_dirty(&mut self) {
        self.redraws_needed.extend(self.active_crtcs.iter());
    }

    /// True if any animation is still in progress and needs continued rendering.
    pub fn has_active_animations(&self) -> bool {
        self.camera_target.is_some()
            || self.zoom_target.is_some()
            || self.edge_pan_velocity.is_some()
            || self.held_action.is_some()
            || (self.momentum.velocity.x != 0.0 || self.momentum.velocity.y != 0.0)
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

    /// Sync each output's position to the current camera, so render_output
    /// automatically applies the canvas→screen transform.
    pub fn update_output_from_camera(&mut self) {
        let camera_i32 = self.camera.to_i32_round();
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            self.space.map_output(&output, camera_i32);
        }
    }

    /// Logical viewport size from the first output.
    pub fn get_viewport_size(&self) -> Size<i32, Logical> {
        self.space
            .outputs()
            .next()
            .and_then(|o| o.current_mode())
            .map(|m| m.size.to_logical(1))
            .unwrap_or((1, 1).into())
    }

    /// Write viewport center + zoom to `$XDG_RUNTIME_DIR/driftwm/state` if changed.
    /// Atomic: writes to .tmp then renames.
    pub fn write_state_file_if_dirty(&mut self) {
        let cam = self.camera;
        let z = self.zoom;
        // Compare with epsilon to avoid writing on sub-pixel jitter
        let layout_dirty = self.state_file_layout != self.active_layout;
        if !layout_dirty
            && (cam.x - self.state_file_camera.x).abs() < 0.5
            && (cam.y - self.state_file_camera.y).abs() < 0.5
            && (z - self.state_file_zoom).abs() < 0.001
        {
            return;
        }
        // Throttle writes to ~10/sec max (100ms between writes)
        if self.state_file_last_write.elapsed() < std::time::Duration::from_millis(100) {
            return;
        }
        self.state_file_camera = cam;
        self.state_file_zoom = z;
        self.state_file_layout = self.active_layout.clone();
        self.state_file_last_write = Instant::now();

        // Convert camera (top-left) to viewport center in canvas coords.
        // Negate Y so positive = above origin (user-facing Y-up convention).
        let vp = self.get_viewport_size();
        let cx = cam.x + vp.w as f64 / (2.0 * z);
        let cy = -(cam.y + vp.h as f64 / (2.0 * z));

        let Some(dir) = state_file_dir() else { return };
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("state");
        let tmp = dir.join("state.tmp");
        let mut content = format!("x={cx:.0}\ny={cy:.0}\nzoom={z:.3}\nlayout={}\n", self.active_layout);

        if let Some((saved_cam, saved_zoom)) = self.home_return {
            let sx = saved_cam.x + vp.w as f64 / (2.0 * saved_zoom);
            let sy = -(saved_cam.y + vp.h as f64 / (2.0 * saved_zoom));
            content += &format!("saved_x={sx:.0}\nsaved_y={sy:.0}\nsaved_zoom={saved_zoom:.3}\n");
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
                tracing::error!("Config reload: failed to read {}: {e}", config_path.display());
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

        // Log non-reloadable changes
        if new_config.keyboard_layout != self.config.keyboard_layout {
            tracing::info!("Config reload: keyboard layout changes require restart");
        }
        if new_config.output_scale != self.config.output_scale {
            tracing::info!("Config reload: output scale changes require restart");
        }
        if new_config.autostart != self.config.autostart {
            tracing::info!("Config reload: autostart changes only apply at startup");
        }

        // Keyboard repeat rate/delay
        if new_config.repeat_rate != self.config.repeat_rate
            || new_config.repeat_delay != self.config.repeat_delay
        {
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.change_repeat_info(new_config.repeat_delay, new_config.repeat_rate);
        }

        // Momentum friction
        if new_config.friction != self.config.friction {
            self.momentum.friction = new_config.friction;
        }

        // Background shader/tile — clear cached state for lazy re-init
        if new_config.background != self.config.background {
            self.background_shader = None;
            self.cached_bg_element = None;
            self.background_tile = None;
        }

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
                self.cursor_buffers.clear();
            }
        }

        // Trackpad settings — only apply to newly connected devices
        if new_config.trackpad != self.config.trackpad {
            tracing::info!(
                "Config reload: trackpad settings changed — will apply to newly connected devices"
            );
        }

        self.config = new_config;
        self.mark_all_dirty();
        tracing::info!("Config reloaded");
    }

    /// Load an xcursor image by name and cache the resulting MemoryRenderBuffer.
    /// Returns a reference to the cached (buffer, hotspot) pair.
    pub fn load_xcursor(
        &mut self,
        name: &str,
    ) -> Option<&(MemoryRenderBuffer, Point<i32, Logical>)> {
        if !self.cursor_buffers.contains_key(name) {
            let theme_name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
            let theme = xcursor::CursorTheme::load(&theme_name);
            let path = theme.load_icon(name)?;
            let data = std::fs::read(path).ok()?;
            let images = xcursor::parser::parse_xcursor(&data)?;

            // Pick the image closest to 24px (standard cursor size)
            let target_size = std::env::var("XCURSOR_SIZE")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(24);
            let image = images
                .iter()
                .min_by_key(|img| (img.size as i32 - target_size as i32).unsigned_abs())?;

            let buffer = MemoryRenderBuffer::from_slice(
                &image.pixels_rgba,
                Fourcc::Abgr8888,
                (image.width as i32, image.height as i32),
                1,
                Transform::Normal,
                None,
            );
            let hotspot = Point::from((image.xhot as i32, image.yhot as i32));
            self.cursor_buffers
                .insert(name.to_string(), (buffer, hotspot));
        }
        self.cursor_buffers.get(name)
    }
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
