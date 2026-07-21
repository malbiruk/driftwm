# Caveats

Things to keep in mind as the codebase grows.

## Never touch `Space` directly — go through the stage

The stage (`src/stage/`) is the source of truth for the window list, z-order, positions, focus history, fullscreen membership, pin-to-screen membership, and fit state. Read window state from it (`stage.windows()`, `stage.position_of`, `DriftWm::element_under` / `window_bbox_with_popups`); mutate through `DriftWm::map_window` / `raise_window` / `unmap_window` and the stage-backed methods. `Space` holds no window elements at all — it survives only as the output registry (`map_output` / `outputs` / `output_geometry`). A clippy `disallowed-methods` lint (see `clippy.toml`) rejects every `Space` element API (reads *and* writes) and `Space::refresh`, and debug builds run `verify_stage_invariants` every frame in `post_render` — a panic there means a mutation bypassed the wrappers.

Per-window output membership (`wl_surface.enter`/`leave`) is driven by `DriftWm::refresh_window_outputs`, not by `Space`: fullscreen windows belong only to their home output, pinned windows only to their pin target, and virtual placeholder outputs (dead `wl_output` global) are never entered. New enter/leave paths must route through it — membership sent from anywhere else reintroduces the multi-output fullscreen leak (a game unfullscreens when another output's camera pans over its parked window).

## Never block the event loop

calloop is single-threaded. A 50ms DNS lookup, a slow file read, a stuck subprocess — anything that blocks the main thread freezes the entire compositor. All I/O must be async or offloaded.

## Never lock `output_state` in a scrutinee

`output_state(output)` returns a `MutexGuard`. In an `if let`/`while let`/`match` scrutinee the guard lives to the end of the body, so re-locking inside deadlocks the event loop — the v0.14.0 freeze when a client destroyed its toplevel while fullscreen. Take the guard in a separate `let` statement. Two guards enforce this: `clippy::significant_drop_in_scrutinee` (warn in `Cargo.toml [lints]`, hard error under CI's `-D warnings`) rejects the pattern statically, and debug builds panic on a re-entrant lock — which also catches the variant clippy can't see, a named guard held across a call that re-locks.

## Client misbehavior must not crash the compositor

Clients can disconnect at any time, send malformed requests, or go unresponsive. Every piece of client-derived data should be validated. Prefer `if let` over `unwrap()` for anything from a client.

## Double-buffered state

Client state changes (attach buffer, set damage, set title) are not visible until `wl_surface.commit()`. Never read uncommitted state — it may be half-updated.

## Frame callbacks are mandatory

After rendering, call `window.send_frame()` for each visible window. This tells clients "your frame was displayed, you can draw the next one." Without it, clients either stop rendering or waste CPU drawing frames that never display.

## Input device ownership is exclusive

On real hardware (udev backend), the compositor owns all input devices via libinput. No other process can read them. In nested mode (winit), the parent compositor owns input and you only see translated events — no raw gestures.

## Serials must be monotonically increasing

`SERIAL_COUNTER.next_serial()` generates unique serials for input events. Reusing or going backwards breaks client-side validation. Always generate a fresh serial per event.

## We lie to clients about being tiled

driftwm sets all four `xdg_toplevel` Tiled states on every CSD window, even though no window is ever actually tiled — driftwm is a floating compositor. We clip client shadow ourselves regardless (via the `corner_clip` shader), so Tiled is **not** load-bearing for shadow suppression. What it actually buys is corner-radius uniformity: GTK/libadwaita/Chromium drop their own rounded corners on seeing Tiled, so our clip arc is the only one visible. Without Tiled, a client that rounds to 8 px inside our 10 px clip shows a subtle double-curve.

This is a deliberate semantic misuse of the protocol. The debt it incurs:

- Some clients (Zed, anything using `gpui`) also drop their own resize edge handles on seeing Tiled, reasoning that a tiled window has compositor-managed size. We compensate with a compositor-side invisible resize margin around every CSD window (`input/mod.rs::surface_under` / `decoration_under`), mirroring what Mutter and KWin do for CSD apps.
- SCTK-based toolkits (Alacritty) interpret `Tiled + size=None` as "stay at current tile size," not "pick preferred." So fit/fullscreen exit paths must always send an explicit size (`window_ext.rs::exit_fit_configure`, `exit_fullscreen_configure`), which in turn requires tracking a restore size (on the stage) separately from `window.geometry().size` because some clients (Chromium) shrink their reported geometry on each round-trip.
- Every new "this client behaves weirdly under Tiled" issue traces back here.

cosmic-comp makes the exact same bet (`clip_floating_windows` default-on in `AppearanceConfig`, `src/shell/element/window.rs:204`) and has carried the same complexity for years. This is a settled hack in Wayland-land, not a novel misstep — but it's still a hack. If a future protocol extension exposes "suppress client chrome" as a first-class signal, migrate to it and delete all of the above.

## xcursor `pixels_rgba` is actually BGRA

The `xcursor` crate's `Image::pixels_rgba` field is misleadingly named. The bytes come straight from the XCursor file, which stores pixels as `uint32` ARGB little-endian — i.e. `[B, G, R, A]` in memory. Interpreted as RGBA, the channels are wrong.

The matching DRM fourcc for that byte order is `Fourcc::Argb8888` (which smithay maps to GL `BGRA_EXT`), **not** `Fourcc::Abgr8888`. Using `Abgr8888` swaps R and B on screen — a yellow cursor renders mint-blue, a red cursor renders violet, etc.

## X11 apps run through xwayland-satellite

driftwm doesn't embed XWayland directly. X11 apps reach the compositor via [`xwayland-satellite`](https://github.com/Supreeeme/xwayland-satellite) (>= 0.7), which is itself a regular Wayland client that proxies X11 windows as plain xdg-toplevels. Implications:

- **External binary required.** Without `xwayland-satellite` in `$PATH`, X11 apps fail to launch (no `DISPLAY` exported). driftwm logs a warning at startup and continues running. Override the path via `[xwayland] path = "..."` if needed.
- **Eager spawn.** Satellite is spawned at compositor startup (not on-demand) and stays resident for the session. ~30MB resident overhead even if no X11 client ever runs. The on-demand `-listenfd` pattern (compositor pre-binds the X11 socket and hands the FD to satellite on first connection) races with multi-layout XKB configs (`layout = "us,ru"` + `options = "grp:win_space_toggle"`) under Xwayland 24.x: the queued X11 connection on the pre-bound socket triggers Xwayland's keyboard initialization before `wl_keyboard.keymap` arrives, satellite panics. Vanilla mode avoids the race. Revisit if upstream fixes the listenfd path.
- **`app_id` matches the X11 `WM_CLASS` instance** (typically lowercase). Window rules keyed on `xclass = "Steam"` no longer exist — use `app_id = "steam"` (note the lowercase).
- **Override-redirect popups arrive as xdg-popups.** The compositor's existing popup positioning handles them; no special render path.
- **Apps that pin windows to absolute screen coordinates** (older notification daemons, some game launchers) won't behave correctly. Run them in a nested compositor like `labwc` if needed.
- **Clipboard works through standard Wayland data-device protocol.** xwayland-satellite owns selections as a normal Wayland client; the compositor doesn't bridge clipboards manually.
- **Single sticky output per window.** driftwm reports exactly one `wl_output` to satellite's client (all other clients get every output their window overlaps). Satellite keeps a single output association per X11 window and derives that window's fake X11 root coordinates from it (see its [`ARCHITECTURE.md`](https://github.com/Supreeeme/xwayland-satellite/blob/master/ARCHITECTURE.md)). On the infinite canvas two viewports routinely co-observe the same window, so full-overlap membership makes the enter/leave for the second viewport strand that association, and X11 context menus then land offset until the window fully leaves and re-enters an output. The sticky policy lives in `state/membership.rs::desired_satellite_membership` and migrates the output only when the window's own center genuinely leaves it.
- **No respawn-on-crash.** If satellite dies mid-session (rare), X11 stays dead until driftwm restart. Future enhancement.

## What to test where

Smithay glue code (handlers, delegates) is not worth unit testing — it's framework boilerplate. Pure logic (canvas math, config parsing, gesture/binding resolution) gets unit tests; stage policy gets the proptest harness; the protocol↔policy wiring gets the in-process headless fixture (`src/tests/`), where a real `DriftWm` serves real wayland clients with no display. The full map and the testing rules live in [testing.md](testing.md).

## Bounding boxes must include popups

Smithay's inherent `Window::bbox()` covers the toplevel and its subsurfaces but **not** popups. `Space` always used the popup-inclusive box (`SpaceElement::bbox` is `bbox_with_popups`), and everything that replaced `Space` must too: hit-testing, render culling, frame-callback throttling, and dirty-marking all go through `window.bbox_with_popups()` (or `DriftWm::window_bbox_with_popups`). A popup-less box clips overhanging popups at output boundaries, throttles their frames to the off-screen heartbeat, and drops focus to the window behind when the popup is hovered. `Window::bbox` is banned via the `disallowed-methods` lint in `clippy.toml`.

Canvas-layer widgets (`LayerSurface`) split the same way: culling, throttling, and dirty-marking use `bbox_with_popups()`, while initial placement (`handle_canvas_layer_commit`) and persistence deliberately use the popup-less `bbox()` — an open menu must not shift where a widget centers or what size gets saved.
