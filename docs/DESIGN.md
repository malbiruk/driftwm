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
  - `xdg-decoration` — negotiate SSD vs CSD (milestone 6)
  - `wlr-layer-shell` — bars, launchers, notifications (milestone 7)
  - XWayland — run X11 apps (milestone 12)

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

## Trackpad gestures

All gestures use libinput gesture events. Finger count + start location
determines the action.

| Fingers | Type      | Start on  | Action                                             |
| ------- | --------- | --------- | -------------------------------------------------- |
| 2       | pan       | desktop   | Pan viewport (scroll the canvas)                   |
| 2       | pinch     | desktop   | Zoom out (bird's eye) / zoom in (to 1.0)           |
| 3       | pan       | on window | Move that window                                   |
| 3+Super | pan       | on window | Resize that window                                 |
| 3       | pinch     | on window | Toggle fullscreen                                  |
| 4       | pan       | desktop   | Center nearest window in pan direction (see below) |
| 4/5     | pinch-in  | anywhere  | Go home — snap viewport to (0, 0)                  |
| 4/5     | pinch-out | anywhere  | Return to previous position (undo home)            |

Note: 4-finger pinch is a toggle: pinch-in saves current `(cx, cy)` and snaps
to `(0, 0)`. Pinch-out (or second pinch-in -- for compatibility with modifier+mouse) restores the saved position. Lets you peek at home
widgets and jump back.

Note: 4-finger pan "center nearest window": searches from cursor position in
the pan direction, using a search band perpendicular to the direction. Band
width: full viewport height for horizontal, full viewport width for vertical,
interpolated (mid-width to mid-height) for diagonals. Trackpad gives a
continuous direction vector so diagonals work naturally; keyboard `Super+Arrow`
is cardinal-only. Skips the currently focused window — everything else is a
valid target. On match: pan viewport to center the window, focus + raise it,
and warp cursor to the window's center. The cursor warp enables chaining —
repeat the gesture to hop window-to-window across the canvas.
If no window is found in the band, nothing happens.

Note: 2-finger pan/scroll _on a window_ passes through to the app (normal
scrolling). The compositor only captures 2-finger gestures when they start on
empty desktop area. Holding `Super` overrides this — `Super` + 2-finger
pan/pinch on a window controls the viewport instead of the app.

### Gesture conflict resolution

When a gesture starts, the target is determined once and **locked for the
gesture's duration** (even if the surface under the cursor changes mid-gesture).

Priority order for 2-finger gestures:

1. **Resize grab zone** (~5px window border) → resize that window
2. **On window surface** → pass through to app (normal scrolling)
3. **On empty desktop** → viewport pan/zoom

`Super` held → always viewport pan/zoom regardless of what's under the cursor.

3-finger gestures always operate on the window under the cursor at gesture start.
If the gesture starts on empty desktop, it's a no-op.

### Edge auto-pan (moving windows beyond viewport)

When dragging a window (3-finger pan) and the cursor enters a ~20px edge zone,
the viewport auto-scrolls in that direction at a constant rate. This lets users
move windows far across the canvas without releasing and re-panning. Standard
drag-and-drop UX pattern — familiar from every OS.

### Mouse equivalents

For users without a trackpad, or for debugging:

| Trackpad gesture             | Mouse + modifier equivalent                                 |
| ---------------------------- | ----------------------------------------------------------- |
| 2-finger pan (viewport)      | Click-drag on empty canvas, or `Super` + left-drag anywhere |
| 3-finger pan (move win)      | `Super+Shift` + left-drag                                   |
| —                            | `Super+Shift` + right-drag → resize window                  |
| 2-finger pinch (zoom)        | `Super` + scroll wheel                                      |
| 4-finger pan (center)        | `Super+Ctrl` + left-drag                                    |
| 4-finger pinch (home toggle) | `Super` + middle-click                                      |

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
| `Super+Home`  | Toggle home (0, 0) ↔ previous pos  |
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

| Shortcut        | Action                              |
| --------------- | ----------------------------------- |
| `Super+L`       | Lock screen (swaylock)              |
| `Super+Shift+E` | Exit compositor (with confirmation) |

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

Once `wlr-layer-shell` is implemented (milestone 10), users can optionally run
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
button_map = "lrm"         # 1=left, 2=right, 3=middle. default: "lrm"
natural_scroll = true      # default: true
```

### Keyboard

```toml
[input.keyboard]
repeat_rate = 25       # keys per second. default: 25
repeat_delay = 300     # ms before repeat starts. default: 300
```

### Scroll / canvas panning

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

Post layer-shell (milestone 10):

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
   background color. Accept xdg-shell clients. Display a terminal. *(done)*
2. **Move and resize**: drag windows with mouse, resize from edges. Basic
   stacking (click to raise). *(done)*
3. **Infinite canvas**: viewport panning (click-drag, scroll, keyboard),
   scroll momentum with friction decay, xcursor theme loading, compositor-
   rendered cursor. *(done)*
4. **Canvas background**: shader and tiled image rendering with dot grid
   default. Essential spatial feedback for panning on an infinite canvas.
5. **Zoom**: GPU-scaled rendering at different zoom levels. Keyboard and
   mouse-scroll zoom (pinch-to-zoom comes with trackpad gestures).
6. **Decorations**: SSD for apps that need it. Resize grab zones with
   cursor shape changes on hover.
7. **Layer shell**: support waybar, fuzzel, mako, notifications. Unlocks
   proper app launcher and status bar.
8. **Config file**: TOML parsing, user-defined keybindings, input settings.
   Required before daily-driving.
9. **udev backend**: DRM/KMS setup, libinput integration, logind session
   management. The "run on real hardware" milestone.
10. **Trackpad gestures**: wire up libinput gesture events. 2-finger pan,
    3-finger move, pinch to zoom. Gesture state machine with conflict
    resolution. Requires udev backend for direct libinput access.
11. **Multi-monitor**: multiple viewports on same canvas. Independent
    camera/zoom per output. Requires udev backend.
12. **XWayland**: run X11 apps (Firefox, Steam, etc).
13. **Widgets + polish**: ship eww preset, animations, shadows, damage
    tracking optimization.
