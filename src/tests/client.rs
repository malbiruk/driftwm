// A full test-client API surface (layer surfaces, maximize, parent, title, …).
// The first scenario module uses only part of it; the rest is deliberate
// scaffolding the later scenario families build on.
#![allow(dead_code)]

use std::cmp::min;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use smithay::reexports::calloop::EventLoop;

use calloop_wayland_source::WaylandSource;
use wayland_client::backend::Backend;
use wayland_client::backend::protocol::ProtocolError;
use wayland_client::globals::Global;
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_display::WlDisplay;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_surface::{self, WlSurface};
use wayland_client::protocol::wl_touch::{self, WlTouch};
use wayland_client::{Connection, Dispatch, Proxy as _, QueueHandle};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::{
    self, ZwpConfinedPointerV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::{
    self, ZwpLockedPointerV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::{
    Lifetime, ZwpPointerConstraintsV1,
};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols::xdg::activation::v1::client::xdg_activation_token_v1::{
    self, XdgActivationTokenV1,
};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;
use wayland_protocols::xdg::shell::client::xdg_popup::{self, XdgPopup};
use wayland_protocols::xdg::shell::client::xdg_positioner::{
    Anchor, ConstraintAdjustment, Gravity, XdgPositioner,
};
use wayland_protocols::xdg::shell::client::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::client::xdg_toplevel::{self, XdgToplevel};
use wayland_protocols::xdg::shell::client::xdg_wm_base::{self, XdgWmBase};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{self, ZwlrLayerShellV1};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, ZwlrLayerSurfaceV1,
};

pub struct Client {
    pub id: ClientId,
    pub event_loop: EventLoop<'static, State>,
    pub connection: Connection,
    pub qh: QueueHandle<State>,
    pub display: WlDisplay,
    pub state: State,
}

/// The client-side dispatch state (distinct from the compositor's
/// `crate::state::DriftWm`). Tracks bound globals and the surfaces this test
/// client created.
pub struct State {
    pub qh: QueueHandle<State>,

    pub globals: Vec<Global>,
    pub outputs: HashMap<WlOutput, String>,

    pub compositor: Option<WlCompositor>,
    pub xdg_wm_base: Option<XdgWmBase>,
    pub layer_shell: Option<ZwlrLayerShellV1>,
    pub spbm: Option<WpSinglePixelBufferManagerV1>,
    pub viewporter: Option<WpViewporter>,
    pub seat: Option<WlSeat>,
    /// Bound alongside the seat, so every scenario can observe touch delivery
    /// without opting in.
    pub touch: Option<WlTouch>,
    /// Bound alongside the seat, for the same reason as `touch`.
    pub pointer: Option<WlPointer>,
    pub pointer_constraints: Option<ZwpPointerConstraintsV1>,
    pub xdg_activation: Option<XdgActivationV1>,
    pub ext_session_lock_manager: Option<ExtSessionLockManagerV1>,

    pub windows: Vec<Window>,
    pub layers: Vec<LayerSurface>,
    pub popups: Vec<Popup>,
    pub session_locks: Vec<Lock>,
    /// Every `wl_touch` event this client has received, oldest first.
    pub touch_events: Vec<TouchEvent>,
    /// Surface-local position carried by every `wl_pointer` enter/motion this
    /// client has received, oldest first.
    pub pointer_positions: Vec<(f64, f64)>,

    /// The token string from the most recent `xdg_activation_token_v1.done`.
    pub activation_token: Option<String>,

    /// Recorded ext-workspace-v1 protocol activity for assertions.
    pub ext_workspace: ExtWorkspace,
}

/// Every ext-workspace-v1 event the compositor sends, captured for inspection.
/// Follows the `WlOutput` recording idiom: handle events land in fields, they
/// never drive behavior. The manager, group and per-workspace handles are all
/// created by the compositor (the manager via the registry, the group and
/// workspaces via `new_id` events), so the recorder owns the client-side proxies.
#[derive(Default)]
pub struct ExtWorkspace {
    pub manager: Option<ExtWorkspaceManagerV1>,
    pub group: Option<ExtWorkspaceGroupHandleV1>,
    pub group_capabilities: Option<u32>,
    /// Count of manager `done` events — the protocol's atomicity barrier.
    pub done_count: usize,
    pub finished: bool,
    /// Group `output_enter` / `output_leave` targets, in arrival order.
    pub output_enters: Vec<WlOutput>,
    pub output_leaves: Vec<WlOutput>,
    /// Group `workspace_enter` / `workspace_leave` targets, in arrival order.
    pub workspace_enters: Vec<ExtWorkspaceHandleV1>,
    pub workspace_leaves: Vec<ExtWorkspaceHandleV1>,
    pub workspaces: Vec<WorkspaceRecord>,
}

/// One `ext_workspace_handle_v1` and its latest advertised properties. `state`
/// and `capabilities` hold the raw bitfields; the last value of each wins.
pub struct WorkspaceRecord {
    pub handle: ExtWorkspaceHandleV1,
    pub id: Option<String>,
    pub name: Option<String>,
    pub capabilities: Option<u32>,
    pub state: Option<u32>,
    pub removed: bool,
}

impl ExtWorkspace {
    /// Names of every workspace the compositor advertised and hasn't removed.
    pub fn names(&self) -> Vec<String> {
        self.workspaces
            .iter()
            .filter(|w| !w.removed)
            .filter_map(|w| w.name.clone())
            .collect()
    }

    /// The live workspace record named `name`, if any.
    pub fn workspace(&self, name: &str) -> Option<&WorkspaceRecord> {
        self.workspaces
            .iter()
            .find(|w| !w.removed && w.name.as_deref() == Some(name))
    }

    /// Name of the workspace currently carrying the `active` state bit, if any.
    pub fn active(&self) -> Option<&str> {
        let active_bit = ext_workspace_handle_v1::State::Active.bits();
        self.workspaces
            .iter()
            .find(|w| !w.removed && w.state.is_some_and(|s| s & active_bit != 0))
            .and_then(|w| w.name.as_deref())
    }

    /// Whether the group emitted `workspace_enter` for the workspace named `name`.
    pub fn entered(&self, name: &str) -> bool {
        self.workspaces
            .iter()
            .any(|w| w.name.as_deref() == Some(name) && self.workspace_enters.contains(&w.handle))
    }
}

pub struct Window {
    pub qh: QueueHandle<State>,
    pub spbm: WpSinglePixelBufferManagerV1,
    pub seat: WlSeat,

    pub surface: WlSurface,
    pub xdg_surface: XdgSurface,
    pub xdg_toplevel: XdgToplevel,
    pub viewport: WpViewport,
    pub pending_configure: Configure,
    pub configures_received: Vec<(u32, Configure)>,
    pub close_requested: bool,

    pub configures_looked_at: usize,
}

pub struct LayerSurface {
    pub qh: QueueHandle<State>,
    pub spbm: WpSinglePixelBufferManagerV1,

    pub surface: WlSurface,
    pub layer_surface: ZwlrLayerSurfaceV1,
    pub viewport: WpViewport,
    pub configures_received: Vec<(u32, LayerConfigure)>,
    pub close_requested: bool,

    pub configures_looked_at: usize,
}

pub struct Popup {
    pub qh: QueueHandle<State>,
    pub spbm: WpSinglePixelBufferManagerV1,
    pub seat: WlSeat,

    pub surface: WlSurface,
    pub xdg_surface: XdgSurface,
    pub xdg_popup: XdgPopup,
    pub pending_configure: PopupConfigure,
    pub configures_received: Vec<(u32, PopupConfigure)>,
    /// Set once the compositor dismisses the popup (`xdg_popup.popup_done`).
    pub popup_done: bool,

    pub configures_looked_at: usize,
}

/// A client-side `ext_session_lock_v1`, plus the lock surfaces created on it —
/// a real lock screen makes one per output. `events` records every
/// `locked`/`finished` this object has received, oldest first.
pub struct Lock {
    pub qh: QueueHandle<State>,
    pub spbm: WpSinglePixelBufferManagerV1,
    pub lock: ExtSessionLockV1,
    pub events: Vec<LockEvent>,
    pub surfaces: Vec<LockSurface>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockEvent {
    Locked,
    Finished,
}

/// A `wl_touch` event, recorded without its payload — these scenarios only
/// care which events arrived, not their coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchEvent {
    Down,
    Up,
    Motion,
    Frame,
    Cancel,
}

pub struct LockSurface {
    pub qh: QueueHandle<State>,
    pub spbm: WpSinglePixelBufferManagerV1,

    pub surface: WlSurface,
    pub lock_surface: ExtSessionLockSurfaceV1,
    pub viewport: WpViewport,
    pub configures_received: Vec<(u32, (u32, u32))>,
}

#[derive(Debug, Clone, Default)]
pub struct Configure {
    pub size: (i32, i32),
    pub bounds: Option<(i32, i32)>,
    pub states: Vec<xdg_toplevel::State>,
}

#[derive(Debug, Clone, Copy)]
pub struct LayerConfigure {
    pub size: (u32, u32),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PopupConfigure {
    pub pos: (i32, i32),
    pub size: (i32, i32),
}

#[derive(Clone, Copy, Default)]
pub struct LayerMargin {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

#[derive(Clone, Copy, Default)]
pub struct LayerConfigureProps {
    pub size: Option<(u32, u32)>,
    pub anchor: Option<zwlr_layer_surface_v1::Anchor>,
    pub exclusive_zone: Option<i32>,
    pub margin: Option<LayerMargin>,
    pub kb_interactivity: Option<zwlr_layer_surface_v1::KeyboardInteractivity>,
    pub layer: Option<zwlr_layer_shell_v1::Layer>,
    pub exclusive_edge: Option<zwlr_layer_surface_v1::Anchor>,
}

/// Positioner state for `create_popup_with`. `Default` is a 200×100 popup
/// whose 1×1 anchor rect sits at the parent's top-left corner with neither
/// anchor nor gravity set, so the popup ends up *centered on* that corner.
/// No constraint adjustment either — tests that need the unconstrain logic
/// to move the popup must set `constraint_adjustment` explicitly.
#[derive(Clone, Copy)]
pub struct PopupProps {
    pub size: (i32, i32),
    pub anchor_rect: (i32, i32, i32, i32),
    pub anchor: Anchor,
    pub gravity: Gravity,
    pub offset: (i32, i32),
    pub constraint_adjustment: ConstraintAdjustment,
}

impl Default for PopupProps {
    fn default() -> Self {
        Self {
            size: (200, 100),
            anchor_rect: (0, 0, 1, 1),
            anchor: Anchor::None,
            gravity: Gravity::None,
            offset: (0, 0),
            constraint_adjustment: ConstraintAdjustment::empty(),
        }
    }
}

#[derive(Default)]
pub struct SyncData {
    pub done: AtomicBool,
}

static CLIENT_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(u64);

impl ClientId {
    fn next() -> ClientId {
        ClientId(CLIENT_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

impl fmt::Display for Configure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "size: {} × {}, ", self.size.0, self.size.1)?;
        if let Some(bounds) = self.bounds {
            write!(f, "bounds: {} × {}, ", bounds.0, bounds.1)?;
        } else {
            write!(f, "bounds: none, ")?;
        }
        write!(f, "states: {:?}", self.states)?;
        Ok(())
    }
}

impl fmt::Display for LayerConfigure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "size: {} × {}", self.size.0, self.size.1)?;
        Ok(())
    }
}

