# Touchscreen plan

Touchscreen is built on an integration branch (`touchscreen`), not directly on
main. PR #163 contributes the input foundation (retargeted onto this branch);
the full experience — grab-based gestures, window management on touch, momentum,
cursor handling, on-screen-keyboard positioning — is built on the branch and
merged to main as one feature, so main never ships a half-working touch UX.

## Status (2026-06-26, real-hardware udev test)

Works:
- Canvas 1-finger pan + 2-finger pinch-zoom.
- 1/2-finger touches forward to apps.

Broken / missing (drives the follow-ups below):
- 3-finger gestures: bad and inconsistent. Pulled from #163; rebuilt as a grab.
- No pan momentum — 1-finger pan has no flick-to-coast.
- Titlebar drag and the SSD close button do nothing on touch.
- Double-tap (e.g. open a folder in thunar) doesn't register — a forwarding bug;
  likely dissolves with the grab rework (see Architecture).
- Multi-output: touches map to `active_output()` instead of the output under the
  touch point — mis-maps on secondary outputs.
- OSK: untested.

## Architecture: grab-based, not a state machine

Both niri and cosmic-comp implement touch gestures / move / resize as smithay
`TouchGrab`s, not a hand-rolled state machine:

- cosmic-comp's `MoveGrab` implements **both** `PointerGrab` and `TouchGrab` on
  one struct (`shell/grabs/moving.rs`); its resize grabs likewise.
- niri shares `MoveGrab` across pointer/touch (`PointerOrTouchStartData`) plus a
  dedicated `TouchOverviewGrab` / `touch_resize_grab`. On touch-down it
  conditionally `set_grab`s, then **always** calls `handle.down(under.surface, …)`
  and lets grab routing decide who consumes the event.

The #163 implementation instead hand-rolls a `TouchGestureMode` state machine
with an `any_on_window` kill-switch tangled into the input handler, and never
uses `set_grab`.

Keep from #163 (correct, matches the references):
- `seat.add_touch()`.
- `FocusTarget: TouchTarget` (already in `state/focus.rs`) — the forwarding target.
- Basic down/motion/up/frame forwarding to the surface-under.

Rework on the branch:
- **Gestures + move + resize → `TouchGrab`s.** Add `TouchGrab` impls to the
  existing `MoveSurfaceGrab` / `ResizeSurfaceGrab` (mirroring cosmic's dual-impl
  `MoveGrab`) and add a canvas-gesture grab (parallel to `PanGrab`). Reuses the
  existing grab logic instead of duplicating move/resize into an enum.
- **Forwarding → unconditional** — `handle.down(under.surface, …)` always; grabs
  intercept. Removes the `any_on_window` kill-switch (3-finger-over-window just
  works) and likely fixes the double-tap bug (the up was only forwarded when no
  gesture was active).
- **Output mapping → output-under-touch**, not `active_output()`.
- Hide the pointer on touch-down (niri sets `pointer_visibility = Disabled`) —
  see the cursor follow-up.

## #163 scope (lands on the `touchscreen` branch)

Contributor PR, kept minimal:
- `cargo fmt` (CI gates on `cargo fmt --check`).
- Config surface split (below).
- Remove the 3-finger swipe path (tested badly; rebuilt as a grab here).

Everything behavioral — double-tap, decorations, momentum, the gesture model,
cursor, OSK — is maintainer work on the branch.

## Config surface (final shape)

Principle: `[input.*]` is device config; behavior lives in its own sections
(driftwm already does this — `[input.mouse]` device vs `[mouse]` bindings; niri's
`input.touch` is off/calibration/map-to-output with behavior in a top-level
`gestures` block).

- `[input.touch] enable` — keep (device on/off; future home for
  `calibration_matrix`, `map_to_output`). Ends up the only field here.
- `[navigation] touch_speed` — pan multiplier, sibling of `trackpad_speed` /
  `mouse_speed`.
- `[zoom] touch_speed` — pinch multiplier; joins the future trackpad (pinch) /
  mouse (wheel) zoom multipliers from the separate zoom-speed issue.
- **Drop `touch_to_focus`** — touch focuses + raises unconditionally, same as the
  (hardcoded) click-to-focus; niri activates on touch-down with no gate, and the
  `widget` rule already covers the don't-raise case. Honor the widget exclusion
  in the hardcoded path.
- **Drop `enable_canvas_gestures` + `swipe_threshold`** — gesture-model knobs the
  bindable rework subsumes; shipping them just means deprecating them later.
  `enable` (whole-device off) is the only touch toggle until the bindable model
  lands.

## Follow-up: SSD decoration interaction on touch

Titlebar drag → move, and the close button, do nothing on touch because the
touch path never hit-tests decorations the way `input/pointer.rs` does. Falls
out of the grab rework: on touch-down, hit-test decorations; a titlebar hit
`set_grab`s the (now touch-capable) move grab; a close-button hit closes on
release if the finger is still inside. Arguably required before announcing touch
support, since a window manager you can't move or close windows on reads as
broken.

## Follow-up: pan momentum

1-finger pan does `set_camera` per motion with no velocity tracking, so there's
no flick-to-coast. Sample velocity over the last few motion events and kick the
existing `drift` momentum animation on last-finger-up — the same coast
mouse/trackpad pan already gets.

## Follow-up: bindable `[touch]` model + interaction rework

Make touch gestures bindable, parallel to `[gestures]` / `[mouse]`, with a
separate `[touch]` behavior section (NOT folded into `[gestures]` — that's
trackpad/libinput relative-gesture semantics; touch is absolute-positioned with
a real on-window/on-canvas distinction baked into where the finger lands).

