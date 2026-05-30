# Profiling driftwm

driftwm is instrumented with [Tracy](https://github.com/wolfpld/tracy) behind
the `profile-with-tracy` cargo feature. The feature compiles out completely
when off — no runtime or binary cost in normal builds — so the instrumentation
stays in the tree permanently.

## 1. Build the Tracy server (one time)

The GUI/CLI server must match the `tracy-client` crate version in `Cargo.toml`
(currently 0.18.x → Tracy v0.13.x). Distro packages are often mismatched or
ship no aarch64 GUI, so build from source:

```sh
git clone https://github.com/wolfpld/tracy ~/tracy
cd ~/tracy
git checkout v0.13.1          # match the tracy-client version table
# GUI (optional, needs GTK/wayland dev libs):
cmake -B profiler/build -S profiler -DCMAKE_BUILD_TYPE=Release
cmake --build profiler/build -j2     # -j2: template-heavy, ~1-2 GB/core
# CSV exporter (required for dev/scripts/tracy_analyze.py):
cmake -B csvexport/build -S csvexport -DCMAKE_BUILD_TYPE=Release
cmake --build csvexport/build -j2
```

If GCC rejects `tracy_robin_hood.h` (`UINT64_C not declared`), add
`#include <cstdint>` near the top of `~/tracy/server/tracy_robin_hood.h`.

## 2. Capture

```sh
cargo run --release --features profile-with-tracy            # udev (from a TTY)
cargo run --release --features profile-with-tracy -- ...     # winit (nested)
```

Connect the live GUI (`~/tracy/profiler/build/tracy-profiler`) before or during
the run, then **drive the interaction you want to measure** (pan, zoom, a
window animation). Save the capture to a `.tracy` file from the GUI.

Tips for a clean capture:
- Profile one interaction at a time on an **empty canvas** when measuring the
  background, so the background dominates the frame.
- Pan/animate *continuously* for several seconds — steady-state cadence matters
  more than one-off frames.
- The render loop parks when nothing moves, so idle pauses appear as
  multi-second frame intervals. The analyzer filters these out (see below).

Cargo features:
- `profile-with-tracy` — spans + frame marks.
- `profile-with-tracy-ondemand` — only profiles while a server is connected.
- `profile-with-tracy-allocations` — also profile the global allocator.

## 3. Analyze

```sh
dev/scripts/tracy_analyze.py CAPTURE.tracy
```

Reports per-zone exec times and the **active** frame cadence (idle gaps >100 ms
removed), classified by vblank multiple (1 vblank ≈ 60 fps, 2 ≈ 30 fps).

The analyzer is generic: when a code path emits a Tracy *plot*, bucket frame
times by it. The chunked tile-bg plots `bg_chunks.target_lod`, so:

```sh
dev/scripts/tracy_analyze.py CAPTURE.tracy --bucket-by bg_chunks.target_lod
```

gives per-LOD frame-time tables. Any future plot works the same way — add the
plot in the code, pass its name to `--bucket-by`, no script change needed. For
the winit backend, pass `--frame-zone winit::frame` (the default is
`udev::render_frame`).

The script finds `tracy-csvexport` via `$TRACY_CSVEXPORT`, then PATH, then
`~/tracy/csvexport/build/`.

## Where instrumentation lives

- `src/main.rs` — Tracy client startup + optional profiling allocator.
- `src/backend/udev.rs` / `src/backend/winit.rs` — per-frame span + frame mark.
- `src/render/mod.rs` — `compose_frame` span.
- `src/render/tile_chunks.rs` — chunked tile-bg spans + `bg_chunks.*` plots.
