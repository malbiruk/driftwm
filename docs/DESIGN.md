# driftwm

A trackpad-first infinite canvas Wayland compositor.

Windows float on an unbounded 2D plane. You pan, zoom, and navigate with
trackpad gestures. No workspaces, no tiling — just drift.

## Tech stack

- **Language**: Rust
- **Compositor library**: [smithay](https://github.com/Smithay/smithay) — handles Wayland protocol, EGL/Vulkan rendering, input via libinput
- **Rendering**: smithay's built-in OpenGL or Vulkan backend
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

  Not yet implemented:
  - `wlr-screencopy` — screenshot support (grim)
  - `ext-image-capture-source` + `ext-image-copy-capture` — newer screenshot/screencast capture (replaces wlr-screencopy, used by xdg-desktop-portal-wlr for OBS/Firefox screen share)
  - `xdg-decoration` — negotiate SSD vs CSD (milestone 12)
  - XWayland — run X11 apps (milestone 13)

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

Multiple monitors = multiple viewports on the same canvas. Each monitor has its
own `(cx, cy)` and `z`. They can look at different parts of the canvas or
overlap. Panning on one monitor moves only that monitor's viewport.

```
Monitor 0: viewport at (0, 0)       Monitor 1: viewport at (3000, 500)
┌──────────────┐                    ┌──────────────┐
│  [terminal]  │                    │   [browser]  │
│        [vim] │                    │              │
└──────────────┘                    └──────────────┘
         ← same infinite canvas →
```

## Input

All input methods — trackpad, mouse, keyboard — feed into the same actions.
Panning is the most frequent action on an infinite canvas, so there are many
ways to do it. All pan methods feed into the momentum system — a quick flick
carries the viewport smoothly until friction stops it.

### Trackpad gestures

Requires libinput (udev backend). Finger count + context determines the action.
Once a gesture starts, the target is **locked for the gesture's duration** (even
if the surface under the cursor changes mid-gesture).

| Fingers | Type         | Context   | Action                             |
| ------- | ------------ | --------- | ---------------------------------- |
| 2       | scroll       | on window | Pass through to app                |
| 2       | scroll       | desktop   | Pan viewport                       |
| 2       | pinch        | on window | Pass through to app                |
| 2       | pinch        | desktop   | Zoom in/out                        |
| 3       | scroll       | anywhere  | Pan viewport (ignores windows)     |
| 3       | dbl-tap+drag | on window | Move window (see below)            |
| 3+Super | drag         | on window | Resize window                      |
| 3       | pinch        | anywhere  | Zoom in/out (ignores windows)      |
| 4       | scroll       | anywhere  | Center nearest window in direction |
| 4       | pinch in     | anywhere  | Zoom-to-fit (overview)             |
| 4       | pinch out    | anywhere  | Toggle home (0,0) ↔ previous       |

**3-finger double-tap-drag**: Tap with three fingers on a window (libinput
generates BTN_MIDDLE via tap-to-click), then immediately start a 3-finger
swipe. The compositor buffers the middle click for 300ms — if a 3-finger swipe
follows, the click is suppressed and the swipe enters move-window mode. If no
swipe follows, the click is flushed to the app as a normal middle-click (paste).
Immediate 3-finger scroll (without a preceding tap) always pans the viewport.

**3-finger+Super resize**: The only trackpad gesture that requires a keyboard
modifier. Needed for trackpads without right-click drag support. Edges
inferred from pointer position in the window (same quadrant logic as mouse).

**4-finger center**: Searches from cursor in the scroll direction for the
nearest window (using a viewport-width search band). Centers it, focuses,
raises, and warps cursor to its center. Repeat to hop window-to-window.

**4-finger pinch**: Pinch-in triggers zoom-to-fit (overview of all windows).
Pinch-out triggers home toggle (snap to origin or return to previous position).
Thresholds: scale < 0.8 for pinch-in, scale > 1.2 for pinch-out.

### Mouse equivalents

| Action           | Mouse input                          |
| ---------------- | ------------------------------------ |
| Pan viewport     | Click-drag on empty canvas           |
| Pan viewport     | `Super` + left-drag (anywhere)       |
| Zoom             | Scroll wheel on empty canvas         |
| Zoom             | `Super` + scroll wheel (anywhere)    |
| Move window      | `Super+Shift` + left-drag            |
| Resize window    | `Super+Shift` + right-drag           |
| Navigate nearest | `Super+Ctrl` + left-drag (natural)   |

**Trackpad vs mouse wheel**: both produce axis events but serve different
purposes. The compositor uses `axis_source` to split them — trackpad scroll
(`Finger`) pans the viewport, mouse wheel (`Wheel`) zooms. This means
scroll-on-canvas does the right thing for each device without extra modifiers.

### Edge auto-pan

When dragging a window to the viewport edge, the viewport auto-pans in that
direction. Speed is depth-proportional — deeper into the zone means faster
panning (quadratic ramp, like a joystick). All 8 directions (corners =
diagonal blend). Stops when cursor leaves the zone or the drag ends.