Decide the surface before it ships so it's release-stable even if the
implementation lands incrementally.

Target interaction model (escalation: fingers go content → system):

- **1 finger** — window = content (forward), empty canvas = pan.
- **2 finger** — window = app's own pinch/scroll (forward), empty canvas = zoom
  viewport.
- **3 finger** — compositor pan + pinch, **anywhere incl. over a window** (apps
  don't claim 3-finger touches, so it's unambiguous). Fixes the current
  limitation where any finger on a window kills gestures, so you can't pan/zoom
  over a dense canvas.
- **4 finger** — global navigation: swipe = navigate-nearest, pinch-in/out =
  overview / home-toggle. Position-independent.
- **3-finger tap** — center-window (position-aware: centers the tapped window;
  empty-canvas fallback = center focused window). Replaces the touchpad's
  `4-finger-hold` — hold has no release-into-action idiom on glass and occludes
  the screen. 3-finger tap is free on touch (it's only middle-click on the
  touchpad's click-emulation layer, a different layer entirely) and lands
  cleanly, unlike an error-prone 4-finger simultaneous tap.
- **3-finger double-tap** (no drag) — fit-window (maximize toggle). Mirrors
  desktop double-click-titlebar-to-maximize.
- **3-finger double-tap + drag** — move-window. Mirrors the existing trackpad
  `3-finger-doubletap-swipe = move-window` exactly.

### Window resize on touch — deliberately minimal

The canvas/zoom model demotes resize: window size and apparent size are
decoupled, so "make this bigger" is served by pinch-zooming the *viewport*, not
resizing the window. Precision resize (true content dimensions) is a power-user
task and touch is the worst modality for it — leave it on keyboard/mouse
(`resize-window`).

- The discrete "fill the screen" intent is covered by `fit-window` (3-finger
  double-tap above), not a drag.
- Don't make the 8px resize border touch-draggable (far below a ~40px fingertip;
  widening it conflicts with content drags near window edges) and don't use
  2-finger-on-window (that's the app's own pinch).
- Precision `resize-window` stays optional, exposed only via the bindable model
  as a 3-finger on-window gesture variant if ever wanted. The action already
  exists; it just needs a trigger.

Rule of thumb: 1–2 fingers over a window belong to the app; reserve 3+ for the
compositor. This is the touchscreen analog of "scroll/pinch → apps, 3–4 finger
swipes → compositor" on the touchpad, so touch ends up consistent with the
trackpad model rather than a special case.

Notes:
- Touch's absolute position makes on-window/on-canvas/anywhere contexts cleaner
  than the trackpad (no cursor-derived ambiguity).
- The gesture-internals bugs in the #163 state machine (swipe-origin divisor,
  stale pinch baseline on finger-count change) are moot — the state machine is
  replaced by grabs, not patched.

## Follow-up: cursor hide-on-touch

Hide the pointer when touch starts, restore on next pointer (mouse/trackpad)
motion — standard mutter behavior (niri sets `pointer_visibility = Disabled` in
`on_touch_down`). #163 does none of this, so a stale arrow sits mid-screen during
touch use.

Shape:
- A separate `hidden_by_touch` bool on `CursorState` — do NOT overwrite
  `cursor_status` with `Hidden` (that field is client-owned; clobbering loses the
  app's requested shape on restore).
- Set on `TouchDown`, clear on the next pointer-motion handler in
  `input/pointer.rs`.
- OR it into the hidden gate in `render/cursor.rs::build_cursor_elements`
  (alongside `CursorImageStatus::Hidden => vec![]`). That one gate also clears
  the KMS hardware-cursor plane on udev (plane is driven from the same render
  elements), so no separate udev path.
- Touch routes through the *touch* handle, not the pointer, so
  `pointer.current_location()` never moves during touch — the cursor reappears
  where it was, no extra bookkeeping.

## Follow-up: OSK camera-positioning (biggest lever for tablet usability)

Protocols are wired already — `input-method-v2` (`InputMethodHandler`,
`handlers/mod.rs`), `text-input-v3`, and `virtual-keyboard-v1` are delegated,
plus wlr-layer-shell. An external OSK (squeekboard via input-method, wvkbd via
virtual-keyboard) can connect, render as a bottom layer surface, type into apps,
and auto show/hide via smithay's text-input↔input-method bridge. The OSK stays an
external program (same philosophy as launcher / lock / screenshot).

The gap is positioning. When a bottom-anchored OSK appears it occludes the lower
screen and the focused text field disappears behind it. The infinite-canvas-
native fix:

- Read the text-input **cursor rectangle** (`set_cursor_rectangle`, surface-
  local), transform to screen space via window canvas-pos → camera/zoom, and if
  it lands in the OSK-occluded band, **animate the camera up** so the caret clears
  — reusing the existing focus-to-window animation. This beats the mobile "shrink
  the fullscreen app" and desktop "exclusive-zone reserve" models: an exclusive
  zone only shrinks where new/maximized windows go, not where a floating focused
  window currently sits.
- `parent_geometry` (`handlers/mod.rs`) returns raw `window.geometry()` (window-
  local size, not camera-transformed) — this positions IME candidate popups (CJK
  completion, emoji). Verify they land correctly at non-1.0 zoom / off-origin
  camera.
- Cursor rect arrives in surface-local px; at zoom ≠ 1.0 the on-screen caret is
  scaled, so both the popup positioning and the camera-pan must multiply by zoom.
