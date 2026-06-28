# Touchscreen plan

Touchscreen is built on an integration branch (`touchscreen`), not directly on
main. PR #163 contributes the input foundation (retargeted onto this branch);
the full experience — grab-based gestures, window management on touch, momentum,
cursor handling, on-screen-keyboard positioning — is built on the branch and
merged to main as one feature, so main never ships a half-working touch UX.

## Status (2026-06-28, post-grab-rework hardware test)

The grab rework landed (commit a21153b). These follow-ups are now implemented:
grab-based architecture (`TouchGestureGrab` + a `TouchGrab` impl on
`MoveSurfaceGrab`), unconditional forwarding, SSD decoration interaction, pan
momentum, and cursor hide-on-touch.

Works on hardware:
- Canvas 1-finger pan (+ flick-to-coast) and 2-finger pinch-zoom.
- 1/2-finger forwarding to apps.
- SSD titlebar drag → move; SSD/CSD close button.
- 3-finger tap → center-window; 3-finger double-tap → fit-window;
  3-finger double-tap + drag → move-window.

Still rough (drives "Post-rework fixes" below):
- 3-finger pan wobbles — pinch-zoom fires on noisy spread during a pan.
- 4-finger swipe is eaten by zoom-to-fit — swipe and pinch both fire.
- CSD titlebar drag → move does nothing (SSD works).
- 3-finger double-tap + drag flashes a center before the move.
- App double-tap inconsistent across toolkits (Nautilus ✓, Thunar content ✗,
  Firefox ✗) — toolkit-side, not a compositor bug.

Still open from the follow-ups below: full output-under-touch (today maps to the
first / touch output, not per-touch-point), the bindable `[touch]` model, and
OSK camera-positioning (untested).

## Post-rework fixes (2026-06-28 hardware test)

Root causes traced (A–D are fixes; E and F are additive gestures in this batch).
All but the app-side double-tap are compositor-side gesture arbitration, not
touch-panel sensitivity. Order: B/C first (usability, low-risk), then A
(self-contained bug), then D (event-loop timer), then E and F (both build on D's
hold timer).

- **A. CSD titlebar drag → move.** `move_request` (`handlers/xdg_shell.rs`) only
  checks the *pointer* grab (`check_grab` → `pointer.grab_start_data()`), so a
  touch-initiated `xdg_toplevel.move` is dropped. Mirror niri: also match the
  *touch* grab serial (`TouchHandle::grab_start_data` / `with_grab`) and start a
  touch `MoveSurfaceGrab`. SSD works only because the compositor hit-tests its own
  titlebar; mouse works because a pointer grab exists.
- **B. 3-finger pan anti-wobble.** `apply_panzoom` applies pinch-zoom every frame
  gated only on `last_spread > 1.0`; the 3-finger spread metric (mean distance to
  centroid) is noisy, so zoom jitters during a pan. Deadzone the per-frame scale
  and/or lock to the dominant axis (pan vs zoom).
- **C. 4-finger swipe / pinch arbitration.** `apply_navigate` tests swipe (~12px)
  and pinch (15% spread) independently; a 4-finger swipe splays >15% and fires
  zoom-to-fit instead of the directional swipe. Decision: keep both on 4 fingers
  but make them mutually exclusive — the first committed motion picks swipe *xor*
  pinch and locks out the other for the gesture. (Rejected: moving pinch to 5
  fingers — ergonomically unreliable, a late finger misfires as a 4-finger
  swipe.)
- **D. 3-finger double-tap + drag center flash.** A single 3-finger tap fires
  `CenterWindow` immediately on lift (`detect_tap`), so the double-tap-drag-move
  always flashes a center first. Defer the single-tap action by `DOUBLE_TAP_MS`
  via a calloop timer; cancel it if a second down / drag arrives within the
  window. Most involved (event-loop wiring) — land A/B/C first, then D.
- **App double-tap (Thunar content / Firefox).** Forwarding is clean (real
  libinput timestamps, fresh serials, frame events, surface-local coords). The
  inconsistency *across toolkits* points to toolkit-level touch→double-click
  emulation, not a compositor bug — no change.
- **E. Cluster move — 3-finger double-tap + hold + drag.** Additive gesture (with
  F). `hold` is the touch analogue of the desktop `Shift` cluster modifier:
  `double-tap + drag` moves the window, holding before the drag extends the move to
  its snap-cluster (reuse the pointer move's `cluster_members` path — `new_touch`
  currently omits it, see `move_grab.rs`). Plain 3-finger drag (pan) is untouched,
  so no misfire on the hot path; the new state is quarantined in the already-opt-in
  double-tap branch, which gains a three-way exit (quick lift = fit, drag = move,
  hold-then-drag = move-cluster). Needs D's timer. Full default vocabulary +
  grammar under the bindable `[touch]` follow-up.
