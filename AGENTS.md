# AGENTS.md

## Project

driftwm — a trackpad-first infinite canvas Wayland compositor written in Rust. Windows float on an unbounded 2D plane navigated via trackpad gestures (pan, zoom, pinch). No workspaces, no tiling. Built on [smithay](https://github.com/Smithay/smithay).

Actively developed and released — a public, multi-contributor project, so match existing conventions carefully. Guidelines live next to the code, read them before working:

- `CONTRIBUTING.md` — PR/issue conventions.
- `dev/docs/caveats.md` — architectural rules and pitfalls (the big one: never touch `Space` window APIs — go through the stage; a clippy lint enforces it).
- `dev/docs/testing.md` — the test-suite map and testing rules (config-reference workflow, proptest conventions).
- `dev/docs/` also holds profiling setup, the `config.reference.toml` grammar, accumulated smithay API notes, and container build recipes.

## Conventions

- Documentation splits by audience: user-facing docs (shaders, window rules, IPC, gigapixel wallpapers) live in `docs/`; internal dev/architecture notes and profiling tooling live in `dev/docs/` and `dev/scripts/`. `README.md` stays at the repo root.
- Config path: `~/.config/driftwm/config.toml` (respects `XDG_CONFIG_HOME`). `config.reference.toml` is the canonical option reference; `docs/config.md` is generated from it — never hand-edit (see `dev/docs/testing.md`).

## Code Style

- Write self-documenting code: clear names, obvious structure, minimal comments.
- No section-separator comments (e.g. `// ---- Protocols ----` or `// === Input ===`). Code structure should be clear from the code itself.
- Comments explain *why*, not *what*. Don't restate what the code does.
- Brief doc comments (`///`) on public functions are fine when the signature isn't self-explanatory.
- Inline comments for non-obvious logic (smithay quirks, coordinate space tricks) are good.
- Formatting is automated: run `cargo fmt` (rustfmt stable defaults, no `rustfmt.toml`). CI gates on `cargo fmt --check`.
- Rust edition **2024** — be aware of edition-specific language features and defaults.

## Build & Run

```bash
cargo build              # build
cargo run                # run nested in existing Wayland session (winit backend)
cargo run -- --backend udev   # run on real hardware (from TTY)
cargo test               # run tests (all of them — no display/GPU needed)
cargo test test_name     # run a single test
cargo clippy             # lint
cargo fmt                # format (CI runs cargo fmt --check)
```

Use `RUST_LOG=debug cargo run` for smithay/libinput event traces.

### Matching CI before you push

`.github/workflows/ci.yml` is the source of truth. It runs, in order:

```bash
cargo fmt --check
cargo check --locked                                    # Cargo.lock in sync
cargo clippy --all-targets -- -D warnings
cargo test --no-run
cargo test -- --include-ignored --skip soak
cargo test --bin driftwm -- --include-ignored --test-threads=1 soak
```

`dev/scripts/ci-local` runs exactly those, in that order, so you don't have to
keep them in your head — `dev/scripts/ci-local clippy` runs a single stage
(`fmt | lock | clippy | test | soak`).

Two ways a local run drifts from CI, worth knowing however you invoke it:

- **CI lints `--all-targets`.** Plain `cargo clippy`, and `cargo clippy --bin
  driftwm`, do not compile `#[cfg(test)]` code — so a lint inside `src/tests/`
  passes locally and fails CI. Clippy checks without linking, so `--all-targets`
  costs little.
- **CI runs `--include-ignored`.** A plain `cargo test` skips ignored tests,
  which is how they rot unnoticed. The soak test additionally wants
  `--test-threads=1` and the process to itself: it reads fd and RSS plateaus
  from `/proc/self`, so a neighbouring test holding a socket open lands inside
  its measurement.

**On a memory-tight machine**, `cargo test` unscoped links all 13 test targets
at once, and that link step is what exhausts RAM. `CI_LOW_MEM=1
dev/scripts/ci-local` builds and runs each target separately instead — same
tests, same flags, one link at a time — and bounds parallelism via `CI_JOBS` /
`CI_TEST_THREADS` (default 2 each). Off by default; unnecessary on a machine
with headroom, where CI's own parallel invocations are faster.

Udev backend build deps (Fedora): `libseat-devel libdisplay-info-devel libinput-devel mesa-libgbm-devel`. Cross-distro build checks: `dev/docs/cross-distro-builds.md`.

## Architecture

The compositor uses a **camera/viewport** model: the screen is a viewport onto an infinite 2D plane. Each window has absolute `(x, y)` canvas coordinates. The viewport has a camera `(cx, cy)` and zoom `z`. Screen position = `(wx - cx) * z`. Multiple monitors = multiple independent viewports on the same canvas.

The crate splits into a **lib** (pure, testable logic: `canvas`, `config`, `layout`, `stage`, `protocols`, `text`, `window_ext`) and a **bin** (everything holding compositor state: `state/`, `backend/`, `render/`, `handlers/`, `input/`, `grabs/`, `ipc/`, plus the in-process test fixture under `src/tests/`). Orientation points, stable ones only — explore the tree for current detail:

- `stage/` — the smithay-free source of truth for windows: list, z-order, positions, focus history, fullscreen/pin/fit membership. Everything window-related routes through it.
- `state/` — the `DriftWm` struct and compositor policy (navigation, fullscreen, fit, persistence, config hot-reload).
- `canvas.rs` + `layout/` — coordinate math, snapping, clustering, auto-placement.
- `backend/` — winit (nested) and udev/DRM (bare metal); render loops live with their backends.
- `render/` — frame composition, shaders (`shaders/` for GLSL), capture/screenshot paths.
- `handlers/` + `protocols/` — Wayland protocol implementations and delegates.
- `input/` + `grabs/` — keyboard/pointer/gesture/touch dispatch and compositor-side grabs.
- `ipc/` — the `driftwm msg` Unix-socket server/client (user docs: `docs/ipc.md`).

## Key Design Decisions

- **CSD-first**: the compositor advertises only `close` and `fullscreen` capabilities (no maximize/minimize). SSD fallback is a minimal title bar + shadow + invisible resize borders, configurable via `[decorations]`.
- **Gesture-driven**: configurable gesture and mouse bindings with context-awareness (on-window/on-canvas/anywhere); unbound gestures forward to apps.
- **Canvas background**: scrolls with the viewport (not fixed to screen); default is a GLSL dot-grid shader, static shaders are cached and only re-render on viewport changes.
- **Widgets**: layer-shell surfaces or xdg-toplevel windows placed at canvas positions via window rules. Canvas layers bypass the layer map and render at fixed canvas coordinates.
- **External tools**: launcher, lock screen, and on-screen screenshots are external programs (bemenu-run, swaylock, grim) — not built into the compositor. Exception: the built-in *canvas/DPI* screenshot (`driftwm msg screenshot`) captures off-screen canvas regions at arbitrary resolution, which grim structurally can't do.

## Reference Codebases

- **[niri](https://github.com/niri-wm/niri)** — a scrollable tiling Wayland compositor also built on smithay. When stuck or unsure how to implement a smithay feature (layer shell, xwayland-satellite integration, udev backend, etc.), explore niri's codebase for a working reference. Local clone at `/tmp/niri` (if missing: `git clone --depth 1 https://github.com/niri-wm/niri.git /tmp/niri`).
- **[cosmic-comp](https://github.com/pop-os/cosmic-comp)** — System76's smithay-based desktop compositor. Different design surface from niri (full DE compositor, multi-workspace, session management) so it's useful as a second reference when niri's pattern doesn't fit a particular path. Local clone at `/tmp/cosmic-comp` (if missing: `git clone --depth 1 https://github.com/pop-os/cosmic-comp.git /tmp/cosmic-comp`).

## Smithay API Reference

When you discover smithay API signatures by reading source in `~/.cargo/registry/src/`, document them in `dev/docs/smithay-api.md` so you don't need to re-read the source next time. Include trait signatures, key type definitions, and how pieces fit together.
