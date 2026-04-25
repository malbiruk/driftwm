<h1 align="center"><img alt="driftwm" src="assets/logo.jpg" width="500"></h1>
<p align="center">A trackpad-first infinite canvas Wayland compositor.</p>
<p align="center">
    <a href="https://github.com/malbiruk/driftwm/blob/main/LICENSE"><img alt="License: GPL-3.0-or-later" src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue"></a>
    <a href="https://github.com/malbiruk/driftwm/releases"><img alt="GitHub Release" src="https://img.shields.io/github/v/release/malbiruk/driftwm?logo=github"></a>
</p>

https://github.com/user-attachments/assets/df24e442-6ad0-4520-9491-cb666da06d05

Traditional window managers arrange windows to fit your screen. driftwm flips this: windows float on an infinite 2D canvas and you move the viewport around them. Designed with laptops in mind — trackpad support keeps getting better while display size stays limited, so treating your screen as a camera onto a larger canvas makes sense. Pan, zoom, and navigate with trackpad gestures. No workspaces, no tiling — just drift.

Built on [smithay](https://github.com/Smithay/smithay). Inspired by [vxwm](https://codeberg.org/wh1tepearl/vxwm), [hevel](https://git.sr.ht/~dlm/hevel), and [niri](https://github.com/YaLTeR/niri).

**WARNING:** This is experimental software. Primarily built with AI. Use at your own risk.

## Concept

Think Figma or Google Maps, but for your desktop. Your screen is a viewport
onto an infinite canvas where windows live. Pan around to find what you need,
zoom out to see everything at once, zoom back in to focus.

Zoom is cursor-anchored — the point under your cursor stays fixed as you zoom
in or out, just like pinch-to-zoom on a map. Multiple monitors are just
multiple viewports on the same canvas.

## Features

### Pan & zoom

https://github.com/user-attachments/assets/a5f14739-7762-4515-abb1-0de6990de4a3

Infinite 2D canvas with viewport panning, zoom, and scroll momentum. A quick
flick carries the viewport smoothly until friction stops it.

| Input              | Action            | Context   |
| ------------------ | ----------------- | --------- |
| 3-finger swipe     | Pan viewport      | anywhere  |
| Trackpad scroll    | Pan viewport      | on-canvas |
| `Mod` + LMB drag   | Pan viewport      | anywhere  |
| `Mod+Ctrl` + arrow | Pan viewport      | —         |
| 2-finger pinch     | Zoom              | on-canvas |
| 3-finger pinch     | Zoom              | anywhere  |
| `Mod` + scroll     | Zoom at cursor    | anywhere  |
| `Mod+=` / `Mod+-`  | Zoom in / out     | —         |
| `Mod+0` / `Mod+Z`  | Reset zoom to 1.0 | —         |

### Window navigation

https://github.com/user-attachments/assets/5b7d89cd-b065-4309-ae74-30bfe68a8abb

Jump to the nearest window in any direction via cone search. MRU cycling
(`Alt-Tab`) with hold-to-commit. Zoom-to-fit shows all windows at once.
Configurable anchors act as navigation targets for directional jumps even
with no window there — useful for areas with pinned widgets.

| Input                        | Action                                     |
| ---------------------------- | ------------------------------------------ |
| 4-finger swipe               | Jump to nearest window (natural direction) |
| `Mod+Ctrl` + LMB drag        | Jump to nearest window (natural direction) |
| `Mod` + arrow                | Jump to nearest window in direction        |
| `Alt-Tab` / `Alt-Shift-Tab`  | Cycle windows (MRU)                        |
| 4-finger pinch in / `Mod+W`  | Zoom-to-fit (overview)                     |
| 4-finger pinch out / `Mod+A` | Home toggle (origin and back)              |
| 4-finger hold / `Mod+C`      | Center focused window                      |
| `Mod+1-4`                    | Jump to bookmarked canvas position         |

All 4-finger navigation gestures also work as `Mod` + 3-finger for smaller
trackpads.

### Move, resize, maximize

https://github.com/user-attachments/assets/363d7252-dc28-4cf0-9c30-b7ca2e617972

Move windows by doubletap-swiping on them. Resize with `Alt` + 3-finger swipe.
Windows snap to nearby edges magnetically during drag. Drag to the viewport
edge and the canvas auto-pans — handy for rearranging windows just beyond the
visible area.

**Tip:** while dragging a window, keyboard shortcuts still work. Use `Mod+1-4`
to jump to a bookmark or `Mod+A` to go home — your held window comes with you.

Fit-window (`Mod+M`) is the maximize analogue — centers the viewport, resets
zoom to 1.0, and resizes the window to fill the screen. Toggle again to
restore. Fullscreen (`Mod+F`) is a viewport mode, not a window state — any canvas
action (launching an app, navigating) naturally exits it.

| Input                         | Action                        |
| ----------------------------- | ----------------------------- |
| 3-finger doubletap-swipe      | Move window                   |
| `Alt` + LMB drag              | Move window                   |
| `Alt` + 3-finger swipe        | Resize window                 |
| `Alt` + RMB drag              | Resize window                 |
| `Alt` + MMB click / `Mod+M`   | Fit window (maximize/restore) |
| `Alt` + 2-finger pinch-in/out | Fit window                    |
| `Alt` + 3-finger pinch-in/out | Toggle fullscreen             |
| `Mod` + MMB click / `Mod+F`   | Toggle fullscreen             |
| `Mod+Shift` + arrow           | Nudge window 20px             |

### Infinite background

https://github.com/user-attachments/assets/9064883c-86ea-4db6-a40a-0418d2ee2f5e

The background is part of the canvas — it scrolls and zooms with the viewport,
not stuck to the screen. This gives spatial awareness when panning.

Two modes: **GLSL shaders** (default: dot grid, or write your own (supports animated)
— see [docs/shaders.md](docs/shaders.md)) and **tiled images** (any PNG/JPG, tiled
infinitely across the canvas). Both are infinite by nature.

```toml
[background]
shader_path = "~/.config/driftwm/bg.glsl"    # custom shader
# tile_path = "~/.config/driftwm/tile.png"   # or tiled image
```

### Window rules

https://github.com/user-attachments/assets/af603001-9f08-4d42-b50a-0342d06e954b

Match windows by `app_id` and/or `title` (glob patterns) and control
everything: position, size, decoration mode, blur, opacity, and widget
behavior. All fields are independent and combine freely.

**Widgets**: set `widget = true` to pin a window in place — immovable, below
normal windows, excluded from Alt-Tab. Works for both regular windows and
layer-shell surfaces (e.g. waybar). Use this for clocks, system stats, trays, or
anything you want fixed on the canvas.

```toml
# Frosted-glass terminal
[[window_rules]]
app_id = "Alacritty"
opacity = 0.85
blur = true

# Desktop widget — pinned, borderless
[[window_rules]]
app_id = "my-clock"
position = [50, 50]
widget = true
decoration = "none"
```

> **Tip:** to find a window's `app_id`, check `$XDG_RUNTIME_DIR/driftwm/state` —
> the `windows` field lists all open windows by their app ID.

### Multi-monitor

<!--
  Video (~10s, two monitors):
  1. Pan on one monitor, show the other monitor's outline moving on canvas
  2. Zoom out on one monitor to a different zoom level than the other
  3. Drag a window across the monitor boundary — it teleports to the other viewport
  4. Mod+Alt+Arrow to send a window to the other output
-->

Multiple monitors are independent viewports on the same canvas. An outline on each monitor shows where the
other monitors' viewports are. Cursor crosses between monitors freely; dragged
windows teleport to the target viewport's canvas position.

| Input             | Action                         |
| ----------------- | ------------------------------ |
| `Mod+Alt` + arrow | Send window to adjacent output |


### Panels, docks & taskbars

https://github.com/user-attachments/assets/83c2ad30-fbfa-4cf2-aa47-905826889dcb

Layer shell surfaces (waybar, fuzzel, mako) work as expected. Foreign toplevel
management means your dock/taskbar shows all windows — click one and the
viewport pans to it and centers it. See [`extras/`](extras/) for a fuzzel
window-search script that lets you search and jump to any open window.

### Everything else

- XWayland for X11 apps (Steam, Wine, JetBrains, etc.)
- Session lock (swaylock), idle notify (swayidle/hypridle)
- Screencasting — full screen and **per-window** (OBS, Firefox, Discord — requires `xdg-desktop-portal-hyprland`)
- Screenshots (grim + slurp)
- Click-to-focus (default) or focus-follows-mouse (sloppy focus)
- All bindings (keyboard, mouse, gesture) fully configurable via TOML
- 31 Wayland protocols

## Install

### Fedora (prebuilt binary)

```bash
curl -fsSL https://raw.githubusercontent.com/malbiruk/driftwm/main/install.sh | sudo sh
```

Installs the binary, session wrapper, desktop entry, and shader wallpapers.
Checks for required runtime libraries and tells you what to install if
anything is missing. To uninstall, run with `sudo sh -s uninstall`.

### Arch Linux (AUR)

```bash
yay -S driftwm
```

### NixOS / Nix

A `flake.nix` is included. To build:

```bash
nix build
```

For development (provides native deps, uses your system Rust):

```bash
nix develop
cargo build
cargo run
```

To add driftwm as a session in your NixOS config:

```nix
let
  driftwm-flake = builtins.getFlake "github:malbiruk/driftwm";
  driftwm = driftwm-flake.packages.x86_64-linux.default;
in
{
  services.displayManager.sessionPackages = [ driftwm ];
  environment.systemPackages = [ driftwm ];
}
```

### Build from source

Requires Rust 1.85+ (edition 2024).

**Fedora:**

```bash
sudo dnf install libseat-devel libdisplay-info-devel libinput-devel mesa-libgbm-devel libxkbcommon-devel
```

**Ubuntu/Debian:**

```bash
sudo apt install libseat-dev libdisplay-info-dev libinput-dev libudev-dev libgbm-dev libxkbcommon-dev libwayland-dev
```

**Arch Linux:**

```bash
sudo pacman -S libdisplay-info libinput seatd mesa libxkbcommon
```

> **Note:** Ubuntu 24.04 ships Rust 1.75 which is too old. Install via
> [rustup](https://rustup.rs/) instead of `apt install rustc`.

```bash
git clone https://github.com/malbiruk/driftwm.git
cd driftwm
cargo build --release
sudo make install
```

### Running

driftwm auto-detects whether it's running nested (inside an existing Wayland
session) or on real hardware (from a TTY). Just run `driftwm`. For display
manager integration, select "driftwm" from the session menu.

## Quick start

`mod` is Super by default. Terminal and launcher are auto-detected
(foot/alacritty/kitty, fuzzel/wofi/bemenu), can be overridden in config.

| Shortcut           | Action                        |
| ------------------ | ----------------------------- |
| `mod+return`       | Open terminal                 |
| `mod+d`            | Open launcher                 |
| `mod+q`            | Close window                  |
| `mod+m`            | Fit window (maximize/restore) |
| `mod+f`            | Toggle fullscreen             |
| `mod+c`            | Center focused window         |
| `mod+x`            | Center window under cursor    |
| `mod+arrow`        | Jump to nearest window        |
| `mod+a`            | Home toggle                   |
| `mod+w`            | Zoom-to-fit (overview)        |
| `mod+=` / `mod+-`  | Zoom in / out                 |
| `mod+scroll`       | Zoom at cursor                |
| `alt+tab`          | Cycle windows                 |
| `mod+l`            | Lock screen                   |
| `mod+ctrl+shift+q` | Quit                          |

All keybindings are configurable — see [`config.example.toml`](config.example.toml).

## Configuration

Config file: `~/.config/driftwm/config.toml` (respects `XDG_CONFIG_HOME`).

```bash
mkdir -p ~/.config/driftwm
cp /etc/driftwm/config.toml ~/.config/driftwm/config.toml
```

Missing file uses built-in defaults. Partial configs merge with defaults —
only specify what you want to change. Use `"none"` to unbind a default binding.
Validate without starting: `driftwm --check-config`.

```toml
# Launch programs at startup
autostart = ["waybar", "swaync", "swayosd-server"]
```

See [`config.example.toml`](config.example.toml) for all options: input
settings, scroll/momentum tuning, snap behavior, decorations, effects,
per-output config, gesture bindings, mouse bindings, and window rules.

## Example setup

driftwm is just a compositor — everything else is standard Wayland tooling.
Here are some tools that work well with it:

- **waybar** — Status bar / taskbar
- **crystal-dock** — macOS-style dock
- **fuzzel / wofi** — App launcher
- **mako / swaync** — Notifications
- **swaylock** — Lock screen
- **swayidle / hypridle** — Idle timeout (lock, suspend)
- **swayosd** — Volume/brightness OSD
- **grim + slurp** — Screenshots
- **wlr-randr / wdisplays** — Output configuration
- **COSMIC Settings** — Wi-Fi, Bluetooth, sound (or nm-applet + blueman + pavucontrol)

The [`extras/`](extras/) directory contains a complete setup — driftwm config,
GLSL shader wallpapers, Python widgets (clock, calendar, system stats, power
menu), waybar with taskbar/tray, fuzzel window-search script, and window rules
tying it all together. Use it as a starting point or steal pieces.

## Community tools

- [driftwm-settings](https://github.com/wwmaxik/driftwm-settings) — GTK4 GUI config editor

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). TL;DR: open an issue before writing non-trivial code, keep PRs small and focused.

## License

GPL-3.0-or-later
