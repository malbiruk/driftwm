# Configuration

<!-- Generated from config.reference.toml — do not edit by hand.
     Regenerate with: UPDATE_CONFIG_DOCS=1 cargo test docs_config_md_is_up_to_date -->

driftwm reads its configuration from `~/.config/driftwm/config.toml` (respecting
`XDG_CONFIG_HOME`). Every field is optional — anything you omit uses the built-in
default shown below. Copy [`config.reference.toml`](../config.reference.toml) to
get started, then uncomment and edit only the lines you want to change. Validate a
config with `driftwm --check-config`.

## General

### `mod_key`

Default: `"super"`

Window manager modifier: "super", "alt", or "mod3" (remap a key onto mod3 in your keymap first).

### `focus_follows_mouse`

Default: `false`

Keyboard focus follows the pointer onto windows, also when a pan or zoom slides a window under a resting cursor; navigation actions keep the focus they set. Empty canvas keeps focus; click it to unfocus.

### `focus_placement`

Default: `"center"`

Where navigation parks a window within the usable area (the screen minus panels): "center", "top", "bottom", "left", "right", "top-left", "top-right", "bottom-left" or "bottom-right". An edge or corner keeps the window snap.outer_gap in from that edge; a window too big for an axis centers on that axis.

center-window, focus-center and center-nearest always park the window here (a filled window returns to the view it was filled in instead). Alt-Tab, activation, `msg focus` and the focus that follows a close park it only when it isn't fully on screen. focus-nearest, for a window not fully on screen, scrolls just far enough to show it when it is in the focused window's snap cluster or overlaps it, and parks it otherwise; a click under auto_navigate_on_click only scrolls it into view. Overridable per monitor (see Outputs).

### `window_placement`

Default: `"auto"`

Where a new window opens when no window rule positions it:

- `"auto"` — docked beside the focused window, or the nearest one in view; falls back to "center" when nothing fits or after you clear focus by clicking empty canvas
- `"center"` — at the focus_placement point; the camera follows it
- `"cursor"` — centered on the cursor, kept inside the usable area; the camera stays put unless zoom.reset_on_new_window zooms it

### `autostart`

Default: `[]`

Commands run at startup, each through sh -c.

A systemd session (driftwm-session or a display manager) also starts XDG autostart entries. To skip one, copy its .desktop file into ~/.config/autostart and add Hidden=true. To skip them all (needs root, and applies to every user and desktop on the machine):

```text
sudo mkdir -p /etc/systemd/user-generators
sudo ln -s /dev/null /etc/systemd/user-generators/systemd-xdg-autostart-generator
systemctl --user daemon-reload
```

**Example:**

```toml
autostart = ["waybar", "swaync"]
```

## `[session]`

Your canvas, saved to ~/.local/state/driftwm/session.json. Suspended windows are always saved; these flags cover the rest. See docs/session.md.

### `suspend_on_close`

Default: `false`

Suspend instead of close when the app closes a window (title bar ×, in-app quit). close-window, `msg close` and taskbar closes still close. Per window: the `suspend_on_close` rule.

### `restore_windows`

Default: `true`

Bring windows still open at logout back as suspended windows on the next launch (they need a .desktop entry; nothing auto-launches). Per window: the `restore_windows` rule.

### `restore_camera`

Default: `true`

Restore each output's camera and zoom on the next launch.

### `restore_bookmarks`

Default: `true`

Keep bookmarks changed at runtime (set-bookmark, `msg bookmark`) across restarts.

## `[env]`

Environment variables set before any client starts; autostart and exec inherit them. They override driftwm's own toolkit defaults (MOZ_ENABLE_WAYLAND, QT_QPA_PLATFORM, SDL_VIDEODRIVER, GDK_BACKEND, ELECTRON_OZONE_PLATFORM_HINT).

**Example:**

```toml
QT_WAYLAND_DISABLE_WINDOWDECORATION = "1"
MOZ_ENABLE_WAYLAND = "1"
```

## `[input.keyboard]`

### `layout`

Default: `"us"`

XKB layout (e.g., "us,ru" for multi-layout)

### `remember_layout_per_window`

Default: `false`

for multi-layout setups

### `variant`

Default: `""`

XKB variant (e.g., "dvorak", or "," for two defaults)

### `options`

Default: `""`

XKB options (e.g., "grp:win_space_toggle"); grp:alt_shift_toggle / grp:ctrl_shift_toggle break alt+shift / ctrl+shift bindings, so use a tap binding (see [keybindings]) instead

### `model`

Default: `""`

XKB model (e.g., "pc105")

### `repeat_rate`

Default: `25`

keys/sec

### `repeat_delay`

Default: `200`

ms before repeat starts

### `layout_independent`

Default: `true`

match bindings by physical key across layouts

### `num_lock`

Default: `true`

num lock state on startup

### `caps_lock`

Default: `false`

caps lock state on startup

## `[input.trackpad]`

### `tap_to_click`

Default: `true`

enable tap-to-click

### `natural_scroll`

Default: `true`

content follows the fingers

### `tap_and_drag`

Default: `true`

double-tap-hold to drag

### `accel_speed`

Default: `0.0`

pointer acceleration (-1.0 to 1.0)

### `accel_profile`

Default: `"adaptive"`

"flat" or "adaptive"

### `click_method`

Default: `"none"`

"none" (device default), "clickfinger" (finger count picks the button) or "button_areas" (where you press picks the button)

### `disable_while_typing`

Default: `true`

palm rejection

### `enable`

Default: `true`

