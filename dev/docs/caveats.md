# Caveats

Things to keep in mind as the codebase grows.

## Never touch `Space` directly — go through the stage

The stage (`src/stage/`) is the source of truth for the window list, z-order, positions, focus history, fullscreen membership, pin-to-screen membership, and fit state. Read window state from it (`stage.windows()`, `stage.position_of`, `DriftWm::element_under` / `window_bbox_with_popups`); mutate through `DriftWm::map_window` / `raise_window` / `unmap_window` (all in `src/state/window_lifecycle.rs`, which holds exactly this set) and the stage-backed methods. `Space` holds no window elements at all — it survives only as the output registry (`map_output` / `outputs` / `output_geometry`). A clippy `disallowed-methods` lint (see `clippy.toml`) rejects every `Space` element API (reads *and* writes) and `Space::refresh`, and debug builds run `verify_stage_invariants` every frame in `post_render` — a panic there means a mutation bypassed the wrappers.

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
- **No respawn-on-crash.** If satellite dies mid-session (rare), X11 stays dead until driftwm restart. Future enhancement.

## What to test where

Smithay glue code (handlers, delegates) is not worth unit testing — it's framework boilerplate. Pure logic (canvas math, config parsing, gesture/binding resolution) gets unit tests; stage policy gets the proptest harness; the protocol↔policy wiring gets the in-process headless fixture (`src/tests/`), where a real `DriftWm` serves real wayland clients with no display. The full map and the testing rules live in [testing.md](testing.md).

## Bounding boxes must include popups

Smithay's inherent `Window::bbox()` covers the toplevel and its subsurfaces but **not** popups. `Space` always used the popup-inclusive box (`SpaceElement::bbox` is `bbox_with_popups`), and everything that replaced `Space` must too: hit-testing, render culling, frame-callback throttling, and dirty-marking all go through `window.bbox_with_popups()` (or `DriftWm::window_bbox_with_popups`). A popup-less box clips overhanging popups at output boundaries, throttles their frames to the off-screen heartbeat, and drops focus to the window behind when the popup is hovered. `Window::bbox` is banned via the `disallowed-methods` lint in `clippy.toml`.

Canvas-layer widgets (`LayerSurface`) split the same way: culling, throttling, and dirty-marking use `bbox_with_popups()`, while initial placement (`handle_canvas_layer_commit`) and persistence deliberately use the popup-less `bbox()` — an open menu must not shift where a widget centers or what size gets saved.

## Input hit-tests the animation's destination, not the drawn rect

Every stage read in the input path takes the destination — `topmost_under`, `element_under_skipping`, `decoration_under` and `surface_under` all walk `Stage::entries`, whose `position` is the settled one (`src/input/mod.rs`) — while the render path draws at `geometry_visual_rect`, which has no consumer anywhere under `src/input/`. The two rects stay disjoint for the whole animation (for a fit, by `old_size/2 - usable.size/2 + gap (+bar)` — hundreds of pixels), so a window can't be clicked where it is drawn: ~120 ms at default speeds, reading as a missed click, and ~4 s at `[effects] animation_speed = 0.02`. This looks like a bug and gets filed as one. It isn't.

