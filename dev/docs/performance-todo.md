# Performance — remaining work

The B1–B14 perf push shipped (see `git log`). What's left, in priority order.
Line numbers predate the push — re-verify on pickup. Profiling tooling:
[profiling.md](profiling.md).

## Blur (B5 + S1 + edge-fade + fullscreen cull)

The only substantive perf work left; deferred behind touchscreen + session
restoration (GH #125). B5 + S1 + edge-fade are one FBO/crop/mask rework; the
fullscreen cull is a separable window-loop early-skip in the same code. So the
blur wins span three contexts: during a pan, on zoom (S1), and under fullscreen.

**B5 — multi-output churn + FBO retention.**

- `src/render/mod.rs` — `blur_cache` is global but `compose_frame` retains per
  output: two outputs showing different blurred windows evict each other every
  frame → `BlurCache::new` re-allocates 3 window-sized textures + full recompute
  per blurred window per frame (~25 MB/frame at 1080p). Fix: retain against the
  union of blur requests across outputs.
- `src/render/blur.rs` — `blur_bg_fbo` is one slot keyed by size; different-sized
  outputs evict each other per frame (~33 MB alloc/free at 4K). Fix: key per
  output name, free in `remove_output`. Also drop the slot when no blur requests
  remain (currently retained forever after the last blurred window closes).

**S1 — blur fully recomputes every frame of a pan _or zoom_.** The cache hash
includes the window's screen-space position (`src/render/blur.rs` hashes
`window_rect.loc`), so any camera motion marks every blurred window dirty every
frame: full-output offscreen FBO repaint, crop, 2×radius Kawase passes, a second
full render for the alpha mask, masking pass. Screen-fixed blur on other monitors
also recomputes (`blur_camera_generation` is a global counter, `src/state/mod.rs`).
Fix options: translate the cached blur texture by the camera delta during
camera-only motion (blur is low-frequency); recompute at half rate while panning;
or key on (quantized position, behind-element commits).

**Edge-fade artifact.** Behind-content is cropped to exactly `win_size`, so the
Kawase kernel clamps at window edges and the blur tapers inward. Fix: blur a
radius-padded region and crop back — same surface as B5/S1. Cost caveat is at the
`blur` field in `config.reference.toml` (rendered in `docs/config.md#window-rules`).

**Fullscreen occlusion-cull.** `compose_frame` does not short-circuit for a
fullscreen output — it runs the full window loop (`src/render/mod.rs:585`, only
viewport-visibility culled at `:650`) and `process_blur_requests` (`:1180`); only
the background (`:1088`) and Top/Bottom layer-shell (`:1123-1132`) are skipped. So
a blurred window behind a fullscreen opaque window still pays its Kawase passes
every frame, _before_ smithay's render-time occlusion cull + direct scanout
(`src/backend/udev.rs:1507`) drop the composited result — wasted GPU that competes
with the fullscreen client, and compounds under screen-share (the capture path
re-composites the whole scene and defeats direct scanout). Only bites when a
blurred window is actually behind fullscreen (empty `blur_requests` → already
fast-skipped). Fix: skip blur for normal windows occluded by the fullscreen
window — but preserve pinned-to-screen windows (they render above fullscreen), the
fullscreen window's own popups, and the Overlay layer, and guard a transparent
fullscreen window.

## Lower-priority backlog (do only if a profile flags it)

- **B7** Gigapixel-TIFF decoder pool: no cancellation of stale in-flight decodes;
  blobs upload regardless of visibility and back up during fast pans
  (`src/render/tile_worker.rs`, `tile_chunks.rs`). Cancel unwanted requests; drop
  off-viewport responses; bound the queue. _Gigapixel-TIFF-wallpaper path only._
- **B11** Momentum auto-launch timer removed + re-inserted per gesture event
  (`src/state/animation.rs`, ~140-1000 Hz during pans). Keep one timer, reschedule.
- **B12** Output-outline strips rebuild pixel Vecs + `MemoryRenderBuffer` + fresh
  element ids per edge per frame (`src/render/mod.rs`), defeating damage tracking.
  _Multi-monitor only._ Cache per (output, color, size).
- **B13 / B15** Held repeatable key (`src/backend/udev.rs`) and the exec loading
  cursor (`src/input/actions.rs`, up to 5 s/launch) mark _all_ outputs dirty at
  refresh rate. Mark only the active/cursor output. _Single-output-marginal — same
  shape as the skipped B1; likely not worth it._
- **B14 (remaining half)** Pointer motion does up to ~6 sequential linear window
  scans with repeated `with_states` locks per event (`src/input/mod.rs`). Moderate;
  only scales with window count. (The `min_zoom`-per-pinch half shipped.)
- **Latent frame spikes** (config-dependent): synchronous shader-chunk bakes
  mid-frame (`src/render/shader_chunks.rs` — pre-bake a margin ring, pool the FBO);
  gigapixel-TIFF tile uploads up to ~25 ms/frame on the render thread
  (`src/render/mod.rs` — time-budget, or upload after `queue_frame`); shadow shader
  evaluates ERF quadrature over the full window+pad quad (`src/shaders/shadow.glsl`
  — early-out interior fragments).
- **Redundant EmptyFrame composites in non-integer refresh:content beats.**
  `post_render` runs after every `render_frame`, including the `EmptyFrame`
  branch (`src/backend/udev.rs:1604`), and the VBlank handler re-renders directly
  (`:651-653`), bypassing the `render_if_needed` gate (`:305-308`). At ratios like
  144Hz/60fps video a second client commit can land mid-cycle and force a full
  `compose_frame` that smithay then drops as `EmptyFrame` — GPU compositing with no
  page flip. Bounded by the estimated-vblank timer (can't spin) and only during
  active rendering, not idle. niri avoids it via `RedrawState` (one render/cycle;
  callbacks sent at defined sequence boundaries, never from an empty-render branch
  — `niri/src/niri.rs:492-504`). Fix: skip the `compose_frame`/callback-send on the
  `EmptyFrame` path, and/or route the VBlank-handler render through the gate.
  Surfaced during the #157 frame-callback dedup-guard removal.
- **niri patterns** not yet adopted: animations sampled at predicted
  presentation time (`niri/src/niri.rs:4601-4604` — small judder source vs
  driftwm's `Instant::now()`); on-demand VRR by window visibility
  (`niri/src/niri.rs:4720-4749` — gaming pass).