impl fmt::Display for PopupConfigure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pos: {} , {}, size: {} × {}",
            self.pos.0, self.pos.1, self.size.0, self.size.1
        )?;
        Ok(())
    }
}

impl Client {
    pub fn new(stream: UnixStream) -> Self {
        let id = ClientId::next();

        let event_loop = EventLoop::try_new().unwrap();
        let backend = Backend::connect(stream).unwrap();
        let connection = Connection::from_backend(backend);
        let queue = connection.new_event_queue();
        let qh = queue.handle();
        WaylandSource::new(connection.clone(), queue)
            .insert(event_loop.handle())
            .unwrap();

        let display = connection.display();
        let _registry = display.get_registry(&qh, ());
        connection.flush().unwrap();

        let state = State {
            qh: qh.clone(),
            globals: Vec::new(),
            outputs: HashMap::new(),
            compositor: None,
            xdg_wm_base: None,
            layer_shell: None,
            spbm: None,
            viewporter: None,
            seat: None,
            touch: None,
            pointer: None,
            pointer_constraints: None,
            xdg_activation: None,
            ext_session_lock_manager: None,
            windows: Vec::new(),
            layers: Vec::new(),
            popups: Vec::new(),
            session_locks: Vec::new(),
            touch_events: Vec::new(),
            pointer_positions: Vec::new(),
            activation_token: None,
            ext_workspace: ExtWorkspace::default(),
        };

        Self {
            id,
            event_loop,
            connection,
            qh,
            display,
            state,
        }
    }

    /// Checks for a protocol error before unwrapping the dispatch result:
    /// calloop's wayland source usually maps a dispatch-step
    /// `WaylandError::Protocol` to a bare `EPROTO`, which would otherwise panic
    /// before revealing the interface, code and message the compositor
    /// actually posted.
    pub fn dispatch(&mut self) {
        let result = self.event_loop.dispatch(Duration::ZERO, &mut self.state);

        if let Some(error) = self.connection.protocol_error() {
            panic!("{error}");
        }

        result.unwrap();
    }

    /// Push queued requests to the compositor without reading anything back —
    /// for a scenario whose next server-side step is expected to kill this
    /// client, where `Fixture::roundtrip` would dispatch it and panic.
    pub fn flush(&self) {
        self.connection.flush().unwrap();
    }