The model: a window animation is eye candy over a state change that is already complete. The window is at its destination the moment the action runs and the picture is catching up, so hit-testing the destination is correct, and so are the surface-local coordinates `surface_under` derives from it. A *camera* animation is the opposite — the viewport genuinely is moving, so input tracks it live, which is why the grab-install chokepoints (`arm_interactive_move`, `begin_client_resize`) take the viewport out of flight rather than freezing input. Going visual-aware would mean threading `geometry_visual_rect(id).loc` into those four reads (checking `geometry_space` first — that accessor's doc warns about Canvas vs Screen), and it carries a non-obvious tax: the idle pointer-focus re-poll has to be suppressed while any transition runs, or moving pixels drag enter/leave across a stationary cursor. Do not reopen without revisiting the model first.

## `resize_on_border` gates resizing, not membership

The option gates `decoration_hit_for` and `pinned_decoration_under` (`src/input/mod.rs`), which produce `DecorationHit::ResizeBorder` — *resize* behaviour. It does not gate `surface_under` or `pinned_window_under`, which decide *membership*: pointer focus, binding context, pick target. With the option off the 8 px band is still uniformly the window's, it just can't be dragged — `pointer_context` asks `resize_margin_under`, an ungated membership walk, whenever the option is off, so the band binds `OnWindow` around a pin and around a canvas window alike (`an_inert_resize_margin_binds_as_window_pinned_or_not` and its SSD sibling in `src/tests/pinned_phantom.rs`; `an_inert_resize_margin_starts_no_resize` pins the resize half).

The opposite direction — letting the inert band fall through to whatever is behind it — was rejected, because it is not a no-op. `pointer_focus_under` continues past `surface_under` into `canvas_layer_under`, widget windows and then `Bottom`/`Background`, so a wallpaper client would take an enter/leave pair every time the cursor crossed any window's ring, and that arm sets `pointer_over_layer`, which makes `maybe_hover_focus` return early — `focus_follows_mouse` would be *suppressed* in the ring rather than falling through to canvas. And `render/shaders.rs` draws the border `border_width_logical` *outside* the window rect, i.e. inside this band, so `[decorations] border_width > 0` plus a fall-through would give a visible border that is click- and hover-through.

## Every position read derives from committed geometry

`msg state`, the state file (`src/state/persistence.rs`) and the `move` read arm all derive a window's position from `window.geometry().size` — one source of truth, and neither `docs/ipc.md` nor `docs/cli.md` documents which size the center comes from. The consequence is that `driftwm msg move` is non-idempotent mid-settle: the write arm (`cmd_move` and the `MoveToBookmark` keybind, both through `map_window_to_rule_point` in `src/state/recenter.rs`) re-aims the owed `pending_recenter` at the requested point instead of dropping it and lands where asked whatever the client is still committing, so a `get` followed by a `move` back to the reported point relocates the window. Changing that one reader alone would only make the three disagree; fix, if ever, by moving every reader onto one accessor at once.

Re-aiming rather than dropping buys the correct landing at the cost `drop_owed_recenter`'s own doc warns about: an entry left owed gates `reflow_grown_snapped_window`. The settle trigger is `geo.size != pre_exit_size` (`src/handlers/compositor.rs`), so a client that acks the exit configure and then commits back at *exactly* its pre-exit size never fires it — the window keeps a provisional placement derived from the size the exit configured rather than the one the client kept (leaving it half that difference off the requested point) and stays out of snap/cluster reflow until it unmaps. Nothing bounds either half.

## A fit or filled window's fullscreen exit restores the configured size

`enter_fullscreen`'s fit/fill arm reads `configured_window_size`, which is what closes the pre-ack race — both `fit_window` and `fill_window` map to their new position in the same breath as the configure, so a fullscreen pressed into that gap would otherwise pair the new position with the pre-fit/pre-fill committed size. But that is pending state and no client-initiated resize updates it. So a client that resizes *itself* after being fit or filled gets its fit/fill-configured size back on the fullscreen exit rather than its current one. That is a deliberate trade, not an oversight: the stage still holds the fitted/filled position, so the configured size is the rect that pairs with it. The deeper consequence is that a self-resize leaves fit/fill membership claiming a rect the window no longer occupies.

Two knock-on effects specific to fit, both accepted: `compute_fit_geometry` does not clamp to `SizeConstraints`, so a client that refuses the fit size has an unattainable size recorded in `set_fullscreen` (the end state is unchanged — the exit re-configures the fit size and the client re-clamps identically), and such a client no longer trips `MIN_RESTORE_FLOOR`.

## Clearing fit strands a snapped fit's cluster shift

`fit_window_snapped` (`src/state/fit.rs`) shifts the primary's snap cluster aside before fitting, and the neighbours only come home when the toggle routes back through `unfit_window_snapped`. Anything that clears fit membership behind that toggle's back makes the return leg unreachable: the neighbours stay where the fit pushed them, and the next `toggle_fit_window_snapped` reads as a fresh fit and pushes them a second time. `begin_client_resize` (`src/state/resize.rs`) already opens the hole — grab a snap-fitted window's border and the shift is orphaned — and `fill_window` (`src/state/fill.rs`) widens it, since filling a fit window is a one-keystroke route to the same state — though only when the fill actually fires, as a fill that computes no change returns before it clears the fit. Accepted as-is: closing it means either replaying the shift from every fit-clearing site or making fit membership own the displacement it caused, and the displaced windows are draggable.

## A zero-net-change resize leaves the window reading as grabbed

Grab start sets `Resizing` in *pending* state only, so a resize that ends where it began leaves `send_pending_configure` with nothing to send and the client with no reason to commit, stranding `ResizeState::WaitingForLastCommit` (`src/grabs/resize_grab.rs`). It self-clears on the window's next repaint, but until then the window reads as under an interactive grab: its next geometry animation is silently skipped and relaunch adoption bails. Recognise the symptom rather than chasing the missing animation.

That gap is unbounded, so anything can place the window inside it, and `handle_resize_commit` has to survive two different kinds of intruder.

A placement that only *moves* the window — an IPC `move`, a bookmark jump, a `SendToOutput`, a nudge, a pin/unpin — is handled by compensating the dragged edge *incrementally*: the live position (or pin `screen_pos`) plus `last_committed_size - current_geo.size`, rather than absolutely from the grab start, which would restore the window to where the grab began. The per-commit deltas telescope to the same total, so a drag nothing else touches settles exactly where the absolute form put it.

A placement that also changes the *size* — fill, fit, fullscreen entry, or any of the three exits — needs the compensation skipped outright, because the delta would then be measured against a size the resize never asked for and applied on top of a position the placement already chose (a fill lands the window a screen-width off-canvas; a fullscreen leaves the output it is meant to fill). `placement_owns_size` is the guard, and each of its witnesses is exact rather than heuristic: `begin_client_resize` clears fit and fill at entry, so either membership at settle time landed after the grab started, and an owed `pending_recenter` is precisely how the exits record having configured a different size. The map itself stays unconditional either way — it doubles as the resize's z-raise.

The grab's own `apply_resize` keeps the absolute formula, correctly: a live drag owns the position.

## Exit placement is one shared tail — grow a new exit through it

The *map to the restored location → equal-size branch or insert a `PendingRecenter`* tail is `DriftWm::establish_exit_placement` (`src/state/recenter.rs`), called by `unfit_window` (`src/state/fit.rs`), `unfill_window` (`src/state/fill.rs`) and `exit_fullscreen_on` (`src/state/fullscreen.rs`). Hand-maintained copies of it drift, and the equal-size branch's `drop_owed_recenter` is the step that goes missing. Three things are deliberately *not* shared, so a fourth exit doesn't try to fold them in:

- **The animate → configure order.** Fit and fill seed the geometry animation *before* their configure; the fullscreen exit animates last, after the configure, the map, the re-pin and the camera restore, from a screen-space seed captured before any of it (and in `AnimSpace::Screen` when the window is re-pinned). Pinned by `unfill_animates_straight_to_the_restored_rect` and the frozen/handover fullscreen-exit family in `src/tests/window_animation.rs`.
- **`target_center` is a parameter, not derived from the mapped location.** `unfit_window` maps to a location `frame_loc_for_center` already truncated out of its center and records the un-truncated one; re-deriving costs up to a pixel per axis (`unfit_settles_on_the_untruncated_center`).
- **`refresh_snap_rect` is true for fit and fill, false for fullscreen.** `fit_window_snapped` and `fill_window` cache a rect of their own that the exit invalidates; `enter_fullscreen` caches none, so the cached rect is still the pre-fullscreen one the exit hands back (`unfit_refreshes_the_snap_rect_its_fit_cached`, `fullscreen_exit_leaves_the_cached_snap_rect_alone`).

## Client resizes and owed-recenter drops have one entry point each

The *clear fit → clear fill → seed `ResizeState` → set `Resizing` → unset `Maximized`* sequence is `DriftWm::begin_client_resize` (`src/state/resize.rs`), called by the four entry points that can start a client resize (`input/pointer.rs`, `handlers/xdg_shell.rs`, `input/gestures/swipe.rs`, `input/touch.rs`). Two other sites pair a missing fit state with an unset `Maximized`, and neither is a fifth entry point: they share only that unset, and only for the reason it is easy to forget — a `Maximized` outliving the fit state is one the client can never shed, because its restore button (or a panel's `unset_maximized`) dispatches an `unmaximize_request` that `unfit_window` drops on the absent saved size. `adopt_relaunched` (`src/state/suspended.rs`) inherits a stage entry that never had fit state; `fill_window` (`src/state/fill.rs`) clears the fit itself, having first lifted the pre-fit size out as the fill's own restore point, and unsets `Maximized` in `send_size_configure`. That helper is shared with `unfill_window`, which is a third caller of the unset and not a sixth site either, because there the unset is inert: the only `set_fit` (`src/state/fit.rs`) is followed by a `clear_fill` and the only `set_fill` (`src/state/fill.rs`) by a `clear_fit`, so fit and fill membership are mutually exclusive and an unfill never sees a fit window. Neither the adopt nor the fill seeds a `ResizeState`, with no resize in flight, and fill *sets* fill membership where the resize clears it. Dropping an owed `pending_recenter` before establishing a placement is `DriftWm::drop_owed_recenter` (`src/state/recenter.rs`), called by seven arms: `input/actions.rs` twice, `state/fullscreen.rs`, `state/fill.rs`, `state/fit.rs`, `state/suspended.rs`, and `establish_exit_placement`'s own equal-size branch.
