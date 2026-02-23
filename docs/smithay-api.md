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
