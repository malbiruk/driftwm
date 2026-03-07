# XWayland Support for driftwm

## Context

driftwm is a pure-Wayland compositor. Many Linux apps (Steam, Wine/Proton games, older GTK2/Qt4 apps, JetBrains IDEs) require X11. Adding XWayland support lets these apps run on the infinite canvas alongside native Wayland windows.

smithay 0.7.0's `Window` type already wraps both Wayland and X11 via `WindowSurface` enum — but driftwm currently calls `.toplevel().unwrap()` **78 times across 15 files**, which will panic for X11 windows. The biggest part of this work is making the codebase window-type-agnostic.

## Approach: Direct XWayland via smithay (not xwayland-satellite)

niri uses external xwayland-satellite. We use smithay's built-in `XWayland` + `X11Wm` + `XwmHandler` instead because:
- Canvas coordinate model needs tight control over X11 window placement
- Override-redirect windows need compositor-level positioning relative to parent
- Simpler dependency chain (no external binary)

---

## Phase 1: Window-Type-Agnostic Refactor (COMPLETE)

All 78 `.toplevel().unwrap()` calls replaced. Also fixed 10 `.wl_surface().unwrap()` calls.

- ~50 became `w.wl_surface()` via `WaylandFocus` trait
- ~8 became `WindowExt` methods (`send_close`, `enter/exit_fullscreen_configure`)
- ~15 find-by-surface patterns became `.wl_surface().as_deref() == Some(&surface)`
- ~5 `with_pending_state`/`send_configure` wrapped in `if let Some(toplevel)`

New file: `src/window_ext.rs` — `WindowExt` trait for type-dispatched operations.

---

## Phase 2: XWayland Plumbing

### 2a. Cargo.toml — add feature

```toml
smithay = { ..., features = [..., "xwayland"] }
```

This enables `WindowSurface::X11`, `Window::new_x11_window()`, `X11Wm`, `XWayland`, `XwmHandler`, `XWaylandShellHandler`.

### 2b. State fields — `src/state/mod.rs`

```rust
pub xwayland_shell_state: XWaylandShellState,
pub x11_wm: Option<X11Wm>,
pub x11_override_redirect: Vec<X11Surface>,  // menus/tooltips, rendered separately
pub x11_display: Option<u32>,
```

Initialize `XWaylandShellState::new::<Self>(&dh)` in `DriftWm::new()`.

### 2c. Process lifecycle — `src/backend/winit.rs` and `src/backend/udev.rs`

Spawn XWayland **after** backend init, **before** setting `WAYLAND_DISPLAY` in env:

```rust
let (xwayland, xwayland_client) = XWayland::spawn(&dh, None, [], true, Stdio::null(), Stdio::null(), |_| ())?;
```

Register calloop source. On `XWaylandEvent::Ready`:
1. Set `DISPLAY=:{display_number}` in process env
2. Call `X11Wm::start_wm(loop_handle, x11_socket, client)`
3. Store result in `state.x11_wm`

On `XWaylandEvent::Error` or `disconnected()`: log error, clear state, optionally retry.

### 2d. Helper — `src/state/mod.rs`

```rust
pub fn find_x11_window(&self, x11: &X11Surface) -> Option<Window> {
    self.space.elements().find(|w| w.x11_surface() == Some(x11)).cloned()
}
```

---

## Phase 3: XwmHandler + XWaylandShellHandler — `src/handlers/xwayland.rs` (new file)

Add `pub mod xwayland;` to `src/handlers/mod.rs`. Add `delegate_xwayland_shell!(DriftWm);`.

### Managed window lifecycle

| Callback | Action |
|----------|--------|
| `new_window` | No-op (wait for map request) |
| `map_window_request` | `set_mapped(true)`, `Window::new_x11_window()`, map to Space at viewport center, set focus, apply SSD if `!is_decorated()`, apply window rules using `class()` as app_id |
| `unmapped_window` | Unmap from Space, remove decorations, update focus history |
| `destroyed_window` | Same as unmapped |

