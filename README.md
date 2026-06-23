<h1 align="center"><img alt="driftwm" src="assets/logo.jpg" width="500"></h1>
<p align="center">A trackpad-first and touchscreen-friendly infinite canvas Wayland compositor.</p>
<p align="center">
    <a href="https://github.com/malbiruk/driftwm/blob/main/LICENSE"><img alt="License: GPL-3.0-or-later" src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue"></a>
    <a href="https://github.com/malbiruk/driftwm/releases"><img alt="GitHub Release" src="https://img.shields.io/github/v/release/malbiruk/driftwm?logo=github"></a>
    <a href="https://repology.org/project/driftwm/versions"><img alt="Packaging status" src="https://repology.org/badge/tiny-repos/driftwm.svg"></a>
</p>
<p align="center"><sub>Primary repository: <a href="https://github.com/malbiruk/driftwm">GitHub</a> · Mirror: <a href="https://codeberg.org/malbiruk/driftwm">Codeberg</a></sub></p>

https://github.com/user-attachments/assets/df24e442-6ad0-4520-9491-cb666da06d05

Traditional window managers arrange windows to fit your screen. Stacking compositors do so by piling windows on top of each other; tiling compositors do so by squeezing them to fit and utilizing workspaces.

`driftwm` is an infinite-canvas compositor: windows live at their native size on an infinite 2D canvas, and your display is a camera viewing it. When two windows come close, they snap together, forming implicit groups that can be moved, resized, and viewed together. No tiling, no workspaces, window overlaps happen only on purpose.

Designed with laptops and touch screens in mind: navigation and window management are trackpad-first (with native touchscreen support); the infinite canvas makes the most of a small screen.

