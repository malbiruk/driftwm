# Fullscreen screen-space plan

Make fullscreen a **screen-space, output-affiliated overlay** instead of a camera
trick on the shared canvas. Resolves the multi-monitor bleed (a fullscreen window
visible to other cameras) and #132 (choosing which monitor a fullscreen window
opens on). Also fixes how fullscreen/pinned windows are reported in the
persistence state file.

## Why now

- **Bleed.** Today fullscreen parks the output's camera on the window and maps the
  window on the canvas at that camera origin
  (`state/fullscreen.rs::enter_fullscreen`). Because the fullscreen window stays a
  canvas citizen at a real `(x, y)`, any *other* output whose camera pans over
  that canvas region renders it too — the fullscreen window floats into the second
  monitor's view, parked at output A's camera-origin coords. (The below-window cull
  is per-output, so it is *not* the cause: the other monitor renders its own canvas
  normally; the leak is purely the stray canvas citizen.)
- **No affiliation.** Displays are independent cameras on one shared canvas, with
  no display affiliation. Fullscreen is the one case that genuinely needs
  affiliation ("fill *this* display"), and the camera trick fakes it badly.
- **#132 falls out.** Once fullscreen targets a chosen output, "which monitor"
  becomes a single decision at one entry point instead of camera math.

## Key discovery: pinned-to-screen already solves per-output isolation

The hard part — "screen-space, bound to one output, invisible to every other
camera" — is already built, shipping, and battle-tested as the pinned-to-screen
feature. Fullscreen just isn't using it.

- **Render** (`state/mod.rs` `window_render_transform`, ~1259): a pinned window
  returns its `screen_pos` at zoom 1.0 for its owning output and **`None` for
  every other output**; `compose_frame` (`render/mod.rs`, ~686) then `continue`s.
  Structurally invisible to other cameras.
- **Hit-test**: canvas path skips pinned (`input/mod.rs`, ~795); `pinned_window_under`
  (`input/mod.rs`, ~864) works in screen space on the active output only — and
  already contains `if self.is_output_fullscreen(&output) { return None; }` with
  the comment *"Fullscreen covers pinned windows on that output (like the top
  layer)."* The codebase already knows fullscreen should occlude.

So fullscreen ≈ **a pinned window at `screen_pos = (0,0)`, configured to the
output's logical size**, kept as its own state because the *policies* differ
(occludes layers, captures all input, transient, exit-to-windowed on disconnect).
The window's buffer is output-sized, so rendering it at `(0,0)` zoom 1.0 fills the
output with no special scaling.

## Scope

