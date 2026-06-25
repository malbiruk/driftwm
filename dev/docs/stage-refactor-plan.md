# Stage refactor plan

Extract a smithay-free `Stage` as the source of truth for window state, z-order, and focus. `smithay::desktop::Space` becomes a render mirror, resynced at end-of-tick. Goal: testable window/focus/lifecycle logic via unit tests and proptest.

## Why now

- Focus, navigation, fullscreen, fit, and destroy-followup logic are policy-heavy and currently untestable end-to-end.
- The architectural conditions are favorable (see sizing below), and the refactor produces its own safety net (proptest harness) — so the cold-refactor risk is bounded.

## Three rules for the refactor

1. **Behavior-preserving.** No semantics changes mixed in. Any observable difference is a bug. Spotted a quirk? Note it, fix in a follow-up.
2. **Done = matches this doc, nothing else.** This doc is the success criterion. No "while I'm here."
3. **Build the harness in the same stretch.** It's the proof. Randomized op-sequence harness running green = extraction preserved behavior.

---

## Sizing (the data behind the decision)

### Current state of `Space` coupling

- **Field:** `DriftWm::space: Space<Window>` at `src/state/mod.rs:324`.
- **Canonical window store:** `Space::elements()` only. No parallel `HashMap<WlSurface, Window>`. ← _structural luck #1_
- **Destroy paths:** unified in `XdgShellHandler::toplevel_destroyed` (`src/handlers/xdg_shell.rs:188`) for both normal close and client crash. ← _structural luck #2_
- **`WindowExt` trait already exists** at `src/window_ext.rs` (181 LOC, methods: `send_close`, `app_id_or_class`, `window_title`, `wants_ssd`, `parent_surface`, `is_modal`, `is_widget`). Half the `StageElement` seam is already designed. ← _structural luck #3_
- **One real parallel store:** `decorations: HashMap<ObjectId, _>` at `src/state/mod.rs:346`. Holds per-window SSD state (hover, dirty flags, cached shaders). Must stay in sync with the Stage window set, but its contents are render-side and do NOT move into `Stage`. Cleanup is driven by Stage remove events, same pattern as `state/render_cache.rs` HashMaps (keyed by `ObjectId` and already cleaned by `CompositorHandler::destroyed`).

### Callsite buckets (115 total)

| Bucket                                   | Count                      | Action                                                            |
| ---------------------------------------- | -------------------------- | ----------------------------------------------------------------- |
| **mutate-model** (map/raise/unmap/focus) | 25 (+ ~4 decorations-sync) | move to `Stage`; emit add/remove events for `decorations` HashMap |
| **read-model** (elements/location/under) | 68                         | convert opportunistically; most read from `Stage` eventually      |
| **render-only** (outputs/refresh)        | 21                         | stay on `Space`                                                   |
| **smithay-internal**                     | 1                          | stay                                                              |

Representative mutate-model sites: `handlers/xdg_shell.rs:70` (new toplevel map), `handlers/xdg_shell.rs:317` (destroy unmap), `state/fit.rs:162` (toggle_fit map), `state/mod.rs:663` (`raise_and_focus`).

### Scattered focus/lifecycle state (the prize surface)

| Piece                                          | Location                                                           |
| ---------------------------------------------- | ------------------------------------------------------------------ |
| `focus_history: Vec<Window>`                   | `src/state/mod.rs:455`                                             |
| `cycle_state: Option<usize>`                   | `src/state/mod.rs:456`                                             |
| `fullscreen: HashMap<Output, FullscreenState>` | `src/state/mod.rs:485`                                             |
| keyboard `set_focus()` calls                   | `handlers/xdg_shell.rs:74,303`; `handlers/mod.rs`                  |
| pointer focus                                  | `input/pointer.rs` (~10 sites)                                     |
| fit state                                      | `state/fit.rs` (no central field — lives in `wl_surface` userdata) |

### Mutation entry points (~8 functions, ~439 lines)

| Function                                 | File:Line                     | Lines |
| ---------------------------------------- | ----------------------------- | ----- |
| `new_toplevel`                           | `handlers/xdg_shell.rs:34`    | 46    |
| `toplevel_destroyed`                     | `handlers/xdg_shell.rs:188`   | 131   |
| `raise_and_focus`                        | `state/mod.rs:663`            | 22    |
| `navigate_to_window`                     | `state/navigation.rs:22`      | 39    |
| `enter_fullscreen`                       | `state/fullscreen.rs:12`      | 59    |
| `exit_fullscreen` / `exit_fullscreen_on` | `state/fullscreen.rs:114,120` | 46    |
| `toggle_fit_window`                      | `state/fit.rs:222`            | 88    |
| `decoration_toggle_fit`                  | `state/fit.rs:344`            | 8     |

Plus two grab-driven cluster mutations that reposition windows and must route through `Stage` — otherwise cluster move/resize stays `Space`-coupled and untestable:

- `MoveSurfaceGrab::update` (`grabs/move_grab.rs`) — cluster moves via `space.map_element`.
- `ResizeSurfaceGrab::motion` (`grabs/resize_grab.rs`) → `ClusterResizeSnapshot::apply_member_shifts` (`state/cluster_snapshot.rs:79`) — repositions cluster members on resize (the snap-reflow path; `resolve_cluster_shifts` itself is already pure + tested, but `apply_member_shifts` writes to `Space`).

---

## Niri reference (the pattern we're copying)

- **`LayoutElement` trait** at `/tmp/niri/src/layout/mod.rs:131–333`. ~50 methods. Pure-data (`id`, `size`, `buf_loc`) vs. delegate-to-window (`request_size`, `send_pending_configure`, `set_activated`). This is the seam.
- **`Layout<W>` struct** at `/tmp/niri/src/layout/mod.rs:336–369`. Owns indices and geometry only — _never_ mutates window protocol state. Sends _requests_ via the trait; the window decides if/when to respond.
- **In `niri::State`:** `pub layout: Layout<Mapped>` (`src/niri.rs:225`). `Space` still exists but only for _output positioning in global coords_ — not window data.
- **`TestWindow` mock** at `/tmp/niri/src/layout/tests.rs:25–285`. ~134 LOC. All `Cell`-based stubs; `request_size` writes to a cell, `send_pending_configure` is a no-op.
- **Proptest harness** at `/tmp/niri/src/layout/tests.rs:3907+`. ~60-variant `Op` enum, loop `for op in ops { op.apply(layout); layout.verify_invariants(); }`. Invariants assert focus consistency, position bounds, z-order, etc.
- **Focus-after-close logic** at `/tmp/niri/src/layout/scrolling.rs:1052–1134` — pure index math, no smithay calls. Layout only computes; the compositor sends `set_activated` afterward.

**The one architectural decision to copy:** Stage does not _own_ protocol state (subsurfaces, damage, buffers). It owns indices, geometry, focus. The trait forces the window to _answer queries_ and _receive requests_. This inversion is what makes proptest viable.

---

## Scope boundary (load-bearing — paste into reviews)

| Stage owns                    | DriftWm keeps                                         |
| ----------------------------- | ----------------------------------------------------- |
| window list, z-order          | outputs, layer surfaces (regular + canvas-positioned) |
| focus stack, MRU, cycle state | keyboard/pointer seat                                 |
| per-window canvas (x,y,w,h)   | camera (cx, cy, zoom)                                 |
| fullscreen state              | momentum, edge-pan, animations                        |
| fit / pre-fit-restore state   | `decorations` HashMap, render caches, shaders         |
| add/remove events             | XWayland, foreign-toplevel                            |

**Snap-cluster note:** `layout/cluster.rs` is already a derived view computed on-demand from window positions. Leave the cluster _computation_ in place; switch its reads from `Space` to `Stage`. The derived view is not a state move — but cluster _writes_ (drag-move and resize-reflow, which reposition members) are position mutations and route through `Stage` like any other. See the grab-driven mutations note in the entry-points section.

**Camera/animation note (why this differs from niri):** niri's `Layout` owns `Clock` because animations are _part of layout_ (column slide). Driftwm's animations are _viewport_ (camera lerp, momentum, edge-pan) — they belong in `DriftWm`, not `Stage`. Drawing this line wrong is how this becomes a 4-week refactor.

**Decorations note:** the `decorations: HashMap` at `state/mod.rs:346` does NOT move into `Stage`. Its contents (cached shaders, hover state, dirty flags) are pure render-side. `Stage` exposes an `add`/`remove` event stream; `DriftWm` plumbs that into the HashMap to add/drop entries. Same pattern as the surface-keyed caches in `state/render_cache.rs`, which are already cleaned by `CompositorHandler::destroyed` and need no change.

**Layer-surface note:** regular layer surfaces (`layer_map_for_output`) are an independent smithay subsystem — never touched `Space`, no change. Canvas-positioned widgets (`CanvasLayerSurface` at `state/mod.rs:86`) have a canvas (x,y) but no focus and no z-order with windows; **keep them as a separate collection on `DriftWm`**, do not force them through `StageElement`. Forcing one trait to fit both is the over-abstraction trap.

---

## `StageElement` trait — minimum viable surface

**Extend `WindowExt`, do not duplicate it.** Half the seam already exists at `src/window_ext.rs`: `send_close`, `app_id_or_class`, `window_title`, `wants_ssd`, `parent_surface`, `is_modal`, `is_widget`. The new trait adds only what `WindowExt` doesn't cover — start with ~5 methods. Add only when a real callsite demands one. Niri's 50-method `LayoutElement` is the ceiling, not the floor.

```
trait StageElement: WindowExt {
    id()               -> ElementId          // pure data (stable handle)
    size()             -> Size               // pure data
    position()         -> CanvasPos          // pure data
    set_position(pos)                        // pure data
    set_activated(b)                         // delegate (smithay: window.set_activated)
    request_size(sz)                         // delegate
    send_configure()                         // delegate
    is_fullscreen()    -> bool               // pure data
}
```

Operations already on `WindowExt` (`send_close`, `is_modal`, `parent_surface`, etc.) are reused as-is for destroy-followup logic, modal-dialog focus rules, and SSD decisions.