Built on [smithay](https://github.com/Smithay/smithay). Inspired by [vxwm](https://codeberg.org/wh1tepearl/vxwm); borrows implementation details from [niri](https://github.com/YaLTeR/niri).

> [!WARNING]
> This is experimental software, primarily built with AI.

## Features

### Pan & zoom

https://github.com/user-attachments/assets/a5f14739-7762-4515-abb1-0de6990de4a3

Infinite 2D canvas with viewport panning, zoom, and scroll momentum. A quick
flick carries the viewport smoothly until friction stops it.

<details><summary><b>Pan &amp; zoom bindings</b></summary>

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
| Touchscreen drag   | Pan viewport      | on-canvas |
| Touchscreen pinch  | Zoom              | on-canvas |

</details>

### Window navigation

https://github.com/user-attachments/assets/5b7d89cd-b065-4309-ae74-30bfe68a8abb

Jump to the nearest window in any direction via cone search. MRU cycling
(`Alt-Tab`) with hold-to-commit. Zoom-to-fit shows all windows at once.
Configurable anchors act as navigation targets for directional jumps even
with no window there — useful for areas with pinned widgets.

<details><summary><b>Navigation bindings</b></summary>

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

</details>

### Snapping

https://github.com/user-attachments/assets/8a468e69-8887-4d27-8457-cdd2753948ca

Move window with 3-finger doubletap-swipe or `Alt` + drag. Resize with `Alt` + 3-finger swipe. Snapping kicks in as edges approach each other. Drag past the viewport edge and the canvas auto-pans.

**Snapped windows form a cluster.** Two benefits: neighbors stay visible at your view's edge for spatial context, and `Shift` + any move/resize/fit action acts on the whole cluster. Shuffle a layout in one drag, resize a row of panes proportionally, or scope an overview to just the cluster (`Mod+Shift+W`). No explicit grouping to manage.

> [!TIP]
> While dragging a window, keyboard shortcuts still work. Use `Mod+1-4`
> to jump to a bookmark or `Mod+A` to go home — your held window comes with you.

Fit-window (`Mod+M`) is the maximize analogue — centers the viewport, resets
zoom to 1.0, and resizes the window to fill the screen. Toggle again to
restore. Fullscreen (`Mod+F`) is a viewport mode, not a window state — any canvas
action (launching an app, navigating) naturally exits it.

<details><summary><b>Snapping &amp; window bindings</b></summary>

| Input                                     | Action                        |
| ----------------------------------------- | ----------------------------- |
| 3-finger doubletap-swipe                  | Move window                   |
| `Alt` + LMB drag                          | Move window                   |
| `Alt+Shift` + LMB drag                    | Move snapped windows          |
| `Alt` + 3-finger swipe                    | Resize window                 |
| `Alt+Shift` + 3-finger swipe              | Resize snapped window         |
| `Alt` + RMB drag                          | Resize window                 |
| `Alt` + MMB click / `Mod+M`               | Fit window (maximize/restore) |
| `Alt+Shift` + MMB click / `Mod+Shift+M`   | Fit snapped window            |
| `Mod` + 4-finger pinch in / `Mod+Shift+W` | Zoom-to-fit snapped windows   |
| `Alt` + 2-finger pinch in/out             | Fit window                    |
| `Alt` + 3-finger pinch in/out             | Toggle fullscreen             |
| `Mod` + MMB click / `Mod+F`               | Toggle fullscreen             |
| `Mod+Shift` + arrow                       | Nudge window 20px             |

</details>

### Infinite background

https://github.com/user-attachments/assets/6e9eb7f7-0c73-4fdd-b7aa-230b8ff0a172

https://github.com/user-attachments/assets/fb1cd5a1-242c-45d7-b302-952a15aaa24d

The background is part of the canvas — it scrolls and zooms with the viewport,
not stuck to the screen. This gives spatial awareness when panning.

Four modes:

- **`shader`** — procedural GLSL, animated or static, optionally sampling an image via `texture`. Default is a dot grid. See [docs/shaders.md](docs/shaders.md) to write your own. Bundled shaders live in `extras/wallpapers/{static,animated,textured}/`.
- **`tile`** — PNG/JPG (single texture, tiled infinitely), or a tiled pyramidal TIFF for [gigapixel wallpapers](docs/gigapixel-wallpapers.md). Set `mirror_tile = true` to mirror-fold a non-seamless image so it tiles without seams (kaleidoscope look).
- **`wallpaper`** — single image stretched to fill viewport (does not scroll/zoom) — a classic desktop wallpaper.
- **`none`** — no built-in background, so an external `wlr-layer-shell` wallpaper daemon (`swaybg`, `swww`, `mpvpaper` for live video) becomes the wallpaper instead.

> [!NOTE]
> GPU cost scales with what a shader reads: one that reads no viewport uniforms renders once (as cheap as `wallpaper`); reading `u_camera`/`u_zoom` redraws on pan/zoom; reading `u_time` redraws every frame. Tiles redraw on pan/zoom; `wallpaper` renders once.

```toml
[background]
type = "shader"
path = "~/.config/driftwm/bg.glsl"
# texture = "~/Pictures/img.jpg"  # if it's a texture-based shader

# Or: type = "tile",      path = "~/Pictures/tile.png"
# Or: type = "tile",      path = "~/Pictures/world.tif"   # pyramidal TIFF
# Or: type = "wallpaper", path = "~/Pictures/wallpaper.jpg"
# Or: type = "none"                                       # external wallpaper daemon (swaybg/mpvpaper/…)
```

### Window rules

https://github.com/user-attachments/assets/af603001-9f08-4d42-b50a-0342d06e954b

Match windows by `app_id` and/or `title` (glob patterns) and control position,
size, decorations, blur, opacity, key pass-through, and placement — fields
combine freely.

Two special placement modes: **`widget = true`** fixes a window to the canvas
(immovable, below normal windows, out of Alt-Tab — clocks, trays, and
layer-shell surfaces like waybar); **`pinned_to_screen = true`** fixes it to the
screen instead, so it ignores pan/zoom and floats above normal windows
(Picture-in-Picture, call toolbars) — toggleable live with `Mod+T`.

```toml
# Frosted-glass terminal
[[window_rules]]
app_id = "Alacritty"
opacity = 0.85
blur = true

# Desktop widget — pinned to the canvas, borderless
[[window_rules]]
app_id = "my-clock"
position = [50, 50]
widget = true
decoration = "none"
```

> [!TIP]
> To find a window's `app_id` or `title`, run `driftwm msg state` — it lists
> every open window with its app ID, title, position, and size.

See [docs/window-rules.md](docs/window-rules.md) for more details.

### Multi-monitor

https://github.com/user-attachments/assets/3f6cc3a8-a4ed-4d78-80fc-d5a92478c48f

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

- New window placement: in viewport center (default), under cursor, or snapped adjacent to the focused window's cluster
- Click-to-focus (default), focus-follows-mouse (sloppy focus), or touch-to-focus
- Touchscreen support: 1-finger canvas drag-panning and 2-finger anchor pinch-to-zoom
- Session lock (swaylock), idle notify (swayidle/hypridle)
- Screen capture: screencasting (OBS, Firefox, Discord) and screenshots, incl. built-in [canvas/DPI capture](docs/ipc.md#screenshots)
- 40+ Wayland protocols
- [IPC control](docs/ipc.md): script the compositor over a Unix socket with `driftwm msg`

## Install

### Arch Linux (AUR)

```bash
yay -S driftwm
```

or for latest main:

```bash
yay -S driftwm-git
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

Requires Rust 1.88+ (edition 2024).

Install build dependencies:

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

> [!NOTE]
> Ubuntu 24.04 ships Rust 1.75 which is too old. Install via
> [rustup](https://rustup.rs/) instead of `apt install rustc`.

Then build and install:

```bash
git clone https://github.com/malbiruk/driftwm.git
cd driftwm
make build
sudo make install
```

To uninstall, run `sudo make uninstall` from the repository.

### Optional runtime dependencies

driftwm runs standalone — none of these are required — but each enables or
improves a feature:

- `xwayland-satellite` (≥ 0.7) — X11 app support (see below).
- `xdg-desktop-portal` + `xdg-desktop-portal-wlr` (≥ 0.8.0) or `xdg-desktop-portal-cosmic` — screencasting. wlr needs a dmenu-style picker in `$PATH` (`wmenu`/`wofi`/`rofi`/`bemenu`/`mew`/`fuzzel`) to choose what to share.
- `grim` + `slurp` — screenshots (+ cropping to region). driftwm also has a built-in canvas/DPI capture: see [IPC › Screenshots](docs/ipc.md#screenshots).
- `adwaita-fonts` — renders SSD title bars in `Adwaita Sans` to match GTK apps; without it a generic sans-serif is substituted. Font, size, weight, and alignment are configurable under `[decorations]`.
- A cursor theme — most desktops set one up already; on a bare install driftwm falls back to a basic built-in arrow.

**X11 apps** run through [xwayland-satellite](https://github.com/Supreeeme/xwayland-satellite),
which driftwm spawns at startup, exporting `DISPLAY=:N` so X11 clients connect
transparently — no extra config beyond having the binary in `$PATH`.

- **Arch:** `sudo pacman -S xwayland-satellite`
- **Fedora:** `sudo dnf install xwayland-satellite`
- **NixOS:** `pkgs.xwayland-satellite`
- **Debian/Ubuntu:** not yet packaged — `cargo install --locked xwayland-satellite`

If satellite isn't found at startup, driftwm logs a warning and continues without
X11 support. You can override the binary path or disable the integration in
[`config.reference.toml`](config.reference.toml) under `[xwayland]`.

### Running

driftwm auto-detects whether it's running nested (inside an existing Wayland
session) or on real hardware (from a TTY). Just run `driftwm`. For display
manager integration, select "driftwm" from the session menu.

> [!TIP]
> When launched by a display manager, driftwm runs as a systemd user service — view logs with `journalctl --user -u driftwm.service` (add `-f` to follow). Run directly and logs go to stderr.

## Quick start

`mod` is Super by default. Terminal and launcher are auto-detected (foot/alacritty/kitty, fuzzel/wofi/bemenu); override in config.

| Shortcut           | Action        |
| ------------------ | ------------- |
| `mod+return`       | Open terminal |
| `mod+d`            | Open launcher |
| `mod+q`            | Close window  |
| `mod+l`            | Lock screen   |
| `mod+ctrl+shift+q` | Quit          |

Feature-specific bindings (navigation, zoom, snap) are in their respective sections above.

## Configuration

Config file: `~/.config/driftwm/config.toml` (respects `XDG_CONFIG_HOME`).

```bash
mkdir -p ~/.config/driftwm
cp /etc/driftwm/config.reference.toml ~/.config/driftwm/config.toml
```

Missing file uses built-in defaults. Partial configs merge with defaults —
only specify what you want to change. Use `"none"` to unbind a default binding.
Validate without starting: `driftwm --check-config`.

```toml
# Launch programs at startup
autostart = ["waybar", "swaync", "swayosd-server"]
```

Every option is documented in **[docs/config.md](docs/config.md)** (generated
from [`config.reference.toml`](config.reference.toml)): input settings,
scroll/momentum tuning, snap behavior, decorations, effects, per-output config,
gesture bindings, mouse bindings, and window rules.

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
- **grim + slurp** — Screenshots (or the built-in canvas/DPI capture: [IPC › Screenshots](docs/ipc.md#screenshots))
- **wlr-randr / wdisplays** — Output configuration
- **COSMIC Settings** — Wi-Fi, Bluetooth, sound (or **nm-applet** + **blueman** + **pavucontrol**)

Compositor-agnostic full Wayland shells like **noctalia**, **wayle**, and **dank-material-shell** should work too (`driftwm` supports `wlr-layer-shell` protocol) but without compositor-specific features.

The [`extras/`](extras/) directory contains a complete setup — driftwm config,
GLSL shader wallpapers, Python widgets (clock, calendar, system stats, power
menu), waybar with taskbar/tray, fuzzel window-search script, and window rules
tying it all together. Use it as a starting point or steal pieces.

## Community

- [driftwm-settings](https://github.com/wwmaxik/driftwm-settings) — GTK4 GUI config editor
- [driftwm-noctalia](https://github.com/youssefvdel/driftwm-noctalia) — noctalia shell fork adapted for driftwm
- [Gallery](https://github.com/malbiruk/driftwm/discussions/143) — community shaders & rices, share your own

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

TL;DR: open an issue before writing non-trivial code, keep PRs small and focused.

## Merch

If you want to support the project (or just want a shirt), this is the way.

<p align="left"><img src="assets/tshirt.png" width="400"></p>

XL

100 GEL · 37 USD · 2800 RUB

Ships worldwide from Tbilisi.

Order via [Telegram](https://t.me/fiyefiyefiye), [Instagram](https://instagram.com/flwrs_in_ur_eyes), or email [2601074@gmail.com](mailto:2601074@gmail.com).

Revenue goes to me as driftwm's primary maintainer. If you've contributed substantively and want a shirt, drop me a line.

## License

GPL-3.0-or-later
