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
- **4/5-finger pinch** — toggle home position

Mouse: scroll wheel zooms, click-drag pans. `Mod` + click/drag works anywhere.

A static wallpaper gives no feedback when panning an infinite canvas, so
the background scrolls with the viewport. Any GLSL fragment shader works as
an infinitely generated background, or you can tile an image of any size.

See [docs/DESIGN.md](docs/DESIGN.md) for the full specification.

## Status

Early development. Current milestone: **5 — Window navigation**.

The compositor runs nested via winit backend, renders xdg-shell clients on an
infinite 2D canvas with viewport panning (scroll, click-drag, keyboard), scroll
momentum with friction decay, GLSL shader backgrounds, tiled image backgrounds,
edge auto-pan during window drag, and compositor-rendered xcursor. 18 Wayland
protocols implemented including DMA-BUF, popups, and cross-app clipboard.

## Build & run

Requires Rust (edition 2024).

```bash
cargo build
cargo run                         # run nested in existing Wayland session
RUST_LOG=debug cargo run          # with debug logging
```

In another terminal, launch apps inside driftwm:

```bash
WAYLAND_DISPLAY=wayland-1 foot       # or alacritty, ptyxis, etc.
```

The socket name is printed at startup — use that if it differs from `wayland-1`.

## Keybinds

| Shortcut                | Action                              |
| ----------------------- | ----------------------------------- |
| `Mod+Return`            | Open terminal                       |
| `Mod+Q`                 | Close window                        |
| `Mod+C`                 | Center focused window               |
| `Mod+Arrow`             | Jump to nearest window in direction |
| `Mod+A`                 | Toggle home (0,0) ↔ previous        |
| `Mod+Shift+Left-click`  | Drag to move window                 |
| `Mod+Shift+Right-click` | Drag to resize window               |
| `Mod+Left-click`        | Drag to pan viewport                |
| `Mod+Shift+Arrow`       | Nudge focused window by 20px        |
| `Mod+Ctrl+Arrow`        | Pan viewport by step                |

CSD-initiated move/resize (title bar drag, border drag) also works.

More keybinds planned — see [docs/DESIGN.md](docs/DESIGN.md#keyboard-shortcuts).

## Configuration

Config file: `~/.config/driftwm/config.toml` (respects `XDG_CONFIG_HOME`).
Not yet implemented — coming in a later milestone.

## Architecture

```
src/
├── main.rs          # entry point, event loop, wayland socket
├── state.rs         # DriftWm struct, protocol state
├── config.rs        # keybindings, actions, config
├── winit.rs         # winit backend + render loop
├── input.rs         # keyboard/pointer handling, Alt+click move/resize
├── grabs/
│   ├── mod.rs       # grab module re-exports
│   ├── move_grab.rs # interactive window move (PointerGrab)
│   ├── resize_grab.rs # interactive window resize (PointerGrab)
│   └── pan_grab.rs  # viewport pan via click-drag (PointerGrab)
└── handlers/
    ├── mod.rs       # seat, data device, output delegates
    ├── compositor.rs # compositor + SHM handlers, resize commit logic
    └── xdg_shell.rs  # xdg-shell (window management, CSD move/resize)
```

## Milestones

1. **Window appears** — winit backend, xdg-shell, terminal renders _(done)_
2. **Move and resize** — drag/resize windows, CSD support _(done)_
3. **Infinite canvas** — viewport panning, scroll momentum, xcursor rendering _(done)_
4. **Canvas background** — GLSL shaders, tiled images, edge auto-pan _(done)_
5. Window navigation — Super+C center, Super+Arrow jump, Alt-Tab cycle
6. Zoom — GPU-scaled rendering, pinch to zoom
7. Decorations — SSD fallback, resize grab zones
8. Layer shell — waybar, fuzzel, notifications
9. Config file — TOML parsing, user keybindings
10. udev backend — DRM/KMS, libinput, session management
11. Trackpad gestures — 3-finger pan/double-tap-drag, gesture state machine
12. Multi-monitor — multiple viewports on same canvas
13. XWayland — X11 app support
14. Widgets + polish — eww preset, animations, shadows, damage optimization

## License

GPL-3.0-or-later
