<h1 align="center"><img alt="driftwm" src="assets/logo.jpg" width="500"></h1>
<p align="center">A trackpad-first infinite canvas Wayland compositor.</p>
<p align="center">
    <a href="https://github.com/malbiruk/driftwm/blob/main/LICENSE"><img alt="License: GPL-3.0-or-later" src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue"></a>
    <a href="https://github.com/malbiruk/driftwm/releases"><img alt="GitHub Release" src="https://img.shields.io/github/v/release/malbiruk/driftwm?logo=github"></a>
    <a href="https://repology.org/project/driftwm/versions"><img alt="Packaging status" src="https://img.shields.io/repology/repositories/driftwm"></a>
</p>
<p align="center"><sub>Primary repository: <a href="https://github.com/malbiruk/driftwm">GitHub</a> · Mirror: <a href="https://codeberg.org/malbiruk/driftwm">Codeberg</a></sub></p>

https://github.com/user-attachments/assets/155511a6-0a6e-4681-9061-21be1e93e02a

Traditional window managers arrange windows to fit your screen. Stacking compositors do so by piling windows on top of each other; tiling compositors do so by squeezing them to fit and utilizing workspaces.

`driftwm` is an infinite-canvas compositor: windows live at their native size on an infinite 2D canvas, and your display is a camera viewing it. When two windows come close, they snap together, forming implicit groups that can be moved, resized, and viewed together. No tiling, no workspaces, window overlaps happen only on purpose.

Designed with laptops in mind: navigation and window management are trackpad-first; the infinite canvas makes the most of a small screen.

