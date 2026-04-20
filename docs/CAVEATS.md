# Caveats

Things to keep in mind as the codebase grows.

## Never block the event loop

calloop is single-threaded. A 50ms DNS lookup, a slow file read, a stuck subprocess — anything that blocks the main thread freezes the entire compositor. All I/O must be async or offloaded.

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
- SCTK-based toolkits (Alacritty) interpret `Tiled + size=None` as "stay at current tile size," not "pick preferred." So fit/fullscreen exit paths must always send an explicit size (`window_ext.rs::exit_fit_configure`, `exit_fullscreen_configure`), which in turn requires tracking a `RestoreSize` separately from `window.geometry().size` because some clients (Chromium) shrink their reported geometry on each round-trip.
- Every new "this client behaves weirdly under Tiled" issue traces back here.

cosmic-comp makes the exact same bet (`clip_floating_windows` default-on in `AppearanceConfig`, `src/shell/element/window.rs:204`) and has carried the same complexity for years. This is a settled hack in Wayland-land, not a novel misstep — but it's still a hack. If a future protocol extension exposes "suppress client chrome" as a first-class signal, migrate to it and delete all of the above.

## xcursor `pixels_rgba` is actually BGRA

The `xcursor` crate's `Image::pixels_rgba` field is misleadingly named. The bytes come straight from the XCursor file, which stores pixels as `uint32` ARGB little-endian — i.e. `[B, G, R, A]` in memory. Interpreted as RGBA, the channels are wrong.

The matching DRM fourcc for that byte order is `Fourcc::Argb8888` (which smithay maps to GL `BGRA_EXT`), **not** `Fourcc::Abgr8888`. Using `Abgr8888` swaps R and B on screen — a yellow cursor renders mint-blue, a red cursor renders violet, etc.

niri gets this right in `src/cursor.rs` (`MemoryRenderBuffer::from_slice(&frame.pixels_rgba, Fourcc::Argb8888, ...)`). Do the same here.

## What to unit test

Smithay glue code (handlers, delegates) is not worth testing — it's framework boilerplate. Write tests for **your** logic:

- **Canvas/viewport math** (milestone 3): coordinate transforms, screen↔canvas conversion, viewport clipping. Pure functions, very testable.
- **Gesture state machine** (milestone 5): feed event sequences, assert state transitions and emitted commands.
- **Keybinding lookup** (when data-driven): binding table resolution, modifier matching, conflict detection.
- **Config parsing** (milestone 12): TOML deserialization, defaults, validation.

Manual testing is fine for everything else until you have a headless backend for integration tests.