false turns the trackpad off; set-trackpad overrides it until a restart or a config edit that changes it

### `disable_on_external_mouse`

Default: `false`

off while an external mouse is connected

## `[input.mouse]`

### `accel_speed`

Default: `0.0`

pointer acceleration (-1.0 to 1.0)

### `accel_profile`

Default: `"flat"`

"flat" or "adaptive"

### `natural_scroll`

Default: `false`

reverse scroll direction

### `left_handed`

Default: `false`

swap left/right buttons

## `[input.touch]`

### `enable`

Default: `true`

Touchscreen hardware. Gesture thresholds and bindings live in [touch].

enable touchscreen support

### `map_to_output`

Default: `"none"`

connector name; "none" auto-detects. Set it only to tell two same-size touchscreens apart.

**Example:**

```toml
map_to_output = "DP-2"
```

## `[input.tablet]`

### `map_to_output`

Default: `"none"`

Output this tablet maps to; "none" auto-detects (libinput tags, physical size, else the internal panel).

## `[cursor]`

### `theme`

Default: `"none"`

XCURSOR_THEME; "none" = inherit from environment (e.g. "Adwaita")

### `size`

Default: `0`

XCURSOR_SIZE; 0 = inherit from environment (e.g. 24)

### `inactive_opacity`

Default: `0.5`

cursor opacity on non-active outputs (0.0–1.0)

## `[navigation]`

### `trackpad_speed`

Default: `1.5`

trackpad (scroll/gestures) pan multiplier (1.0 = direct, 0 = off)

### `mouse_speed`

Default: `1.0`

mouse (drag) pan multiplier (1.0 = direct, 0 = off)

### `touch_speed`

Default: `1.0`

touchscreen gesture pan multiplier (1.0 = direct, 0 = off)

### `drift`

Default: `0.5`

momentum coast: 0 = off, 0.5 = default, 1 = floatiest

### `camera_speed`

Default: `0.3`

camera lerp factor, 0-1 (higher = faster, 1 = instant)

### `auto_navigate_on_close`

Default: `true`

on close, pan to the newly focused window if off-screen false = camera stays put; focus only moves to a visible window

### `auto_navigate_on_click`

Default: `false`

completed click on a partially off-screen window also pans it in fully visible → focus only

### `nudge_step`

Default: `20`

px per nudge-window action (mod-shift-arrow by default)

### `resize_step`

Default: `20`

px per grow-window/shrink-window action (both unbound by default; does not affect drag-resize)

### `pan_step`

Default: `100.0`

px per pan-viewport action (mod-ctrl-arrow by default)

### `workspace_source`

Default: `"bookmarks"`

What ext-workspace pills represent. "bookmarks" keeps one pill per named bookmark and mod+1..4 jump to bookmarks 1..4. "zones" exports one pill per connected snap cluster (with a synthetic pill on an empty canvas), and mod+1..4 frame zones 1..4.

### `anchors`

Default: `[]`

Anchors: canvas points discoverable by focus-nearest / center-nearest (4-finger swipe / Mod+Arrow) even when no window is there. Uses Y-up.

**Example: origin + 4 corners**

```toml
anchors = [[0, 0], [-1750, 1750], [1750, 1750], [1750, -1750], [-1750, -1750]]
```

## `[navigation.bookmarks]`

Named canvas points (Y-up) for go-to-bookmark, set-bookmark, move-to-bookmark and `msg bookmark`. These seed the registry at startup; an empty table drops the default seeds. A bookmark stores a position, not a zoom.

| Name | Position |
| --- | --- |
| `1` | `[-1750, 1750]` |
| `2` | `[1750, 1750]` |
| `3` | `[1750, -1750]` |
| `4` | `[-1750, -1750]` |

## `[navigation.edge_pan]`

### `zone`

Default: `100.0`

activation zone width (px from viewport edge)

### `speed_min`

Default: `4.0`

px/frame at zone boundary

### `speed_max`

Default: `10.0`

px/frame at viewport edge

### `cursor_pan`

Default: `false`

also pan when the bare cursor touches an edge

### `cursor_zone`

Default: `20.0`

bare-cursor zone (px); pans at speed_max

### `latency_ms`

Default: `120`

delay before panning at an edge shared with another monitor (0 = none)

## `[zoom]`

### `step`

Default: `1.1`

multiplier per keypress (1.1 = 10% per press)

### `trackpad_speed`

Default: `1.0`

trackpad pinch-zoom multiplier (1.0 = direct, 0 = off)

### `mouse_speed`

Default: `1.0`

mouse-wheel zoom multiplier (1.0 = direct, 0 = off)

### `touch_speed`

Default: `1.0`

touchscreen zoom multiplier (1.0 = direct, 0 = off)

### `fit_padding`

Default: `80.0`

screen px kept around zoom-to-fit

### `reset_on_new_window`

Default: `false`

zoom to 1.0 when a new window opens

### `reset_on_activation`

Default: `true`

zoom to 1.0 when an off-screen window asks for focus

### `interact_min`

Default: `0.5`

zoom below which canvas windows take no pointer input: a left click navigates to the window, a drag moves it. 0 disables. The zoom floor is half of zoom-to-fit, so a low value may never be reached.

## `[snap]`

### `enabled`

Default: `true`

snap windows together while dragging

### `gap`

Default: `12.0`

canvas px between windows: snapping, auto placement, fill-window, and which windows form a snap cluster

### `outer_gap`

Default: `12.0`

canvas px from the usable area's edge, for fill-window, focus_placement, focus-nearest and resize-to-snap (fit-window is edge to edge)

