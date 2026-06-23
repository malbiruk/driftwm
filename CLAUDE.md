# CLAUDE.md

## Project

driftwm — a trackpad-first infinite canvas Wayland compositor written in Rust. Windows float on an unbounded 2D plane navigated via trackpad gestures (pan, zoom, pinch). No workspaces, no tiling. Built on [smithay](https://github.com/Smithay/smithay).

Actively developed and released (currently v0.11.x) — a public, multi-contributor project, so match existing conventions carefully. See `dev/docs/CAVEATS.md` for architectural pitfalls.

## Conventions

- Documentation splits by audience: user-facing docs (shaders, window rules, IPC, gigapixel wallpapers) live in `docs/`; internal dev/architecture notes and profiling tooling live in `dev/docs/` and `dev/scripts/`. `README.md` stays at the repo root.
- Config path: `~/.config/driftwm/config.toml` (respects `XDG_CONFIG_HOME`).

## Code Style

- Write self-documenting code: clear names, obvious structure, minimal comments.
- No section-separator comments (e.g. `// ---- Protocols ----` or `// === Input ===`). Code structure should be clear from the code itself.
- Comments explain *why*, not *what*. Don't restate what the code does.
- Brief doc comments (`///`) on public functions are fine when the signature isn't self-explanatory.
- Inline comments for non-obvious logic (smithay quirks, coordinate space tricks) are good.
- Formatting is automated: run `cargo fmt` (rustfmt stable defaults, no `rustfmt.toml`). CI gates on `cargo fmt --check`.

## Build & Run

```bash
cargo build              # build
cargo run                # run nested in existing Wayland session (winit backend)
cargo run -- --backend udev   # run on real hardware (from TTY)
cargo test               # run tests
cargo test test_name     # run a single test
cargo clippy             # lint
cargo fmt                # format (CI runs cargo fmt --check)
```

Use `RUST_LOG=debug cargo run` for smithay/libinput event traces.

Udev backend build deps (Fedora): `libseat-devel libdisplay-info-devel libinput-devel mesa-libgbm-devel`.

### Cross-distro build testing (containers)

Use podman to test builds on other distros (Docker Desktop is flaky on Fedora):

```bash
# Arch Linux
podman run --rm -it --security-opt label=disable -v ~/Documents/work/scripts/driftwm:/src archlinux:latest bash
pacman -Syu --noconfirm rust cargo pkg-config libdisplay-info libinput seatd mesa libxkbcommon
cd /src && cargo build
```

Notes:
- `--security-opt label=disable` is required on Fedora (SELinux blocks libc inside container otherwise).
- Use `cargo build` (not `--release`) for faster dep checking — release optimizations are slow and unnecessary for verifying deps.
- Don't copy the repo inside the container — `target/` is huge. Mount directly without `:ro` so cargo can write to `target/`.

## Architecture

The compositor uses a **camera/viewport** model: the screen is a viewport onto an infinite 2D plane. Each window has absolute `(x, y)` canvas coordinates. The viewport has a camera `(cx, cy)` and zoom `z`. Screen position = `(wx - cx) * z`. Multiple monitors = multiple independent viewports on the same canvas.

Current source layout:

- `main.rs` — entry point (CLI args, backend selection), `lib.rs` — crate root (module declarations)
- `backend/` — `mod.rs` (Backend enum: Winit/Udev + renderer accessor), `winit.rs` (winit backend init + ~60fps timer render loop), `udev.rs` (udev/DRM backend init + VBlank-driven render loop, libseat session, libinput, hotplug), `cvt.rs` (VESA CVT modeline synthesis via libdisplay-info), `gamma.rs` (per-CRTC gamma LUT: atomic GAMMA_LUT blob + legacy ioctl fallback)
- `state/` — `mod.rs` (DriftWm struct, FullscreenState, ClientState), `init.rs` (DriftWm constructor: wires every protocol state, seat, runtime fields), `animation.rs` (camera/zoom/momentum/edge-pan animation, key repeat), `navigation.rs` (navigate_to_window, focus history, MRU cycle), `fullscreen.rs` (enter/exit fullscreen, pointer remap), `fit.rs` (per-window fit-to-viewport toggle, pre-fit size restore), `focus.rs` (FocusTarget(WlSurface) newtype with KeyboardTarget/PointerTarget/TouchTarget impls), `cursor.rs` (cursor state), `render_cache.rs` (cached render state), `cluster_snapshot.rs` (cluster membership + per-member offsets captured at drag/resize grab start), `errors.rs` (on-screen error-bar state, errors keyed by source), `persistence.rs` (state file under `$XDG_RUNTIME_DIR/driftwm/state` for external tools to read camera/zoom/inventory), `reload.rs` (config hot-reload; a bad edit keeps the old config and never crashes)
- `config/` — `mod.rs` (Config struct, load/parse, context-aware lookup methods), `types.rs` (Action, Direction, Modifiers, KeyCombo, MouseBinding/MouseTrigger/MouseAction, GestureBinding/GestureTrigger, ContinuousAction/ThresholdAction, ContextBindings, BindingContext), `parse.rs` (string→type parsers for combos/actions/gestures), `parse_helpers.rs` (raw serde structs → processed types: defaults, clamping, validation), `defaults.rs` (default key/mouse/gesture bindings per context, terminal/launcher detection), `toml.rs` (serde structs, config path)
- `canvas.rs` — coordinate transforms (ScreenPos/CanvasPos), camera math, cone search, zoom helpers (zoom_to_fit, zoom_anchor_camera, snap_zoom, dynamic_min_zoom)
- `decorations.rs` — per-window SSD state, CPU-rendered title bar, hit-testing helpers
- `text.rs` — SSD title-bar text: shaping/measurement/tail-ellipsis truncation + cosmic-text rasterization onto a CPU buffer (shared FontSystem warmed off-thread)
- `render/` — `mod.rs` (compose_frame, post_render, OutputRenderElements), `elements.rs` (tile/cursor/layer rendering helpers), `layers.rs`, `cursor.rs`, `blur.rs` (blur pipeline helpers), `capture.rs` (screencopy/capture helpers), `screenshot.rs` (off-screen canvas→PNG for `driftwm msg screenshot`; tiles + stitches captures larger than GPU max texture), `capture_background.rs` (fresh per-tile background for off-screen captures), `background.rs`, `error_bar.rs` (bottom-edge error-bar render element — internal chrome, input passes through), `lifecycle.rs`, `shaders.rs`, `shader_chunks.rs` (GPU-bakes static `u_camera`-only shaders into canvas-aligned texture chunks so panning samples cached textures), `tile_chunks.rs`/`tile_chunks_tiff.rs`/`tile_worker.rs` (gigapixel wallpaper: pyramidal-TIFF LOD source, on-demand chunk decode/upload, off-thread decoder pool)
- `shaders/` — GLSL shader source files (dot_grid, shadow, blur_down/blur_up/blur_mask, corner_clip, border, tile_bg, chunk_bg, wallpaper_bg)
- `region.rs` — decompose a `RegionAttributes` (additive/subtractive rects) into a non-overlapping rect list
- `signals.rs` — graceful shutdown via SIGINT/SIGTERM/SIGHUP
- `surface_tree.rs` — test whether a focused surface (incl. popups) belongs to a given toplevel window
- `layout/` — window position relationships on the canvas. `snap.rs` (magnetic edge alignment during drag, defines `SnapRect`), `cluster.rs` (BFS over snap-adjacency graph for the focused window's connected component, computed on-demand), `auto_placement.rs` (smart placement of a new window adjacent to the focused window's cluster)
- `window_ext.rs` — `WindowExt` trait for window operations (close, app_id, title, configure)
- `xwayland.rs` — eager `xwayland-satellite` spawn at compositor startup; vanilla mode (satellite binds its own X11 socket). niri's on-demand `-listenfd` pattern races with multi-layout XKB configs under Xwayland 24.x — see `dev/docs/CAVEATS.md`.
- `input/` — `mod.rs` (pointer motion absolute+relative, surface_under hit-testing), `keyboard.rs` (key events: VT-switch, session-lock forwarding, action lookup + execution, key-repeat), `actions.rs` (execute_action dispatch for all keybindings), `pointer.rs` (context-aware mouse dispatch, button/axis handling, compositor resize/pan grabs), `gestures.rs` + `gestures/` (`device_config.rs`, `swipe.rs`, `pinch.rs`, `hold.rs` — table-driven continuous/threshold gesture state machine, libinput device config, client forwarding)
- `grabs/` — `mod.rs`, `move_grab.rs` (MoveSurfaceGrab), `resize_grab.rs` (ResizeSurfaceGrab, ResizeState), `pan_grab.rs` (PanGrab for viewport panning), `navigate_grab.rs` (NavigateGrab for directional window navigation)
- `handlers/` — `compositor.rs` (commit, resize repositioning, dmabuf, layer commit), `layer_shell.rs` (wlr-layer-shell handler), `xdg_shell.rs` (CSD move/resize, window centering, fullscreen, popup grabs), `background_effect.rs` (background-effect handler), `mod.rs` (seat, data device, output, cursor_shape, foreign toplevel, session lock, xdg-decoration, output management, protocol delegates)
- `protocols/` — `mod.rs`, `foreign_toplevel.rs` (zwlr-foreign-toplevel-management-v1), `output_management.rs` (zwlr-output-management-v1), `screencopy.rs` (wlr-screencopy), `image_copy_capture.rs` (ext-image-copy-capture-v1), `image_capture_source.rs` (ext-image-capture-source-v1), `gamma_control.rs` (zwlr-gamma-control-unstable-v1), `output_power.rs` (zwlr-output-power-management-v1)
- `ipc/` — `mod.rs` (compositor-side IPC server on a `$XDG_RUNTIME_DIR` Unix socket; backs `driftwm msg`), `client.rs` (the `driftwm msg` client: connect, send one request, print reply — same binary, never starts a compositor), `protocol.rs` (shared line-delimited-JSON `Request`/`Reply` wire types; debuggable with `socat`)

## Key Design Decisions

- **CSD-first**: compositor advertises only `close` and `fullscreen` capabilities (no maximize/minimize). SSD fallback: 25px title bar (rounded corners, radius 10), × close button, Gaussian shadow shader (radius 14), invisible resize borders (8px). Configurable `bg_color`/`fg_color` via `[decorations]`.
- **Gesture-driven**: configurable gesture and mouse bindings with context-awareness (on-window/on-canvas/anywhere). Defaults: 2-finger pinch for viewport zoom, 3-finger swipe for pan, 4-finger for navigation. Mouse equivalents use Mod+click modifiers. Unbound gestures forward to apps.
- **Canvas background**: scrolls with viewport (not fixed to screen). Default is a GLSL dot-grid shader; static shaders are cached and only re-render on viewport changes.
- **Widgets**: layer-shell surfaces or xdg-toplevel windows placed at canvas positions via window rules (`app_id` glob matching, `position` field). Canvas layers bypass the layer map and render at fixed canvas coordinates.
- **External tools**: launcher, lock screen, and on-screen screenshots are external programs (bemenu-run, swaylock, grim) — not built into the compositor. Exception: a built-in *canvas/DPI* screenshot (`driftwm msg screenshot`) captures off-screen canvas regions at arbitrary resolution, which grim structurally can't do; grim remains the default for plain on-screen captures.

## Reference Codebases

- **[niri](https://github.com/niri-wm/niri)** — a scrollable tiling Wayland compositor also built on smithay. When stuck or unsure how to implement a smithay feature (layer shell, xwayland-satellite integration, udev backend, etc.), explore niri's codebase for a working reference. Local clone at `/tmp/niri` (if missing: `git clone --depth 1 https://github.com/niri-wm/niri.git /tmp/niri`).
- **[cosmic-comp](https://github.com/pop-os/cosmic-comp)** — System76's smithay-based desktop compositor. Different design surface from niri (full DE compositor, multi-workspace, session management) so it's useful as a second reference when niri's pattern doesn't fit a particular path. Local clone at `/tmp/cosmic-comp` (if missing: `git clone --depth 1 https://github.com/pop-os/cosmic-comp.git /tmp/cosmic-comp`).

## Smithay API Reference

When you discover smithay API signatures by reading source in `~/.cargo/registry/src/`, document them in `dev/docs/smithay-api.md` so you don't need to re-read the source next time. Include trait signatures, key type definitions, and how pieces fit together.

## Rust Edition

Uses Rust edition **2024** — be aware of edition-specific language features and defaults.