    /// The protocol error the compositor posted on this client, if any — reads
    /// it without panicking, unlike [`Self::dispatch`] (and so
    /// `Fixture::roundtrip`).
    ///
    /// Only catches errors posted on a *live* object: one posted on a destroyed
    /// proxy is never serialized, and reads as a bare socket close (`Io(_)`,
    /// which maps to `None` here) instead.
    ///
    /// Leaves this client's `WaylandSource` errored but still registered in the
    /// fixture's outer loop, so any later `Fixture::roundtrip`/`dispatch` — even
    /// for a different client — panics on it. Call `Fixture::kill_client`
    /// first.
    pub fn protocol_error(&mut self) -> Option<ProtocolError> {
        let result = self.event_loop.dispatch(Duration::ZERO, &mut self.state);
        let error = self.connection.protocol_error();
        // A dispatch failure that isn't a caught protocol error would
        // otherwise vanish behind this `None`, leaving a caller's panic with
        // nothing but its `why` string and no clue what dispatch actually saw.
        if error.is_none()
            && let Err(err) = result
        {
            eprintln!(
                "Client::protocol_error: dispatch failed with no protocol error latched: {err}"
            );
        }
        error
    }

    pub fn send_sync(&self) -> Arc<SyncData> {
        let data = Arc::new(SyncData::default());
        self.display.sync(&self.qh, data.clone());
        self.connection.flush().unwrap();
        data
    }

    /// Request `wl_surface.frame()` on `surface` and commit it — a request only
    /// moves from pending to current double-buffered state on commit. Reuses
    /// the `wl_display.sync` dispatch (`Dispatch<WlCallback, Arc<SyncData>>`)
    /// since it's the same `wl_callback::Event::Done`.
    ///
    /// The commit runs a full commit cycle server-side (marks the output
    /// dirty, re-arranges the layer map, re-runs keyboard focus for a layer
    /// surface) — not inert, so probing focus/animation state around a `frame`
    /// call means probing it across that.
    pub fn frame(&self, surface: &WlSurface) -> Arc<SyncData> {
        let data = Arc::new(SyncData::default());
        surface.frame(&self.qh, data.clone());
        surface.commit();
        self.connection.flush().unwrap();
        data
    }

    pub fn create_window(&mut self) -> &mut Window {
        self.state.create_window()
    }

    pub fn window(&mut self, surface: &WlSurface) -> &mut Window {
        self.state.window(surface)
    }

    pub fn set_opaque_region(&mut self, surface: &WlSurface, rects: &[(i32, i32, i32, i32)]) {
        self.state.set_opaque_region(surface, rects);
    }

    pub fn create_layer(
        &mut self,
        output: Option<&WlOutput>,
        layer: zwlr_layer_shell_v1::Layer,
        namespace: &str,
    ) -> &mut LayerSurface {
        self.state.create_layer(output, layer, namespace.to_owned())
    }

    pub fn recreate_layer(
        &mut self,
        surface: &WlSurface,
        output: Option<&WlOutput>,
        layer: zwlr_layer_shell_v1::Layer,
        namespace: &str,
    ) -> &mut LayerSurface {
        self.state
            .recreate_layer(surface, output, layer, namespace.to_owned())
    }

    pub fn layer(&mut self, surface: &WlSurface) -> &mut LayerSurface {
        self.state.layer(surface)
    }

    /// Create an xdg popup whose parent is the toplevel backing `parent`.
    pub fn create_popup(&mut self, parent: &WlSurface) -> &mut Popup {
        self.state.create_popup(parent)
    }

    /// Create an xdg popup whose parent is the toplevel backing `parent`,
    /// with a custom positioner (see [`PopupProps`]).
    pub fn create_popup_with(&mut self, parent: &WlSurface, props: PopupProps) -> &mut Popup {
        self.state.create_popup_with(parent, props)
    }

    /// Create an xdg popup whose parent is the layer surface backing
    /// `parent` (`zwlr_layer_surface_v1.get_popup`, per protocol issued on an
    /// xdg_popup created with a null xdg parent).
    pub fn create_layer_popup(&mut self, parent: &WlSurface) -> &mut Popup {
        self.state.create_layer_popup(parent)
    }

    /// [`Client::create_layer_popup`] with a custom positioner (see
    /// [`PopupProps`]).
    pub fn create_layer_popup_with(&mut self, parent: &WlSurface, props: PopupProps) -> &mut Popup {
        self.state.create_layer_popup_with(parent, props)
    }

    pub fn popup(&mut self, surface: &WlSurface) -> &mut Popup {
        self.state.popup(surface)
    }

    /// Build an activation token and commit it. `with_serial` decides whether
    /// the token carries a seat serial (an input-driven request) or not (a
    /// spontaneous attention request). The token string arrives asynchronously
    /// via `done`; roundtrip before calling [`Client::activate`].
    pub fn request_activation_token(&mut self, requester: &WlSurface, with_serial: bool) {
        self.state.request_activation_token(requester, with_serial);
    }

    /// Activate `target` with the most recently received token string.
    pub fn activate(&mut self, target: &WlSurface) {
        self.state.activate(target);
    }

    pub fn output(&mut self, name: &str) -> WlOutput {
        self.state
            .outputs
            .iter()
            .find(|(_, v)| *v == name)
            .unwrap()
            .0
            .clone()
    }

    /// Lock the pointer to `surface` with a persistent lifetime. The lock only
    /// becomes active once the pointer is over the surface.
    pub fn lock_pointer(&mut self, surface: &WlSurface) -> ZwpLockedPointerV1 {
        self.state.lock_pointer(surface)
    }

    /// Confine the pointer to `surface` with a persistent lifetime and no
    /// region, i.e. to the whole surface. Unlike a lock, the cursor keeps
    /// moving inside it.
    pub fn confine_pointer(&mut self, surface: &WlSurface) -> ZwpConfinedPointerV1 {
        self.state.confine_pointer(surface)
    }

    /// Send `ext_session_lock_manager_v1.lock`, entering
    /// `SessionLockHandler::lock` on the compositor. The created lock object
    /// is tracked as this client's most recent [`Lock`]; its `locked`/
    /// `finished` events land in [`Client::lock_events`].
    pub fn lock_session(&mut self) {
        self.state.lock_session();
    }

    /// Every `locked`/`finished` event received on this client's most recent
    /// [`Lock`], oldest first.
    pub fn lock_events(&mut self) -> &[LockEvent] {
        &self.state.session_locks.last().unwrap().events
    }

    /// Create a lock surface for `output` on this client's most recent
    /// [`Lock`] (`ext_session_lock_v1.get_lock_surface`).
    pub fn create_lock_surface(&mut self, output: &WlOutput) -> &mut LockSurface {
        self.state.create_lock_surface(output)
    }

