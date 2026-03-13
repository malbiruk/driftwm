# driftwm

A trackpad-first infinite canvas Wayland compositor.

<!-- TODO: hero GIF here — pan/zoom across a few windows -->

Traditional window managers arrange windows to fit your screen. driftwm flips this: windows float on an infinite 2D canvas and you move the viewport around them. Pan, zoom, and navigate with trackpad gestures. No workspaces, no tiling — just drift.

Built on [smithay](https://github.com/Smithay/smithay). Inspired by [vxwm](https://codeberg.org/wh1tepearl/vxwm) and [niri](https://github.com/YaLTeR/niri).

## How it works

The screen is a viewport onto an infinite 2D plane. Each window has absolute
coordinates on this plane. You navigate with trackpad gestures:

<!-- TODO: GIF — gesture pan across canvas -->

| Gesture                              | Action                                   |
| ------------------------------------ | ---------------------------------------- |
| 3-finger swipe                       | Pan viewport                             |
| 2-finger pinch (on canvas)           | Zoom                                     |
| 3-finger pinch                       | Zoom                                     |
| 4-finger swipe                       | Jump to nearest window in that direction |
| 4-finger pinch in                    | Zoom-to-fit (overview of all windows)    |
| 4-finger pinch out                   | Home toggle                              |
| 4-finger hold                        | Center focused window                    |
| 3-finger doubletap-swipe (on window) | Move window                              |
| Alt + 3-finger swipe (on window)     | Resize window                            |

**Small trackpad?** Hold `Mod` to use 3-finger instead of 4-finger for all
navigation gestures.

**Mouse:** trackpad scroll pans, mouse wheel zooms on empty canvas.
`Mod` + drag/scroll works anywhere. `Mod+Ctrl` + drag navigates to nearest
window. `Alt` + drag to move (LMB) / resize (RMB).

All gesture and mouse bindings are context-aware (on-window, on-canvas,
anywhere) and fully configurable. Unbound gestures forward to apps.

## Features

<!-- TODO: small GIFs inline for highlights -->

**Canvas & navigation**

- Infinite 2D canvas with viewport panning, zoom, and scroll momentum
- GPU-scaled zoom with cursor-anchored zoom (like Google Maps / Figma)
- Window navigation: directional jump via cone search, MRU cycling (Alt-Tab), home toggle
- Canvas anchors — save positions and jump between them (Mod+1-4)
- Edge auto-pan when dragging windows near viewport edges
- Magnetic window snapping during drag

<!-- TODO: GIF — zoom out to overview, then zoom back in -->

**Input**

- Configurable trackpad gestures with context-awareness (on-window/on-canvas/anywhere)
- Configurable mouse bindings with same context system
- All keybindings configurable via TOML
- XKB keyboard layout support with layout-independent binding matching

**Display**

- Multi-monitor with independent viewports per output, hotplug, runtime output config (wlr-randr, wdisplays)
- Viewport outlines showing where other monitors are looking on the canvas
- GLSL shader backgrounds (or tiled images) that scroll with the viewport
- Custom shaders for background — see [docs/shaders.md](docs/shaders.md)

<!-- TODO: GIF — frosted glass terminal with blur -->

**Window management**

- Window blur and transparency via window rules (frosted-glass terminals)
- Window rules: match by app_id/title glob, set position, size, widget mode, decoration, blur, opacity
- Server-side decorations (title bar, shadows, resize borders) for non-CSD apps
- XWayland support for X11 apps (Steam, Wine, JetBrains, etc.)
- Click-to-focus model — no accidental focus changes while panning

**Ecosystem**

- Layer shell (waybar, fuzzel, mako) + foreign toplevel management
- Session lock (swaylock), idle notify (swayidle), screencasting (OBS, Firefox)
- 29 Wayland protocols
- Runs nested (winit) for development or on real hardware (udev/DRM with libseat)

## Install

Requires Rust (edition 2024) and system libraries.

**Fedora:**

```bash
sudo dnf install libseat-devel libdisplay-info-devel libinput-devel mesa-libgbm-devel
```

**Ubuntu/Debian:**

```bash
sudo apt install libseat-dev libdisplay-info-dev libinput-dev libudev-dev
```

```bash
git clone https://github.com/user/driftwm.git  # TODO: real URL
cd driftwm
cargo build --release
```

### Running

```bash
# Nested in an existing Wayland session (for trying it out):
cargo run

# On real hardware (from a TTY):
cargo run -- --backend udev
```

## Quick start

`mod` is Super by default (configurable via `mod_key`).

| Shortcut           | Action                              |
| ------------------ | ----------------------------------- |
| `mod+return`       | Open terminal                       |
| `mod+d`            | Open launcher                       |
| `mod+q`            | Close window                        |
| `mod+f`            | Toggle fullscreen                   |
| `mod+c`            | Center focused window               |
| `mod+arrow`        | Jump to nearest window in direction |
| `mod+a`            | Home toggle (origin and back)       |
| `mod+w`            | Zoom-to-fit (overview)              |
| `mod+scroll`       | Zoom at cursor                      |
| `alt+tab`          | Cycle windows                       |
| `mod+l`            | Lock screen                         |
| `mod+ctrl+shift+q` | Quit compositor                     |

Terminal and launcher are auto-detected (foot/alacritty/kitty, fuzzel/wofi/bemenu).
All keybindings are configurable — see [`config.example.toml`](config.example.toml).

## Configuration

Config file: `~/.config/driftwm/config.toml` (respects `XDG_CONFIG_HOME`).

```bash
mkdir -p ~/.config/driftwm
cp config.example.toml ~/.config/driftwm/config.toml
```

Missing file uses built-in defaults. Partial configs merge with defaults —
only specify what you want to change. Use `"none"` to unbind a default binding.
Validate without starting: `driftwm --check-config`.

### Window rules

Match windows by `app_id` and/or `title` (glob patterns) to control placement,
decoration, blur, and more:

```toml
# Frosted-glass terminal
[[window_rules]]
app_id = "Alacritty"
opacity = 0.85
blur = true

# Desktop widget — pinned, below normal windows, borderless
[[window_rules]]
app_id = "conky"
position = [50, 50]
widget = true
decoration = "none"
```

### Autostart

```toml
autostart = [
    "waybar",
    "swaync",
    "swayosd-server",
]
```

See [`config.example.toml`](config.example.toml) for all options: input
settings, scroll/momentum tuning, snap behavior, decorations, effects,
per-output config, gesture bindings, and mouse bindings.

See [docs/DESIGN.md](docs/DESIGN.md) for the full design specification.

## Ecosystem

All external — the compositor delegates to standard Wayland tools:

| Tool                  | Purpose               |
| --------------------- | --------------------- |
| waybar                | Status bar            |
| fuzzel / wofi         | App launcher          |
| mako / swaync         | Notifications         |
| swaylock              | Lock screen           |
| swayosd               | Volume/brightness OSD |
| grim + slurp          | Screenshots           |
| wlr-randr / wdisplays | Output configuration  |

## Example setup

The [`extras/`](extras/) directory contains a complete rice — config files,
GLSL shader wallpapers, Python widgets in alacritty windows with custom
app IDs (clock, calendar, system stats, power menu), waybar taskbar/tray,
fuzzel with a window-search script, and more. Window rules match the custom
app IDs to pin the widgets in place and make them borderless.

See [`extras/README.md`](extras/README.md) for the full breakdown.

## License

GPL-3.0-or-later