## Keyboard shortcuts

Minimal set. Defaults below, all configurable via `[keybinds]` table (maps key combo → built-in action or `exec` command). Implementation: data-driven binding lookup from day one, initially populated from defaults, later merged with user config.

### Window management

| Shortcut            | Action                                 |
| ------------------- | -------------------------------------- |
| `Alt-Tab`           | Cycle windows forward (raise+center)   |
| `Alt-Shift-Tab`     | Cycle windows backward                 |
| `Super+Q`           | Close focused window                   |
| `Super+C`           | Center focused window in viewport      |
| `Super+F`           | Toggle fullscreen                      |
| `Super+Shift+Arrow` | Nudge focused window 20px in direction |

### Navigation

| Shortcut      | Action                             |
| ------------- | ---------------------------------- |
| `Super+Arrow` | Center nearest window in direction |
| `Super+A`     | Toggle home (0, 0) ↔ previous pos  |
| `Super+W`     | Zoom-to-fit — show all windows     |

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
| `Super+D`      | Open launcher (bemenu-run) |
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

**Strategy**: CSD-first. Compositor advertises only `close` and `fullscreen`
capabilities via `xdg-toplevel` — no maximize, no minimize. GTK/Qt apps will
hide those buttons automatically.

- **CSD apps** (GTK4, GTK3, most GNOME apps): draw their own title bar with
  close button only. Compositor does nothing.
- **SSD fallback** (XWayland apps, some Qt apps that render with zero
  decorations): compositor draws a minimal title bar + close button. Not needed
  for v1 but eventually required for compatibility.
- **Resize grabs**: invisible border zone (~5px) around every window for resize.
  Cursor changes on hover. Always provided by compositor.
- **Shadows**: compositor renders drop shadows behind each window (blurred
  rect or 9-slice texture). Nice-to-have, not essential for v1.

Negotiate via `xdg-decoration` protocol. Default to CSD, fall back to SSD.

## Focus model

**Click-to-focus.** Clicking or gesture-interacting with a window focuses and
raises it. This avoids accidental focus changes when panning over windows.

- Click on window → focus + raise
- 3-finger drag on window → focus + raise (at gesture start)
- 4-finger pan jump → focus + raise target window
- During a gesture, keyboard input goes to the focused window (the one being
  dragged, or the previously focused window if gesturing on desktop)

## Window placement

New windows open at the **center of the current viewport** — wherever the user
is looking. Placing at `(0, 0)` would be wrong since the user could be far away
on the canvas.

## Stacking / overlap

Windows can overlap. Click or gesture-interact with a window to raise it.
No minimize, no maximize (fullscreen replaces maximize). Hidden windows aren't
hidden — they're just somewhere else on the canvas. Pan to find them.

## Widgets

eww windows as regular `xdg-toplevel` surfaces placed at known canvas
coordinates. They're normal windows — users can move them, they pan with the
viewport. Matched by window rules to stay below normal windows and skip alt-tab.

```toml
[[window_rules]]
app_id = "eww-*"
skip_taskbar = true
always_below = true
```

Convention: widgets live near `(0, 0)`. Use home gesture to peek at them,
repeat to go back.

### Default widget preset (ships with driftwm)

driftwm ships an eww config as a starter kit so the desktop feels complete
out of the box:

```
Canvas around (0, 0):

         (-400, -200)                    (400, -200)
              ┌──────────┐          ┌──────────┐
              │  clock   │          │ battery  │
              │ datetime │          │ wifi/bt  │
              └──────────┘          └──────────┘

                    (0, 0) ← home

         (-400, 100)                     (400, 100)
              ┌──────────┐          ┌──────────┐
              │  cpu /   │          │ volume / │
              │  memory  │          │ kbd lay  │
              └──────────┘          └──────────┘
```

Users can rearrange, remove, or add their own eww widgets. They're just windows.

### Later: waybar via layer-shell

Once `wlr-layer-shell` is implemented (milestone 7), users can optionally run
waybar as a traditional top bar. Not needed for v1.

## Canvas background

The background is part of the canvas — it scrolls with the viewport, not stuck
to the screen. This provides spatial awareness when panning and makes the canvas
feel like a real surface.

### Background modes

1. **Shader** (default): GLSL fragment shader. Compositor passes
   `(cx, cy, z, time, resolution)` as uniforms. Ships with a built-in dot grid
   shader as default. Users can swap to any custom shader — noise, gradients,
   animated patterns, etc.
2. **Tiled image**: user provides a seamless (loopable) texture. Repeats
   infinitely across the canvas. Scales with zoom.

Both modes are infinite by nature.

Static shaders (no `time` dependency) are cached and only re-rendered when the
viewport changes (pan/zoom). Zero idle GPU cost. Animated shaders force a
redraw every frame — opt-in via `animate = true`.

Config example:

```toml
[background]
mode = "shader"                                # or "tile"
shader_path = "~/.config/driftwm/bg.frag"      # omit for built-in dot grid
bg_color = "#1e1e2e"                           # passed as uniform
animate = false                                # true: redraw every frame (for time-based shaders)

# mode = "tile"
# tile_path = "~/.config/driftwm/tile.png"
```

