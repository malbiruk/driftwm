# driftwm

A trackpad-first infinite canvas Wayland compositor.

Traditional window managers arrange windows to fit your viewport. driftwm
flips this: windows float on an infinite 2D canvas and you move the viewport
around them. Designed with laptops in mind where trackpad support keeps
getting better and display size is limited. You pan, zoom, and navigate with
trackpad gestures. No workspaces, no tiling — just drift.

Inspired by [vxwm](https://codeberg.org/wh1tepearl/vxwm) and [niri](https://github.com/YaLTeR/niri). Built on [smithay](https://github.com/Smithay/smithay).

## How it works

The screen is a viewport onto an infinite 2D plane. Each window has absolute
coordinates on this plane. You move around with trackpad gestures:

- **2-finger scroll** on empty desktop — pan viewport
- **3-finger scroll** anywhere — pan viewport (ignores windows)
- **3-finger double-tap+drag** on a window — move that window
- **`Mod` + 3-finger drag** on a window — resize that window
- **2-finger pinch** on empty desktop — zoom
- **3-finger pinch** anywhere — zoom (ignores windows)
- **4-finger swipe** — jump to the nearest window in that direction
- **4-finger pinch in** — toggle overview (zoom-to-fit)
- **4-finger pinch out** — toggle home position
- **4-finger hold (tap)** — center focused window

Mouse: scroll wheel zooms, click-drag pans. `Mod` + click/drag works anywhere.
`Mod+Ctrl` + drag navigates to nearest window. `Mod` + trackpad scroll pans anywhere.

A static wallpaper gives no feedback when panning an infinite canvas, so
the background scrolls with the viewport. Any GLSL fragment shader works as
an infinitely generated background, or you can tile an image of any size.

See [docs/DESIGN.md](docs/DESIGN.md) for the full specification.

## Features

Early development — usable as a daily driver on single-monitor setups.

- Infinite 2D canvas with viewport panning, zoom, and scroll momentum
- GPU-scaled zoom with cursor-anchored zoom and dynamic min-zoom
- Window navigation: directional jump (cone search), MRU cycling, home toggle
- Layer shell support (waybar, fuzzel, mako) + foreign toplevel management
- GLSL shader backgrounds or tiled images, scrolling with the viewport
- Trackpad gestures: 3-finger pan/move/resize, pinch zoom, 4-finger navigate/home/overview
- Runs nested (winit) or on real hardware (udev/DRM with libseat)
- TOML config — all keybindings, mouse bindings, and input device settings are configurable
- 20+ Wayland protocols: DMA-BUF, popups, clipboard, layer shell, and more

## Build & run

Requires Rust (edition 2024) and these system libraries:

**Fedora:**

```bash
sudo dnf install libseat-devel libdisplay-info-devel libinput-devel mesa-libgbm-devel
```

**Ubuntu/Debian:**

```bash
sudo apt install libseat-dev libdisplay-info-dev libinput-dev libudev-dev
```

```bash
cargo build
cargo run                         # run nested in existing Wayland session
cargo run -- --backend udev       # run on real hardware (from TTY)
RUST_LOG=debug cargo run          # with debug logging
```

In another terminal, launch apps inside driftwm:

```bash
WAYLAND_DISPLAY=wayland-1 foot       # or alacritty, ptyxis, etc.
```

The socket name is printed at startup — use that if it differs from `wayland-1`.

## Quick start

`mod` is Super by default (configurable). Essential keybindings:

| Shortcut           | Action          |
| ------------------ | --------------- |
| `mod+return`       | Open terminal   |
| `mod+d`            | Open launcher   |
| `mod+q`            | Close window    |
| `mod+ctrl+shift+q` | Quit compositor |
| `mod+scroll`       | Zoom at cursor  |
| `alt+tab`          | Cycle windows   |

All keybindings are configurable. See [`config.example.toml`](config.example.toml)
for the full list of defaults, mouse bindings, and settings.

## Configuration

Config file: `~/.config/driftwm/config.toml` (respects `XDG_CONFIG_HOME`).

Copy the example, uncomment what you want to changeIn compose_frame(), change the render order so canvas layer elements go between
  zoomed windows and widget windows (the widget = true xdg windows that
  enforce_below_windows pushes down), instead of below all windows:

  cursor > overlay > top > normal_windows > canvas_layers > widget_windows > bottom >
   bg

  This means splitting zoomed_windows into normal vs widget windows, or inserting
  canvas layer elements at the right point. The result: waybar + tooltips render
  above the no_focus terminal widgets, below regular app windows. No popup extraction
   needed.:

```bash
mkdir -p ~/.config/driftwm
cp config.example.toml ~/.config/driftwm/config.toml
```

Missing file uses built-in defaults. Partial configs merge with defaults —
only specify what you want to change. Use `"none"` to unbind a default binding.

## Milestones

1. **Window appears** — winit backend, xdg-shell, terminal renders _(done)_
2. **Move and resize** — drag/resize windows, CSD support _(done)_
3. **Infinite canvas** — viewport panning, scroll momentum, xcursor rendering _(done)_
4. **Canvas background** — GLSL shaders, tiled images, edge auto-pan _(done)_
5. **Window navigation** — center, directional jump, Alt-Tab cycle, home toggle _(done)_
6. **Zoom** — GPU-scaled rendering, cursor-anchored zoom, dynamic min-zoom _(done)_
7. **Layer shell** — waybar, fuzzel, foreign toplevel management _(done)_
8. **Config file** — TOML parsing, user keybindings/mouse bindings/settings _(done)_
9. **udev backend** — DRM/KMS, libinput, libseat session management _(done)_
10. **Trackpad gestures** — gesture state machine, libinput device config _(done)_
11. Multi-monitor — multiple viewports on same canvas
12. Decorations — SSD fallback, resize grab zones
13. XWayland — X11 app support
14. Widgets + polish

## License

GPL-3.0-or-later