### Configure requests (canvas coordinate mapping)

**Managed windows**: approve size changes, **ignore position requests**. Position is compositor-controlled on the infinite canvas.

```rust
fn configure_request(&mut self, _, window, _x, _y, w, h, _reorder) {
    let mut geo = window.geometry();
    if let Some(w) = w { geo.size.w = w as i32; }
    if let Some(h) = h { geo.size.h = h as i32; }
    window.configure(Rectangle::from_loc_and_size((0, 0), geo.size)).ok();
}
```

### Override-redirect windows (menus, tooltips, dropdowns)

| Callback | Action |
|----------|--------|
| `new_override_redirect_window` | No-op (wait for mapped) |
| `mapped_override_redirect_window` | Push to `x11_override_redirect` vec |
| `configure_notify` (for OR) | Update tracked geometry — honor position relative to parent |

OR windows are **not** in the `Space`. Rendered manually in `compose_frame()` above managed windows. Position mapping:
- Find parent via `transient_for()` → look up parent X11Surface → find canvas position
- Compute: `canvas_pos = parent_canvas_pos + (or_x11_pos - parent_x11_pos)`
- **Fallback**: if no parent, use focused window or raw X11 coords offset by camera

### State change requests

| Request | Handler |
|---------|---------|
| `fullscreen_request` | Find Window, call existing `enter_fullscreen()` |
| `unfullscreen_request` | Find output, call `exit_fullscreen_on()` |
| `move_request` | Initiate `MoveSurfaceGrab` |
| `resize_request` | Initiate `ResizeSurfaceGrab` |
| `maximize/minimize` | Ignore (no maximize/minimize in driftwm) |

### Clipboard/selection bridging

```rust
fn allow_selection_access(&mut self, ..) -> bool { true }
fn send_selection(..) { self.x11_wm.as_mut().unwrap().send_selection(...) }
fn new_selection(..) { self.x11_wm.as_mut().unwrap().new_selection(...) }
fn cleared_selection(..) { self.x11_wm.as_mut().unwrap().cleared_selection(...) }
```

---

## Phase 4: SSD Decorations for X11

Existing `WindowDecoration` struct works — keyed by `wl_surface.id()`.

- In `map_window_request`: if `!window.is_decorated()` (MOTIF hints), create SSD
- Window rules also apply: `decoration = "server"` forces SSD
- Hit-testing works identically (operates on screen coords)

**WlSurface timing issue**: `X11Surface::wl_surface()` returns `None` until xwayland-shell serial matching completes. Focus, decorations, foreign-toplevel all need WlSurface. Strategy:

1. In `map_window_request`: `set_mapped(true)`, create Window, map to Space. If `wl_surface()` is `Some`, do focus + decorations immediately. Otherwise mark pending.
2. In `surface_associated()` callback: check if pending, then set focus, create decorations, announce foreign-toplevel.
3. Alternative: defer to first `commit()` in compositor handler.

---

## Phase 5: Rendering — `src/render.rs`

### Managed X11 windows
No changes needed. `Window::new_x11_window()` implements `SpaceElement` and `AsRenderElements<R>`.

### Override-redirect windows
Add to `compose_frame()`: iterate `state.x11_override_redirect`, for each with a `wl_surface()`:
1. Compute canvas position (parent offset + OR relative position)
2. Create `WaylandSurfaceRenderElement`
3. Wrap in `RescaleRenderElement` for zoom
4. Insert between Window and Layer elements

---

## Phase 6: Config — `src/config/toml.rs`, `src/config/mod.rs`

```toml
[xwayland]
enabled = true  # default true
```

Window rules match on `app_id` — for X11, use `class()` via `WindowExt::app_id_or_class()`.

---

## Implementation Order