    /// Destroy this client's most recent [`Lock`] object without unlocking, and
    /// without disconnecting the client. Legal only while the lock is
    /// unconfirmed — smithay posts `invalid_destroy` once the locker has been
    /// consumed. Any lock surfaces made on it stay alive.
    ///
    /// The destroyed proxy stays in `session_locks`, and stays its `last()`:
    /// every other lock accessor here reaches for the most recent lock, so one
    /// called on this client afterwards silently uses the dead object.
    pub fn destroy_lock_object(&mut self) {
        self.state.destroy_lock_object();
    }

    /// Unlock the session over the wire
    /// (`ext_session_lock_v1.unlock_and_destroy`), the way a lock screen ends a
    /// lock it confirmed. Prefer this to calling `DriftWm::unlock` directly:
    /// only the request clears smithay's own `locked_outputs`.
    ///
    /// Like [`Self::destroy_lock_object`], the destroyed proxy stays `last()` in
    /// `session_locks`, so any lock accessor called on this client afterwards
    /// silently uses the dead object. The lock surfaces made on it are still
    /// reachable through [`Self::lock_surface`].
    pub fn unlock_session(&mut self) {
        self.state.unlock_session();
    }

    /// Take a fresh `ext_session_lock_surface_v1` on a `wl_surface` that already
    /// carried one, on this client's most recent [`Lock`] — a lock screen
    /// re-locking on the surfaces it kept. The tracked entry is swapped over to
    /// the new role and moved onto that lock, so both [`Self::lock_surface`] and
    /// [`Self::last_lock_surface`] return it.
    pub fn retake_lock_surface(
        &mut self,
        surface: &WlSurface,
        output: &WlOutput,
    ) -> &mut LockSurface {
        self.state.retake_lock_surface(surface, output)
    }

    pub fn lock_surface(&mut self, surface: &WlSurface) -> &mut LockSurface {
        self.state.lock_surface(surface)
    }

    /// The lock surface most recently created on this client's most recent
    /// [`Lock`] — the handle back to one a helper made on the caller's behalf.
    pub fn last_lock_surface(&mut self) -> &mut LockSurface {
        self.state.last_lock_surface()
    }
}

impl State {
    pub fn create_window(&mut self) -> &mut Window {
        let compositor = self.compositor.as_ref().unwrap();
        let xdg_wm_base = self.xdg_wm_base.as_ref().unwrap();
        let viewporter = self.viewporter.as_ref().unwrap();

        let surface = compositor.create_surface(&self.qh, ());
        let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, &self.qh, ());
        let xdg_toplevel = xdg_surface.get_toplevel(&self.qh, ());
        let viewport = viewporter.get_viewport(&surface, &self.qh, ());

        let window = Window {
            qh: self.qh.clone(),
            spbm: self.spbm.clone().unwrap(),
            seat: self.seat.clone().unwrap(),

            surface,
            xdg_surface,
            xdg_toplevel,
            viewport,
            pending_configure: Configure::default(),
            configures_received: Vec::new(),
            close_requested: false,

            configures_looked_at: 0,
        };

