# driftwm

A trackpad-first infinite canvas Wayland compositor.

Windows float on an unbounded 2D plane. You pan, zoom, and navigate with
trackpad gestures. No workspaces, no tiling — just drift.

## Tech stack

- **Language**: Rust
- **Compositor library**: [smithay](https://github.com/Smithay/smithay) — handles Wayland protocol, EGL/Vulkan rendering, input via libinput
- **Rendering**: smithay's OpenGL ES (GlesRenderer) backend
- **Input**: libinput (via smithay) — provides trackpad gesture events (swipe, pinch, hold)
- **Event loop**: [calloop](https://github.com/Smithay/calloop) — smithay's event loop. All async sources (libinput, wayland clients, timers for animations/edge-pan) are wired through it
- **Protocols**:

  Implemented:
  - `wl_compositor` — surface management
  - `wl_shm` — CPU shared-memory buffers
  - `xdg-shell` — core window management (toplevel, popup, popup grabs)
  - `wl_seat` — keyboard, pointer input
  - `wl_data_device` — clipboard / drag-and-drop (cross-app)
  - `wl_output` + `xdg-output` — monitor info
  - `wp_cursor_shape` — client cursor shape negotiation
  - `wp_linux_dmabuf` v3 — GPU buffer sharing (GTK4, Qt6, browsers)
  - `wp_viewporter` — surface cropping/scaling
  - `wp_fractional_scale` — HiDPI fractional scaling
  - `xdg-activation` — cross-app focus requests
  - `wp_primary_selection` — middle-click paste
  - `wlr-data-control` — wl-copy/wl-paste clipboard access
  - `wp_pointer_constraints` — pointer lock/confine
  - `wp_relative_pointer` — relative motion events
  - `keyboard-shortcuts-inhibit` — let apps grab shortcuts
  - `idle-inhibit` — prevent screen dimming
  - `wp_presentation_time` — frame timing feedback
  - `wlr-screencopy` — screenshot/screencast support (grim, OBS)
  - `xdg-decoration` — negotiate SSD vs CSD (CSD-first strategy)
  - `ext-session-lock` — screen locking (swaylock)
  - `wlr-layer-shell` — status bars, launchers, overlays (waybar, fuzzel)
  - `zwlr-foreign-toplevel-management` — taskbar window switching
  - `zwlr-output-management` — runtime output configuration (wlr-randr, wdisplays)
  - `ext-image-capture-source` + `ext-image-copy-capture` — screencasting (xdg-desktop-portal-wlr, OBS, Firefox)
  - `wp_pointer_gestures` — gesture forwarding to clients
  - `xwayland-shell` — X11 app support via Xwayland
  - `ext-idle-notify` — idle detection (swayidle, hypridle)
  - `wp_single_pixel_buffer` — 1x1 solid color buffers (GTK4 backgrounds/separators)
  - `zwp_virtual_keyboard_v1` — synthesized key events (wtype, clipboard auto-paste). Gated behind `wp_security_context_v1`.
  - `wp_security_context_v1` — sandbox attribution for clients; restricts access to privileged protocols

## Core concept: infinite canvas

The screen is a viewport onto an infinite 2D plane. Each window has absolute
`(x, y)` coordinates on this plane. The viewport has a camera position `(cx, cy)`
and a zoom level `z` (default 1.0).

A window at canvas coords `(wx, wy)` is rendered on screen at:

```
screen_x = (wx - cx) * z
screen_y = (wy - cy) * z
screen_w = w * z
screen_h = h * z
```

### Zoom behavior

- **Maximum zoom**: `1.0` — windows are never rendered larger than native resolution
- **Minimum zoom**: dynamic — computed so all windows fit within the viewport (zoom-to-fit)
- **Snap-to-1.0**: when pinch-zooming near 1.0, snap to exactly 1.0 (dead zone ±0.05).
  Avoids the "99% zoom" state
- **Zoom anchor**: always cursor position — the canvas point under the cursor stays
  fixed during both zoom in and zoom out (same as Google Maps / Figma)
- **Cursor size**: fixed — does not scale with zoom level

## Multi-monitor

Multiple monitors = multiple independent viewports on the same canvas. Each
monitor has its own camera `(cx, cy)` and zoom `z`. Panning/zooming on one
monitor affects only that monitor's viewport. Windows exist at canvas coordinates
shared across all monitors.

```
Monitor A: viewport at (0, 0) z=1.0    Monitor B: viewport at (3000, 500) z=0.5
┌──────────────┐                        ┌──────────────┐
│  [terminal]  │                        │ [terminal]   │
│        [vim] │                        │   [vim]      │
└──────────────┘                        │   [browser]  │
                                        └──────────────┘
              ← same infinite canvas →
```

Monitors are cameras, not containers. Windows live on the canvas, not on
monitors. Most compositor code doesn't need to know about multiple monitors —
only the render pipeline and input routing do.

### Per-output state

Each output has independent viewport state (stored via smithay's `UserDataMap`
on the `Output` object):

- camera, zoom, zoom_target, zoom_animation_center, last_rendered_zoom
- overview_return, camera_target
- last_scroll_pan, momentum, panning, edge_pan_velocity
- frame_counter, last_frame_instant, last_rendered_camera
- layout_position, home_return
- cached_bg_element (keyed by output name on DriftWm)
- fullscreen (keyed by Output on DriftWm)
- lock_surface (keyed by Output on DriftWm)

Everything else is global: space, seat, config, focus_history, decorations,
protocol states, gesture state, cursor state.

### Pointer crossing

The cursor crosses between monitors in screen space — move it off the right edge
of monitor A and it appears on the left edge of monitor B. The cursor's canvas
position changes discontinuously because the two viewports are looking at
different canvas areas. Pointer crossing is free — no sticky boundary.

### Dragging windows between monitors

When dragging a window (`MoveSurfaceGrab`) and the cursor crosses to another
monitor, the window's canvas position is adjusted to stay under the cursor
relative to the new viewport's canvas space. A velocity threshold prevents
accidental crossings during slow drags near edges — slow movement clamps at the
boundary, fast movement breaks through.

`SendToOutput` action (default: `Mod+Alt+Arrow`) moves the focused window's
canvas position to the center of the target output's viewport.

### Window placement and navigation

- New windows open at the center of the **active output's** viewport
- `center-nearest` direction search uses the active output's viewport
- `zoom-to-fit` fits all windows within the active output's viewport
- `home-toggle` returns the active output to origin / zoom 1.0
- Layer shell surfaces bind to a specific output; unspecified → active output
- Foreign toplevel activation pans the active output to the target window

### Output configuration

```toml
[[outputs]]
name = "eDP-1"           # connector name (required, find with wlr-randr)
scale = 1.5              # fractional scale (default: 1.0)
transform = "normal"     # normal, 90, 180, 270, flipped, flipped-90, etc.
position = "auto"        # "auto" (default) or [x, y] in layout coords
mode = "preferred"       # "preferred" (default) or "WxH" or "WxH@Hz"
```

`position = "auto"` arranges outputs left-to-right in connection order. The
winit backend ignores `[[outputs]]` config (always one virtual output).

The `zwlr-output-management-unstable-v1` protocol enables runtime output
configuration via GUI tools (wdisplays) and CLI tools (wlr-randr). Runtime
changes are ephemeral — use config.toml or kanshi for persistence.

### Disconnect safety

When all monitors disconnect, the compositor keeps the last output in the space
as a virtual/disconnected placeholder. Renders are no-ops but all code that
calls `active_output()` continues to work. When a monitor reconnects, the
virtual output is replaced by the real one.

### State file

The state file has two layers:

- **Flat keys** (`x`, `y`, `zoom`, etc.) — always reflect the active output's
  viewport. Widgets read these without needing to know about multiple outputs.
- **Per-output keys** (`outputs.eDP-1.camera_x`, etc.) — used for save/restore
  on reconnect.

## Input

All input methods — trackpad, mouse, keyboard — feed into the same actions.
Panning is the most frequent action on an infinite canvas, so there are many
ways to do it. All pan methods feed into the momentum system — a quick flick
carries the viewport smoothly until friction stops it.

### Trackpad gestures

Requires libinput (udev backend). All gesture bindings are configurable via
`[gestures.on-window]`, `[gestures.on-canvas]`, and `[gestures.anywhere]` in
config. Context resolution: specific context checked first, then anywhere as
fallback. Unbound gestures are forwarded to the focused app.

Once a gesture starts, the target is **locked for the gesture's duration** (even
if the surface under the cursor changes mid-gesture).

Default bindings:

| Gesture                      | Context   | Action                             |
| ---------------------------- | --------- | ---------------------------------- |
| 2-finger pinch               | on-canvas | Zoom in/out                        |
| 2-finger pinch               | on-window | Forward to app (unbound)           |
| 3-finger swipe               | anywhere  | Pan viewport (continuous)          |
| 3-finger doubletap-swipe     | on-window | Move window                        |
| Alt+3-finger swipe           | on-window | Resize window                      |
| Alt+2-finger pinch-in/out    | on-window | Fit window (toggle)                |
| Alt+3-finger pinch-in/out    | on-window | Toggle fullscreen                  |
| 3-finger pinch               | anywhere  | Zoom in/out (continuous)           |
| Mod+3-finger swipe           | anywhere  | Center nearest window (threshold)  |
| Mod+3-finger pinch-in        | anywhere  | Zoom-to-fit                        |
| Mod+3-finger pinch-out       | anywhere  | Home toggle                        |
| Mod+3-finger hold            | anywhere  | Center focused window              |
| 4-finger swipe               | anywhere  | Center nearest window (threshold)  |
| 4-finger pinch-in            | anywhere  | Zoom-to-fit                        |
| 4-finger pinch-out           | anywhere  | Home toggle                        |
| 4-finger hold                | anywhere  | Center focused window              |

Gesture triggers are either **continuous** (per-frame dx/dy or scale updates) or
**threshold** (accumulate input, fire once). For swipe, the action determines
which: `pan-viewport` is continuous, `center-nearest` is threshold. For pinch,
the trigger determines which: `pinch` is continuous, `pinch-in`/`pinch-out` are
threshold. Per-direction swipe overrides (`swipe-up`, `swipe-down`, etc.) are
also available for mapping individual directions to discrete actions.

**3-finger doubletap-swipe**: Tap with three fingers on a window (libinput
generates BTN_MIDDLE via tap-to-click), then immediately start a 3-finger
swipe. The compositor buffers the middle click for 300ms — if a 3-finger swipe
follows, the click is suppressed and the swipe enters move-window mode. If no
swipe follows, the click is flushed to the app as a normal middle-click (paste).

**Alt+3-finger resize**: Edges inferred from pointer position in the window
(same quadrant logic as mouse). Uses Alt instead of Mod to avoid conflict with
Mod+3-finger navigation gestures.

**Mod+3-finger alternatives**: All 4-finger gestures (navigate, overview, home,
center) are also available as Mod+3-finger for smaller trackpads where 4-finger
gestures are awkward.

**Threshold swipe (center-nearest)**: Accumulates swipe delta until a 16px
threshold, detects one of 8 directions (4 cardinal + 4 diagonal using 45°
sectors), then fires the action once.

**Threshold pinch**: Pinch-in fires when scale < 0.8, pinch-out when
scale > 1.2.

**Hold**: Place fingers on the trackpad and lift without swiping or pinching.
Action fires on release.

### Mouse equivalents

Mouse bindings are context-aware via `[mouse.on-window]`, `[mouse.on-canvas]`,
and `[mouse.anywhere]`. Default bindings:

| Action           | Trigger                            | Context   |
| ---------------- | ---------------------------------- | --------- |
| Pan viewport     | Left-click drag                    | on-canvas |
| Pan viewport     | `Mod` + left-drag                  | anywhere  |
| Zoom             | Mouse wheel                        | on-canvas |
| Zoom             | `Mod` + mouse wheel                | anywhere  |
| Pan viewport     | Trackpad scroll                    | on-canvas |
| Pan viewport     | `Mod` + trackpad scroll            | anywhere  |
| Move window      | `Alt` + left-drag                  | on-window |
| Resize window    | `Alt` + right-drag                 | on-window |
| Fit window       | `Alt` + middle-click               | on-window |
| Toggle fullscreen| `Mod` + middle-click               | on-window |
| Center nearest   | `Mod+Ctrl` + left-drag (natural)   | anywhere  |

**Trackpad vs mouse wheel**: both produce axis events but serve different
purposes. Separate triggers (`trackpad-scroll` and `wheel-scroll`) allow
per-device bindings — by default trackpad scroll pans the viewport while mouse
wheel zooms on canvas.

### Edge auto-pan

When dragging a window to the viewport edge, the viewport auto-pans in that
direction. Speed is depth-proportional — deeper into the zone means faster
panning (quadratic ramp, like a joystick). All 8 directions (corners =
diagonal blend). Stops when cursor leaves the zone or the drag ends.

```toml
[navigation.edge_pan]
zone = 100.0               # activation zone width (px from viewport edge)
speed_min = 4.0            # px/frame at zone boundary
speed_max = 20.0           # px/frame at viewport edge
```

### Window snapping

When dragging a window near another window's edge, the dragged window snaps to
align edges magnetically. Accounts for SSD title bar boundaries.

```toml
[snap]
enabled = true             # magnetic edge snapping during window drag
gap = 12.0                 # gap between snapped windows (canvas px)
distance = 24.0            # activation threshold (screen px from edge)
break_force = 32.0         # screen px past snap to break free
```

## Keyboard shortcuts

Minimal set. Defaults below, all configurable via `[keybindings]` table.
Data-driven binding lookup, populated from defaults and merged with user config.

Two command actions: `exec <cmd>` shows a loading cursor until the window
appears (for apps), `spawn <cmd>` runs silently (for toggles, OSD, screenshots).

### Window management

| Shortcut            | Action                                 |
| ------------------- | -------------------------------------- |
| `Alt-Tab`           | Cycle windows forward (raise+center)   |
| `Alt-Shift-Tab`     | Cycle windows backward                 |
| `Super+Q`           | Close focused window                   |
| `Super+C`           | Center focused window + reset zoom     |
| `Super+F`           | Toggle fullscreen                      |
| `Super+M`           | Fit window to viewport (maximize/restore) |
| `Super+Shift+Arrow` | Nudge focused window 20px in direction |

### Navigation

| Shortcut      | Action                             |
| ------------- | ---------------------------------- |
| `Super+Arrow` | Center nearest window in direction |
| `Super+A`     | Toggle home (0, 0) ↔ previous pos  |
| `Super+W`     | Zoom-to-fit — show all windows     |
| `Super+1-4`   | Go to canvas corner (↙ ↖ ↗ ↘)     |

### Viewport

| Shortcut           | Action               |
| ------------------ | -------------------- |
| `Super+Ctrl+Arrow` | Pan viewport by step |
| `Super+Plus`       | Zoom in              |
| `Super+Minus`      | Zoom out             |
| `Super+0`          | Reset zoom to 1.0    |

### Launchers

| Shortcut       | Action                     |
| -------------- | -------------------------- |
| `Super+Return` | Open terminal              |
| `Super+D`      | Open launcher (fuzzel)     |
| `Super+Space`  | Switch keyboard layout     |

### Media / hardware keys

| Shortcut                | Action            |
| ----------------------- | ----------------- |
| `XF86AudioRaiseVolume`  | Volume up         |
| `XF86AudioLowerVolume`  | Volume down       |
| `XF86AudioMute`         | Toggle mute       |
| `XF86MonBrightnessUp`   | Brightness up     |
| `XF86MonBrightnessDown` | Brightness down   |
| `Print`                 | Screenshot (grim) |

### Session

| Shortcut             | Action                 |
| -------------------- | ---------------------- |
| `Super+L`            | Lock screen (swaylock) |
| `Super+Ctrl+Shift+Q` | Exit compositor        |

## Window decorations

**Strategy**: CSD-preferred via `xdg-decoration` protocol. Compositor advertises
only `close` and `fullscreen` capabilities via `xdg-toplevel` — no maximize,
no minimize. GTK/Qt apps hide those buttons automatically.

All CSD and SSD windows get consistent compositor-applied treatment:
- **Corner rounding**: compositor clips windows to `corner_radius` (default 8).
  Overrides client-drawn corners for consistency (some GTK3 apps render square
  or mismatched corners).
- **Shadow**: compositor strips client shadows and renders its own Gaussian drop
  shadow (radius 14, GLSL shader). Consistent shadow appearance across all apps.

### CSD (default)

CSD apps (GTK4, GTK3, most GNOME apps) draw their own title bar with close
button only. Compositor adds corner rounding and shadow on top.

### SSD fallback

XWayland apps and some Qt apps that render with zero decorations get
compositor-drawn decorations:
- 25px title bar with rounded top corners, no title text
- Thin × close button, right-aligned with 8px padding
- Invisible resize borders (8px) around SSD windows for edge/corner resize
- **Double-tap title bar** triggers fit-window (maximize/restore)

### Borderless

Window rules can set `decoration = "none"` — client removes its CSD via
`xdg-decoration`, compositor draws nothing. No corner rounding, no shadow.
Truly borderless. Used for widgets and special windows.

### Interaction and config

- **Interaction**: click title bar to drag, click × to close, drag borders to
  resize, hover × changes cursor to pointer.
- **Window rules**: `decoration` field controls mode — `"client"` (default,
  CSD), `"server"` (force SSD), `"none"` (borderless).
- **Configuration**: `bg_color`, `fg_color`, and `corner_radius` are
  configurable in `[decorations]`. Dimensions and shadow parameters are
  hardcoded.
- **Snapping**: window snapping accounts for SSD title bar boundaries.

## Focus model

Two modes, configured via `focus_follows_mouse` (default: `false`):

**Click-to-focus (default).** Clicking or gesture-interacting with a window
focuses and raises it. Avoids accidental focus changes when panning over windows.

**Focus-follows-mouse (sloppy focus).** Keyboard focus follows the pointer to
windows without raising them. Moving to empty canvas preserves focus; clicking
empty canvas unfocuses. Widgets and layer surfaces are ignored (click still
focuses them).

Common behavior in both modes:

- Click on window → focus + raise
- 3-finger drag on window → focus + raise (at gesture start)
- 4-finger pan jump → focus + raise target window
- `focus-center` (Mod+X) → focus + raise + center + reset zoom on window under pointer
- During a gesture, keyboard input goes to the focused window (the one being
  dragged, or the previously focused window if gesturing on desktop)

## Window placement

New windows open at the **center of the current viewport** — wherever the user
is looking. Placing at `(0, 0)` would be wrong since the user could be far away
on the canvas.

## Stacking / overlap

Windows can overlap. Click or gesture-interact with a window to raise it.
No minimize. **Fit-window** (`Super+M`) is the maximize analogue — it centers
the viewport on the focused window, resets zoom to 1.0, and resizes the window
to fill the viewport. Toggling again restores the original window size but
leaves zoom unchanged. Fullscreen (`Super+F`) is a separate concept (true
exclusive fullscreen). Hidden windows aren't hidden — they're just somewhere
else on the canvas. Pan to find them.

## Widgets

Widgets are regular windows (layer-shell or xdg-toplevel) managed via window
rules. A `widget = true` rule makes the window pinned (immovable), excluded
from navigation/alt-tab, and always stacked below normal windows.

```toml
[[window_rules]]
app_id = "waybar"
widget = true

[[window_rules]]
app_id = "conky"
widget = true
position = [50, 50]
decoration = "none"
opacity = 0.8
blur = true
```

Window rules match by `app_id` and/or `title` (glob patterns) and can set:
`position`, `size`, `widget`, `decoration` (client/server/none),
`blur`, `opacity`.

Status bar: waybar via layer-shell. Volume/brightness OSD: swayosd.

## Canvas background

The background is part of the canvas — it scrolls with the viewport, not stuck
to the screen. This provides spatial awareness when panning and makes the canvas
feel like a real surface.

### Background modes

1. **Shader** (default): GLSL fragment shader. Compositor passes
   `(cx, cy, z, time, resolution)` as uniforms. Ships with a built-in dot grid
   shader as default. Users can swap to any custom shader — noise, gradients,
   procedural patterns, etc. See `docs/shaders.md` for how to write them.
2. **Tiled image**: user provides a seamless (loopable) texture. Repeats
   infinitely across the canvas. Scales with zoom.

Both modes are infinite by nature.

Shaders are static (no time uniform) — cached and only re-rendered when the
viewport changes (pan/zoom). Zero idle GPU cost.

Config example:

```toml
[background]
shader_path = "~/.config/driftwm/bg.frag"      # omit for built-in dot grid
# tile_path = "~/.config/driftwm/tile.png"     # alternative: tiled image
```

## Configuration

Config file: `~/.config/driftwm/config.toml` (respects `XDG_CONFIG_HOME`).
Validate without starting: `driftwm --check-config`.

Missing fields use built-in defaults. Partial configs merge with defaults —
only specify what you want to change. Use `"none"` to unbind a default binding.

### Autostart

```toml
# Commands to run at startup (after WAYLAND_DISPLAY is set).
# Each entry is passed to sh -c, so full shell syntax works.
autostart = [
    "waybar",
    "swaync",
    "swayosd-server",
]
```

### Environment variables

```toml
# Set before any clients launch. Override toolkit defaults.
[env]
QT_WAYLAND_DISABLE_WINDOWDECORATION = "1"
MOZ_ENABLE_WAYLAND = "1"
```

The compositor also sets `XDG_SESSION_TYPE=wayland`, `XDG_CURRENT_DESKTOP=driftwm`,
`XCURSOR_THEME`, `XCURSOR_SIZE`, and Wayland toolkit hints automatically.

### Trackpad / libinput

The compositor owns the input devices on real hardware, so basic libinput
settings are exposed in config:

```toml
[input.trackpad]
tap_to_click = true        # default: true
tap_and_drag = true        # double-tap-hold = drag. default: true
natural_scroll = true      # default: true
accel_speed = 0.0          # pointer acceleration (-1.0 to 1.0). default: 0.0
```

Trackpad gestures and mouse bindings are fully configurable via context-aware
sections (`on-window`, `on-canvas`, `anywhere`). See `config.example.toml` for
the full default binding set and trigger/action vocabulary.

### Keyboard

```toml
[input.keyboard]
layout = "us"              # XKB layout (e.g., "us,ru" for multi-layout)
variant = ""               # XKB variant (e.g., "dvorak")
options = ""               # XKB options (e.g., "grp:win_space_toggle")
repeat_rate = 25           # keys/sec. default: 25
repeat_delay = 200         # ms before repeat starts. default: 200
layout_independent = true  # match bindings by physical key position across layouts
```

`layout_independent` means keybindings work by physical position regardless of
active keyboard layout — `Super+Q` stays the top-left key even on Cyrillic.

### Scroll / viewport panning

```toml
[input.scroll]
speed = 1.5                # viewport pan speed multiplier. default: 1.5
friction = 0.94            # momentum decay per frame (0.90=snappy, 0.98=floaty). default: 0.94
```

Only affects viewport panning. Scroll events forwarded to windows use raw deltas
(no multiplier, no momentum).

### Cursor

```toml
[cursor]
theme = "Adwaita"          # default: "default"
size = 24                  # default: 24
inactive_opacity = 0.5     # cursor opacity on non-active outputs (0.0–1.0)
```

### Navigation

```toml
[navigation]
animation_speed = 0.3      # camera lerp factor (higher = faster)
nudge_step = 20            # px per nudge-window action
pan_step = 100.0           # px per pan-viewport action

# Canvas anchors: named positions reachable via go-to (Mod+1-4).
# Uses Y-up coordinate system. Default: [[0, 0]] (home only).
anchors = [[0, 0], [-1750, 1750], [1750, 1750], [1750, -1750], [-1750, -1750]]
```

### Zoom

```toml
[zoom]
step = 1.1                 # multiplier per keypress (1.1 = 10% per press)
fit_padding = 100.0        # canvas px padding for zoom-to-fit
```

### Effects

```toml
[effects]
blur_radius = 2            # number of Kawase down+up passes (default: 2)
blur_strength = 1.1        # per-pass texel spread (default: 1.1)
```

Window blur is enabled per-window via window rules (`blur = true`). Combined
with `opacity < 1.0`, this gives frosted-glass terminals and widgets. The blur
uses a multi-pass Kawase algorithm with separate down/up sample shaders and a
mask shader for the window shape.

### Output

```toml
[output]
scale = 1.0                # default scale for all outputs

[output.outline]
color = "#ffffff"           # outline color for other monitors' viewports on canvas
thickness = 1              # pixels (0 to disable)
opacity = 0.5              # 0.0–1.0
```

The output outline renders a rectangle on the canvas showing where other
monitors' viewports are looking — spatial awareness for multi-monitor setups.

## Launcher

Not built into the compositor. `Super+D` runs whatever command is configured
(default: `fuzzel`). Users can swap to wofi, tofi, bemenu-run, etc.

```toml
[keybindings]
"mod+d" = "exec fuzzel"
```

## Ecosystem tools

All external — compositor delegates to standard Wayland tools.

| Tool           | Purpose                              |
| -------------- | ------------------------------------ |
| `waybar`       | Status bar (coords/zoom, clock, kbd) |
| `swaync`       | Quick settings + notifications       |
| `swayosd`      | Volume/brightness OSD                |
| `fuzzel`       | App launcher                         |
| `crystal-dock` | Dock / taskbar                       |
| `swaylock`     | Lock screen (`ext-session-lock`)     |

Waybar modules: canvas x,y,z from driftwm, clock/date, keyboard layout,
swaync integration, logout menu.

## Theming / integration

The compositor inherits the desktop theme automatically:

- **GTK theme**: apps read from `gsettings` / dconf (persists from GNOME config)
- **Icons**: same, via `gsettings`
- **Cursor**: set via `[cursor]` config (compositor also exports `XCURSOR_THEME`/`XCURSOR_SIZE` to child processes)
- **Fonts**: system fontconfig, no compositor involvement

## Dev workflow

### Nested Wayland (primary method)

Wayland compositors can run inside an existing Wayland session as a window.
smithay provides two backends:

- **winit backend**: runs compositor as a regular window on your current desktop.
  Perfect for development. No VM needed.
- **udev/libinput backend**: takes over real hardware (DRM/KMS). For production.

Development loop:

```bash
# From your GNOME Wayland session:
cargo run                        # opens driftwm as a window on your desktop
# Inside that window, apps think they're on a real compositor
WAYLAND_DISPLAY=wayland-1 foot   # open a terminal inside driftwm
```

### Limitations of nested mode

- Trackpad gestures may be intercepted by the parent compositor (GNOME) before
  reaching your nested instance. Test gesture code on real hardware or in a VM.
- Multi-monitor can't be tested nested — need real hardware or VM with virtual
  displays.

### When you need real hardware testing

```bash
# Switch to a TTY (Ctrl+Alt+F3), log in, run:
cargo run -- --backend udev
# This takes over the GPU directly. Ctrl+Alt+F2 to get back to GNOME.
```

### Logging

Use `RUST_LOG=debug cargo run` for smithay/libinput event traces. Essential for
debugging gesture recognition and input handling.

## Architecture sketch

```
src/
├── main.rs
├── lib.rs
├── canvas.rs
├── focus.rs
├── decorations.rs
├── render.rs
├── snap.rs
├── window_ext.rs
├── shaders/
│   ├── blur_down.glsl
│   ├── blur_mask.glsl
│   ├── blur_up.glsl
│   ├── corner_clip.glsl
│   ├── dot_grid.glsl
│   ├── shadow.glsl
│   └── tile_bg.glsl
├── backend/
│   ├── mod.rs
│   ├── winit.rs
│   └── udev.rs
├── state/
│   ├── mod.rs
│   ├── animation.rs
│   ├── navigation.rs
│   ├── fullscreen.rs
│   └── fit.rs
├── config/
│   ├── mod.rs
│   ├── types.rs
│   ├── parse.rs
│   ├── defaults.rs
│   └── toml.rs
├── input/
│   ├── mod.rs
│   ├── actions.rs
│   ├── pointer.rs
│   └── gestures.rs
├── grabs/
│   ├── mod.rs
│   ├── move_grab.rs
│   ├── resize_grab.rs
│   ├── pan_grab.rs
│   └── navigate_grab.rs
├── handlers/
│   ├── mod.rs
│   ├── compositor.rs
│   ├── xdg_shell.rs
│   ├── xwayland.rs
│   └── layer_shell.rs
└── protocols/
    ├── mod.rs
    ├── foreign_toplevel.rs
    ├── output_management.rs
    ├── screencopy.rs
    ├── image_capture_source.rs
    └── image_copy_capture.rs
```

## Milestones

Ordered to maximize what can be developed in winit (nested) mode before
requiring real hardware (udev/TTY). Milestones 1–8 work entirely in winit.

1. **Window appears** _(done)_
2. **Move and resize** _(done)_
3. **Infinite canvas** _(done)_
4. **Canvas background** _(done)_
5. **Window navigation** _(done)_
6. **Zoom** _(done)_
7. **Layer shell** _(done)_
8. **Config file** _(done)_
9. **udev backend** _(done)_
10. **Trackpad gestures** _(done)_
11. **Window rules** — app_id matching, widget mode, state file, xdg-decoration _(done)_
12. **Decorations** — SSD fallback, title bar, shadows, resize grab zones _(done)_
13. **Multi-monitor** — per-output viewports, input routing, hotplug, output config, wlr-output-management _(done)_
14. **XWayland** — X11 app support via Xwayland, WindowExt trait for polymorphism _(done)_
15. **Blur** — multi-pass Kawase blur, per-window via window rules, opacity support _(done)_
16. **Pinned-to-screen** — `pinned_to_screen` window rule: window stays always on top, position in viewport (screen) coordinates instead of canvas coordinates
17. **Text input / IME** — `text-input` v3, `input-method` v2, `virtual-keyboard` v1 _(done, gated by `wp_security_context_v1`)_. Text-input and input-method still required for CJK input (Chinese/Japanese/Korean) and on-screen keyboards. Input method popup positioning on the canvas.
18. **Input polish** — NumLock-on-startup config option (`[input.keyboard] numlock = true`), virtual pointer protocol (`zwp-virtual-pointer-v1`) for remote desktop tools (wayvnc, GNOME Remote Desktop)