## Configuration

Config file: `~/.config/driftwm/config.toml` (respects `XDG_CONFIG_HOME`).

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

Trackpad gesture mappings are **not configurable**. The gesture state machine
encodes context-dependent logic (finger count × gesture type × hit-testing ×
thresholds) that doesn't reduce to a simple config table. The gestures are the
product's opinionated core UX. Libinput device settings (tap-to-click, natural
scroll, accel) remain configurable since they affect low-level input behavior,
not gesture semantics.

### Keyboard

```toml
[input.keyboard]
repeat_rate = 25       # keys per second. default: 25
repeat_delay = 300     # ms before repeat starts. default: 300
```

### Scroll / viewport panning

```toml
[input.scroll]
canvas_speed = 1.5     # multiplier for viewport pan deltas. default: 1.5
friction = 0.96        # momentum decay per frame (0.90 = snappy, 0.98 = floaty). default: 0.96
```

Only affects viewport panning. Scroll events forwarded to windows use raw deltas
(no multiplier, no momentum).

### Cursor

```toml
[cursor]
theme = "Adwaita"      # default: "default"
size = 24              # default: 24
```

## Launcher

Not built into the compositor. `Super+D` runs whatever command is configured.
Default: `bemenu-run` (works as a regular Wayland window, no layer-shell needed).
Users can swap to fuzzel, wofi, tofi, etc. once layer-shell is implemented.

```toml
[commands]
terminal = "foot"
launcher = "bemenu-run"
```

## Ecosystem tools

v1 (no layer-shell needed):

| Tool       | Purpose                          |
| ---------- | -------------------------------- |
| `eww`      | Canvas widgets (regular windows) |
| `grim`     | Screenshot                       |
| `swaylock` | Lock screen                      |

Post layer-shell (milestone 7):

| Tool     | Purpose                    |
| -------- | -------------------------- |
| `fuzzel` | App launcher (alternative) |
| `waybar` | Traditional status bar     |
| `mako`   | Notifications              |

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
├── main.rs              # entry point, backend selection
├── state.rs             # compositor state (canvas, viewports, window list)
├── canvas.rs            # viewport math, coordinate transforms, zoom
├── input/
│   ├── mod.rs
│   ├── gestures.rs      # trackpad gesture state machine
│   ├── keyboard.rs      # keybinds
│   └── mouse.rs         # mouse fallbacks
├── window/
│   ├── mod.rs
│   ├── decorations.rs   # SSD rendering (title bar, resize grabs)
│   └── stacking.rs      # z-order, raise/lower
├── shell/
│   ├── xdg.rs           # xdg-shell implementation
│   ├── layer.rs         # wlr-layer-shell (bars, launchers)
│   └── xwayland.rs      # X11 app support
├── output.rs            # multi-monitor / viewport management
└── render.rs            # frame rendering, damage tracking, zoom scaling
```

## Milestones

Ordered to maximize what can be developed in winit (nested) mode before
requiring real hardware (udev/TTY). Milestones 1–8 work entirely in winit.

1. **Window appears**: smithay winit backend, open a window, render a solid
   background color. Accept xdg-shell clients. Display a terminal. _(done)_
2. **Move and resize**: drag windows with mouse, resize from edges. Basic
   stacking (click to raise). _(done)_
3. **Infinite canvas**: viewport panning (click-drag, scroll, keyboard),
   scroll momentum with friction decay, xcursor theme loading, compositor-
   rendered cursor. _(done)_
4. **Canvas background**: shader and tiled image rendering with dot grid
   default. Essential spatial feedback for panning on an infinite canvas.
5. **Window navigation**: `Super+C` center focused window, `Super+Arrow`
   center nearest window in direction, `Alt-Tab` cycle windows (raise +
   center). Pure camera math — makes the canvas usable for daily work.
6. **Zoom**: GPU-scaled rendering at different zoom levels. Keyboard and
   mouse-scroll zoom (pinch-to-zoom comes with trackpad gestures).
7. **Layer shell**: support waybar, fuzzel, mako, notifications. Unlocks
   proper app launcher and status bar.
8. **Config file**: TOML parsing, user-defined keybindings, input settings.
   Required before daily-driving.
9. **udev backend**: DRM/KMS setup, libinput integration, logind session
   management. The "run on real hardware" milestone.
10. **Trackpad gestures**: wire up libinput gesture events. 3-finger pan
    (viewport), 3-finger double-tap-drag (move window), pinch to zoom.
    Gesture state machine with conflict resolution. Requires udev backend.
11. **Multi-monitor**: multiple viewports on same canvas. Independent
    camera/zoom per output. Requires udev backend.
12. **Decorations**: SSD for apps that need it. Resize grab zones with
    cursor shape changes on hover. Mainly needed for XWayland/legacy Qt.
13. **XWayland**: run X11 apps (Firefox, Steam, etc).
14. **Widgets + polish**: ship eww preset, animations, shadows, damage
    tracking optimization.