**In:**
1. Fullscreen → screen-space overlay; delete the camera trick.
2. Cross-output duplicate guard (same window can't be fullscreen on two outputs).
3. #132 output selection: client `wl_output` → window-rule `output` → active output.
4. Persistence honesty for pinned + fullscreen.

**Out (follow-ups, agreed):**
- Canvas-layer widget + edge-layer position enrichment in persistence. Most
  consumers read `windows=`, which stays canvas-only — so deferring this is safe.
- "Re-fullscreen on a survivor" on disconnect — keep exit-to-windowed.

## Design

### 1. Fullscreen as screen-space overlay (`state/fullscreen.rs`, `state/mod.rs`, `render/`)

- **`enter_fullscreen(window, target_output)`**:
  - Resolve `target_output` (see #3); configure the window to that output's
    logical size (`enter_fullscreen_configure`).
  - Record `FullscreenState { window, output, saved_size, saved_pinned, saved_location }`.
    Keep `saved_location` (exact windowed-position restore; cheap, and leaves room
    for cluster/restore logic later).
  - **Do not** touch the output's camera/zoom. Drop `saved_camera`,
    `saved_zoom`, the integer-snap, and the "lock viewport" block.
  - **Do not** `map_element` at the camera origin — the window keeps its canvas
    home (for focus/stacking), rendered via the screen-space branch.
  - **Preserve**: hide Top/Bottom layers, reset `pointer_over_layer`, pointer-focus
    the surface, and the pointer-constraint (game cursor-lock) activation that the
    current `enter_fullscreen` does at the end.
  - **Preserve the same-output idempotent guard** (`state/fullscreen.rs:35-42`):
    re-asserting fullscreen on a window already fullscreen on *this* output must be
    a no-op reconfigure, never recapturing `saved_size` from the current
    (fullscreen) geometry — toolkits re-assert on focus changes. §2's dup-guard
    only handles the *cross-output* case; this same-output guard stays.
- **`window_render_transform`** (`state/mod.rs:1259`): add a fullscreen case
  mirroring the pinned branch — owning output → `screen_pos − geom_loc` at zoom
  1.0 with `screen_pos = (0,0)` (i.e. `(0,0) − geom_loc`, **not** a literal
  `(0,0)`; subtracting `geom_loc` avoids a CSD-shadow-margin offset bug), `None`
  on all others. Note the `None` path also feeds the off-screen screenshot pass
  (`render/mod.rs:272`, `output: None`), so `driftwm msg screenshot` will exclude
  the fullscreen window — **intentional**, and consistent with fullscreen leaving
  `windows=` in persistence.
- **`render/mod.rs`**: **keep** the below-fullscreen cull (`render/mod.rs:611`,
  gated on `is_output_fullscreen(output)` at ~537). It is *not* global — it runs
  inside the per-output compose loop. The fullscreen window still lives in
  `space.elements()` (we keep its canvas home), so on the fullscreen output this
  cull skips every now-fully-occluded below window before the overlay draws on
  top; deleting it would re-render N occluded windows every frame and overdraw
  them. On non-fullscreen outputs `is_output_fullscreen` is false, so their canvas
  renders normally. (`enforce_below_windows` only re-orders z-stacking, it doesn't
  cull.) Cross-check the blur occlusion-cull path, which keys off the same flag.
- **`exit_fullscreen_on`**: reconfigure to `saved_size`, re-pin if `saved_pinned`.
  No camera restore (it never moved).
- **Hit-test** (`input/mod.rs`): fullscreen window hit-tested in screen space on
  its output, above pinned. Likely a `fullscreen_window_under` sibling to
  `pinned_window_under`, or fold both into one screen-space pass.
- **Input/nav gating (correctness requirement, not a nicety)**: pan/zoom on the
  fullscreen output must be *fully inert* — forwarded to the client or dropped.
  This is now **load-bearing**: with `saved_camera` gone and no camera-restore on
  exit, the gate is the *sole* thing keeping the output's camera from drifting
  under the overlay (drift would make exit land on the wrong view). It replaces the
  old viewport-lock block. Generalize the existing momentum-stop into a "this output
  is in fullscreen" gate across the canvas-gesture/pointer paths.

### 2. Cross-output duplicate guard (`state/fullscreen.rs`)

At the top of `enter_fullscreen`, before capturing any saved state:
`if let Some(other) = self.find_fullscreen_output_for_surface(surface)` and
`other != target_output`, call `exit_fullscreen_on(&other)` **first**. Exiting
first restores the window to its real windowed geometry, so `saved_size` is
captured honestly (same hazard the existing same-output idempotent guard dodges).
Today `self.fullscreen` is keyed by output with no such guard, so one window can
end up fullscreen on two outputs.

### 3. #132 output selection (`handlers/xdg_shell.rs`, `config/`)

- **`fullscreen_request(surface, output)`**: resolve target =
  `output` (client `wl_output` → `Output::from_resource`) **else** window-rule
  `output` **else** `active_output()`. **Precedence decision:** rule `output`
  *overrides* the client request when set (explicit per-app user intent); client
  request honored when no rule; active output last. (Per-app rules aren't the
  blanket "all fullscreen → main monitor" footgun raised on the issue.)
- **Deferred path**: `pending_fullscreen` is a `HashSet<WlSurface>` (field at
  `state/mod.rs:474`, used in `handlers/xdg_shell.rs`). Change to a map carrying
  the requested-output choice so a not-yet-sized window honors its target on first
  commit.
- **Window rule field**: add `output: Option<String>` to `WindowRule`,
  `WindowRuleFile`, `AppliedWindowRule` (`config/types.rs`, `config/toml.rs`);
  pass-through in `config/parse_helpers.rs`. Match against `output.name()`
  (`"DP-1"`, etc.) — same lookup as `handlers/mod.rs` (~872).
- **Docs**: add the field to `config.reference.toml` + regen `docs/config.md`
  (`UPDATE_CONFIG_DOCS=1 cargo test docs_config_md_is_up_to_date`); document in
  `docs/window-rules.md`.

### 4. Persistence (`state/persistence.rs`, `ipc/protocol.rs`)

State describes **current presentation**, each entity in its true coordinate
space. Canvas-space → `windows=` (canvas coords); screen-space → grouped per
output, mirroring the existing `outputs.{name}.` namespacing.

- `windows=` stays canvas-only and **stops including fullscreen windows** (they're
  no longer canvas; today they leak in at the bogus camera-origin coord). Existing
  consumers unaffected.
- Add `outputs.{name}.fullscreen={app_id,title}` and
  `outputs.{name}.pinned=[{app_id,title,pos,size}]` (screen/output-relative
  coords). Pinned is currently *excluded entirely* (`persistence.rs`, ~48) — now
  surfaced here. The same scheme reserves `outputs.{name}.layers=[...]` for the
  deferred edge-layer enrichment.
- Reuse the existing dirty-detection + 100ms throttle; extend the change-detection
  set to cover fullscreen/pinned membership.

## Files to touch

- `state/fullscreen.rs` — enter/exit rewrite, dup guard.
- `state/mod.rs` — `window_render_transform` fullscreen case; `FullscreenState` fields.
- `render/mod.rs` — keep per-output below-cull (verify it still skips correctly post-rewrite); verify blur occlusion-cull path.
- `input/mod.rs` — fullscreen screen-space hit-test; input/nav gating on fullscreen output.
- `handlers/xdg_shell.rs` — `fullscreen_request` output resolve; `pending_fullscreen` → map.
- `config/types.rs`, `config/toml.rs`, `config/parse_helpers.rs` — `output` rule field.
- `state/persistence.rs`, `ipc/protocol.rs` — schema.
- `backend/udev.rs` — disconnect already calls `exit_fullscreen_on` (~1351); verify still correct post-rewrite.
- `config.reference.toml`, `docs/window-rules.md` — docs.

## Verification

- `cargo test`, `cargo clippy`, `cargo fmt --check`.
- **New tests**: cross-output dup guard (fullscreen A then B → only B); output
  selection precedence (rule > client > active).
- **Manual (multi-output needs udev/hardware — winit is single-output)**, reproduce
  the original experiments:
  - Fullscreen on A; pan B over A's region → B does **not** see it; A shows only it.
  - Fullscreen same window on B → exits on A.
  - Unplug A while fullscreen → window exits to windowed on a survivor; pinned
    windows reassign (already handled by `reassign_orphaned_pinned`).
  - #132: a client that requests a `wl_output`, and a `output = "DP-1"` rule, each
    land on the intended monitor.
- **Persistence**: `driftwm msg state` / read the state file → fullscreen & pinned
  appear under `outputs.{name}.*`; `windows=` is canvas-only with no camera-origin
  entry.

## Settled decisions

- **Precedence**: window-rule `output` overrides the client `wl_output` request
  when set; client request honored when no rule; active output last. Accepted
  tradeoff: a per-app rule silently overrides a client's explicit
  fullscreen-target request (explicit user config beats app heuristics).
- **Persistence naming**: per-output grouping `outputs.{name}.fullscreen` /
  `.pinned` now, with `.layers` reserved for the follow-up. `windows=` stays
  canvas-only and unchanged for existing consumers.
- **Keep `saved_location`** in `FullscreenState` — low cost, leaves room for
  cluster/restore use later.
