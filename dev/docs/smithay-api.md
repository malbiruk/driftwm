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

### TabletSeat / TabletSeatHandler (`tablet-v2` protocol)

`wp_tablet_manager_v2` is used to expose high-definition graphics tablet pen input (pressure, tilt, rotation) to drawing tools like Krita.

```rust
// Init:
let tablet_state = TabletManagerState::new::<DriftWm>(&dh);
delegate_tablet_manager!(DriftWm);

// Impl:
impl TabletSeatHandler for DriftWm {
    fn tablet_tool_image(
        &mut self,
        _tool: &smithay::backend::input::TabletToolDescriptor,
        image: CursorImageStatus,
    ) {
        self.cursor.cursor_status = image;
    }
}
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

## xcursor Crate (0.3)

```rust
let theme = xcursor::CursorTheme::load("default");  // respects XCURSOR_PATH
let path = theme.load_icon("default")?;              // -> PathBuf
let images = xcursor::parser::parse_xcursor(&std::fs::read(path)?)?;
// Image { width, height, xhot, yhot, pixels_rgba: Vec<u8>, pixels_argb: Vec<u8>, size, delay }
```

## Gotchas

### Compositor / Protocol Essentials

- **Must call `on_commit_buffer_handler::<DriftWm>(surface)`** in `CompositorHandler::commit()` — NOT done by `delegate_compositor!`. Without it, `RendererSurfaceState` is never populated, `surface_view` stays None, `bbox_from_surface_tree()` returns 0x0, windows invisible.
- **Must call `output.create_global::<DriftWm>(&display_handle)`** — `space.map_output()` is internal only; clients need a `wl_output` global to see monitors.
- **`ToplevelSurface::send_configure()`** must be called in `new_toplevel` — clients won't render until they receive an initial configure.
- **`PopupSurface::send_configure()`** must be called in `new_popup` — same as toplevels. Also set geometry from positioner: `surface.with_pending_state(|s| s.geometry = positioner.get_geometry())`.
- **Cross-app clipboard requires `set_data_device_focus` + `set_primary_focus`** in `SeatHandler::focus_changed()`. Without this, newly focused clients don't receive `wl_data_device.selection` events and can't paste from other apps. Extract client via `dh.get_client(surface.id()).ok()`.

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
- **`LayerMap` guard (MutexGuard) must be dropped before calling `keyboard.set_focus()`** — `set_focus` triggers `SeatHandler::focus_changed()` which may need `&mut self`.
- **Layer surface exclusive focus must be guarded** — only grab keyboard focus when it's not already on this surface. Otherwise every commit from an Exclusive layer surface steals focus back.
- **`pointer_over_layer` must be reset on layer destroy and fullscreen enter** — stale flag breaks all input until next motion event.

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
