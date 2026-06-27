# Fullscreen multi-monitor isolation plan

Today fullscreen already renders correctly on its *own* output — the defect is
purely that the fullscreen window is a canvas citizen that other monitors'
cameras can see. So make fullscreen **invisible to every camera except its own
output's**, on top of the existing camera-park mechanism — *not* a rewrite to a
screen-space overlay. This resolves the multi-monitor bleed and #132 (choosing
which monitor a fullscreen window opens on), and fixes how fullscreen/pinned
windows are reported in the persistence state file.

**Approach: keep the camera-park, isolate it per-output (≈2 targeted changes),
not the screen-space-overlay rewrite.** See "Why not the overlay rewrite" below.

## How fullscreen works today (and why it already renders right)

`state/fullscreen.rs::enter_fullscreen` parks the output's camera and does three
things that together place the window at screen `(0,0)`:

- sets `os.zoom = 1.0`,
- snaps the camera to integer coords,
- `map_element`s the window **at the camera origin**.

At zoom 1 with the window at the camera origin, `screen = (loc − camera) * zoom =
(0,0)` and **canvas-space coords equal screen-space coords**. That equality is
load-bearing: it is *why* pointer focus, hit-testing, and pointer-constraint
(game cursor-lock) all work today through the ordinary canvas input path — no
screen-space special-casing. The window's buffer is output-sized, so it fills the
output 1:1.

The camera provably never moves during fullscreen: every navigation path exits
fullscreen first — 3+ finger swipe (`swipe.rs:43`), 3+ finger pinch
(`pinch.rs:29`; 2-finger forwards to the app), any non-allowlisted action
(`actions.rs:41`), bound clicks (`pointer.rs:143`; plain clicks forward to the
app). So the window stays pinned to screen `(0,0)` for the whole fullscreen
lifetime.

**The only defect:** because the window keeps a real canvas `(x, y)` (the camera
origin), any *other* output whose camera pans over that region renders it too —
the game floats into the second monitor's view. (The below-window cull is
per-output, so it is *not* the cause; the other monitor just renders its own
canvas, stray citizen included.)

## The fix: per-output isolation

Make the fullscreen window return nothing for every camera but its own. Two
targeted changes; the camera-park, input path, and cursor-lock are untouched.

1. **`window_render_transform`** (`state/mod.rs:1259`): if the window is
   fullscreen on output `O`, return `None` for any render output ≠ `O` (and for
   the `output: None` off-screen screenshot pass). On `O` itself it falls through
   to the existing canvas branch, which already yields `(0,0)` at zoom 1 —
   unchanged. This mirrors the existing pinned branch, which already returns
   `None` for non-owning outputs; `compose_frame` (`render/mod.rs:684`) then
   `continue`s.
2. **Canvas `surface_under`** (`input/mod.rs:791`): skip a window that's
   fullscreen on an output *other than* the active one (cameras can overlap on
   the canvas, so the active output's hit-test could otherwise reach another
   output's fullscreen window). On the fullscreen output itself the existing path
   still hit-tests it, so input keeps reaching the game.

That's the whole rendering/input change. No `fullscreen_window_under`, no
screen-space pointer re-derivation, no overlay branch.

## Why not the overlay rewrite (Option A, rejected)

The tempting "cleaner" design drops the camera-park, renders the window at screen
`(0,0)` over a *frozen, arbitrary* camera/zoom, and treats fullscreen as a
pinned-style screen-space overlay. Rejected:

- It forces pointer focus **and pointer-constraint (cursor-lock) routing** to be
  re-derived in screen space at arbitrary zoom — net-new coordinate surgery on
  the highest-risk, least-CI-testable surface (a real game grabbing the pointer
  on real multi-output hardware). #135 (pointer-constraint re-assertion at a
  monitor boundary) shows this path is *already* fragile; Option A would rewrite
  it wholesale.