Built on [smithay](https://github.com/Smithay/smithay). Inspired by [vxwm](https://codeberg.org/wh1tepearl/vxwm); borrows implementation details from [niri](https://github.com/YaLTeR/niri).

> [!WARNING]
> This is experimental software, primarily built with AI.

## Features

### Pan & zoom

https://github.com/user-attachments/assets/97c2ff83-acfa-40ec-ae02-b16ad9a47318

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
| Mouse wheel        | Zoom              | on-canvas |
| `Mod` + scroll     | Zoom at cursor    | anywhere  |
| `Mod+=` / `Mod+-`  | Zoom in / out     | —         |
| `Mod+0` / `Mod+Z`  | Reset zoom to 1.0 | —         |

</details>

### Window navigation

https://github.com/user-attachments/assets/ab80b545-817d-4d9f-beea-d332b3bb3dfa

Jump to the nearest window in any direction. MRU cycling (`Alt-Tab`) with
hold-to-commit. Zoom-to-fit shows all windows at once. Configurable _anchors_
act as navigation targets for directional jumps even with no window there —
useful for areas with pinned widgets. _Bookmarks_: named canvas
spots the camera jumps straight to.

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
| `Mod+Shift+1-4`              | Save the current position as that bookmark |

All 4-finger navigation gestures also work as `Mod` + 3-finger for smaller
trackpads.

</details>

### Snapping

https://github.com/user-attachments/assets/6bfc3458-664f-4746-a176-18f40337d94d

Move window with 3-finger doubletap-swipe or `Alt` + drag. Resize with `Alt` + 3-finger swipe. Snapping kicks in as edges approach each other. Drag past the viewport edge and the canvas auto-pans.

**Snapped windows form a cluster.** Neighbors stay visible at your view's edge for spatial context, and `Shift` + any move/resize/fit action acts on the whole cluster. Shuffle a layout in one drag, resize a row of panes proportionally, or scope an overview to just the cluster (`Mod+Shift+W`). No explicit grouping to manage.

> [!TIP]
> While dragging a window, keyboard shortcuts still work. Use `Mod+1-4`
> to jump to a bookmark or `Mod+A` to go home — your held window comes with you.

Fit-window (`Mod+M`) is the maximize analogue — centers the viewport, resets
zoom to 1.0, and resizes the window to fill the screen. Toggle again to
restore. Fullscreen (`Mod+F`) fills the screen with the focused window until any
canvas action — launching an app, navigating — exits it.

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
| `Mod+Shift` + arrow                       | Nudge window (default 20px)   |

</details>

### Touchscreen

https://github.com/user-attachments/assets/35316541-ad39-4c36-95ab-4093bd48c172

Everything works by touch too: pan and zoom the canvas, jump between windows, and
move or resize windows — even whole window groups — exactly as you would on a
trackpad.

<details><summary><b>Touch gestures</b></summary>

| Input                    | Action                      | Context   |
| ------------------------ | --------------------------- | --------- |
| 1-finger swipe           | Pan viewport                | on-canvas |
| 2-finger swipe           | Pan viewport                | on-canvas |
| 3-finger swipe           | Pan viewport                | anywhere  |
| 2-finger pinch           | Zoom                        | on-canvas |
| 3-finger pinch           | Zoom                        | anywhere  |
| 4-finger swipe           | Jump to nearest window      | anywhere  |
| 4-finger pinch in / out  | Zoom-to-fit / home toggle   | anywhere  |
| 3-finger tap             | Center window               | anywhere  |
| 3-finger double-tap      | Fit window                  | on-window |
| 3-finger doubletap-swipe | Move window (hold: cluster) | on-window |
| 3-finger hold-swipe      | Resize window               | on-window |

</details>

### Infinite background

https://github.com/user-attachments/assets/b1581182-5e21-45c8-8559-99ab54bb5093

https://github.com/user-attachments/assets/fb1cd5a1-242c-45d7-b302-952a15aaa24d

The background is part of the canvas — it scrolls and zooms with the viewport,
not stuck to the screen.

Five modes:

- **`default`** — the built-in dot grid, what you get with no configuration.
- **`shader`** — procedural GLSL, animated or static, optionally sampling an image via `texture`. See [docs/shaders.md](docs/shaders.md) to write your own. Bundled shaders live in `extras/wallpapers/{static,animated,textured}/`.
- **`tile`** — PNG/JPG (single texture, tiled infinitely), or a tiled pyramidal TIFF for [gigapixel wallpapers](docs/gigapixel-wallpapers.md). Set `mirror_tile = true` to mirror-fold a non-seamless image so it tiles without seams (kaleidoscope look).
- **`wallpaper`** — single image scaled to cover the viewport, aspect-preserving (does not scroll/zoom) — a classic desktop wallpaper.
- **`none`** — no built-in background, so an external `wlr-layer-shell` wallpaper daemon (`swaybg`, `swww`, `mpvpaper` for live video) becomes the wallpaper instead.

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

https://github.com/user-attachments/assets/e30e3821-1e84-4f6e-be60-adcb0ee58d3c

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

<details><summary><b>Multi-monitor bindings</b></summary>

| Input             | Action                         |
| ----------------- | ------------------------------ |
| `Mod+Alt` + arrow | Send window to adjacent output |

</details>

### Panels, docks & taskbars

https://github.com/user-attachments/assets/31c235e6-baae-4843-bb43-aca749e41f04

Layer shell surfaces (waybar, fuzzel, mako) work as expected. Docks and taskbars
see every window — click one and the viewport pans to it and centers it.
Bookmarks are exported as workspaces over `ext-workspace-v1` — bars that support
it can list them and click to jump. It's an export for tooling, not real
workspaces; driftwm is still one canvas.

### Window suspend & session restore

https://github.com/user-attachments/assets/a91d92bd-55af-4646-8be8-81f35f8f38ca

Close a window and leave a placeholder behind instead of losing it:
`suspend-window` swaps the window for a compositor-drawn stand-in at the same
canvas spot — press `Enter` or click its name to bring the app right back, in
the same place. `suspend_on_close` does this automatically for every
client-initiated close. `[session].restore_windows` saves your whole canvas on quit/logout
and restores it (dormant, nothing auto-launches) on the next start, with
`restore_camera` bringing each output's view back too.

See [docs/session.md](docs/session.md).

### Everything else

- New window placement: in viewport center (default), under cursor, or snapped adjacent to whatever is in view
- Click-to-focus (default) or focus-follows-mouse (sloppy focus)
- Hot corners: any keybinding action fires when the cursor reaches a screen corner, configured per monitor
- Cursor edge-pan: push the pointer against a screen edge and the viewport pans, toggled with `Mod+E`
- Session lock and idle notification
- Screen capture: screencasting (OBS, Firefox, Discord) and screenshots, incl. built-in [canvas/DPI capture](docs/ipc.md#screenshots)
- 40+ Wayland protocols
- [IPC control](docs/ipc.md): script the compositor over a Unix socket with `driftwm msg` (full command/flag reference: [docs/cli.md](docs/cli.md))

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

To enable `driftwm` on NixOS, you can import and use the provided NixOS module in your configuration.

Using Flakes:

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    driftwm.url = "github:malbiruk/driftwm";
  };

  outputs = { self, nixpkgs, driftwm, ... }: {
    nixosConfigurations.myHost = nixpkgs.lib.nixosSystem {
      modules = [
        driftwm.nixosModules.default
        ./configuration.nix
      ];
    };
  };
}
```

Then, enable it in your configuration:

```nix
# configuration.nix
{
  programs.driftwm.enable = true;
}
```

Without flakes, import the flake's output directly:

```nix
let
  driftwm-flake = builtins.getFlake "github:malbiruk/driftwm";