Two impls:

- `impl StageElement for smithay::desktop::Window` — real, delegates.
- `struct TestWindow` with `Cell`-based fields — mock for unit/proptest. Target: <100 LOC. Must also implement `WindowExt` (trivially, with stub values).

---

## Sequencing (each step independently revertable)

1. **This doc, reviewed and approved.** Trait shape, boundary, op list, invariant list all settled before any Rust.
2. **Extract `Stage` + `StageElement` + `TestWindow` in one PR, unwired.** Just data structure, trait, mock, and unit tests for obvious invariants (add → focus, remove → focus follows, raise → z-order). Compiles, tests pass, nothing calls it. ~1 day.
3. **Route the 8 mutation entry points through `Stage`, one at a time.** After each, resync `Space` from `Stage` at end-of-tick. Run the compositor between each. Eyeball: open/close windows, alt-tab, fullscreen, fit. Read-model callsites mostly stay on `Space` for this phase — convert opportunistically only when adjacent to a mutation already being touched. Do _not_ sweep.
4. **Build the proptest harness** with `Op` variants covering the 8 entry points + a destroy variant + grab-driven `MoveCluster` / `ResizeCluster` variants (the reflow path — see the cluster-mutation note above). Call `Stage::verify_invariants()` after each op. If it finds something: fix the `Stage` bug, never lower the invariant.
5. **After proptest green:** convert remaining read-model callsites from `Space` to `Stage`; demote `Space` to render mirror only. Separate PR.

---

## Invariants (`Stage::verify_invariants`)

The list to assert after every op. Start with these; add as bugs surface.

- Every window in `focus_history` exists in the window list.
- `cycle_state` index, if `Some`, is `< focus_history.len()`.
- At most one fullscreen window per output (`fullscreen` HashMap keys are unique outputs).
- A fullscreen window's saved pre-fullscreen geometry is non-zero.
- A fit window's saved pre-fit size is non-zero.
- Z-order has no duplicates and contains exactly the window-list set.
- Every child window is stacked above its own parent, and a child is never above a window unrelated to its parent chain (guards the #153 modal-dialog regression).
- The focused window (if any) is in the window list.
- A closed window appears nowhere: not in `focus_history`, `cycle_state` target, `fullscreen`, fit state, or z-order.
- For every window in `Stage`, there is exactly one entry in the `decorations` HashMap (key parity, checked at end-of-tick).
- After a cluster move/resize, snapped members stay non-overlapping and preserve `snap.gap` along the shared edge (the reflow invariant — exercises `apply_member_shifts` cascade convergence and the constraint-clamp seam).

---

## Risks (planned-for, not surprises)

1. **Scope creep on focus consolidation.** The scattered focus state _is_ the prize, but moving it byte-for-byte (not fixing quirks in the same pass) is what keeps this behavior-preserving. Any quirk-fix is a follow-up PR with its own test.
2. **`StageElement`-trait bloat.** Hard cap: only add a trait method when a _real callsite_ demands it. Treat niri's `LayoutElement` as a ceiling, not a floor.
3. **Fit state moving off `wl_surface` userdata.** Highest-leverage regression site. Must have explicit tests for pre-fit-restore.
4. **Camera vs. layout boundary.** If `Stage` starts to grow camera/animation fields, stop and re-read the boundary table.

---

## Out of scope (note here, do not do)

- Replacing `Space` outright. It stays as render mirror.
- Refactoring decoration **rendering**, shaders, blur pipeline. (Wiring `decorations` HashMap to Stage events IS in scope; touching the renderer is not.)
- Restructuring `state/render_cache.rs`. Per-surface caches are already cleaned by `CompositorHandler::destroyed`; no Stage changes needed.
- Touching XWayland, foreign-toplevel, screencopy.
- Forcing `CanvasLayerSurface` through `StageElement`. Canvas-positioned layer surfaces stay as a separate `Vec` on `DriftWm`.
- Any "while I'm here" cleanups in handlers/, input/, render/.
- Multi-monitor canvas state restructuring (today: one canvas, many viewports — stays).

---

## Follow-up this unlocks: id-based IPC (post-refactor, separate PR)

`Stage`'s `ElementId` (`StageElement::id`) is the stable per-window handle external tools currently lack — a minimap can't target one of three same-`app_id` windows (#168). Once `Stage` lands, a small separate PR surfaces it:

- Add `id` to `WindowInfo` → flows to both `msg state` and the state file for free (shared struct).
- Add id-targeted IPC variants: `focus <id>`, `move <id>`, `close <id>`. **Additive** — keep the existing `focus <app_id>` (substring) and focused-window `move`/`close` forms; they're the right ergonomics for keybinds and "focus my browser"-style scripts, where an id is the wrong handle. CLI disambiguates id vs app_id with a flag (`focus --id 7`); a bare integer is ambiguous.
- Extend the `focus` no-arg query (`Response::Focused`) to also report the focused window's id — no separate "get focused id" command needed (consumers can also read `state.windows[0]`, which is focused-first).