        self.windows.push(window);
        self.windows.last_mut().unwrap()
    }

    pub fn window(&mut self, surface: &WlSurface) -> &mut Window {
        self.windows
            .iter_mut()
            .find(|w| w.surface == *surface)
            .unwrap()
    }

    /// Set `surface`'s pending opaque region to the union of `rects`
    /// (surface-local `(x, y, w, h)`), the same request a real CSD client
    /// issues to declare which part of its buffer is fully opaque. Takes
    /// effect on the surface's next commit.
    pub fn set_opaque_region(&mut self, surface: &WlSurface, rects: &[(i32, i32, i32, i32)]) {
        let compositor = self.compositor.as_ref().unwrap();
        let region = compositor.create_region(&self.qh, ());
        for &(x, y, w, h) in rects {
            region.add(x, y, w, h);
        }
        surface.set_opaque_region(Some(&region));
        region.destroy();
    }

    pub fn create_layer(
        &mut self,
        output: Option<&WlOutput>,
        layer: zwlr_layer_shell_v1::Layer,
        namespace: String,
    ) -> &mut LayerSurface {
        let compositor = self.compositor.as_ref().unwrap();
        let layer_shell = self.layer_shell.as_ref().unwrap();
        let viewporter = self.viewporter.as_ref().unwrap();

        let surface = compositor.create_surface(&self.qh, ());
        let layer_surface =
            layer_shell.get_layer_surface(&surface, output, layer, namespace, &self.qh, ());
        let viewport = viewporter.get_viewport(&surface, &self.qh, ());

        let layer_surface = LayerSurface {
            qh: self.qh.clone(),
            spbm: self.spbm.clone().unwrap(),

            surface,
            layer_surface,
            viewport,
            configures_received: Vec::new(),
            close_requested: false,

            configures_looked_at: 0,
        };

        self.layers.push(layer_surface);
        self.layers.last_mut().unwrap()
    }

    /// Destroy the layer role on `surface` and take a fresh one on the same
    /// `wl_surface` — what an OSD does when it re-arms. The role is swapped in
    /// place: a second entry for one `wl_surface` would shadow the first in
    /// [`State::layer`] and panic the configure dispatch, and a second
    /// `wp_viewport` on it is a protocol error.
    pub fn recreate_layer(
        &mut self,
        surface: &WlSurface,
        output: Option<&WlOutput>,
        layer: zwlr_layer_shell_v1::Layer,
        namespace: String,
    ) -> &mut LayerSurface {
        let shell = self.layer_shell.clone().unwrap();
        let entry = self
            .layers
            .iter_mut()
            .find(|l| l.surface == *surface)
            .unwrap();
        entry.layer_surface.destroy();
        entry.layer_surface =
            shell.get_layer_surface(surface, output, layer, namespace, &self.qh, ());
        entry.configures_received.clear();
        entry.configures_looked_at = 0;
        entry.close_requested = false;
        entry
    }

    pub fn layer(&mut self, surface: &WlSurface) -> &mut LayerSurface {
        self.layers
            .iter_mut()
            .find(|w| w.surface == *surface)
            .unwrap()
    }

    /// Build an xdg positioner from `props`. Only consumed by `get_popup`;
    /// the caller destroys it right after.
    fn build_positioner(&self, props: PopupProps) -> XdgPositioner {
        let xdg_wm_base = self.xdg_wm_base.as_ref().unwrap();
        let positioner = xdg_wm_base.create_positioner(&self.qh, ());
        positioner.set_size(props.size.0, props.size.1);
        let (x, y, w, h) = props.anchor_rect;
        positioner.set_anchor_rect(x, y, w, h);
        positioner.set_anchor(props.anchor);
        positioner.set_gravity(props.gravity);
        positioner.set_constraint_adjustment(props.constraint_adjustment);
        positioner.set_offset(props.offset.0, props.offset.1);
        positioner
    }

    pub fn create_popup(&mut self, parent: &WlSurface) -> &mut Popup {
        self.create_popup_with(parent, PopupProps::default())
    }

    pub fn create_popup_with(&mut self, parent: &WlSurface, props: PopupProps) -> &mut Popup {
        let parent_xdg = self
            .windows
            .iter()
            .find(|w| w.surface == *parent)
            .unwrap()
            .xdg_surface
            .clone();

        let positioner = self.build_positioner(props);

        let compositor = self.compositor.as_ref().unwrap();
        let xdg_wm_base = self.xdg_wm_base.as_ref().unwrap();
        let surface = compositor.create_surface(&self.qh, ());
        let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, &self.qh, ());
        let xdg_popup = xdg_surface.get_popup(Some(&parent_xdg), &positioner, &self.qh, ());
        positioner.destroy();

        self.finish_popup(surface, xdg_surface, xdg_popup)
    }

    pub fn create_layer_popup(&mut self, parent: &WlSurface) -> &mut Popup {
        self.create_layer_popup_with(parent, PopupProps::default())
    }

    pub fn create_layer_popup_with(&mut self, parent: &WlSurface, props: PopupProps) -> &mut Popup {
        let parent_layer = self
            .layers
            .iter()
            .find(|l| l.surface == *parent)
            .unwrap()
            .layer_surface
            .clone();

        let positioner = self.build_positioner(props);

        let compositor = self.compositor.as_ref().unwrap();
        let xdg_wm_base = self.xdg_wm_base.as_ref().unwrap();
        let surface = compositor.create_surface(&self.qh, ());
        let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, &self.qh, ());
        // xdg parent must be null: the layer surface assigns itself as parent
        // via `get_popup` below, before the popup's initial commit.
        let xdg_popup = xdg_surface.get_popup(None, &positioner, &self.qh, ());
        positioner.destroy();
        parent_layer.get_popup(&xdg_popup);

        self.finish_popup(surface, xdg_surface, xdg_popup)
    }

    fn finish_popup(
        &mut self,
        surface: WlSurface,
        xdg_surface: XdgSurface,
        xdg_popup: XdgPopup,
    ) -> &mut Popup {
        let popup = Popup {
            qh: self.qh.clone(),
            spbm: self.spbm.clone().unwrap(),
            seat: self.seat.clone().unwrap(),

            surface,
            xdg_surface,
            xdg_popup,
            pending_configure: PopupConfigure::default(),
            configures_received: Vec::new(),
            popup_done: false,

            configures_looked_at: 0,
        };

        self.popups.push(popup);
        self.popups.last_mut().unwrap()
    }

    pub fn popup(&mut self, surface: &WlSurface) -> &mut Popup {
        self.popups
            .iter_mut()
            .find(|p| p.surface == *surface)
            .unwrap()
    }

    pub fn request_activation_token(&mut self, requester: &WlSurface, with_serial: bool) {
        let activation = self.xdg_activation.as_ref().unwrap();
        let token = activation.get_activation_token(&self.qh, ());
        if with_serial {
            let seat = self.seat.as_ref().unwrap();
            token.set_serial(1, seat);
        }
        token.set_surface(requester);
        token.commit();

        self.activation_token = None;
    }

    pub fn activate(&mut self, target: &WlSurface) {
        let activation = self.xdg_activation.as_ref().unwrap();
        let token = self.activation_token.clone().unwrap();
        activation.activate(token, target);
    }

    pub fn lock_pointer(&mut self, surface: &WlSurface) -> ZwpLockedPointerV1 {
        let constraints = self.pointer_constraints.as_ref().unwrap();
        let pointer = self.pointer.as_ref().unwrap();
        constraints.lock_pointer(surface, pointer, None, Lifetime::Persistent, &self.qh, ())
    }

    pub fn confine_pointer(&mut self, surface: &WlSurface) -> ZwpConfinedPointerV1 {
        let constraints = self.pointer_constraints.as_ref().unwrap();
        let pointer = self.pointer.as_ref().unwrap();
        constraints.confine_pointer(surface, pointer, None, Lifetime::Persistent, &self.qh, ())
    }

    pub fn lock_session(&mut self) {
        let manager = self.ext_session_lock_manager.as_ref().unwrap();
        let lock = manager.lock(&self.qh, ());
        self.session_locks.push(Lock {
            qh: self.qh.clone(),
            spbm: self.spbm.clone().unwrap(),
            lock,
            events: Vec::new(),
            surfaces: Vec::new(),
        });
    }

    pub fn create_lock_surface(&mut self, output: &WlOutput) -> &mut LockSurface {
        let compositor = self.compositor.as_ref().unwrap();
        let viewporter = self.viewporter.as_ref().unwrap();
        let lock = self.session_locks.last().unwrap().lock.clone();

        let surface = compositor.create_surface(&self.qh, ());
        let lock_surface = lock.get_lock_surface(&surface, output, &self.qh, ());
        let viewport = viewporter.get_viewport(&surface, &self.qh, ());

        let lock_surface = LockSurface {
            qh: self.qh.clone(),
            spbm: self.spbm.clone().unwrap(),

            surface,
            lock_surface,
            viewport,
            configures_received: Vec::new(),
        };

        let lock = self.session_locks.last_mut().unwrap();
        lock.surfaces.push(lock_surface);
        lock.surfaces.last_mut().unwrap()
    }

    /// The role is swapped in place and the entry moved onto the current lock,
    /// as [`Self::recreate_layer`] does: a second entry for one `wl_surface`
    /// would shadow the first in [`Self::lock_surface`], handing later helpers
    /// the dead role proxy, and a second `wp_viewport` on it is a protocol
    /// error. Destroying the old proxy here is inert when the caller already
    /// did it — which the orphan scenarios must, since they commit in between.
    ///
    /// Must be called on a freshly created lock, after an unlock — not a
    /// second surface on the *same* still-locked lock. `get_lock_surface`'s
    /// `DuplicateOutput` guard (`session_lock/lock.rs:59-62`) checks
    /// `locked_outputs`, which still holds this client's cached `wl_output`
    /// binding from the first surface, and [`Client::output`] hands back that
    /// same binding — so a same-lock retake trips the guard and kills the
    /// client.
    pub fn retake_lock_surface(
        &mut self,
        surface: &WlSurface,
        output: &WlOutput,
    ) -> &mut LockSurface {
        let lock = self.session_locks.last().unwrap().lock.clone();
        let (index, position) = self
            .session_locks
            .iter()
            .enumerate()
            .find_map(|(index, l)| {
                let position = l.surfaces.iter().position(|s| s.surface == *surface)?;
                Some((index, position))
            })
            .unwrap();
        let mut entry = self.session_locks[index].surfaces.remove(position);

        entry.lock_surface.destroy();
        entry.lock_surface = lock.get_lock_surface(surface, output, &self.qh, ());
        entry.configures_received.clear();

        let lock = self.session_locks.last_mut().unwrap();
        lock.surfaces.push(entry);
        lock.surfaces.last_mut().unwrap()
    }

    pub fn lock_surface(&mut self, surface: &WlSurface) -> &mut LockSurface {
        self.session_locks
            .iter_mut()
            .flat_map(|l| l.surfaces.iter_mut())
            .find(|s| s.surface == *surface)
            .unwrap()
    }

    pub fn last_lock_surface(&mut self) -> &mut LockSurface {
        self.session_locks
            .last_mut()
            .unwrap()
            .surfaces
            .last_mut()
            .unwrap()
    }

    pub fn destroy_lock_object(&mut self) {
        self.session_locks.last().unwrap().lock.destroy();
    }

    pub fn unlock_session(&mut self) {
        self.session_locks.last().unwrap().lock.unlock_and_destroy();
    }
}