in
{
  imports = [ driftwm-flake.nixosModules.default ];
  programs.driftwm.enable = true;
}
```

`programs.driftwm.package` selects the package providing the compositor binary.
XWayland via `xwayland-satellite` is on by default; set
`programs.xwayland.enable = false;` to disable it.

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

### Running

driftwm auto-detects whether it's running nested (inside an existing Wayland
session) or on real hardware (from a TTY). Just run `driftwm`. For display
manager integration, select "driftwm" from the session menu.

`mod` is Super by default. The terminal is `$TERMINAL`, else the first of foot,
alacritty, ptyxis, kitty, wezterm, gnome-terminal, konsole; the launcher is
`$LAUNCHER`, else the first of fuzzel, wofi, rofi, bemenu-run, wmenu-run,
tofi-drun, mew-run. Both are overridable in config.

| Shortcut           | Action        |
| ------------------ | ------------- |
| `mod+return`       | Open terminal |
| `mod+d`            | Open launcher |
| `mod+q`            | Close window  |
| `mod+l`            | Lock screen   |
| `mod+ctrl+shift+q` | Quit          |

Feature-specific bindings (navigation, zoom, snap) are in their respective sections above.

Nested sessions don't save or restore [state](docs/session.md#nested-sessions)
unless you pass `--session-file <path>`.

> [!TIP]
> When launched by a display manager, driftwm runs as a systemd user service — view logs with `journalctl --user -u driftwm.service` (add `-f` to follow). Run directly and logs go to stderr.

### Optional runtime dependencies

Each of these enables or improves a feature:

- `xwayland-satellite` (≥ 0.7) — X11 app support (see below).
- `xdg-desktop-portal` + `xdg-desktop-portal-wlr` (≥ 0.8.0) or `xdg-desktop-portal-cosmic` — screencasting, and screenshot apps that go through the portal (e.g. Flameshot). wlr needs a dmenu-style picker in `$PATH` (`wmenu`/`wofi`/`rofi`/`bemenu`/`mew`/`fuzzel`) to choose what to share.
- `grim` + `slurp` — screenshots (+ cropping to region).
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

Every option is documented in **[docs/config.md](docs/config.md)**, generated
from [`config.reference.toml`](config.reference.toml).

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
- **wlr-randr / wdisplays** — Output configuration
- **COSMIC Settings** — Wi-Fi, Bluetooth, sound (or **nm-applet** + **blueman** + **pavucontrol**)

Compositor-agnostic full Wayland shells like **noctalia**, **wayle**, and **dank-material-shell** should work too (`driftwm` supports `wlr-layer-shell` protocol) but without compositor-specific features.

The [`extras/`](extras/) directory contains a complete setup — driftwm config,
GLSL shader wallpapers, Python widgets (clock, calendar, system stats, power
menu), waybar with taskbar/tray, a fuzzel spotlight script that lists open
windows, suspended windows, and installed apps together, and window rules tying
it all together. Use it as a starting point or steal pieces.

## Community

- [driftwm-settings](https://github.com/wwmaxik/driftwm-settings) — GTK4 GUI config editor
- [driftwm-noctalia](https://github.com/youssefvdel/driftwm-noctalia) — noctalia shell fork adapted for driftwm
- [Just Enough Shell](https://github.com/ORFLEM/just_enough_shell) — minimal QuickShell desktop shell, driftwm-focused
- [Driftmap](https://github.com/rywby-dot/driftwm-minimap) — interactive overlay minimap for driftwm
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

Revenue goes to me as driftwm's primary maintainer.

## License

GPL-3.0-or-later