### `distance`

Default: `24.0`

screen px from an edge where snapping starts

### `break_force`

Default: `32.0`

screen px past a snap to break free

### `corners`

Default: `true`

also line up parallel edges (e.g. top to top)

### `centers`

Default: `false`

also line up midpoints

## `[decorations]`

### `bg_color`

Default: `"#ffffff"`

title bar background

### `fg_color`

Default: `"#333333"`

title text + close button × color

### `corner_radius`

Default: `10`

rounded corners; off while the window covers the usable area

### `shadow`

Default: `true`

drop shadow; off while the window covers the usable area

### `title_bar_height`

Default: `25`

driftwm's own title bars (server decorations). The font comes from fontconfig; install `adwaita-fonts` for the default look.

title bar height in px

### `font`

Default: `"Adwaita Sans"`

title text font family

### `font_size`

Default: `11`

title text size in points

### `font_weight`

Default: `"medium"`

thin/extralight/light/regular/medium/ semibold/bold/extrabold/black

### `title_align`

Default: `"center"`

"left" or "center" (long titles left-align)

### `default_mode`

Default: `"client"`

Decoration mode for windows without a rule:

- `"client"` — the app draws its own title bar (CSD)
- `"minimal"` — no title bar; driftwm's shadow, corners and border
- `"none"` — no chrome at all, per-window chrome rules included

