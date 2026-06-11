# Performance & battery audit

Audited 2026-06-11 at rev `b1d07dc`; pan-stutter investigation concluded the
same week across five Tracy captures (`~/driftwm_traces/client_pan_lag*.tracy`).
Method: code reading, niri cross-reference, Tracy traces — see
[PROFILING.md](PROFILING.md) for tooling, including the `frame.commits` /
`frame.camera_x/y` / `frame.visible_windows` plots and `tracy_analyze.py
--gpu` added during this investigation.

## Status & roadmap

- ✅ **S5 done** — viewport pans no longer spam pointer motion to the hover
  client (commit `8687627`; resync coalesced to one motion per frame).
- ✅ **Pan stutter root-caused** — AGX firmware GPU power states, not
  compositor work. Not fixable in driftwm today; see Part 2.
- → **Next: B1–B4** (input dirty-marking, canvas-layer widgets, winit loop,
  animated-bg occlusion).
- → **Then: blur optimizations** (B5 multi-output churn + S1 pan-time
  recompute + FBO retention).
- Backlog: B6–B15, latent frame-spike items, niri pattern adoptions (Part 3).

## Part 1 — battery / resource leaks

### Next up

**B1. Every input event marks ALL outputs dirty.**
`src/input/mod.rs:109` — first line of `process_input_event` is
`self.mark_all_dirty()`, unconditionally, for every event type. Each dirty
output runs the full `compose_frame` element build before damage detection;
unchanged outputs end in `EmptyFrame` plus an estimated-vblank timer cycle.
Typing or moving the mouse composes **every** output at refresh rate; scales
with output count on the real udev path. The precise alternative exists:
`mark_dirty_for_surface` (`src/state/mod.rs:793`). Fix: pointer motion →
mark only the output under the cursor; keyboard → mark nothing eagerly (the
client commit triggers the precise path); keep `mark_all_dirty` only for
events that genuinely change global state.

**B2. Canvas-layer widgets bypass all three visibility filters.**
Windows are culled correctly everywhere; canvas-positioned layer-shell
widgets miss every filter: (1) frame callbacks sent unconditionally
(`src/render/lifecycle.rs:248-255`, no `visible_rect` test — contrast windows
at `:215-224`); (2) commits fall through `mark_dirty_for_surface` to the
catch-all `mark_all_dirty()` (`src/state/mod.rs:862` — canvas layers never
enter the `LayerMap`, `src/handlers/layer_shell.rs:102-108`); (3) no viewport
cull in `build_canvas_layer_elements` (`src/render/layers.rs:190-237`). An
animated widget parked off-viewport renders at full FPS forever and
re-composes every output at its commit rate. Fix: apply the same
`visible_rect.overlaps(bbox)` test in all three places.

**B3. Winit backend: free-running 60 fps loop, unconditional swap.**
`src/backend/winit.rs:127` + `:299` — 16 ms timer re-armed unconditionally;
every tick runs the full pipeline even with zero clients and zero damage.
Smithay skips the GPU draw on empty damage but the result is discarded and
`backend.submit(None)` (`winit.rs:266`) full-frame-swaps anyway, forcing the
host compositor to recomposite at 60 fps forever. Dev-only backend, but the
biggest wakeup source nested. Fix (cheap): skip `submit` when the damage
tracker returns `None`, pass damage rects otherwise — niri's winit backend
does exactly this (`/tmp/niri/src/backend/winit.rs:257-283`). Fix (full):
port the udev `redraws_needed` gating. Note: the timer also re-arms *after*
the tick's work (real period ≈ 17-20 ms beating against host vsync), so a
fixed-cadence deadline or host-frame-callback-driven rendering would also fix
nested stutter.

**B4. Animated shader background dirties fully-covered outputs.**
`src/backend/udev.rs:246-253` — `background_is_animated` triggers
`mark_all_dirty()` every cycle regardless of visibility. Fullscreen outputs
skip the background entirely (`src/render/mod.rs:537-541`) yet still get
composed at refresh rate forever (EmptyFrame → estimated-vblank → re-mark →
repeat). Worst case is the battery-sensitive one: fullscreen video with an
animated bg configured. Fix: skip outputs in `state.fullscreen` when marking
(one-line filter); ideally also outputs fully covered by opaque windows.

### Then: blur optimizations

**B5. Multi-output blur: cross-eviction thrash + single-slot bg FBO +
retention.**
- `src/render/mod.rs:1187-1196` — `blur_cache` is global but `compose_frame`
  retains per output: two outputs showing different blurred windows evict
  each other every frame → `BlurCache::new` re-allocates 3 window-sized
  textures + full recompute per blurred window per frame (~25 MB/frame churn
  at 1080p). Fix: retain against the union of blur requests across outputs.
- `src/render/blur.rs:298-308`, `:623` — `blur_bg_fbo` is one slot keyed by
  size; different-sized outputs evict each other per frame (~33 MB alloc/free
  at 4K). Fix: key per output name, free in `remove_output`.