impl Window {
    pub fn commit(&self) {
        self.surface.commit();
    }

    /// Clean client-initiated close: tear down the xdg role and surface in
    /// protocol order (toplevel → xdg_surface → viewport → wl_surface). The
    /// server sees `toplevel_destroyed` on the next dispatch.
    pub fn destroy(&self) {
        self.xdg_toplevel.destroy();
        self.xdg_surface.destroy();
        self.viewport.destroy();
        self.surface.destroy();
    }

    pub fn ack_last(&self) {
        let serial = self.configures_received.last().unwrap().0;
        self.xdg_surface.ack_configure(serial);
    }

    pub fn ack_last_and_commit(&self) {
        self.ack_last();
        self.commit();
    }

    pub fn attach_new_buffer(&self) {
        let buffer = self.spbm.create_u32_rgba_buffer(0, 0, 0, 0, &self.qh, ());
        self.surface.attach(Some(&buffer), 0, 0);
    }

    pub fn attach_null(&self) {
        self.surface.attach(None, 0, 0);
    }

    pub fn set_size(&self, w: u16, h: u16) {
        self.viewport.set_destination(i32::from(w), i32::from(h));
    }

    /// Declare a minimum size, the floor the compositor's `SizeConstraints`
    /// clamps a resize request to. Applies on the next commit.
    pub fn set_min_size(&self, w: i32, h: i32) {
        self.xdg_toplevel.set_min_size(w, h);
    }

    /// Declare a maximum size, the ceiling `SizeConstraints` clamps to.
    pub fn set_max_size(&self, w: i32, h: i32) {
        self.xdg_toplevel.set_max_size(w, h);
    }

    /// Declare the visible bounds inside the surface: it extends `(x, y)` beyond
    /// the window at the top-left, the way a client drawing its own shadows does.
    pub fn set_geometry(&self, x: i32, y: i32, w: i32, h: i32) {
        self.xdg_surface.set_window_geometry(x, y, w, h);
    }

    pub fn set_fullscreen(&self, output: Option<&WlOutput>) {
        self.xdg_toplevel.set_fullscreen(output);
    }

    pub fn unset_fullscreen(&self) {
        self.xdg_toplevel.unset_fullscreen();
    }

    pub fn set_maximized(&self) {
        self.xdg_toplevel.set_maximized();
    }

    pub fn unset_maximized(&self) {
        self.xdg_toplevel.unset_maximized();
    }

    /// Ask the compositor to resize this window from `edges`, as a CSD client
    /// does when the user drags its own border. The serial isn't validated —
    /// the compositor gates the request on the seat's pointer grab instead.
    pub fn resize(&self, edges: xdg_toplevel::ResizeEdge, serial: u32) {
        self.xdg_toplevel.resize(&self.seat, serial, edges);
    }

    pub fn set_parent(&self, parent: Option<&XdgToplevel>) {
        self.xdg_toplevel.set_parent(parent);
    }

    pub fn set_title(&self, title: &str) {
        self.xdg_toplevel.set_title(title.to_owned());
    }

    /// Window rules match on `app_id`; scenarios that exercise them must set
    /// one before the first commit.
    pub fn set_app_id(&self, app_id: &str) {
        self.xdg_toplevel.set_app_id(app_id.to_owned());
    }

    pub fn recent_configures(&mut self) -> impl Iterator<Item = &Configure> {
        let start = self.configures_looked_at;
        self.configures_looked_at = self.configures_received.len();
        self.configures_received[start..].iter().map(|(_, c)| c)
    }

    pub fn format_recent_configures(&mut self) -> String {
        let mut buf = String::new();
        for configure in self.recent_configures() {
            if !buf.is_empty() {
                buf.push('\n');
            }
            write!(buf, "{configure}").unwrap();
        }
        buf
    }
}

impl LayerSurface {
    pub fn commit(&self) {
        self.surface.commit();
    }

    pub fn ack_last(&self) {
        let serial = self.configures_received.last().unwrap().0;
        self.layer_surface.ack_configure(serial);
    }

    pub fn ack_last_and_commit(&self) {
        self.ack_last();
        self.commit();
    }

    pub fn set_configure_props(&self, props: LayerConfigureProps) {
        let LayerConfigureProps {
            size,
            anchor,
            exclusive_zone,
            margin,
            kb_interactivity,
            layer,
            exclusive_edge,
        } = props;

        if let Some(x) = size {
            self.layer_surface.set_size(x.0, x.1);
        }
        if let Some(x) = anchor {
            self.layer_surface.set_anchor(x);
        }
        if let Some(x) = exclusive_zone {
            self.layer_surface.set_exclusive_zone(x);
        }
        if let Some(x) = margin {
            self.layer_surface
                .set_margin(x.top, x.right, x.bottom, x.left);
        }
        if let Some(x) = kb_interactivity {
            self.layer_surface.set_keyboard_interactivity(x);
        }
        if let Some(x) = layer {
            self.layer_surface.set_layer(x);
        }
        if let Some(x) = exclusive_edge {
            self.layer_surface.set_exclusive_edge(x);
        }
    }

    pub fn attach_new_buffer(&self) {
        let buffer = self.spbm.create_u32_rgba_buffer(0, 0, 0, 0, &self.qh, ());
        self.surface.attach(Some(&buffer), 0, 0);
    }

    pub fn attach_null(&self) {
        self.surface.attach(None, 0, 0);
    }

    pub fn set_size(&self, w: u16, h: u16) {
        self.viewport.set_destination(i32::from(w), i32::from(h));
    }

    pub fn recent_configures(&mut self) -> impl Iterator<Item = &LayerConfigure> {
        let start = self.configures_looked_at;
        self.configures_looked_at = self.configures_received.len();
        self.configures_received[start..].iter().map(|(_, c)| c)
    }

    pub fn format_recent_configures(&mut self) -> String {
        let mut buf = String::new();
        for configure in self.recent_configures() {
            if !buf.is_empty() {
                buf.push('\n');
            }
            write!(buf, "{configure}").unwrap();
        }
        buf
    }
}

impl Popup {
    pub fn commit(&self) {
        self.surface.commit();
    }

    /// Tear down the popup role and surface in protocol order (popup →
    /// xdg_surface → wl_surface).
    pub fn destroy(&self) {
        self.xdg_popup.destroy();
        self.xdg_surface.destroy();
        self.surface.destroy();
    }