1. **Phase 1** — COMPLETE
2. **Phase 2** — Cargo feature + state fields + XWayland spawn
3. **Phase 3** — XwmHandler impl (X11 apps appear as windows)
4. **Phase 4** — SSD decorations for X11 + window rules
5. **Phase 5** — Override-redirect rendering
6. **Phase 6** — Clipboard bridging + config toggle

## Verification

1. `cargo build && cargo clippy` after each phase
2. Phase 2: `ps aux | grep Xwayland` shows running process, `echo $DISPLAY` shows `:N`
3. Phase 3: `DISPLAY=:N xterm` opens a window on the canvas
4. Phase 4: X11 windows have title bar and shadow, window rules apply
5. Phase 5: right-click in xterm shows menu at correct position
6. Phase 6: copy/paste works cross-toolkit

## Key Risks

- **WlSurface timing** (highest risk): must defer focus/decorations/foreign-toplevel to `surface_associated()` or first commit
- **OR window positioning** (trickiest): X11 OR windows use absolute screen coords, must compute canvas-relative via parent lookup
- **Multi-monitor**: X11 sees a single virtual screen (acceptable, same as Sway)

## Smithay XWayland API Reference

### XWayland::spawn signature
```rust
pub fn spawn(dh: &DisplayHandle, display: impl Into<Option<u32>>, envs: I,
    open_abstract_socket: bool, stdout: impl Into<Stdio>, stderr: impl Into<Stdio>,
    user_data: F) -> io::Result<(Self, Client)>
```

### XWaylandEvent
```rust
pub enum XWaylandEvent {
    Ready { x11_socket: UnixStream, display_number: u32 },
    Error,
}
```

### X11Wm::start_wm
```rust
pub fn start_wm<D>(handle: LoopHandle<'static, D>, connection: UnixStream,
    client: Client) -> Result<Self, Box<dyn Error>>
where D: XwmHandler + XWaylandShellHandler + 'static
```

### XwmHandler trait (key methods)
```rust
fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm;
fn new_window(&mut self, xwm: XwmId, window: X11Surface);
fn map_window_request(&mut self, xwm: XwmId, window: X11Surface);
fn mapped_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface);
fn unmapped_window(&mut self, xwm: XwmId, window: X11Surface);
fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface);
fn configure_request(&mut self, xwm: XwmId, window: X11Surface,
    x: Option<i32>, y: Option<i32>, w: Option<u32>, h: Option<u32>,
    reorder: Option<Reorder>);
fn configure_notify(&mut self, xwm: XwmId, window: X11Surface,
    geometry: Rectangle<i32, Logical>, above: Option<X11Window>);
fn resize_request(&mut self, xwm: XwmId, window: X11Surface,
    button: u32, resize_edge: ResizeEdge);
fn move_request(&mut self, xwm: XwmId, window: X11Surface, button: u32);
fn allow_selection_access(&mut self, xwm: XwmId, selection: SelectionTarget) -> bool;
fn send_selection(&mut self, ..., fd: OwnedFd);
fn new_selection(&mut self, ..., mime_types: Vec<String>);
fn cleared_selection(&mut self, ..., selection: SelectionTarget);
```

### XWaylandShellHandler trait
```rust
fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState;
fn surface_associated(&mut self, xwm: XwmId, wl_surface: WlSurface, surface: X11Surface);
```

### X11Surface key methods
```rust
pub fn wl_surface(&self) -> Option<WlSurface>
pub fn set_mapped(&self, mapped: bool) -> Result<(), X11SurfaceError>
pub fn configure(&self, rect: impl Into<Option<Rectangle<i32, Logical>>>) -> Result<()>
pub fn is_override_redirect(&self) -> bool
pub fn is_decorated(&self) -> bool  // MOTIF hints
pub fn class(&self) -> Option<String>
pub fn title(&self) -> Option<String>
pub fn geometry(&self) -> Rectangle<i32, Logical>
pub fn transient_for(&self) -> Option<X11Window>
```
