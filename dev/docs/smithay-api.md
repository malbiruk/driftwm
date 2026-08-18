# Smithay 0.7.0 API Reference

Quick reference for key smithay APIs used in driftwm. See the source at
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/smithay-0.7.0/`.

## PointerGrab System

### `PointerGrab<D>` trait
Source: `src/input/pointer/grab.rs`

13-method trait for intercepting pointer events during a grab:
```rust
trait PointerGrab<D: SeatHandler>: Send + Downcast {
    fn motion(&mut self, data: &mut D, handle: &mut PointerInnerHandle<'_, D>,
              focus: Option<(PointerFocus, Point<f64, Logical>)>, event: &MotionEvent);
    fn relative_motion(&mut self, data: &mut D, handle: &mut PointerInnerHandle<'_, D>,
                       focus: Option<(PointerFocus, Point<f64, Logical>)>, event: &RelativeMotionEvent);
    fn button(&mut self, data: &mut D, handle: &mut PointerInnerHandle<'_, D>, event: &ButtonEvent);
    fn axis(&mut self, data: &mut D, handle: &mut PointerInnerHandle<'_, D>, details: AxisFrame);
    fn frame(&mut self, data: &mut D, handle: &mut PointerInnerHandle<'_, D>);
    fn gesture_swipe_begin/update/end(...);  // 3 methods
    fn gesture_pinch_begin/update/end(...);  // 3 methods
    fn gesture_hold_begin/end(...);          // 2 methods
    fn start_data(&self) -> &GrabStartData<D>;
    fn unset(&mut self, data: &mut D);
}
```

### `GrabStartData<D>`
```rust
pub struct GrabStartData<D: SeatHandler> {
    pub focus: Option<(<D as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
    pub button: u32,
    pub location: Point<f64, Logical>,
}
```

### `PointerHandle` (external API)
```rust
impl PointerHandle<D> {
    fn set_grab(&self, data: &mut D, grab: G, serial: Serial, focus: Focus);
    fn unset_grab(&self, data: &mut D, serial: Serial, time: u32);
    fn button(&self, data: &mut D, event: &ButtonEvent);
    // button() updates pressed_buttons BEFORE calling grab.button()
    fn grab_start_data(&self) -> Option<GrabStartData<D>>;
    fn current_location(&self) -> Point<f64, Logical>;
}
```

### `PointerInnerHandle` (inside grab methods)
```rust
impl PointerInnerHandle<'_, D> {
    fn motion(&mut self, data: &mut D, focus: Option<(Focus, Point)>, event: &MotionEvent);
    fn button(&mut self, data: &mut D, event: &ButtonEvent);
    fn axis(&mut self, data: &mut D, details: AxisFrame);
    fn frame(&mut self, data: &mut D);
    fn unset_grab(&mut self, handler: &mut dyn PointerGrab<D>, data: &mut D,
                  serial: Serial, time: u32, restore_focus: bool);
    fn current_pressed(&self) -> &[u32];
    fn current_focus(&self) -> Option<(PointerFocus, Point<f64, Logical>)>;
    fn current_location(&self) -> Point<f64, Logical>;
    // + gesture forwarding methods
}
```

### `Focus` enum
```rust
pub enum Focus { Keep, Clear }
```

## Key Patterns

### DataMap (surface user data)
Source: `src/utils/user_data.rs`

```rust
// get_or_insert returns &T (immutable!) — use RefCell for mutation
states.data_map.get_or_insert(|| RefCell::new(MyState::default())).borrow()     // read
states.data_map.get_or_insert(|| RefCell::new(MyState::default())).replace(val) // write
```

### xdg_toplevel::ResizeEdge
Plain enum (NOT bitflags). Values: None=0, Top=1, Bottom=2, Left=4, Right=8,
TopLeft=5, TopRight=9, BottomLeft=6, BottomRight=10.
Use `(edge as u32) & bit` for component checks.

### ToplevelSurface resize protocol
```rust
toplevel.with_pending_state(|state| {
    state.size = Some(new_size);
    state.states.set(xdg_toplevel::State::Resizing);
});
toplevel.send_pending_configure();
```

### Detecting an unacked configure
`XdgToplevelSurfaceRoleAttributes::pending_configures() -> &[ToplevelConfigure]`
is public, reached via `with_states(surface, |s| s.data_map.get::<XdgToplevelSurfaceData>())`
(`XdgToplevelSurfaceData = Mutex<XdgToplevelSurfaceRoleAttributes>`, so
`.lock().unwrap().pending_configures()`). Entries are pruned in `ack_configure`
with `retain(|c| c.serial > serial)`, so non-empty ⇔ the latest configure is not
yet acked. Treat a missing data entry as "no pending" (`unwrap_or(false)`).
Non-empty does **not** imply a pending *resize*: a compositor queues size-less
(`state.size == None`/`(0,0)`, "client picks") configures too, so to detect an
owed resize inspect each `ToplevelConfigure`'s `state.size` for a real
(non-zero) size differing from the committed geometry, not just list length.

Pruning is latched to the `ack_configure` *request* (the acked configure moves
to `last_acked`), not to the commit that applies it — and real clients (GTK4)
ack as soon as they process the event, then keep committing old-size frames
until their next resized render. So "no pending configures" must **not** be
read as "committed geometry is current". To gate on a size transition actually
completing, track it compositor-side off the committed geometry changing (see
`pending_recenter`), not off ack state.

### `send_pending_configure` before the initial configure
`ToplevelSurface::send_pending_configure()` gates on `has_pending_changes()`,
which is `!initial_configure_sent || <server_pending differs>`. So it does **not**
no-op before the initial configure — it forces one out. To flush a mid-session
change (e.g. an Activated flip on focus change) without prematurely sending, and
thereby fragmenting, a batched first-commit configure, guard on
`ToplevelSurface::is_initial_configure_sent()` first. `Window::set_activated(bool)
-> bool` returns whether the hint actually changed, so pair the two: flush only
when `set_activated` reports a change and the initial configure is already out.

### Keyboard modifier state
```rust
let modifiers = self.seat.get_keyboard().unwrap().modifier_state();
if modifiers.alt { ... }
```

## Cursor Rendering

### CursorImageStatus
Source: `src/input/pointer/cursor_image.rs`
```rust
pub enum CursorImageStatus {
    Hidden,
    Named(CursorIcon),       // CursorIcon from cursor_icon crate
    Surface(WlSurface),      // client-provided cursor
}
impl CursorImageStatus {
    pub fn default_named() -> Self { Self::Named(CursorIcon::Default) }
}
```
`CursorIcon::name()` returns CSS cursor names: `"default"`, `"pointer"`, `"grabbing"`, etc.

### CursorShapeManagerState
Source: `src/wayland/cursor_shape.rs`
```rust
// Init: requires TabletSeatHandler impl (even empty)
let state = CursorShapeManagerState::new::<DriftWm>(&display_handle);
delegate_cursor_shape!(DriftWm);
// Also need: impl TabletSeatHandler for DriftWm {}
```

### MemoryRenderBuffer
Source: `src/backend/renderer/element/memory.rs`
```rust
// Create from pixel data:
let buffer = MemoryRenderBuffer::from_slice(
    &pixels_rgba,          // &[u8]
    Fourcc::Abgr8888,      // format (xcursor pixels_rgba is ABGR)
    (width, height),       // impl Into<Size<i32, Buffer>>
    1,                     // scale
    Transform::Normal,
    None,                  // opaque_regions
);

// Create render element:
let elem = MemoryRenderBufferRenderElement::from_buffer(
    renderer,              // &mut R where R: ImportMem
    location,              // impl Into<Point<f64, Physical>> — PHYSICAL coords!
    &buffer,
    None,                  // alpha: Option<f32>
    None,                  // src: Option<Rectangle<f64, Logical>>
    None,                  // size: Option<Size<i32, Logical>>
    Kind::Cursor,          // Kind enum
)?;
```

### render_output (space version)
Source: `src/desktop/space/mod.rs`
```rust
pub fn render_output<R, C, E, S>(
    output: &Output,
    renderer: &mut R,
    framebuffer: &mut R::Framebuffer<'_>,
    alpha: f32,
    age: usize,
    spaces: S,
    custom_elements: &[C],    // C: RenderElement<R> — rendered ON TOP of space
    damage_tracker: &mut OutputDamageTracker,
    clear_color: impl Into<Color32F>,
) -> Result<RenderOutputResult, OutputDamageTrackerError<R::Error>>
```

## Popup System

### PopupSurface
Source: `src/wayland/shell/xdg/mod.rs`
```rust
impl PopupSurface {
    pub fn send_configure(&self) -> Result<Serial, PopupConfigureError>;
    pub fn send_repositioned(&self, token: u32);
    pub fn with_pending_state<F, T>(&self, f: F) -> T
    where F: FnOnce(&mut PopupState) -> T;
}
```
**Must call `send_configure()` in `new_popup`** — client won't commit until it receives this.
Set geometry first: `surface.with_pending_state(|s| s.geometry = positioner.get_geometry())`.

### PopupManager
Source: `src/desktop/wayland/popup/manager.rs`
```rust
impl PopupManager {
    pub fn track_popup(&mut self, kind: PopupKind) -> Result<(), ...>;
    pub fn commit(surface: &WlSurface);       // call in CompositorHandler::commit()
    pub fn cleanup(&mut self);                 // call each frame
    // Static — used internally by Window::render_elements()
    pub fn popups_for_surface(surface: &WlSurface)
        -> impl Iterator<Item = (PopupKind, Point<i32, Logical>)>;
}
```

### Popup Rendering Flow
`render_output()` → `Window::render_elements()` → `PopupManager::popups_for_surface()` →
`render_elements_from_surface_tree()` per popup. Fully automatic — no compositor render code needed.

### Never track_popup in WlrLayerShellHandler::new_popup
The protocol flow for a popup on a layer surface is `xdg_surface.get_popup(None, positioner)` then
`zwlr_layer_surface_v1.get_popup(xdg_popup)`. The first call fires `XdgShellHandler::new_popup` with
`parent = None`, and `track_popup` queues a parentless popup into `unmapped_popups`; the popup's
first commit drains that entry and inserts it into the popup tree (the parent is set by then).
Calling `track_popup` again from the layer handler inserts a second tree node — `PopupTree::insert`
never dedupes — and `popups_for_surface` then yields the popup twice, so it renders twice
(double-alpha on translucent pixels, doubled per-popup render work). The layer handler should only
`unconstrain_popup`; the xdg-path unmapped entry handles tracking. `find_popup` searches
`unmapped_popups` too, so the popup is resolvable in the pre-commit window.

### Bounding boxes: `bbox()` vs `bbox_with_popups()`
`Window::bbox()` / `LayerSurface::bbox()` cover the toplevel and its subsurfaces but **not** popups;
the `_with_popups` variants merge in every popup from `PopupManager::popups_for_surface`.
`SpaceElement::bbox` for `Window` is `bbox_with_popups()` — `Space` never used the popup-less box
(source: `src/desktop/space/wayland/window.rs`). `Window::send_frame` and `LayerSurface::send_frame`
both send frame callbacks to popup surface trees too, so throttling decisions keyed on a popup-less
bbox starve visible popups.

### Input-region hit-testing: `is_in_input_region` vs `surface_under`
`SpaceElement::is_in_input_region` for `Window` is literally
`self.surface_under(*point, WindowSurfaceType::ALL).is_some()`
(source: `src/desktop/space/wayland/window.rs`) — the "cheap predicate" and the "full lookup" are the
same traversal, so a bbox test in front of it is the only actual early-out.

`WindowSurfaceType::ALL` = `TOPLEVEL | SUBSURFACE | POPUP`, and `Window::surface_under` tests
**popups first** (in `PopupManager::popups_for_surface` order, each offset by
`geometry().loc + location - popup.geometry().loc`) before descending the toplevel's surface tree.

Per-surface, `contains_point` (`src/desktop/wayland/utils.rs`) rejects points outside the buffer
rect, then returns `true` when `input_region.is_none()` — **a surface that declares no input region
is input across its whole buffer**. A CSD client's shadow margin is usually exactly that, so it
hit-tests as content, not as empty space.

`Window::bbox()` is a commit-time cache: `on_commit` recomputes it from the surface tree, and
driftwm only calls `on_commit` for non-sync-subsurface commits (`src/handlers/compositor.rs`). The
popup half of `bbox_with_popups()` is computed live. So the box and `surface_under` can disagree
transiently — treat the bbox as a conservative filter, not as an equivalent test.

## Selection / Clipboard

### Cross-app clipboard
Source: `src/wayland/selection/data_device/mod.rs`
```rust
pub fn set_data_device_focus<D>(dh: &DisplayHandle, seat: &Seat<D>, client: Option<Client>)
where D: SeatHandler + DataDeviceHandler + 'static;
```
Sends `wl_data_device.selection` to newly focused client. Call in `SeatHandler::focus_changed()`.

### Primary selection (middle-click paste)
Source: `src/wayland/selection/primary_selection/mod.rs`
```rust
pub fn set_primary_focus<D>(dh: &DisplayHandle, seat: &Seat<D>, client: Option<Client>)
where D: SeatHandler + PrimarySelectionHandler + 'static;
```
Same pattern — call alongside `set_data_device_focus`.

### Usage in focus_changed
```rust
fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&Self::KeyboardFocus>) {
    let dh = &self.display_handle;
    let client = focused.and_then(|f| dh.get_client(f.0.id()).ok());
    set_data_device_focus(dh, seat, client.clone());
    set_primary_focus(dh, seat, client);
}
```

## xdg-activation (`src/wayland/xdg_activation/`)

Startup-notification / focus-request tokens. `XdgActivationState` owns a
`HashMap<XdgActivationToken, XdgActivationTokenData>`. `XdgActivationToken` is a
newtype over a random 32-char alphanumeric `String` (`Deref<Target = str>`,
`as_str()`, `From<String>`).

```rust
// Compositor-minted token, no client association. Does NOT call token_created.
// Returns refs borrowed from the &mut XdgActivationState.
pub fn create_external_token(
    &mut self,
    data: impl Into<Option<XdgActivationTokenData>>,
) -> (&XdgActivationToken, &XdgActivationTokenData);

pub fn remove_token(&mut self, token: &XdgActivationToken) -> bool;
pub fn data_for_token(&self, token: &XdgActivationToken) -> Option<&XdgActivationTokenData>;
pub fn retain_tokens<F: FnMut(&XdgActivationToken, &XdgActivationTokenData) -> bool>(&mut self, f: F);
```

`XdgActivationTokenData` fields: `client_id`, `serial: Option<(Serial, WlSeat)>`
(the input serial — `None` for a compositor-minted or spontaneous token),
`app_id`, `surface` (the *requesting* surface, not the one to activate),
`timestamp: Instant`, and `user_data: Arc<UserDataMap>`.

**Attaching custom data:** stamp `user_data` (an `Arc<UserDataMap>`) right after
minting via the returned data ref:
```rust
let (token, data) = state.create_external_token(None);
data.user_data.insert_if_missing_threadsafe(|| MyMarker(id)); // T: Send + Sync + 'static
let token = token.clone();
```
`UserDataMap::get::<T>()` returns `Option<&T>`; a value inserted with the
non-threadsafe `insert_if_missing` is only visible from the thread it was
inserted on (use `insert_if_missing_threadsafe` to be thread-agnostic).

**Round-trip:** on a client `xdg_activation_v1.activate { token, surface }`, the
dispatch looks up `known_tokens.get(&token).cloned()` and calls
`XdgActivationHandler::request_activation(token, token_data, surface)`. The
`token_data` is a *clone*, but `user_data` is an `Arc`, so a stamped marker is
visible there. An unknown token string is silently dropped (no handler call).
The token stays in the pool after `request_activation` until `remove_token` /
`retain_tokens`. `token_created` fires only for client-built tokens (the
`.commit()` path), never for `create_external_token`.

## xcursor Crate (0.3)

```rust
let theme = xcursor::CursorTheme::load("default");  // respects XCURSOR_PATH
let path = theme.load_icon("default")?;              // -> PathBuf
let images = xcursor::parser::parse_xcursor(&std::fs::read(path)?)?;
// Image { width, height, xhot, yhot, pixels_rgba: Vec<u8>, pixels_argb: Vec<u8>, size, delay }
```

## DrmCompositor Mode Changes

### `DrmCompositor::use_mode(mode)` is safe with a page flip in flight
Source: `src/backend/drm/compositor/mod.rs` (`use_mode`), `src/backend/drm/surface/atomic.rs` (`AtomicDrmSurface::use_mode`); git checkout under `~/.cargo/git/checkouts/smithay-*/`.

`use_mode` does **not** modeset immediately:
1. `AtomicDrmSurface::use_mode` creates a mode property blob + a throwaway test buffer, submits a `TEST_ONLY` atomic commit to validate, and on success just stores the mode in the surface's `pending` state.
2. `DrmCompositor::use_mode` then resizes the swapchain to the new dimensions.
3. The **real** modeset lands with the next frame commit (`queue_frame` picks up the pending state, commits with `ALLOW_MODESET`).

So it never races an in-flight page flip — the kernel serializes atomic commits per CRTC, and the pending frame's fb holds its own reference. niri calls `use_mode` unconditionally at config-apply time with no deferral (`tty.rs`, `on_output_config_changed`) and only handles the `Err`. Deferring/queueing around `frames_pending` before calling it is unnecessary.

## InputBackend (synthetic input)

Source: `src/backend/input/mod.rs`, `src/backend/input/tablet.rs`.

`InputBackend` is **25 associated types and zero methods** — a pure type-level
description of what a backend can emit. It is declared `pub trait InputBackend:
Sized`, so it is never object-safe: backends are selected with a type parameter
(`process_input_event::<I>`), never a `dyn`. Implementing it costs nothing but
the event types you actually produce:

- **`pub enum UnusedEvent {}`** is uninhabited and carries a blanket impl of
  *every* event trait (each method is `match *self {}`). Any associated type you
  don't emit is `= UnusedEvent` with no code written. `SpecialEvent` has no trait
  bound at all.
- **`Device: PartialEq + Eq + Hash`** — `id() -> String`, `name() -> String`,
  `has_capability(DeviceCapability) -> bool`, `usb_id() -> Option<(u32, u32)>`,
  `syspath() -> Option<PathBuf>`. `DeviceCapability` is `Copy + Eq` but **not
  `Hash`**, so a device holding a capability list can't derive `Hash` — hash the
  id instead.
- **`Event<B>::time()` is MICROseconds**; `time_msec()` is provided and divides
  by 1000. `device()` returns `B::Device` **by value**, so the device type is
  usually `Clone`.
- Marker traits over shared supertraits: `PointerMotionAbsoluteEvent<B>` and
  `TouchDownEvent<B>` add nothing to `AbsolutePositionEvent<B>` (`x`, `y`,
  `x_transformed(width)`, `y_transformed(height)`, plus provided `position` /
  `position_transformed`) and `TouchEvent<B>` (`slot() -> TouchSlot`) — impl the
  supertrait, then `impl Marker for T {}`.
- `PointerButtonEvent<B>` needs only `button_code() -> u32` and
  `state() -> ButtonState`; `button() -> Option<MouseButton>` is provided.
- `InputEvent<B>` derives `Debug`, which bounds on `B: Debug` — a backend that
  isn't `Debug` still works, its events just can't be printed.

driftwm's synthetic backend for tests lives in `src/tests/input_backend.rs`.

## Gotchas

### Compositor / Protocol Essentials

- **Must call `on_commit_buffer_handler::<DriftWm>(surface)`** in `CompositorHandler::commit()` — NOT done by `delegate_compositor!`. Without it, `RendererSurfaceState` is never populated, `surface_view` stays None, `bbox_from_surface_tree()` returns 0x0, windows invisible.
- **Must call `output.create_global::<DriftWm>(&display_handle)`** — `space.map_output()` is internal only; clients need a `wl_output` global to see monitors.
- **`ToplevelSurface::send_configure()`** must be called in `new_toplevel` — clients won't render until they receive an initial configure.
- **`PopupSurface::send_configure()`** must be called in `new_popup` — same as toplevels. Also set geometry from positioner: `surface.with_pending_state(|s| s.geometry = positioner.get_geometry())`.
- **Cross-app clipboard requires `set_data_device_focus` + `set_primary_focus`** in `SeatHandler::focus_changed()`. Without this, newly focused clients don't receive `wl_data_device.selection` events and can't paste from other apps. Extract client via `dh.get_client(surface.id()).ok()`.

### Protocol Errors

- **`post_error` on an already-destroyed resource kills the client WITHOUT delivering `wl_display.error`.** `send_event` resolves the object argument, gets `Err(InvalidId)` because the destructor already removed it from the object map, and the send result is discarded ("errors are ignored, as the client will be killed anyway") — wayland-backend `rs/server_impl/client.rs`. The socket just EOFs, so client-side it is `WaylandError::Io`, and `Connection::protocol_error()` maps `Io(_) => None`. Tests must not assert on `protocol_error()` for these; the observable symptom is a bare `Broken pipe (os error 32)`.

### Backend

- **Winit backend needs `Transform::Flipped180`** on the output — EGL Y-axis is inverted relative to Wayland coordinates.
- **`Transform::Normal` for udev** — DRM handles orientation natively.
- **WAYLAND_DISPLAY must NOT be set before `winit::init()`** — winit connects to the parent compositor; setting our socket first causes a deadlock.
- **Backend on state** — winit backend stored as `Option<WinitGraphicsBackend<GlesRenderer>>` on DriftWm. Timer closure uses take/put pattern to split borrows. Required for DmabufHandler to access renderer.
- **DMA-BUF v3 (create_global)** sufficient for winit backend — advertises formats, no device info. v4 (create_global_with_default_feedback) adds render device hints for multi-GPU. `ImportDma::dmabuf_formats()` on GlesRenderer gets formats from EGL.
- Benign `EGL BAD_SURFACE` error on first frame is from `buffer_age()` before the surface is ready; `unwrap_or(0)` handles it.

### Input / Keycodes

- **Don't add +8 to keycodes** from smithay input events — they're already XKB keycodes. Adding 8 double-offsets every key.
- **Mouse wheel vs trackpad scroll** — `PointerAxisEvent::amount()` returns `None` for discrete mouse wheels. Use `amount_v120()` (120 = one notch) as fallback: `event.amount(axis).or_else(|| event.amount_v120(axis).map(|v| v * 15.0 / 120.0))`.

### Data Types / Borrow Patterns

- **DataMap::get_or_insert returns `&T` (immutable)** — wrap state in `RefCell` for mutation. Use `.borrow()` / `.replace()`.
- **`insert_if_missing_threadsafe` requires `Sync`** — `RefCell` is not `Sync`, use `Mutex` for data stored in smithay's `UserDataMap` (e.g. `AppliedWindowRule` on surface data_map).
- **`xdg_toplevel::ResizeEdge` is an enum, NOT bitflags** — use `(edge as u32) & bit` for component checks.
- **Resize position adjustment must be absolute, not incremental** — store `initial_window_location` in `ResizeState` and compute `new_loc = initial_loc + (initial_size - current_size)`. Incremental `loc += delta` causes cumulative drift.

### Focus / Grabs

- **Popup grabs require `FocusTarget` wrapper** — `PopupGrab` needs `KeyboardFocus: From<PopupKind>`. Can't impl `From<PopupKind> for WlSurface` (orphan rule), so use a `FocusTarget(WlSurface)` newtype that impls `From<PopupKind>`, `WaylandFocus`, `KeyboardTarget`, `PointerTarget`, `TouchTarget`.
- **PopupKeyboardGrab/PopupPointerGrab** at `smithay::desktop::{PopupKeyboardGrab, PopupPointerGrab}`.
- **PopupUngrabStrategy** at `smithay::desktop::PopupUngrabStrategy`, NOT `smithay::wayland::shell::xdg`.
- **Never call `data.seat.get_pointer()` inside `PointerGrab::unset()`** — `unset()` runs while smithay's internal pointer mutex is held; re-entering it via `get_pointer()` deadlocks. Do all side-effects in `button()` before `handle.unset_grab()`.
- **The per-surface user-data mutex is not re-entrant either, and far more code takes it than the name suggests.** `compositor::with_states` locks it for the whole closure, and so do the many APIs built on it — `with_pointer_constraint` (via `with_constraint_data`), `Window::geometry`, `get_parent`, `send_configure`, `with_pending_state`. Re-entering it from another closure on the same surface hangs the compositor: no panic, no log, 0% CPU, every thread in `__futex_wait`. Smithay's own `pointer_constraints::commit_hook` hits the same trap — its comment records the bug biting upstream. See `caveats.md` for the fix.
- **`LayerMap` guard (MutexGuard) must be dropped before calling `keyboard.set_focus()`** — `set_focus` triggers `SeatHandler::focus_changed()` which may need `&mut self`.
- **Layer surface exclusive focus must be guarded** — only grab keyboard focus when it's not already on this surface. Otherwise every commit from an Exclusive layer surface steals focus back.
- **`pointer_over_layer` must be reset on layer destroy and fullscreen enter** — stale flag breaks all input until next motion event.
- **`TouchHandle::is_grabbed()` is true after any `down()`, and `grab_start_data()` is no better** — smithay's touch `DefaultGrab::down` (`input/touch/grab.rs`) installs its own `TouchDownGrab` unconditionally, so `is_grabbed()` can't distinguish "the compositor set a grab" from "a finger is down". `grab_start_data()` is the same tautology under another name: `set_grab` stores *every* grab as `GrabStatus::Active` (`input/touch/mod.rs`) and `grab_start_data()` returns `Some` for any `Active`, so it is `Some` after any `down()` too — it reports the start data of whichever grab is installed, not who installed it. Nothing on `TouchHandle` answers the question; assert on the compositor's own grab bookkeeping instead.

### Trait Impls / Method Clashes

- **WlSurface protocol methods clash with trait methods** — `WlSurface::enter()` is the `wl_surface.enter(output)` protocol method. When delegating `KeyboardTarget::enter` etc. from a newtype, use fully-qualified syntax: `KT::<D>::enter(&self.0, ...)`.
- **TouchTarget has 7 methods, not 5** — `down`, `up`, `motion`, `frame`, `cancel` plus `shape` and `orientation`.

### Rendering

- **Custom shader background via `PixelShaderElement`** — `GlesRenderer::compile_custom_pixel_shader(src, &[UniformName])` compiles GLSL (auto-prepends `#version 100`). `PixelShaderElement::new(shader, area, opaque_regions, alpha, uniforms, kind)` creates a render element. Built-in varyings: `v_coords` (0-1), `size` (output pixels), `alpha`. Custom uniforms via `Uniform::new(name, value)`.
- **`render_elements!` macro `<=` form** for concrete renderer: `render_elements! { pub Name<=GlesRenderer>; Variant=Type, }` — generates non-generic enum with `impl RenderElement<GlesRenderer>`.
- **`space_render_elements()` is public** at `smithay::desktop::space::space_render_elements`. Returns `Vec<SpaceRenderElements<R, E>>`.
- **Element ordering in damage tracker**: first element = topmost (drawn last), last element = bottommost (drawn first). For background behind windows: use `damage_tracker.render_output()` directly with [cursor, space_elements, background] ordering.
- **`Element::opaque_regions()` and `damage_since()` must return ELEMENT-LOCAL coords** (relative to `geometry().loc`). `OutputDamageTracker` translates them by `element_loc = geometry(scale).loc` itself (`damage/mod.rs`: `region.loc += element_loc`). `geometry()` itself is absolute. Returning *absolute* opaque regions double-offsets them to `2 × geometry.loc` — the renderer then skips clearing under that phantom rect and nothing draws there, leaving unpainted holes (black live / uninitialized-magenta in a fresh capture buffer). A full-screen element at origin `(0,0)` hides this (local == absolute); elements at non-zero positions (e.g. tiled-bg chunks) expose it. The default `damage_since()` returns `Rectangle::from_size(geometry(scale).size)` at `(0,0)` — already local — so custom elements only need to fix `opaque_regions()`.
- **`gles::uniform` module is private** — types (`Uniform`, `UniformName`, `UniformType`) re-exported at `smithay::backend::renderer::gles::{Uniform, UniformName, UniformType}`.
- **`RescaleRenderElement`** at `smithay::backend::renderer::element::utils::RescaleRenderElement` — `from_element(elem, physical_origin, scale)` scales position+size. Used for zoom.
- **`Space::render_elements_for_region()`** — returns `Vec<WaylandSurfaceRenderElement<R>>` for an arbitrary rectangle. Positions offset by `-region.loc` (camera). Doesn't clip to output geometry — essential for zoom < 1.0.

### Layer Shell

- **`WlrLayerShellHandler::new_layer_surface` takes `wlr_layer::LayerSurface` (protocol type), NOT `desktop::LayerSurface`** — wrap with `desktop::LayerSurface::new(surface, namespace)` before passing to `layer_map_for_output().map_layer()`.
- **Pointer must ALWAYS stay in canvas coords** — even when over a layer surface. `layer_surface_under()` returns an adjusted focus location so smithay computes correct surface-local coords.
- **The layer-shell validation `pre_commit_hook` is registered once per `wl_surface` and NEVER removed** (`wlr_layer/handlers.rs`, gated on `if initial`; smithay does not expose its `HookId`). Its error ladder, in order (`wlr_layer/mod.rs`), each posting on `role.surface` and returning: `InvalidSize` for `size.w == 0 && !anchor.anchored_horizontally()`, `InvalidSize` for the vertical twin, `InvalidExclusiveEdge` for an `exclusive_edge` outside `anchor`, then `InvalidSurfaceState` ("must ack the initial configure before attaching buffer") when `has_buffer && role.last_acked.is_none()`. `has_buffer` comes from `SurfaceAttributes::pending().buffer`: `NewBuffer` → true, `Removed` → false, `None` → `had_buffer_before` — and that one is a *different* `pending()`, `LayerSurfaceCachedState::pending().last_acked.is_some()`, not anything on `SurfaceAttributes`.
- **`zwlr_layer_surface_v1` destroy does not unregister that hook**, and `destroyed` (`wlr_layer/handlers.rs`) zeroes what it validates against — `LayerSurfaceAttributes::reset()` plus `*pending() = *current() = Default::default()` on `LayerSurfaceCachedState`. Every later commit on that `wl_surface` therefore re-runs the ladder against zeroed state and posts on the destroyed proxy. Upstream bug; driftwm neutralises it from an earlier hook (`handlers/compositor.rs` + `LayerDestroyedMarker`).
- **`destroyed` removes the entry from `known_layers` and drops the lock BEFORE calling `WlrLayerShellHandler::layer_destroyed`** — so `WlrLayerShellState::layer_surfaces()` (`impl DoubleEndedIterator<Item = LayerSurface>`, clones the list) sees only survivors from inside that callback. `PrivateSurfaceData::set_role` permits taking the *same* role twice (`compositor/tree.rs` errors only when the new role differs), so one `wl_surface` can carry two live `ZwlrLayerSurfaceV1` — and since `LayerSurfaceAttributes.surface` is private, `!role.surface.alive()` can't be used to tell them apart.
- **`LayerSurfaceCachedState`'s `Cacheable::commit` is `*self`** and `CachedState::commit` never resets `pending` (`compositor/cache.rs`) — so anything written into `pending()` survives a commit *and* survives into the next role taken on that `wl_surface`. `get_layer_surface` only sets `pending().layer`; the `destroyed` wipe has already run by then.
- **A pre-commit-hook write to `LayerSurfaceCachedState::pending()` lands in `current()` too — and `current()` is the half that gets read.** `commit` is `*self` and `merge_into` is `*into = self` (`wlr_layer/mod.rs:191-198`), so the commit that follows the hook copies the poisoned `pending` wholesale over `current`. `LayerMap::arrange` reads `LayerSurface::cached_state()` (`desktop/wayland/layer.rs:309`) → `with_cached_state` → `f(guard.current())` (`wlr_layer/mod.rs:454-462`). Anything a hook forces in must therefore be undone in **both** halves; undoing only `pending()` passes tests only by accident of `arrange` refusing to configure before `initial_configure_sent`.
- **A future `DrmSyncobjState` would reintroduce this crash on a different object.** Its pre-commit hook is registered at `wp_linux_drm_syncobj_manager_v1.get_surface` (`drm_syncobj/mod.rs`), i.e. *after* driftwm's `new_surface` hooks, so it would see the stripped buffer and post `NoBuffer` ("acquire point without buffer") on a still-live `wp_linux_drm_syncobj_surface_v1`. On top of that, `RendererSurfaceState::update_buffer`'s `Removed` arm calls `reset()` and returns before signalling `release_point`, so the client's release fence would never fire for the stripped commit.

### Session Lock

- **The lock validation `pre_commit_hook` is registered *unconditionally* at `get_lock_surface`** (`session_lock/lock.rs`) — unlike layer shell's `if initial`, so a lock → unlock → lock cycle on the same `wl_surface` stacks a second hook. The `HookId` is discarded, so neither can be removed.
- **`ExtSessionLockSurfaceV1::destroyed` calls no `SessionLockHandler` method** (`session_lock/surface.rs`) — there is no per-role destroy seam to hook. It runs `LockSurfaceAttributes::reset()` (`server_pending`, `pending_configures`, `last_acked` all cleared) and zeroes both halves of `LockSurfaceCachedState`, while leaving `attributes.surface` pointing at the **dead proxy**. Every later commit on that `wl_surface` therefore posts on a destroyed object.
- **Its error ladder is asymmetric with layer shell's: the first check is unconditional.** `last_acked.is_none()` → `CommitBeforeFirstAck`, before anything is read about the buffer — so a bare `commit()` on an orphaned role kills the client with no buffer involved at all. Then, on `SurfaceAttributes::pending().buffer`: `NewBuffer` whose logical size (viewport `dst` if set, else buffer size / scale / transform) differs from the acked `state.size` → `DimensionsMismatch`; **`Removed` → `NullBuffer` outright** — lock surfaces are not allowed to unmap, so the layer trick of neutralising a commit by forcing `Removed` is itself fatal here. `None` always passes (it falls through to `had_buffer_before`), and so does a `NewBuffer` whose `buffer_dimensions()` is `None`: the size comparison sits inside `if let Some(buf_size)`.
- **`pending_configures` empty *and* `last_acked` unset is reachable only through `destroyed`** — the invariant driftwm's `neutralise_orphaned_lock_commit` detects on. `send_configure` pushes onto `pending_configures` before the event goes out, and `ack_configure` retains only `serial > acked` while setting `last_acked = Some`, which then stays `Some`. So live-and-unacked still holds a pending configure, and live-and-acked holds the ack. A role the compositor never configured shows the same shape — hence the second, "we configured this" half of the gate.
- **A `wl_surface` can carry two live lock roles at once, and destroying the stale one resets the live one's state.** `give_role` succeeds when the role string *matches* (`compositor/tree.rs`), and the `DuplicateOutput` guard compares per-client `wl_output` *resources* (`session_lock/lock.rs`), so a client that binds `wl_output` twice can call `get_lock_surface` on the same `wl_surface` twice. The second call repoints `attributes.surface` and resets nothing; both `ExtSessionLockSurfaceV1`s stay live over one shared `LockSurfaceAttributes`. Destroying the *stale* proxy then runs `destroyed`'s reset while the current role is live and acked — producing the empty shape on a live role. Nothing downstream can tell them apart: `LockSurfaceAttributes::surface` is `pub(crate)`, `LockSurface`'s own handle is private, and `LockSurface::alive()` reports the `wl_surface`, not the role. driftwm's gate therefore latches that surface as orphaned; every commit after that also has its pending buffer taken, so a live lock surface freezes at its last frame (the buffer itself is still released, so the client doesn't stall on it) — which fails safe (session stays locked, nothing leaks, no client is killed) and needs a client-side protocol violation smithay declines to reject.
- **`get_lock_surface` re-takes cleanly on a `wl_surface` that already had the role.** `give_role` only errors on a *different* role, and the `AlreadyConstructed` check reads `SurfaceAttributes` `pending().buffer` and `current().buffer` — both `None` on a settled surface, since `RendererSurfaceState::update_buffer` takes the current one and `Cacheable::commit` takes the pending one. The re-take only repoints `attributes.surface`; it resets **nothing**, so anything written into `LockSurfaceAttributes` survives into the new role.
- **smithay sends its own initial configure after `SessionLockHandler::new_surface` returns** (`lock.rs`), but only if the handler left `server_pending` set to a state the role has not already been told. `get_pending_state` starts with `server_pending.take()?` and then drops it anyway when it equals `current_server_state()` — the last pending configure, else the last ack. So a handler that returns early without calling `with_pending_state` leaves the role live and permanently unconfigured, and so does one that re-requests the state a surviving ack already carries.
- **A future `DrmSyncobjState` would reintroduce the kill on a different object**, exactly as it would for layer shell: its hook registers at `wp_linux_drm_syncobj_manager_v1.get_surface`, i.e. after driftwm's `new_surface` hooks, so it would find the stripped buffer and post `NoBuffer` on a still-live `wp_linux_drm_syncobj_surface_v1`. No `drm_syncobj` today.

### Buffers / `wl_buffer.release`

- **Release-on-*replace* happens in exactly one place, `SurfaceAttributes::merge_into`** (`compositor/handlers.rs`): replacing a pending buffer during the cache merge releases the displaced one unless it's the same `WlBuffer` (`if Some(&buffer) != new_buffer { buffer.release(); }`). `Cacheable::commit` is `buffer: self.buffer.take()` — a plain move. So overwriting `pending().buffer` from a **pre-commit hook** sends no release at all; do it explicitly or a client recycling a small shm pool stalls forever.
- **Release-on-*drop* is the separate second path: `InnerBuffer::drop` calls `buffer.release()`** (`backend/renderer/utils/wayland.rs`) — which is why `BufferAssignment::Removed` releases the previously committed buffer and `None` does not: `RendererSurfaceState::update_buffer` maps `Removed` → `reset()` → `self.buffer = None`, dropping the last `Arc<InnerBuffer>`. Read the currently held buffer via `states.data_map.get::<RendererSurfaceStateUserData>()` → `RendererSurfaceState::buffer() -> Option<&Buffer>` (`Buffer: PartialEq<WlBuffer>`) to avoid double-releasing one a client re-attached.

### Decorations

- **`XdgDecorationState`** at `smithay::wayland::shell::xdg::decoration::XdgDecorationState`. Mode enum at `wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode` (`ServerSide`/`ClientSide`).
- **Minimal windows**: force `Mode::ServerSide` on the toplevel — client removes its CSD. Compositor draws no titlebar (still draws shadow + corner clip).

### Udev/DRM Backend

- **`drm_fourcc::DrmFormat` re-exported as `smithay::backend::allocator::Format`** — smithay renames it.
- **Connector subpixel is `connector::SubPixel`** (not `SubpixelOrder`). From `smithay::reexports::drm::control::connector`.
- **`DrmDevice::create_surface()` needs `&mut self`**.
- **`RegistrationToken` has no `Default`** — calloop tokens can only be created by `insert_source()`.
- **`smithay-drm-extras` from git** (not crates.io 0.1) — needed for libdisplay-info 0.3.0 compat (Arch Linux).
- **`DrmCompositor::frame_submitted()` must be called on VBlank** — otherwise buffers aren't recycled and rendering stalls.
- **LibSeatSession needs `mut`** — `session.open()` requires mutable reference.
- **Libinput `suspend()`/`resume()` take `&self`** (not `&mut self`) — clone is fine.
- **Both Backend enum variants should be `Box`ed** — `WinitGraphicsBackend` ~6.5KB, `GlesRenderer` ~6.3KB. Clippy warns about variant size differences.

### Rust 2024 Edition

- **Temporaries in `if let` live until end of block** — separate `let x = expr.cloned(); if let Some(x) = x {` when needing `&mut self` inside the block.
- **DMA-BUF blocker uses let-chains** — `if let Some(dmabuf) = ... && let Ok((blocker, source)) = ... && let Some(client) = ... { }` is idiomatic Rust 2024.

### Output membership (`SpaceElement` on `Window`)

driftwm drives per-window `wl_surface.enter`/`leave` itself (`DriftWm::refresh_window_outputs`) instead of `Space::refresh`. The relevant `SpaceElement` methods on `Window` (`smithay::desktop::space::SpaceElement`; call fully-qualified — `Window` has inherent methods with clashing names):

- **`SpaceElement::output_enter(&self, output, overlap)`** — inserts `overlap` (relative to the window origin) into the `Window`'s private `WindowOutputUserData` overlap map (keyed by a downgraded `Output`) and calls `refresh()` to push the enter to the toplevel + its popups. Idempotent per output; re-sends on a changed overlap.
- **`SpaceElement::output_leave(&self, output)`** — drops `output` from that map and sends `wl_surface.leave` for the toplevel and every popup. No-op after `Output::leave_all()`.
- **`SpaceElement::refresh(&self)`** — re-runs the per-surface `output_update` for the toplevel and popups from the `Window`'s own overlap map; keeps popup enter/leave fresh without a full diff.
- **`SpaceElement::bbox(&self)`** = `Window::bbox_with_popups()`; **`geometry()`** = the window geometry (decoration-excluded). `Space::refresh` translated the bbox by `location - geometry().loc` before intersecting each output — replicate this when computing overlaps.
- **`Space::refresh` semantics (replaced)** — retained alive elements, diffed each element's bbox against every mapped output's geometry (`output_geometry(o).unwrap_or_else(Rectangle::zero)`), sent `output_enter`/`output_leave` on change, called `SpaceElement::refresh` per element, then `Output::cleanup` per output.
- **`Output::cleanup(&self)`** — prunes dead surfaces from the output's own enter tracking. **`Output::leave_all(&self)`** — sends `wl_surface.leave` for every surface currently entered on the output (used on placeholder-output teardown).
- **`UserDataMap`** is a lock-free append-only boxed list: a `&T` from `get`/`get_or_insert` stays valid across a later `insert_if_missing` of a *different* type, so holding one userdata borrow while a `SpaceElement` method touches another userdata entry is sound.