    /// Request a popup grab with `serial`. Must be sent before the popup is
    /// mapped (before it attaches a buffer), or the compositor raises
    /// `invalid_grab`. Tests pass stale/bogus serials here on purpose.
    pub fn grab(&self, serial: u32) {
        self.xdg_popup.grab(&self.seat, serial);
    }

    pub fn ack_last(&self) {
        let serial = self.configures_received.last().unwrap().0;
        self.xdg_surface.ack_configure(serial);
    }

    pub fn ack_last_and_commit(&self) {
        self.ack_last();
        self.commit();
    }

    pub fn attach_new_buffer(&self) {
        let buffer = self.spbm.create_u32_rgba_buffer(0, 0, 0, 0, &self.qh, ());
        self.surface.attach(Some(&buffer), 0, 0);
    }

    pub fn recent_configures(&mut self) -> impl Iterator<Item = &PopupConfigure> {
        let start = self.configures_looked_at;
        self.configures_looked_at = self.configures_received.len();
        self.configures_received[start..].iter().map(|(_, c)| c)
    }

    pub fn format_recent_configures(&mut self) -> String {
        let mut buf = String::new();
        for configure in self.recent_configures() {
            if !buf.is_empty() {
                buf.push('\n');
            }
            write!(buf, "{configure}").unwrap();
        }
        buf
    }
}

impl LockSurface {
    pub fn commit(&self) {
        self.surface.commit();
    }

    pub fn ack_last(&self) {
        let serial = self.configures_received.last().unwrap().0;
        self.lock_surface.ack_configure(serial);
    }

    pub fn ack_last_and_commit(&self) {
        self.ack_last();
        self.commit();
    }

    pub fn attach_new_buffer(&self) {
        let buffer = self.spbm.create_u32_rgba_buffer(0, 0, 0, 0, &self.qh, ());
        self.surface.attach(Some(&buffer), 0, 0);
    }

    pub fn attach_null(&self) {
        self.surface.attach(None, 0, 0);
    }

    /// Destroy the `ext_session_lock_surface_v1` role, leaving the `wl_surface`
    /// alive — what a toolkit resetting the surface's role does.
    pub fn destroy_role(&self) {
        self.lock_surface.destroy();
    }

    /// Scale the 1×1 buffer to `(w, h)` via `wp_viewport`, matching a
    /// `LockSurfaceState` size — the compositor rejects a mismatch.
    pub fn set_size(&self, w: u32, h: u32) {
        self.viewport.set_destination(w as i32, h as i32);
    }
}