- The same slot is retained forever after the last blurred window closes.
  Fix: drop it when no blur requests remain.

**S1. Blur fully recomputes every frame of a pan.**
The cache-validity hash includes the window's screen-space position
(`src/render/blur.rs:29-30` hashes `window_rect.loc`), so during a pan every
blurred window is dirty every frame: render all elements behind the window
into a full-output offscreen FBO (age 0 → full repaint), crop, 2×radius
Kawase passes, second full render for the alpha mask, masking pass
(`blur.rs:359-572`). N blurred windows at distinct depths → behind-content
rendered once per depth. Screen-fixed blur on other monitors also recomputes
because `blur_camera_generation` is a global counter
(`src/state/mod.rs:1240-1242`). Fix options: translate the cached blur
texture by the camera delta during camera-only motion (blur is low-frequency
content), or recompute at half rate while panning, or key on (quantized
position, behind-element commits).

### Backlog (low severity)

- **B6** `src/state/persistence.rs:94` — `write_state_file_if_dirty` builds
  the full `window_inventory()` (Vec + string clones under `with_states`
  locks) *before* the dirty/throttle checks; runs per output per rendered
  frame. Check the cheap epsilons and the 100 ms throttle first.
- **B7** Tile decoder pool: no cancellation of stale in-flight decodes;
  decoded blobs upload regardless of visibility and back up in the channel
  during fast pans (`src/render/tile_worker.rs:43-51`,
  `tile_chunks.rs:340-400`). Skip requests no longer wanted; drop off-viewport
  responses; bound the queue.
- **B8** Screencopy/capture offscreen textures (~33 MB at 4K, up to 2/output)
  retained after the capture client disconnects (`src/render/capture.rs:11-28`);
  freed only on output disconnect. Drop when the protocol queue empties.
- **B9** `pending_screencopies`/`pending_captures` stranded on output
  removal/DPMS-off (`src/state/mod.rs:430,438`) — buffer fds + dead `Output`
  retained, client never gets `failed()`. `retain()` on output removal.
  Related latency bug: `ScreencopyHandler::frame` queues without marking the
  output dirty (`src/handlers/mod.rs:682-685`) — an idle-system `grim` capture
  stalls until unrelated damage. Needs a dirty-mark kick.
- **B10** Config watcher polls mtime every 500 ms forever (`src/main.rs:227-256`)
  — the only periodic wakeup at deep idle. inotify on the config dir, or
  stretch the interval.
- **B11** Momentum auto-launch timer removed + re-inserted per gesture event
  (`src/state/animation.rs:175-190`, ~140-1000 Hz during pans). Keep one
  timer and reschedule.
- **B12** Output-outline strips rebuild pixel Vecs + `MemoryRenderBuffer` +
  fresh element ids per edge per frame (`src/render/mod.rs:1287-1318`),
  defeating damage tracking (multi-monitor only). Cache per (output, color,
  size).
- **B13** Held repeatable key renders all outputs at refresh rate
  (`src/backend/udev.rs:246-253`). Mark only the active output.
- **B14** Pointer motion: up to ~6 sequential linear window scans with
  repeated `with_states` locks per event (`src/input/mod.rs:193-242`,
  `:741-812`); `min_zoom()` (full window scan) recomputed per pinch-update
  event (`src/input/gestures/pinch.rs:106`) — compute at gesture begin.
- **B15** Exec loading cursor marks all outputs dirty at refresh rate for up
  to 5 s per launch (`src/input/actions.rs:49-51`). Mark only the cursor's
  output.
- **Latent frame spikes** (config-dependent, not daily-driver issues):
  synchronous shader-chunk bakes mid-frame at chunk boundaries
  (`src/render/shader_chunks.rs:161-247`, 4-16 MB alloc + full shader render,
  hitch every `1024 × zoom` px into unbaked territory — pre-bake a margin
  ring, pool the FBO); gigapixel-TIFF tile uploads up to ~25 ms/frame on the
  render thread (`src/render/mod.rs:1061-1067` — time-budget instead of
  count-budget, or upload after `queue_frame`). The shadow shader evaluates
  its ERF quadrature over the full window+pad quad with the interior masked
  only on the last line (`src/shaders/shadow.glsl`) — latent cost for
  shadow-enabled configs; early-out interior fragments.

### Bounded-by-design (informational)

- N monitors → N chunk caches each entitled to full `cache_budget_mb`, plus
  an uncounted fallback texture (≤67 MB) per output — bounded, but the knob
  under-reports the multi-monitor ceiling.
- Fullscreen enter drops chunk caches (intended); exit re-decodes the
  fallback LOD and respawns the worker pool — repeated toggling costs
  multi-hundred-ms decode bursts.

### Verified correct — do not re-fix