- **F. Resize — 3-finger hold + drag, edge by origin.** Touch's absolute position
  picks the edge: 3 fingers land on a window, hold (the dwell is the discriminator
  that keeps plain 3-finger drag as pan), then drag. Map where the fingers landed
  to a `ResizeEdge` via a 3×3 grid over the window (bottom-right region → BR
  corner, right band → right edge, …); a small window where the fingers span the
  frame falls back to the bottom-right corner. The chosen edge is fixed for the
  gesture (opposite corner anchored); the drag drives a `ResizeSurfaceGrab`, which
  needs a `TouchGrab` impl (still pointer-only — mirror the dual-impl
  `MoveSurfaceGrab`). On-window only; empty canvas stays pan. Shares D's hold
  timer, so it lands with E. Detail under the bindable `[touch]` resize section.

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
- **3-finger double-tap + hold + drag** — move-cluster. `hold` is the touch
  `Shift`: the same move gesture, but holding before the drag extends it to the
  window's snap-cluster. A *per-drag* choice (like desktop `Shift`-drag), not a
  global flag — which is why it's a gesture and not a setting. Lands in the
  current batch (fix E).
- **3-finger hold + drag** (no double-tap) — resize-window. Touch is absolute, so
  *where the fingers land* picks the edge (3×3 grid → `ResizeEdge`; small windows
  default to the bottom-right corner) — one gesture, all 8 directions. The plain
  hold keeps it distinct from move/cluster and off the pan path. Lands in the
  current batch (fix F).

### Default 3-finger vocabulary

The 3-finger band carries the window-management verbs. This is the default
binding set (remappable via the grammar below):

| 3-finger | action |
|---|---|
| drag | pan + pinch *(hot path — untouched)* |
| tap | center window |
| double-tap | fit window |
| double-tap + drag | move window |
| double-tap + hold + drag | move cluster |
| hold + drag | resize window (edge chosen by origin) |

### Trigger grammar (what `[touch]` exposes)

Expose the grammar, not a fixed gesture list — same as `[gestures]` / `[mouse]`.
A binding is a point in:

- **fingers**: 1–4 (never 5 — panel max-contacts vary and a late finger misfires
  as a 4-finger gesture).
- **prefix**: none | double-tap (never triple, never *multi-finger* double-tap —
  all-down → all-up → all-down with >1 finger inside ~300 ms misfires).
- **terminal**: tap | drag | hold-then-drag (avoid hold-then-*release* — no
  release-into-action idiom on glass; it occludes the screen).
- **drag sub-type**: free (pan) | pinch | swipe⟨dir⟩.
- **context**: on-window | on-canvas | anywhere (touch's superpower — absolute
  position, no cursor-derived ambiguity).

Everything in the table is a point in this grammar (`double-tap + hold + drag` =
`{fingers: 3, prefix: double, terminal: hold-drag}`). Anything inside fingers ≤ 4
× {none, double} × {tap, drag, hold-drag} is fair game; 5-finger and multi-finger
double-tap are deliberately outside it.

### Window resize on touch — one gesture, edge by origin

The canvas/zoom model demotes resize: viewport pinch-zoom scales the *whole
scene* (it is **not** a per-window magnifier), and `fit-window` (3-finger
double-tap) handles "make this fill the screen." What's left is arbitrary
in-between dimensions, which touch is a poor modality for — so resize gets
exactly one gesture (`3-finger hold + drag`, fix F) and no more.

- The discrete "fill the screen" intent is covered by `fit-window` (3-finger
  double-tap above), not a drag.
- Don't make the 8px resize border touch-draggable (far below a ~40px fingertip;
  widening it conflicts with content drags near window edges) and don't use
  2-finger-on-window (that's the app's own pinch). The `3-finger hold + drag`
  gesture sidesteps both: it's compositor-only (apps never see 3 fingers) and the
  hold keeps it off the pan path.
- The edge is chosen by *where the fingers land* (touch is absolute, unlike the
  trackpad's relative `modifier + swipe` resize): a 3×3 grid over the window maps
  the landing centroid to a `ResizeEdge`; a small window where the fingers span
  the frame falls back to the bottom-right corner. One gesture covers all 8
  directions — no per-edge variants, no visible grip chrome.
- A per-window magnifier (zoom one window without scaling the rest) is rejected —
  it fights the single-camera invariant (everything in canvas space scales with
  zoom together); `fit-window` is the right primitive instead.
- Reuses `ResizeSurfaceGrab`, which needs a `TouchGrab` impl (still pointer-only;
  mirror the dual-impl `MoveSurfaceGrab`). The `resize-window` action already
  exists — this just gives it a touch trigger.

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