impl Dispatch<WlCallback, Arc<SyncData>> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlCallback,
        event: <WlCallback as wayland_client::Proxy>::Event,
        data: &Arc<SyncData>,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_callback::Event::Done { .. } => data.done.store(true, Ordering::Relaxed),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == WlCompositor::interface().name {
                    let version = min(version, WlCompositor::interface().version);
                    state.compositor = Some(registry.bind(name, version, qh, ()));
                } else if interface == XdgWmBase::interface().name {
                    let version = min(version, XdgWmBase::interface().version);
                    state.xdg_wm_base = Some(registry.bind(name, version, qh, ()));
                } else if interface == ZwlrLayerShellV1::interface().name {
                    let version = min(version, ZwlrLayerShellV1::interface().version);
                    state.layer_shell = Some(registry.bind(name, version, qh, ()));
                } else if interface == WpSinglePixelBufferManagerV1::interface().name {
                    let version = min(version, WpSinglePixelBufferManagerV1::interface().version);
                    state.spbm = Some(registry.bind(name, version, qh, ()));
                } else if interface == WpViewporter::interface().name {
                    let version = min(version, WpViewporter::interface().version);
                    state.viewporter = Some(registry.bind(name, version, qh, ()));
                } else if interface == WlSeat::interface().name {
                    let version = min(version, WlSeat::interface().version);
                    let seat: WlSeat = registry.bind(name, version, qh, ());
                    state.touch = Some(seat.get_touch(qh, ()));
                    state.pointer = Some(seat.get_pointer(qh, ()));
                    state.seat = Some(seat);
                } else if interface == ZwpPointerConstraintsV1::interface().name {
                    let version = min(version, ZwpPointerConstraintsV1::interface().version);
                    state.pointer_constraints = Some(registry.bind(name, version, qh, ()));
                } else if interface == XdgActivationV1::interface().name {
                    let version = min(version, XdgActivationV1::interface().version);
                    state.xdg_activation = Some(registry.bind(name, version, qh, ()));
                } else if interface == WlOutput::interface().name {
                    let version = min(version, WlOutput::interface().version);
                    let output = registry.bind(name, version, qh, ());
                    state.outputs.insert(output, String::new());
                } else if interface == ExtWorkspaceManagerV1::interface().name {
                    let version = min(version, ExtWorkspaceManagerV1::interface().version);
                    state.ext_workspace.manager = Some(registry.bind(name, version, qh, ()));
                } else if interface == ExtSessionLockManagerV1::interface().name {
                    let version = min(version, ExtSessionLockManagerV1::interface().version);
                    state.ext_session_lock_manager = Some(registry.bind(name, version, qh, ()));
                }

                let global = Global {
                    name,
                    interface,
                    version,
                };
                state.globals.push(global);
            }
            wl_registry::Event::GlobalRemove { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: <WlOutput as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Geometry { .. } => (),
            wl_output::Event::Mode { .. } => (),
            wl_output::Event::Done => (),
            wl_output::Event::Scale { .. } => (),
            wl_output::Event::Name { name } => {
                *state.outputs.get_mut(output).unwrap() = name;
            }
            wl_output::Event::Description { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _manager: &ExtWorkspaceManagerV1,
        event: <ExtWorkspaceManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                state.ext_workspace.group = Some(workspace_group);
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                state.ext_workspace.workspaces.push(WorkspaceRecord {
                    handle: workspace,
                    id: None,
                    name: None,
                    capabilities: None,
                    state: None,
                    removed: false,
                });
            }
            ext_workspace_manager_v1::Event::Done => state.ext_workspace.done_count += 1,
            ext_workspace_manager_v1::Event::Finished => state.ext_workspace.finished = true,
            _ => unreachable!(),
        }
    }

    wayland_client::event_created_child!(State, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        _group: &ExtWorkspaceGroupHandleV1,
        event: <ExtWorkspaceGroupHandleV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_group_handle_v1::Event::Capabilities { capabilities } => {
                state.ext_workspace.group_capabilities = Some(capabilities.into());
            }
            ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
                state.ext_workspace.output_enters.push(output);
            }
            ext_workspace_group_handle_v1::Event::OutputLeave { output } => {
                state.ext_workspace.output_leaves.push(output);
            }
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                state.ext_workspace.workspace_enters.push(workspace);
            }
            ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
                state.ext_workspace.workspace_leaves.push(workspace);
            }
            ext_workspace_group_handle_v1::Event::Removed => {
                state.ext_workspace.group = None;
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtWorkspaceHandleV1,
        event: <ExtWorkspaceHandleV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let Some(record) = state
            .ext_workspace
            .workspaces
            .iter_mut()
            .find(|w| &w.handle == handle)
        else {
            return;
        };
        match event {
            ext_workspace_handle_v1::Event::Id { id } => record.id = Some(id),
            ext_workspace_handle_v1::Event::Name { name } => record.name = Some(name),
            ext_workspace_handle_v1::Event::Capabilities { capabilities } => {
                record.capabilities = Some(capabilities.into());
            }
            ext_workspace_handle_v1::Event::State { state } => {
                record.state = Some(state.into());
            }
            ext_workspace_handle_v1::Event::Coordinates { .. } => (),
            ext_workspace_handle_v1::Event::Removed => record.removed = true,
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlCompositor, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WlRegion, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegion,
        _event: <WlRegion as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _state: &mut Self,
        xdg_wm_base: &XdgWmBase,
        event: <XdgWmBase as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            xdg_wm_base::Event::Ping { serial } => {
                xdg_wm_base.pong(serial);
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrLayerShellV1,
        _event: <ZwlrLayerShellV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        event: <WlSurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::Enter { .. } => (),
            wl_surface::Event::Leave { .. } => (),
            wl_surface::Event::PreferredBufferScale { .. } => (),
            wl_surface::Event::PreferredBufferTransform { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: <XdgSurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            xdg_surface::Event::Configure { serial } => {
                if let Some(window) = state
                    .windows
                    .iter_mut()
                    .find(|w| w.xdg_surface == *xdg_surface)
                {
                    let configure = window.pending_configure.clone();
                    window.configures_received.push((serial, configure));
                } else if let Some(popup) = state
                    .popups
                    .iter_mut()
                    .find(|p| p.xdg_surface == *xdg_surface)
                {
                    let configure = popup.pending_configure;
                    popup.configures_received.push((serial, configure));
                } else {
                    // Entries are never removed, so an unmatched xdg_surface is
                    // a harness bug; a swallowed configure would surface later
                    // as an opaque hang.
                    unreachable!();
                }
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        xdg_toplevel: &XdgToplevel,
        event: <XdgToplevel as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let window = state
            .windows
            .iter_mut()
            .find(|w| w.xdg_toplevel == *xdg_toplevel)
            .unwrap();

        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                let configure = &mut window.pending_configure;
                configure.size = (width, height);
                configure.states = states
                    .chunks_exact(4)
                    .flat_map(TryInto::<[u8; 4]>::try_into)
                    .map(u32::from_ne_bytes)
                    .flat_map(xdg_toplevel::State::try_from)
                    .collect();
            }
            xdg_toplevel::Event::Close => {
                window.close_requested = true;
            }
            xdg_toplevel::Event::ConfigureBounds { width, height } => {
                window.pending_configure.bounds = Some((width, height));
            }
            xdg_toplevel::Event::WmCapabilities { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let layer_surface = state
            .layers
            .iter_mut()
            .find(|w| w.layer_surface == *layer_surface)
            .unwrap();

        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                let configure = LayerConfigure {
                    size: (width, height),
                };
                layer_surface.configures_received.push((serial, configure));
            }
            zwlr_layer_surface_v1::Event::Closed => layer_surface.close_requested = true,
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        event: <WlBuffer as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_buffer::Event::Release => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WpSinglePixelBufferManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpSinglePixelBufferManagerV1,
        _event: <WpSinglePixelBufferManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WpViewporter, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewporter,
        _event: <WpViewporter as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WpViewport, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: <WpViewport as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_seat::Event::Capabilities { .. } => (),
            wl_seat::Event::Name { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlTouch, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlTouch,
        event: <WlTouch as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let recorded = match event {
            wl_touch::Event::Down { .. } => TouchEvent::Down,
            wl_touch::Event::Up { .. } => TouchEvent::Up,
            wl_touch::Event::Motion { .. } => TouchEvent::Motion,
            wl_touch::Event::Frame => TouchEvent::Frame,
            wl_touch::Event::Cancel => TouchEvent::Cancel,
            _ => unreachable!(),
        };
        state.touch_events.push(recorded);
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlPointer,
        event: <WlPointer as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // `enter` carries a position too, so a regression that dropped focus and
        // re-entered instead of re-motioning would still land one on the client.
        match event {
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => state.pointer_positions.push((surface_x, surface_y)),
            _ => (),
        }
    }
}

impl Dispatch<ZwpPointerConstraintsV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpPointerConstraintsV1,
        _event: <ZwpPointerConstraintsV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpLockedPointerV1,
        event: <ZwpLockedPointerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => (),
            zwp_locked_pointer_v1::Event::Unlocked => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ZwpConfinedPointerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpConfinedPointerV1,
        event: <ZwpConfinedPointerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwp_confined_pointer_v1::Event::Confined => (),
            zwp_confined_pointer_v1::Event::Unconfined => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<XdgPositioner, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &XdgPositioner,
        _event: <XdgPositioner as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<XdgPopup, ()> for State {
    fn event(
        state: &mut Self,
        xdg_popup: &XdgPopup,
        event: <XdgPopup as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let popup = state
            .popups
            .iter_mut()
            .find(|p| p.xdg_popup == *xdg_popup)
            .unwrap();

        match event {
            xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } => {
                popup.pending_configure.pos = (x, y);
                popup.pending_configure.size = (width, height);
            }
            xdg_popup::Event::PopupDone => popup.popup_done = true,
            xdg_popup::Event::Repositioned { .. } => (),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ExtSessionLockManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtSessionLockManagerV1,
        _event: <ExtSessionLockManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<ExtSessionLockV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtSessionLockV1,
        event: <ExtSessionLockV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let lock = state
            .session_locks
            .iter_mut()
            .find(|l| l.lock == *proxy)
            .unwrap();

        match event {
            ext_session_lock_v1::Event::Locked => lock.events.push(LockEvent::Locked),
            ext_session_lock_v1::Event::Finished => lock.events.push(LockEvent::Finished),
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ExtSessionLockSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        lock_surface: &ExtSessionLockSurfaceV1,
        event: <ExtSessionLockSurfaceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let lock_surface = state
            .session_locks
            .iter_mut()
            .flat_map(|l| l.surfaces.iter_mut())
            .find(|s| s.lock_surface == *lock_surface)
            .unwrap();

        match event {
            ext_session_lock_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                lock_surface
                    .configures_received
                    .push((serial, (width, height)));
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<XdgActivationV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &XdgActivationV1,
        _event: <XdgActivationV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<XdgActivationTokenV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &XdgActivationTokenV1,
        event: <XdgActivationTokenV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            xdg_activation_token_v1::Event::Done { token } => {
                state.activation_token = Some(token);
                // The token object is single-use; the protocol expects destroy
                // after done.
                proxy.destroy();
            }
            _ => unreachable!(),
        }
    }
}