"server" (driftwm's title bar) is per window only, via [[window_rules]]: many toolkits draw their own anyway, which would double the title bar.

### `border_width`

Default: `0`

Borders apply in every mode but "none".

px; 0 = no border

### `border_color`

Default: `"#333333"`

unfocused border

### `border_color_focused`

Default: `"#333333"`

focused border; set it apart from border_color for a focus indicator

### `opacity`

Default: `1.0`

Focus-dependent opacity for windows with no `opacity` rule or `msg opacity` of their own. Fullscreen windows and widgets stay opaque. Under an animated [background], each translucent window costs a full recomposite per frame.

unfocused windows

### `opacity_focused`

Default: `1.0`

focused windows

### `blur`

Default: `false`

Frost the backdrop of every window, as the `blur` rule does for one. Fullscreen windows and layer surfaces are left alone.

**Example: dim + frost**

```toml
opacity = 0.85
blur = true
```

## `[effects]`

### `blur_radius`

Default: `5`

number of Kawase down+up passes

### `blur_strength`

Default: `1.5`

per-pass texel spread

### `animate_blur_fps`

Default: `20`

cap (0-144) on re-sampling a background that moves on its own (u_time shaders, wallpaper daemons); 0 freezes it. Pans and zooms always refresh.

### `animation_speed`

Default: `0.5`

window animation lerp factor (1 = instant)

### `animation_scale`

Default: `0.95`

open/close grow/shrink amplitude (1 = fade only)

## `[background]`

### `type`

Default: `"default"`

"default" (the dot grid), "shader", "tile", "wallpaper" or "none". For shader, tile and wallpaper, `path` is the source.

**Example:**

```toml
type = "shader"  # procedural GLSL (scrolls with canvas)
path = "/usr/local/share/driftwm/wallpapers/animated/fast_smoke.glsl"

type = "tile"  # image tiled across the canvas
path = "~/Pictures/Wallpapers/tile.png"

type = "wallpaper"  # single image fixed to viewport (does not scroll/zoom)
path = "~/Pictures/Wallpapers/wallpaper.jpg"

type = "none"  # render no built-in background (path ignored), so a
               # wlr-layer-shell wallpaper daemon (swaybg, swww, mpvpaper for live video)
               # is the wallpaper. Launch it yourself; see docs/shaders.md.
```

A shader can also sample an image through `texture`. See docs/shaders.md.

**Example: shader sampling an image**

```toml
type = "shader"
path = "/usr/local/share/driftwm/wallpapers/textured/ripple.glsl"
texture = "~/Pictures/Wallpapers/photo.jpg"
```

### `mirror_tile`

Default: `false`

Tile mode: mirror the image in a 2×2 reflected block, so an image that isn't seamless tiles without seams. Gigapixel TIFFs aren't mirrored.

### `cache_shader`

Default: `false`

Render a static shader once and pan the result. Animated (u_time) and zoom-dependent (u_zoom) shaders always render live.

### `transparent_shader`

Default: `false`

Honor a shader's alpha, so transparent pixels show a layer-shell wallpaper underneath. Turns cache_shader off. See docs/shaders.md.

### `cache_budget_mb`

Default: `128`

Memory (MB) shared by cache_shader and gigapixel TIFF wallpapers. Lower it on a low-memory machine; too low only makes the background blurrier.

### `animate_fps`

Default: `0`

Frame-rate cap (0-1000) for animated (u_time) shaders; 0 = every frame. Lowering it cuts the background's GPU cost.

## `[bindings]`

### `disable_defaults`

Default: `[]`

Drop a whole category of built-in bindings ("keys", "mouse", "gestures", "touch") and keep only your own entries for it. To drop one binding, set it to "none" instead.

**Example: bring your own keyboard scheme, keep mouse + gesture + touch defaults**

```toml
disable_defaults = ["keys"]
```

## `[keybindings]`

Keyboard bindings: "Modifier+...+Keysym" = "action [arg]", merged with the defaults; "none" unbinds one. "mod" is mod_key; literal modifiers: alt, super (logo), ctrl (control), shift, mod3. Keysyms are XKB names, any case. A bare modifier chord ("alt+shift") is a tap binding, fired on release.

Actions:

- `exec <cmd>` — launch an app (loading cursor; exits fullscreen)
- `exec-terminal` — launch $TERMINAL, else the first terminal found
- `exec-launcher` — launch $LAUNCHER, else the first launcher found
- `spawn <cmd>` — run a command (no loading cursor; keeps fullscreen)
- `close-window` — close the focused window
- `suspend-window` — close the focused window, leaving a suspended stand-in (needs a .desktop entry); on a stand-in, dismiss it
- `nudge-window <dir>` — move the focused window by nudge_step px
- `move-to-snap <dir>` — move the focused window to the next snap stop that way, never onto another window
- `grow-window <dir>` — push the focused window's `<dir>` edge out by resize_step px
- `shrink-window <dir>` — pull the focused window's `<dir>` edge in by resize_step px
- `resize-to-snap <dir>` — right/down grow the right/bottom edge to the next snap stop, never over a window; left/up pull it back
- `pan-viewport <dir>` — pan by pan_step px
- `center-window` — park the focused window at focus_placement, zoom reset (a filled window returns to its fill view)
- `focus-center` — focus the window under the pointer and park it; a pin is focused in place; with none there, center-window
- `center-nearest <dir>` — park the nearest window that way at focus_placement
- `focus-nearest <dir>` — focus the nearest window that way (see focus_placement)
- `cycle-windows forward` — Alt-Tab style window cycling
- `cycle-windows backward` — reverse cycle
- `home-toggle` — jump to the origin and back
- `zoom-in` — step zoom in
- `zoom-out` — step zoom out
- `zoom-reset` — zoom to 1.0
- `go-to-bookmark <name>` — jump the camera to a saved bookmark
- `go-to-workspace <n>` — frame zone <n> in zones mode; in bookmarks mode, jump to bookmark "<n>" (alias: go-to-zone)
- `set-bookmark <name>` — save the current camera center as a bookmark (create or overwrite)
- `move-to-bookmark <name>` — move the focused window's center to a bookmark point
- `zoom-to-fit` — fit all windows in viewport
- `zoom-to-fit-snapped` — fit only the focused window's snap cluster
- `toggle-fullscreen` — toggle focused window fullscreen
- `fit-window` — toggle maximize: centers + resets zoom + fills viewport; restore only resizes back
- `fit-window-snapped` — fit-window for the focused window's whole snap cluster
- `fill-window` — grow in place to fill free space, up to snap.outer_gap from the usable area's edge and snap.gap from a neighbour; edges past those pull back; clears maximize; press again to restore
- `toggle-pin-to-screen` — pin/unpin the focused window to the screen (ignores pan/zoom, floats above)
- `reload-config` — hot-reload config file
- `toggle-cursor-pan` — toggle cursor edge-pan (see [navigation.edge_pan])
- `set-trackpad on|off|toggle` — enable/disable every trackpad for the rest of the session (see [input.trackpad] enable)
- `quit` — exit the compositor
- `send-to-output <dir>` — move the focused window to the adjacent output
- `send-cursor-to-output <dir>` — move the cursor to the adjacent output
- `switch-layout <target>` — keyboard layout: next, prev, or a 0-based index
- `none` — unbind this key combo

Directions: up, down, left, right, up-left, up-right, down-left, down-right

| Binding | Action | Notes |
| --- | --- | --- |
| `"mod+return"` | `exec-terminal` | $TERMINAL, else first of: foot, alacritty, ptyxis, kitty, wezterm, gnome-terminal, konsole |
| `"mod+d"` | `exec-launcher` | $LAUNCHER, else first of: fuzzel, wofi, rofi, bemenu-run, wmenu-run, tofi-drun, mew-run |
| `"mod+q"` | `close-window` |  |
| `"mod+shift+q"` | `suspend-window` |  |
| `"mod+f"` | `toggle-fullscreen` |  |
| `"mod+m"` | `fit-window-snapped` |  |
| `"mod+shift+m"` | `fit-window` |  |
| `"mod+t"` | `toggle-pin-to-screen` |  |
| `"mod+c"` | `center-window` |  |
| `"mod+x"` | `focus-center` |  |
| `"mod+a"` | `home-toggle` |  |
| `"mod+up"` | `focus-nearest up` |  |
| `"mod+down"` | `focus-nearest down` |  |
| `"mod+left"` | `focus-nearest left` |  |
| `"mod+right"` | `focus-nearest right` |  |
| `"mod+shift+up"` | `move-to-snap up` |  |
| `"mod+shift+down"` | `move-to-snap down` |  |
| `"mod+shift+left"` | `move-to-snap left` |  |
| `"mod+shift+right"` | `move-to-snap right` |  |
| `"mod+ctrl+up"` | `pan-viewport up` |  |
| `"mod+ctrl+down"` | `pan-viewport down` |  |
| `"mod+ctrl+left"` | `pan-viewport left` |  |
| `"mod+ctrl+right"` | `pan-viewport right` |  |
| `"alt+tab"` | `cycle-windows forward` |  |
| `"alt+shift+tab"` | `cycle-windows backward` |  |
| `"mod+equal"` | `zoom-in` |  |
| `"mod+minus"` | `zoom-out` |  |
| `"mod+0"` | `zoom-reset` |  |
| `"mod+w"` | `zoom-to-fit` |  |
| `"mod+shift+w"` | `zoom-to-fit-snapped` |  |
| `"mod+1"` | `go-to-workspace 1` | zones: frame zone 1; bookmarks: jump to bookmark 1 (top-left by default) |
| `"mod+2"` | `go-to-workspace 2` | zones: frame zone 2; bookmarks: jump to bookmark 2 (top-right) |
| `"mod+3"` | `go-to-workspace 3` | zones: frame zone 3; bookmarks: jump to bookmark 3 (bottom-right) |
| `"mod+4"` | `go-to-workspace 4` | zones: frame zone 4; bookmarks: jump to bookmark 4 (bottom-left) |
| `"mod+shift+1"` | `set-bookmark 1` | overwrite bookmark 1 at the current view |
| `"mod+shift+2"` | `set-bookmark 2` |  |
| `"mod+shift+3"` | `set-bookmark 3` |  |
| `"mod+shift+4"` | `set-bookmark 4` |  |
| `"mod+alt+up"` | `send-to-output up` | move window to output above |
| `"mod+alt+down"` | `send-to-output down` |  |
| `"mod+alt+left"` | `send-to-output left` |  |
| `"mod+alt+right"` | `send-to-output right` |  |
| `"mod+l"` | `spawn swaylock -f -c 000000 -kl` |  |
| `"mod+ctrl+shift+q"` | `quit` |  |
| `"XF86AudioRaiseVolume"` | `spawn wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+` |  |
| `"XF86AudioLowerVolume"` | `spawn wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-` |  |
| `"XF86AudioMute"` | `spawn wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle` |  |
| `"XF86MonBrightnessUp"` | `spawn brightnessctl set +5%` |  |
| `"XF86MonBrightnessDown"` | `spawn brightnessctl set 5%-` |  |
| `"XF86AudioPlay"` | `spawn playerctl play-pause` |  |
| `"XF86AudioPause"` | `spawn playerctl play-pause` |  |
| `"XF86AudioNext"` | `spawn playerctl next` |  |
| `"XF86AudioPrev"` | `spawn playerctl previous` |  |
| `"XF86AudioStop"` | `spawn playerctl stop` |  |
| `"Print"` | `spawn grim - \| wl-copy` |  |
| `"shift+Print"` | `spawn grim -g \"$(slurp -d)\" - \| wl-copy` |  |

**Example: built-in window capture (isolated window + shadow) to clipboard**

```toml
"ctrl+Print" = "spawn driftwm msg screenshot window -o - | wl-copy"
```

**Example: center-nearest, which parks the target even when it's on screen**

```toml
"mod+alt+shift+up" = "center-nearest up"
```

**Example: tap binding, a bare modifier chord fired on release**

```toml
"alt+shift" = "switch-layout next"
```

**Example: fill-window on a key (mod+middle runs it by default)**

```toml
"mod+g" = "fill-window"
```

**Example: set-trackpad (unbound by default)**

```toml
"mod+f10" = "set-trackpad toggle"   # flip the trackpad between on and off
"mod+shift+f10" = "set-trackpad off"   # turn it off whatever the current state
```

**Example: grow and shrink the right edge by resize_step px (unbound by default)**

```toml
"mod+ctrl+alt+right" = "grow-window right"
"mod+ctrl+alt+left" = "shrink-window right"
```

**Example: resize-to-snap (unbound by default)**

```toml
"mod+ctrl+shift+up" = "resize-to-snap up"
"mod+ctrl+shift+down" = "resize-to-snap down"
"mod+ctrl+shift+left" = "resize-to-snap left"
"mod+ctrl+shift+right" = "resize-to-snap right"
```

**Example: nudge-window by nudge_step px (unbound by default)**

```toml
"mod+alt+shift+right" = "nudge-window right"
```

## `[mouse]`

### `resize_on_border`

Default: `true`

Drag a window's edge or corner to resize it. false leaves resizing to bindings; on-window bindings still apply over the border either way.

### `decoration_resize_snapped`

Default: `true`

A border drag carries the window's snap cluster along, as resize-window-snapped does. Bindings follow their own action (resize-window or resize-window-snapped).

### `decoration_fit_snapped`

Default: `true`

A maximize from the window (its maximize button or a title-bar double-click), the app or a taskbar carries the window's snap cluster along, as fit-window-snapped does. Bindings follow their own action (fit-window or fit-window-snapped).

Mouse bindings: "Modifier+...+Trigger" = "action", merged with the defaults; "none" unbinds one. Contexts: on-window, on-canvas, and anywhere as the fallback. Triggers: left, right, middle, trackpad-scroll, wheel-scroll, and wheel-up / wheel-down (once per notch).

Mouse actions:

- `move-window` — drag the window
- `move-snapped-windows` — drag the window and its snap cluster
- `resize-window` — drag-resize the window
- `resize-window-snapped` — drag-resize, carrying the snap cluster
- `pan-viewport` — drag or scroll to pan the camera
- `zoom` — drag or scroll to zoom
- `center-nearest` — park the nearest window in the drag's pan sense (drag right looks left)
- `focus-nearest` — focus-nearest in the drag's pan sense

Button and wheel-up / wheel-down triggers also take any [keybindings] action.

## `[mouse.on-window]`

| Binding | Action | Notes |
| --- | --- | --- |
| `"alt+left"` | `move-window` |  |
| `"alt+shift+left"` | `move-snapped-windows` |  |
| `"alt+right"` | `resize-window-snapped` |  |
| `"alt+shift+right"` | `resize-window` |  |
| `"alt+middle"` | `fit-window-snapped` |  |
| `"alt+shift+middle"` | `fit-window` |  |
| `"mod+middle"` | `fill-window` |  |

## `[mouse.on-canvas]`

| Binding | Action | Notes |
| --- | --- | --- |
| `"left"` | `pan-viewport` | unmodified left-click on empty canvas → pan |
| `"trackpad-scroll"` | `pan-viewport` | trackpad scroll on empty canvas → pan |
| `"wheel-scroll"` | `zoom` | mouse wheel on empty canvas → zoom |

## `[mouse.anywhere]`

| Binding | Action | Notes |
| --- | --- | --- |
| `"mod+left"` | `pan-viewport` |  |
| `"mod+ctrl+left"` | `focus-nearest` | direction from the drag, pan sense |
| `"mod+trackpad-scroll"` | `pan-viewport` |  |
| `"mod+wheel-scroll"` | `zoom` |  |

## `[gestures]`

### `swipe_threshold`

Default: `12.0`

Trackpad gesture thresholds. [touch] has keys of the same names, but its swipe_threshold is in millimetres.

px before a directional swipe fires

### `pinch_in_threshold`

Default: `0.85`

scale below which pinch-in fires (1.0 = no pinch)

### `pinch_out_threshold`

Default: `1.15`

scale above which pinch-out fires (1.0 = no pinch)

Gesture bindings: `"Modifier+N-finger-<type>" = "action"`; "none" unbinds, and unbound gestures go to the app. Contexts as for the mouse.

Gesture types (2–5 fingers):

- `N-finger-swipe` — continuous or threshold, by action
- `N-finger-swipe-up/down/left/right` — threshold, physical finger direction
- `3-finger-doubletap-swipe` — continuous (tap, then swipe)
- `N-finger-pinch` — continuous (pinch-in/out for one-shot)
- `N-finger-pinch-in/out` — threshold
- `N-finger-hold` — threshold, fires on release

Continuous actions, locked to their triggers:

- `pan-viewport` — swipe
- `zoom` — pinch
- `move-window` — swipe or doubletap-swipe
- `move-snapped-windows` — swipe or doubletap-swipe
- `resize-window` — swipe or doubletap-swipe
- `resize-window-snapped` — swipe or doubletap-swipe

Threshold actions: any [keybindings] action. Bare center-nearest and focus-nearest take the swipe's direction in the pan sense (fingers left look right).

## `[gestures.on-window]`

| Binding | Action | Notes |
| --- | --- | --- |
| `"alt+3-finger-swipe"` | `resize-window-snapped` |  |
| `"alt+shift+3-finger-swipe"` | `resize-window` |  |
| `"3-finger-doubletap-swipe"` | `move-window` |  |

**Example: fit and fullscreen on a pinch (unbound by default)**

```toml
"alt+2-finger-pinch-out" = "fit-window-snapped"
"alt+3-finger-pinch-out" = "toggle-fullscreen"
```

## `[gestures.on-canvas]`

| Binding | Action | Notes |
| --- | --- | --- |
| `"2-finger-pinch"` | `zoom` |  |

## `[gestures.anywhere]`

The mod+3-finger lines repeat the 4-finger ones for small trackpads.

| Binding | Action | Notes |
| --- | --- | --- |
| `"3-finger-swipe"` | `pan-viewport` | continuous (per-frame dx/dy) |
| `"4-finger-swipe"` | `focus-nearest` | threshold, direction from the swipe |
| `"mod+3-finger-swipe"` | `focus-nearest` |  |

**Example: per-direction overrides (threshold only)**

```toml
"4-finger-swipe-up" = "exec brightnessctl set +5%"
"4-finger-swipe-down" = "exec brightnessctl set 5%-"
"4-finger-swipe-right" = "cycle-windows forward"
"4-finger-swipe-left" = "cycle-windows backward"
```

| Binding | Action | Notes |
| --- | --- | --- |
| `"mod+2-finger-pinch"` | `zoom` | mod overrides app forwarding |
| `"3-finger-pinch"` | `zoom` | continuous |
| `"4-finger-pinch-in"` | `zoom-to-fit` | threshold |
| `"mod+4-finger-pinch-in"` | `zoom-to-fit-snapped` |  |
| `"4-finger-pinch-out"` | `home-toggle` | threshold |
| `"mod+3-finger-pinch-in"` | `zoom-to-fit` |  |
| `"mod+3-finger-pinch-out"` | `home-toggle` |  |
| `"4-finger-hold"` | `focus-center` | fires on release |
| `"mod+3-finger-hold"` | `focus-center` |  |

## `[touch]`

### `swipe_threshold`

Default: `15.0`

Touchscreen gesture thresholds; the hardware is set up under [input.touch]. swipe_threshold is in millimetres here (px under [gestures]); 0 or less falls back to the default.

mm before a directional swipe fires

### `pinch_in_threshold`

Default: `0.85`

scale below which pinch-in fires (1.0 = no pinch)

### `pinch_out_threshold`

Default: `1.15`

scale above which pinch-out fires (1.0 = no pinch)

### `tap_time`

Default: `250`

Tap and hold timings: raise them for a deliberate touch, lower them for a snappier one. A tap bound to center-window (the default 3-finger tap) waits out double_tap_time before it fires.

ms a tap may last

### `double_tap_time`

Default: `300`

ms for a second tap to make a double-tap

### `hold_time`

Default: `350`

ms of dwell before a drag becomes a hold gesture (hold-swipe, doubletap-hold-swipe)

### `tap_travel`

Default: `2.0`

mm a tap may drift before it becomes a drag

Bindings: `"N-finger-<type>" = "action"` (touch has no modifiers). Contexts and actions as under [gestures]; unbound gestures go to the app.

Touch gesture types (1–5 fingers):

- `N-finger-swipe` — continuous or threshold, by action
- `N-finger-swipe-up/down/left/right` — threshold, physical finger direction
- `N-finger-pinch` — continuous (pinch-in/out for one-shot)
- `N-finger-pinch-in/out` — threshold
- `N-finger-tap` — threshold (quick touch, no movement)
- `N-finger-doubletap` — threshold (two quick taps)
- `N-finger-doubletap-swipe` — continuous (tap, then drag)
- `N-finger-doubletap-hold-swipe` — continuous (tap, hold, then drag)
- `N-finger-hold-swipe` — continuous (dwell, then drag)

## `[touch.on-window]`

These act on the window under the fingers; 1–2 fingers on a window go to the app.

| Binding | Action | Notes |
| --- | --- | --- |
| `"3-finger-doubletap"` | `fit-window-snapped` |  |
| `"3-finger-doubletap-swipe"` | `move-window` | continuous |
| `"3-finger-hold-swipe"` | `resize-window-snapped` | continuous (dwell, then drag) |
| `"3-finger-doubletap-hold-swipe"` | `move-snapped-windows` | continuous (tap, hold, then drag) |

## `[touch.on-canvas]`

| Binding | Action | Notes |
| --- | --- | --- |
| `"1-finger-swipe"` | `pan-viewport` |  |
| `"2-finger-swipe"` | `pan-viewport` |  |
| `"2-finger-pinch"` | `zoom` |  |

## `[touch.anywhere]`

| Binding | Action | Notes |
| --- | --- | --- |
| `"3-finger-swipe"` | `pan-viewport` | continuous |
| `"3-finger-pinch"` | `zoom` | continuous |
| `"4-finger-swipe"` | `focus-nearest` | threshold, direction from the drag |
| `"4-finger-pinch-in"` | `zoom-to-fit` | threshold |
| `"4-finger-pinch-out"` | `home-toggle` | threshold |
| `"5-finger-swipe"` | `focus-nearest` | threshold, direction from the drag |
| `"5-finger-pinch-in"` | `zoom-to-fit` | threshold |
| `"5-finger-pinch-out"` | `home-toggle` | threshold |
| `"3-finger-tap"` | `center-window` |  |

## `[xwayland]`

### `enabled`

Default: `true`

X11 apps run through xwayland-satellite, started at launch with DISPLAY exported. Without the binary, X11 support is off (with a warning).

spawn xwayland-satellite at startup

### `path`

Default: `"xwayland-satellite"`

path to xwayland-satellite binary ($PATH lookup works)

## `[backend]`

### `wait_for_frame_completion`

Default: `false`

Hardware workarounds, all off by default. Try them on flicker, crashes or rendering glitches, especially with NVIDIA's driver. NVIDIA users can also export these before starting driftwm:

```text
export SMITHAY_USE_LEGACY=1          # legacy DRM API instead of atomic modesetting
export __GL_GSYNC_ALLOWED=0
export __GL_VRR_ALLOWED=0
export __GL_MaxFramesAllowed=1
export NVD_BACKEND=direct
```

GPU fence wait before every page flip (already automatic where the driver needs it)

### `disable_direct_scanout`

Default: `false`

always composite with EGL

### `disable_hardware_cursor`

Default: `false`

draw the cursor into the frame (direct scan-out stays on); fixes a stuttering cursor on NVIDIA

### `max_capture_fps`

Default: `0`

FPS cap for screen recording and casting (0 = none; 30 or 60 trims the cost while gaming and streaming); screenshots are never capped

## `[output.outline]`

### `color`

Default: `"#ffffff"`

outline color for other monitors' viewports

### `thickness`

Default: `1`

pixels (0 to disable)

### `opacity`

Default: `0.5`

0.0–1.0

## Outputs

Per-monitor settings, matched by connector name (see `wlr-randr` or the startup log). An output with no entry gets scale 1.0. On the winit backend only `position` and `focus_placement` apply.

`name = "*"` matches every output without its own entry; its `position` is ignored (always "auto"). An exact entry replaces it wholesale: fields don't merge.

`mode`: "preferred" (the default, and the safe choice), "max" (highest resolution, then refresh), "WxH" (an advertised mode, else preferred) or "WxH@Hz" (forced, with a generated modeline if the monitor doesn't advertise it; some panels reject it).

Supported fields:

- `name` — connector name, or "*" for the fallback entry; required.
- `scale` — fractional scale factor (default: 1.0).
- `transform` — normal, 90, 180, 270, flipped, flipped-90, flipped-180, or flipped-270 (default: normal).
- `position` — "auto" (left to right) or [x, y] in layout coords.
- `mode` — see above (default: preferred).
- `focus_placement` — overrides the top-level focus_placement on this monitor.
- `hot_corners` — corner actions for this monitor (see below).

Hot corners run any [keybindings] action when the cursor enters a corner of this output, and re-arm once it leaves. "none" leaves a corner unbound.

**Example:**

```toml
[[outputs]]
name = "eDP-1"           # connector name, or "*" for a fallback entry (required)
scale = 1.5              # fractional scale (default: 1.0)
transform = "normal"     # normal, 90, 180, 270, flipped, flipped-90, flipped-180, flipped-270
position = "auto"        # "auto" (left-to-right) or [x, y] in layout coords
mode = "preferred"       # "preferred", "max", "1920x1080", or "2560x1440@144"

[[outputs]]
name = "HDMI-A-1"
scale = 1.0
mode = "1920x1080@60"
focus_placement = "right"   # park the active window against this ultrawide's right edge

[outputs.hot_corners]
threshold = 4         # corner zone size in logical px (default: 4)
top_left = "zoom-to-fit"
top_right = "none"
bottom_left = "spawn grim - | wl-copy"
bottom_right = "exec-launcher"
disable_when_fullscreen = true   # suppress hot-corners while a fullscreen window is on this output (default: true)
disable_while_dragging  = true   # suppress hot-corners while any mouse button is held, or a pointer grab (move/resize/pan) is active (default: true)
```

## Window rules

Match windows and override settings per window. Every matching rule applies, in config order: later rules win for values, and true flags stick. Recipes and matching details: docs/window-rules.md.

Supported fields:

- `app_id` — match: Wayland app_id (X11 apps: their WM_CLASS instance, usually lowercase). Needs app_id or title; every criterion given must match.
- `title` — match: window title.
- `position` — [x, y] of the visual frame's center, Y-up: canvas coords, or output-relative (origin at the output center) when pinned_to_screen.
- `size` — [width, height] of the visual frame (content plus driftwm's title bar and border) at open; you and the app can resize it later (widget = true blocks resizing by hand).
- `fullscreen` — true: open the window fullscreen.
- `focus_on_open` — false: map the window without focusing it or moving the camera to it (default: true).
- `widget` — true: can't be moved by hand (`msg move` still works), sits below normal windows, skipped by navigation and Alt-Tab (default: false).
- `pinned_to_screen` — true: lock the window to the screen, ignoring pan and zoom; `position` becomes output-relative. Toggle with toggle-pin-to-screen (Mod+T). (default: false)
- `suspend_on_close` — override [session].suspend_on_close for matched windows (default: inherit).
- `restore_windows` — override [session].restore_windows for matched windows. Match on app_id: a title-only rule governs saving only. (default: inherit)
- `preserve_aspect_ratio` — true: keep the aspect ratio while resizing by hand (default: false).
- `decoration` — overrides [decorations] default_mode:
  - "client"  — the app's own title bar
  - "server"  — driftwm's title bar
  - "minimal" — no title bar; shadow, corners and border apply
  - "none"    — no chrome, per-window chrome rules included
- `blur` — true: frost the backdrop behind the window (default: false). Costs GPU time, less when zoomed out.
- `opacity` — 0.0–1.0 (default: [decorations] opacity / opacity_focused). A translucent fullscreen window shows [background] behind it (not a wallpaper daemon) and costs that output direct scan-out, so leave games opaque.
- `border_width` — border width in px; 0 removes it. Ignored for decoration = "none".
- `border_color` — unfocused border color, "#rrggbb" or "#rrggbbaa".
- `border_color_focused` — focused border color, "#rrggbb" or "#rrggbbaa".
- `corner_radius` — corner radius in px. Ignored for decoration = "none".
- `shadow` — shadow on or off. Ignored for decoration = "none".
- `output` — output name (e.g. "DP-1") for the window's fullscreen and its first screen pin; dragging or send-to-output moves it later. Names are under `outputs.*` in `driftwm msg state`. (default: unset)
- `pass_keys` — compositor keybindings to forward to the app:
  - pass_keys = true — forward ALL keys (game-friendly)
  - pass_keys = ["mod+q", "ctrl+q"] — forward ONLY these combos
  - pass_keys = false / omit — compositor handles everything (default)
  - VT switching (Ctrl+Alt+F1–F12) — always stays with the compositor
- `pass_mouse` — compositor mouse bindings ([mouse.*]) to forward to the app:
  - pass_mouse = true — forward ALL mouse bindings over this window
  - pass_mouse = ["alt+left", "alt+right"] — forward ONLY these combos
  - pass_mouse = false / omit — compositor handles everything (default)
  - SSD title bar, resize borders and close button — always stay with the compositor
- `layer_order` — stacking among layer surfaces on the same layer (higher on top; ties stack newest on top). Ignored for regular windows.

Pattern syntax (applies to all match fields):

- Plain string `"kitty"` — exact match
- Glob `"steam_app_*"` — `*` wildcard
- Regex `"/^steam_app_\\d+$/"` — wrap in `/…/`

To find a window's identifiers, run while the window is open:

```text
`driftwm msg state`
```

Layer surfaces (bars, notifications) match their namespace against `app_id`. `decoration` doesn't apply, and chrome (border_width, corner_radius, shadow) is opt-in per rule, never inherited from [decorations].

A few representative rules follow; docs/window-rules.md collects the rest.

**Example: Picture-in-Picture, pinned to the screen, stays put while you pan/zoom**

```toml
[[window_rules]]
title            = "Picture-in-Picture"
pinned_to_screen = true
position         = [0, -300]   # output-relative: 300px below center; movable, drop to center on the output
```

**Example: Game, only let ctrl+q through, keep everything else (mod+q still closes)**

```toml
[[window_rules]]
app_id    = "factorio"
pass_keys = ["ctrl+q"]
```

**Example: Blender, let alt+drag reach the app (orbit / three-button emulation), keep all keys**

```toml
[[window_rules]]
app_id     = "blender"
pass_mouse = ["alt+left", "alt+middle", "alt+right"]
```

**Example: Compose rules, blur from first rule, opacity from second (both apply)**

```toml
[[window_rules]]
app_id = "Alacritty"
blur   = true

[[window_rules]]
app_id  = "Alacritty"
opacity = 0.9
```