- It buys nothing this change needs: outcomes (no bleed, #132, persistence) are
  identical. Its only real benefit — freeing the fullscreen output's camera —
  serves only features that are explicitly out of scope (transparent fullscreen,
  canvas-behind).
- Correct sequencing: when a future feature genuinely requires a non-frozen
  camera under fullscreen, do the overlay rewrite *then*, with that feature as
  the forcing function and test case.

## Invariant: the camera must not move during fullscreen

Now load-bearing for **rendering correctness**, not just UX: the fullscreen
window is pinned to its output's camera-origin canvas coord at zoom 1, so any
camera movement would visibly slide it off `(0,0)`.

- It holds today because every nav path exits fullscreen first (the four sites
  above).
- **Audit the action allowlist** (`actions.rs:41`): the actions that *don't*
  exit fullscreen are `ToggleFullscreen | Spawn | ReloadConfig | SwitchLayout |
  ToggleCursorPan`. Confirm none can move the camera — `ToggleCursorPan`
  (cursor-driven panning) and `ReloadConfig` (may recompute zoom/camera limits)
  are the two to check.
- **Cheap enforcement**: have `set_camera` early-return (or `debug_assert`) when
  the target output is fullscreen, so a future camera-moving path can't silently
  reintroduce the bleed.

## Scope

**In:**
1. Per-output isolation (the two changes above).
2. Cross-output duplicate guard (same window can't be fullscreen on two outputs).
3. #132 output selection: client `wl_output` → window-rule `output` → active output.
4. Persistence honesty for pinned + fullscreen.

**Optional add-on (recommended, see Related issues):**
- #135 pointer-constraint re-assertion on re-entry to a fullscreen output.

**Out (follow-ups, agreed):**
- The overlay rewrite (Option A) and anything needing a non-frozen fullscreen
  camera.
- #133 fullscreen image quality / direct-scanout — separate investigation,
  untouched here.
- Canvas-layer widget + edge-layer position enrichment in persistence. Most
  consumers read `windows=`, which stays canvas-only — so deferring is safe.
- "Re-fullscreen on a survivor" on disconnect — keep exit-to-windowed.

## Design

### 1. Per-output isolation (`state/mod.rs`, `input/mod.rs`)

- `window_render_transform` (`state/mod.rs:1259`): add a fullscreen guard →
  `None` off-output (and for the `output: None` off-screen capture); falls
  through to the existing canvas branch on the owning output.
- Canvas `surface_under` (`input/mod.rs:791`): skip windows fullscreen on a
  different output than the active one.
- `FullscreenState` is **unchanged** — `saved_camera`/`saved_zoom` stay, since
  the park stays and `exit_fullscreen_on` still restores them.
- The per-output below-cull (`render/mod.rs:611`, gated on `is_output_fullscreen`
  at ~537) stays exactly as-is. Cross-check the blur occlusion-cull path keys off
  the same flag.

### 2. Cross-output duplicate guard (`state/fullscreen.rs`)

At the top of `enter_fullscreen`, before capturing any saved state: if
`find_fullscreen_output_for_surface(surface)` returns some `other !=
target_output`, call `exit_fullscreen_on(&other)` **first**. Exiting first
restores the window to its real windowed geometry, so `saved_size` is captured
honestly (same hazard the existing same-output idempotent guard at
`state/fullscreen.rs:35-42` dodges — keep that guard too). Today `self.fullscreen`
is keyed by output with no such guard, so one window can end up fullscreen on two
outputs.

### 3. #132 output selection (`handlers/xdg_shell.rs`, `state/fullscreen.rs`, `config/`)

- **`fullscreen_request(surface, output)`** (`xdg_shell.rs:168`, currently
  ignores `_output`): resolve target = `output` (client `wl_output` →
  `Output::from_resource`) **else** window-rule `output` **else**
  `active_output()`. **Precedence:** rule `output` *overrides* the client request
  when set (explicit per-app intent); client request honored when no rule; active
  output last.
- **Generalize the park to the chosen output.** This is the one place the camera
  change isn't trivial: `enter_fullscreen` currently hardcodes `active_output()`
  / `self.camera()` / `get_viewport_size()`. Take `target_output` and park *that*
  output's camera/zoom/viewport. Mechanical, but it's the sole new camera code.
- **Deferred path**: `pending_fullscreen` (field `state/mod.rs:474`, inserted
  `xdg_shell.rs:178`) is a `HashSet<WlSurface>`. Change to a map carrying the
  requested-output choice so a not-yet-sized window honors its target on first
  commit.
- **Window rule field**: add `output: Option<String>` to `WindowRule`,
  `WindowRuleFile`, `AppliedWindowRule` (`config/types.rs`, `config/toml.rs`);
  pass-through in `config/parse_helpers.rs`. Match against `output.name()`
  (`"DP-1"`, etc.) — same lookup as `handlers/mod.rs:875`.
- **Docs**: add the field to `config.reference.toml` + regen `docs/config.md`
  (`UPDATE_CONFIG_DOCS=1 cargo test docs_config_md_is_up_to_date`); document in
  `docs/window-rules.md`.

### 4. Persistence (`state/persistence.rs`, `ipc/protocol.rs`)

State describes **current presentation**, each entity in its true coordinate
space. Canvas-space → `windows=` (canvas coords); screen-space → grouped per
output, mirroring the existing `outputs.{name}.` namespacing.

- `windows=` stays canvas-only and **stops including fullscreen windows** — an
  explicit `is_output_fullscreen`/membership filter in `window_inventory` (the
  window is still a canvas citizen at the camera-origin coord, so this is a
  *filter*, not a structural consequence). Existing consumers unaffected.
- Add `outputs.{name}.fullscreen={app_id,title}` and
  `outputs.{name}.pinned=[{app_id,title,pos,size}]` (screen/output-relative
  coords). Pinned is currently *excluded entirely* (`persistence.rs:50`) — now
  surfaced here. The same scheme reserves `outputs.{name}.layers=[...]` for the
  deferred edge-layer enrichment.
- Reuse the existing dirty-detection + 100ms throttle; extend the
  change-detection set to cover fullscreen/pinned membership.

## Related issues (#132–135)

| Issue | Fixed by this work? | Notes |
|-------|---------------------|-------|
| **#132** choose fullscreen monitor | **Yes** — scope item 3 | Output selection + window-rule `output` field is the whole fix. |
| **#133** fullscreen image quality / downscaled look | **No** (out of scope) | Root cause is direct-scanout / blit-scale, a separate axis from isolation. Option B leaves the scanout path untouched, so it neither fixes nor regresses #133; investigate `enter_fullscreen_configure` size + scanout eligibility separately. (This is another reason to avoid Option A, which would perturb the fullscreen render element.) |
| **#134** snap match-size | **No / N/A** | Unrelated to fullscreen, and already **CLOSED**. |
| **#135** pointer stuck behind fullscreen after monitor round-trip | **Not as written** — but it's the natural add-on | Distinct fix: on pointer re-entry to a fullscreen output, re-run the focus + constraint assertion `enter_fullscreen` performs. Independent of A/B. **Recommended to bundle** — we're already in the fullscreen multi-monitor input path. See below. |

**#135 add-on (if bundled):** on the pointer-motion path
(`on_pointer_motion_absolute`/`_relative`, `input/mod.rs`), when the pointer
crosses *into* a fullscreen output, re-assert what `enter_fullscreen` does at its
tail — force pointer focus to the fullscreen surface, deactivate any stale
constraint on the previous focus, send `pointer.motion(...)` + `frame`, and call
`maybe_activate_pointer_constraint()`. Honest caveat (from the issue): if the
game's confine is `oneshot` it is destroyed on deactivation and only the *client*
can re-request it — re-assertion fixes the focus/region timing but cannot resurrect
a destroyed oneshot constraint. Worth implementing the architectural re-assertion
either way; verify against CS2.

**Why #135 reinforces Option B:** the pointer-constraint path across outputs is
already brittle (that's #135). Option A would *rewrite* that exact path in screen
space at arbitrary zoom; Option B leaves it intact, so #135 can be fixed
surgically and in isolation rather than entangled with a rendering rewrite.

## Files to touch

- `state/mod.rs` — `window_render_transform` fullscreen guard.
- `input/mod.rs` — canvas `surface_under` skip for off-output fullscreen windows; (optional) #135 re-assertion on the motion path.
- `state/fullscreen.rs` — dup guard; `enter_fullscreen` takes `target_output` (keep the same-output idempotent guard).
- `handlers/xdg_shell.rs` — `fullscreen_request` output resolve; `pending_fullscreen` → map.
- `config/types.rs`, `config/toml.rs`, `config/parse_helpers.rs` — `output` rule field.
- `state/persistence.rs`, `ipc/protocol.rs` — schema + fullscreen filter.
- `state/mod.rs` (wherever `set_camera` lives) — optional fullscreen guard enforcing the invariant.
- `backend/udev.rs` — disconnect already calls `exit_fullscreen_on` (`udev.rs:1351`); unchanged, but verify it still restores correctly.
- `config.reference.toml`, `docs/window-rules.md` — docs.

`render/mod.rs` is **not** touched — the per-output below-cull stays as-is.

## Verification

- `cargo test`, `cargo clippy`, `cargo fmt --check`.
- **New tests**: cross-output dup guard (fullscreen A then B → only B); output
  selection precedence (rule > client > active).
- **Manual (multi-output needs udev/hardware — winit is single-output)**:
  - Fullscreen on A; pan B over A's region → B does **not** see it; A shows only it.
  - Fullscreen same window on B → exits on A.
  - Unplug A while fullscreen → window exits to windowed on a survivor; pinned
    windows reassign (already handled by `reassign_orphaned_pinned`).
  - #132: a client that requests a `wl_output`, and an `output = "DP-1"` rule,
    each land on the intended monitor.
  - **Cursor-lock sanity**: a fullscreen game still grabs/locks the pointer. The
    path is unchanged, but it's exactly what this approach protects — confirm it.
  - **#135 (if bundled)**: fullscreen game on A with a confined cursor (menu) →
    move to B → return to A → game recaptures the pointer.
- **Persistence**: `driftwm msg state` / read the state file → fullscreen & pinned
  appear under `outputs.{name}.*`; `windows=` is canvas-only with no camera-origin
  entry.

## Settled decisions

- **Keep the camera-park; isolate per-output.** Rejected the screen-space-overlay
  rewrite (Option A): identical outcomes, but it would re-derive
  cursor-lock/pointer routing at arbitrary zoom (high-risk, hard to test — see
  #135) for a camera-freedom benefit that's out of scope. Revisit only when a
  feature forces a non-frozen fullscreen camera.
- **Precedence**: window-rule `output` overrides the client `wl_output` request
  when set; client request honored when no rule; active output last. Accepted
  tradeoff: a per-app rule silently overrides a client's explicit
  fullscreen-target request (explicit user config beats app heuristics).
- **Persistence naming**: per-output grouping `outputs.{name}.fullscreen` /
  `.pinned` now, with `.layers` reserved for the follow-up. `windows=` stays
  canvas-only and unchanged for existing consumers.
- **Camera-frozen invariant** is now a rendering-correctness requirement,
  enforced via the `set_camera` guard + the action-allowlist audit.