- **udev idle**: `render_if_needed` fast-path bails when nothing is pending
  (`udev.rs:164-172`); the VBlank handler doesn't chain unconditionally
  (`udev.rs:626-628`); fully idle = zero renders, loop asleep in epoll.
- **Empty-damage skip**: `EmptyFrame` → no flip, estimated-vblank timer paces
  re-renders (`udev.rs:1493-1498`, `:1597-1626`) — same trick as niri.
- **Frame callbacks**: viewport-culled (off-screen clients throttle to 0 FPS,
  `lifecycle.rs:206-224`) + per-vsync-sequence throttle defeats
  vsync-ignoring clients (`lifecycle.rs:25-66`); pinned windows re-anchored
  to stay inside the filter.
- **VT switch / DPMS / session lock** all stop rendering and callbacks.
- **Animations terminate**: momentum hard-stops at 15 px/s (`canvas.rs:349`);
  camera/zoom lerps snap-and-clear at epsilon.
- **Per-output precise dirty marking** for surface commits
  (`state/mod.rs:793-863`); commits between vblanks coalesce.
- **Decoder pool** parks on a condvar; **font warmup** is one-shot;
  **persistence write** debounced + atomic (modulo B6); **SSD title bars /
  error bar / shadow / border** caches change-gated with correct teardown on
  normal and crash paths; **chunk caches** have unified LRU eviction with
  correct VRAM accounting.

## Part 2 — pan stutter: resolved (platform, not compositor)

Symptom: occasional hitches while panning with windows visible; smooth over
bare background and during cluster drags. Investigated across five captures;
every compositor-side hypothesis was measured and eliminated in turn:
client re-rendering (`frame.commits` flat — zero-commit frames miss equally),
blur/shadow (disabled in all captures; pixel-shader draws are borders),
raw GPU load (compositor pass 1.6–4 ms typical), CPU work (cheap in 95% of
missed frames), motion judder (camera advances evenly — the cadence halves).

Root cause, measured via `[backend] wait_for_frame_completion = true`
(turns the kernel-side fence wait into CPU-visible time): **AGX GPU
frame-completion latency sits at discrete power-state-dependent levels —
~7.5 / 12.5 / 13.4 / 17.5 ms p50 — for near-identical trivial frames.**
Cadence follows the level exactly: ≈7.5 ms → clean 60 fps; ≈12.5 ms →
marginal; ≥13.4 ms → CPU (2–4 ms) + GPU completion exceeds one vblank →
the flip slips. Under continuous rendering (animated bg) this becomes a
**bistable 30 fps lock** (low fps → low utilization → firmware stays in the
slow state), in runs of up to 93 frames; under normal on-demand rendering it
surfaces as the original ~4% isolated single-vblank hitches at power-state
transitions. Input activity bumps the state (pans ran ≈7.5 ms / 60 fps in
capture 5). AGX power states are managed by Apple firmware on the ASC
coprocessor; no devfreq/sysfs override exists. KWin's "locks to 30 FPS with
no clear pattern" reports are the same symptom class. (Note: Tracy GPU-span
durations are unreliable on AGX — the fence wait is ground truth.)

Conclusions:
1. `wait_for_frame_completion` does **not** help here — it relocates the same
   wait into the event loop, delaying input. Keep it **off** on this
   hardware (it exists for NVIDIA-style implicit-sync gaps).
2. The one compositor-side fix that would restore 60 fps under >16.7 ms GPU
   latency is **frame pipelining / triple buffering** (two frames in flight;
   +1 frame latency). smithay's `DrmCompositor` allows a single in-flight
   frame — upstream feature work; mutter grew dynamic triple buffering for
   exactly this failure mode.
3. The discrete fence-latency levels are solid evidence for an
   AsahiLinux/linux report — affects every vsync'd double-buffered
   compositor on this hardware.
4. The static/cached background default is already the best configuration —
   on-demand rendering avoids the sustained low-utilization trap.

## Part 3 — niri patterns worth adopting

driftwm's udev backend already matches niri's core invariants (event-driven
redraw gating, empty-damage skip + estimated-vblank pacing, callback
throttling, DPMS/VT shutdown, gesture coalescing). Still worth stealing:

1. **Winit: skip submit on empty damage** (`niri/src/backend/winit.rs:257-283`)
   — direct fix for B3.
2. **~1 Hz fallback callbacks for invisible surfaces** (`niri/src/niri.rs:193`,
   `:5118-5185`) — driftwm's 0 FPS throttle is better for battery but means an
   off-viewport client waiting on a callback stalls completely until panned
   back into view; the 1 Hz tick is the escape hatch if that ever bites.
3. **Animations sampled at predicted presentation time** — niri freezes a
   shared clock at `frame_clock.next_presentation_time()` per redraw
   (`niri/src/niri.rs:4601-4604`); driftwm ticks from `Instant::now()` at
   render time — a small judder source during momentum/zoom animations.
4. **On-demand VRR by window visibility** (`niri/src/niri.rs:4720-4749`) —
   future QoL candidate.
