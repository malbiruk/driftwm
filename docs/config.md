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

Window manager modifier key: "super" (default) or "alt"

### `focus_follows_mouse`

Default: `false`

Sloppy focus: keyboard focus follows the pointer to windows. Moving to empty canvas keeps focus; click empty canvas to unfocus.

### `window_placement`

Default: `"center"`

Where new windows spawn when no window rule positions them:

- `"center"` — viewport center; camera animates to the new window.
- `"cursor"` — centered on the cursor (clamped to the active output's usable area); camera stays put unless zoomed out and zoom.reset_on_new_window is true.
- `"auto"` — snap-place adjacent to the focused window's cluster: try focused's edges (clockwise from the one nearest the viewport center), then BFS to neighbors. Falls back to "center" when no focused window or no valid placement was found.

### `autostart`

Default: `[]`

Commands to run at startup (after WAYLAND_DISPLAY is set). Each entry is passed to sh -c, so full shell syntax works (pipes, &&, env vars).

Running driftwm as a systemd session (via driftwm-session / a display manager) also launches XDG autostart entries (~/.config/autostart, /etc/xdg/autostart), in addition to the autostart list above. To opt out:

```text
systemctl --user mask xdg-desktop-autostart.target
```

## `[env]`

Environment variables set before any clients launch. Child processes (autostart, exec bindings) inherit these. These override the compositor's built-in toolkit defaults (MOZ_ENABLE_WAYLAND, QT_QPA_PLATFORM, SDL_VIDEODRIVER, GDK_BACKEND, ELECTRON_OZONE_PLATFORM_HINT).

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

XKB options (e.g., "grp:win_space_toggle" for Super+Space layout switch) grp:alt_shift_toggle/grp:ctrl_shift_toggle break those chord bindings — use a tap binding (see [keybindings]) instead

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

match bindings by physical key position across layouts

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

reverse scroll direction (content follows fingers)

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

none = device default; clickfinger = finger count (1=left, 2=right, 3=middle); button_areas = position on trackpad

### `disable_while_typing`

Default: `true`

ignore trackpad input shortly after a key press (palm rejection)

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

trackpad (scroll/gestures) pan multiplier

### `mouse_speed`

Default: `1.0`

mouse (drag) pan multiplier (1.0 = direct)

### `drift`

Default: `0.5`

momentum coast: 0 = off, 0.5 = default, 1 = floatiest

### `animation_speed`

Default: `0.3`

camera lerp factor (higher = faster)

### `auto_navigate_on_close`

Default: `true`

on close, pan to the newly focused window if off-screen false = camera stays put; focus only moves to a visible window

### `nudge_step`

Default: `20`

px per nudge-window action (mod-shift-arrow by default)

### `pan_step`

Default: `100.0`

px per pan-viewport action (mod-ctrl-arrow by default)

### `anchors`

Default: `[[0, 0]]`

Anchors: canvas points discoverable by center-nearest (4-finger swipe / Mod+Arrow) even when no window is there. Uses Y-up coordinate system.

**Example: 4 corners**

```toml
anchors = [[0, 0], [-1750, 1750], [1750, 1750], [1750, -1750], [-1750, -1750]]
```

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

pan when the bare cursor touches a screen edge (not just while dragging). Toggle: "toggle-cursor-pan".

### `cursor_zone`

Default: `20.0`

cursor edge-pan activation zone (px) — kept small so it doesn't trigger by accident. Pans at a constant speed_max within the zone (steady, push-speed independent); speed_min is unused here.

## `[zoom]`

### `step`

Default: `1.1`

multiplier per keypress (1.1 = 10% per press)

### `fit_padding`

Default: `80.0`

viewport px padding for zoom-to-fit (screen space)

### `reset_on_new_window`

Default: `true`

animate zoom to 1.0 when a new window is mapped (false = keep current zoom, pan only)

### `reset_on_activation`

Default: `true`

animate zoom to 1.0 when an off-screen window requests focus (false = keep current zoom, pan only)

## `[snap]`

### `enabled`

Default: `true`

magnetic edge snapping during window drag

### `gap`

Default: `12.0`

gap between snapped windows (canvas px)

### `distance`

Default: `24.0`

activation threshold (screen px from edge)

### `break_force`

Default: `32.0`

screen px past snap to break free

### `same_edge`

Default: `false`

also snap same edges (left-to-left, top-to-top)

### `edge_center`

Default: `false`

also snap edge midpoints (centers align along the moved axis)

## `[decorations]`

### `bg_color`

Default: `"#303030"`

title bar background (default: dark gray)

### `fg_color`

Default: `"#FFFFFF"`

title text + close button × color (default: white)

### `corner_radius`

Default: `10`

clip window corners to this radius

### `shadow`

Default: `true`

drop shadow under window chrome

### `title_bar_height`

Default: `25`

SSD title bar text. The font is resolved via fontconfig — install the `adwaita-fonts` package for the default look; otherwise a generic sans is substituted.

SSD title bar height in px (default: 25)

### `font`

Default: `"Adwaita Sans"`

title text font family

### `font_size`

Default: `11`

title text size in points (default: 11)

### `font_weight`

Default: `"medium"`

thin/extralight/light/regular/medium/ semibold/bold/extrabold/black (default: medium)

### `title_align`

Default: `"center"`

"left" or "center" (default: center). "center" centers short titles and left-aligns + ellipsizes long ones

### `default_mode`

Default: `"client"`

Decoration mode for windows without a rule:

- `"client"` — CSD: client draws its own titlebar (default)
- `"minimal"` — SSD: no titlebar; shadow, corners, and border still apply via [decorations] + per-window rules
- `"none"` — bare client surface: compositor adds zero chrome; per-window border/corner/shadow rules are ignored

"server" (driftwm titlebar) is intentionally not allowed as a global default: many toolkits (GTK, Electron) ignore xdg-decoration and keep drawing CSD, producing a misaligned double titlebar. Use it per-app via [[window_rules]] instead.

### `border_width`

Default: `0`

Borders apply to "client", "server", and "minimal" modes. "none" mode has no border unless one is set per-app in [[window_rules]].

px; 0 disables the border

### `border_color`

Default: `"#303030"`

unfocused border (default: dark gray)

### `border_color_focused`

Default: `"#303030"`

focused border. Same default as above — set this to a different color to get a focus indicator.

## `[effects]`

### `blur_radius`

Default: `2`

number of Kawase down+up passes (default: 2)

### `blur_strength`

Default: `1.1`

per-pass texel spread (default: 1.1)

### `animate_blur`

Default: `false`

re-blur every frame when the wallpaper is animated (expensive; default: false — blur is captured once and only refreshed when geometry/camera/static bg change)

## `[background]`

### `type`

Default: `"default"`

Five types: "default" (built-in dot-grid — the default), "shader", "tile", "wallpaper", "none". For shader/tile/wallpaper, `path` is the source.

**Example:**

```toml
type = "shader" — procedural GLSL (scrolls with canvas)
path = "/usr/local/share/driftwm/wallpapers/animated/fast_smoke.glsl"

type = "tile" — image tiled across the canvas
path = "~/Pictures/Wallpapers/tile.png"

type = "wallpaper" — single image fixed to viewport (does not scroll/zoom)
path = "~/Pictures/Wallpapers/wallpaper.jpg"

type = "none" — render no built-in background (path ignored), so a
  wlr-layer-shell wallpaper daemon (swaybg, swww, mpvpaper for live video)
  is the wallpaper. Launch it yourself; see docs/shaders.md.
```

A "shader" can also sample an image via `texture` (bound to the shader's `tex` sampler) — a procedural effect *on* your image. See docs/shaders.md.

**Example: shader sampling an image**

```toml
type = "shader"
path = "/usr/local/share/driftwm/wallpapers/textured/ripple.glsl"
texture = "~/Pictures/Wallpapers/photo.jpg"
```

### `cache_shader`

Default: `false`

Bake a heavy static shader to a texture once, then pan that — so it pans as cheaply as an image instead of recomputing every frame. ONLY correct for shaders that slide rigidly with the camera: u_camera used once, at full scale, as the only camera term. In GLSL:

```text
vec2 canvas = v_coords * size + u_camera;   // pan shifts the image 1:1
```

Parallax (u_camera * factor) renders WRONG; animated (u_time) and zoom-dependent (u_zoom) shaders are never cached and render live.

### `transparent_shader`

Default: `false`

Honor a shader's output alpha so transparent pixels reveal whatever sits below the background — e.g. a wlr-layer-shell Background surface from an external wallpaper engine. Forces the shader onto the live path, so cache_shader has no effect while this is on. Image backgrounds ignore it: tile/wallpaper PNGs with an alpha channel become see-through automatically. See docs/shaders.md.

### `cache_budget_mb`

Default: `128`

Memory ceiling (MB) shared by cache_shader and gigapixel-TIFF wallpapers, with LRU eviction. Raise it for sharper revisits on large / HiDPI displays; lower it on memory-constrained machines (too low just keeps the background blurrier).

## `[keybindings]`

Keyboard bindings: "Modifier+...+Keysym" = "action [arg]" Merges with defaults. Use "none" to unbind a default binding. "mod" expands to mod_key. Literal modifiers: alt, super, ctrl, shift. Keysyms are XKB names (case-insensitive): return, tab, up, a, equal, etc. A bare modifier combo (e.g. "alt+shift") is a tap binding (fires on chord release; see [input.keyboard] options).

Actions:

- `exec <cmd>` — launch an app (shows loading cursor until window appears, exits fullscreen)
- `exec-terminal` — launch the auto-detected terminal (see [keybindings] below; override with $TERMINAL)
- `exec-launcher` — launch the auto-detected app launcher (see [keybindings] below; override with $LAUNCHER)
- `spawn <cmd>` — run a command without loading cursor and exiting fullscreen (toggles, OSD, screenshots)
- `close-window` — close the focused window
- `nudge-window <dir>` — move focused window by nudge_step px
- `pan-viewport <dir>` — pan camera by pan_step px
- `center-window` — center viewport on focused window + reset zoom
- `focus-center` — focus + center on the window under the pointer + reset zoom
- `center-nearest <dir>` — navigate to nearest window in direction
- `cycle-windows forward` — Alt-Tab style window cycling
- `cycle-windows backward` — reverse cycle
- `home-toggle` — toggle between current position and origin
- `zoom-in / zoom-out` — step zoom
- `zoom-reset` — zoom to 1.0
- `go-to <x> <y>` — jump camera to canvas position (bookmarks, Y-up)
- `zoom-to-fit` — fit all windows in viewport
- `zoom-to-fit-snapped` — fit only the focused window's snap cluster
- `toggle-fullscreen` — toggle focused window fullscreen
- `fit-window` — toggle maximize: centers + resets zoom + fills viewport; restore only resizes back
- `fit-window-snapped` — fit-window for the focused window's whole snap cluster
- `toggle-pin-to-screen` — pin/unpin the focused window to the screen (ignores pan/zoom, floats above)
- `reload-config` — hot-reload config file
- `quit` — exit the compositor
- `send-to-output <dir>` — move focused window to adjacent output
- `switch-layout <target>` — cycle keyboard layout: next, prev, or a 0-based index
- `none` — unbind this key combo

Directions: up, down, left, right, up-left, up-right, down-left, down-right

| Binding | Action | Notes |
| --- | --- | --- |
| `"mod+return"` | `exec-terminal` | $TERMINAL, else first of: foot, alacritty, ptyxis, kitty, wezterm, gnome-terminal, konsole |
| `"mod+d"` | `exec-launcher` | $LAUNCHER, else first of: fuzzel, wofi, rofi, bemenu-run, wmenu-run, tofi-drun, mew-run |
| `"mod+q"` | `close-window` |  |
| `"mod+e"` | `toggle-cursor-pan` | toggle cursor edge-pan (see [navigation.edge_pan]) |
| `"mod+f"` | `toggle-fullscreen` |  |
| `"mod+m"` | `fit-window` |  |
| `"mod+shift+m"` | `fit-window-snapped` |  |
| `"mod+t"` | `toggle-pin-to-screen` |  |
| `"mod+c"` | `center-window` |  |
| `"mod+x"` | `focus-center` |  |
| `"mod+a"` | `home-toggle` |  |
| `"mod+up"` | `center-nearest up` |  |
| `"mod+down"` | `center-nearest down` |  |
| `"mod+left"` | `center-nearest left` |  |
| `"mod+right"` | `center-nearest right` |  |
| `"mod+shift+up"` | `nudge-window up` |  |
| `"mod+shift+down"` | `nudge-window down` |  |
| `"mod+shift+left"` | `nudge-window left` |  |
| `"mod+shift+right"` | `nudge-window right` |  |
| `"mod+ctrl+up"` | `pan-viewport up` |  |
| `"mod+ctrl+down"` | `pan-viewport down` |  |
| `"mod+ctrl+left"` | `pan-viewport left` |  |
| `"mod+ctrl+right"` | `pan-viewport right` |  |
| `"alt+tab"` | `cycle-windows forward` |  |
| `"alt+shift+tab"` | `cycle-windows backward` |  |
| `"mod+equal"` | `zoom-in` |  |
| `"mod+minus"` | `zoom-out` |  |
| `"mod+0"` | `zoom-reset` |  |
| `"mod+z"` | `zoom-reset` |  |
| `"mod+w"` | `zoom-to-fit` |  |
| `"mod+shift+w"` | `zoom-to-fit-snapped` |  |
| `"mod+1"` | `go-to -1750 1750` | top-left bookmark |
| `"mod+2"` | `go-to 1750 1750` | top-right bookmark |
| `"mod+3"` | `go-to 1750 -1750` | bottom-right bookmark |
| `"mod+4"` | `go-to -1750 -1750` | bottom-left bookmark |
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

**Example: tap binding — bare modifier chord, fires on release with no key on top**

```toml
"alt+shift" = "switch-layout next"
```

## `[mouse]`

### `resize_on_border`

Default: `true`

When true (default), dragging a window's edge or corner resizes it via the invisible resize border (SSD frame or CSD margin). Set false to make that border inert — resize only through explicit bindings (e.g. alt+right) or gestures.

### `decoration_resize_snapped`

Default: `false`

When true, resizing a window by dragging its edge (SSD or CSD border) propagates to every window connected to it via snap adjacency. Keybinding/gesture resize is unaffected — bind `resize-window-snapped` explicitly if you want cluster-aware resize there too.

### `decoration_fit_snapped`

Default: `false`

When true, maximize/unmaximize initiated via window decoration (CSD maximize button, SSD title-bar double-click, or xdg/foreign-toplevel set_maximized) propagates to every window connected via snap adjacency. Keybinding/gesture fit is unaffected — bind `fit-window-snapped` explicitly if you want cluster-aware fit there too.

Mouse bindings: "Modifier+...+Trigger" = "action" Context-aware: on-window, on-canvas, anywhere. Specific context checked first, then "anywhere" as fallback. Click-to-focus and SSD decoration clicks are always hardcoded. Triggers: left, right, middle (buttons), trackpad-scroll, wheel-scroll Merges with defaults. Use "none" to unbind.

Mouse actions: move-window, move-snapped-windows, resize-window, resize-window-snapped, pan-viewport, zoom, center-nearest Any keyboard action also works for button triggers: exec, close-window, toggle-fullscreen, etc.

move-window / resize-window act on the focused window only. move-snapped-windows / resize-window-snapped also translate every window connected to the focused one via snap adjacency

## `[mouse.on-window]`

| Binding | Action | Notes |
| --- | --- | --- |
| `"alt+left"` | `move-window` |  |
| `"alt+shift+left"` | `move-snapped-windows` |  |
| `"alt+right"` | `resize-window` |  |
| `"alt+shift+right"` | `resize-window-snapped` |  |
| `"alt+middle"` | `fit-window` |  |
| `"alt+shift+middle"` | `fit-window-snapped` |  |
| `"mod+middle"` | `toggle-fullscreen` |  |

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
| `"mod+ctrl+left"` | `center-nearest` | direction from drag delta |
| `"mod+trackpad-scroll"` | `pan-viewport` |  |
| `"mod+wheel-scroll"` | `zoom` |  |

## `[gestures]`

### `swipe_threshold`

Default: `12.0`

Gesture thresholds — tune for your touchpad size.

px cumulative distance before directional swipe fires

### `pinch_in_threshold`

Default: `0.85`

scale below which pinch-in fires (1.0 = no pinch)

### `pinch_out_threshold`

Default: `1.15`

scale above which pinch-out fires (1.0 = no pinch)

Gesture bindings: "Modifier+N-finger-<type>" = "action" Context-aware: on-window, on-canvas, anywhere. Unbound gestures are forwarded to the focused app. "none" unbinds (prevents anywhere fallback, still forwards).

Gesture types:

- `N-finger-swipe` — continuous OR threshold (action determines behavior)
- `N-finger-swipe-up/down/left/right` — threshold only, checked before swipe fallback
- `3-finger-doubletap-swipe` — continuous OR threshold (3-finger tap then swipe)
- `N-finger-pinch` — continuous only (use pinch-in/out for discrete)
- `N-finger-pinch-in/out` — threshold only
- `N-finger-hold` — threshold only (fires on release)

Continuous actions: pan-viewport, zoom, move-window, resize-window, resize-window-snapped Threshold actions: center-nearest, center-window, home-toggle, zoom-to-fit, zoom-to-fit-snapped, fit-window, fit-window-snapped, exec <cmd>, etc.

## `[gestures.on-window]`

| Binding | Action | Notes |
| --- | --- | --- |
| `"alt+3-finger-swipe"` | `resize-window` |  |
| `"alt+shift+3-finger-swipe"` | `resize-window-snapped` |  |
| `"3-finger-doubletap-swipe"` | `move-window` |  |
| `"alt+2-finger-pinch-in"` | `fit-window` |  |
| `"alt+2-finger-pinch-out"` | `fit-window` |  |
| `"alt+shift+2-finger-pinch-in"` | `fit-window-snapped` |  |
| `"alt+shift+2-finger-pinch-out"` | `fit-window-snapped` |  |
| `"alt+3-finger-pinch-in"` | `toggle-fullscreen` |  |
| `"alt+3-finger-pinch-out"` | `toggle-fullscreen` |  |

## `[gestures.on-canvas]`

| Binding | Action | Notes |
| --- | --- | --- |
| `"2-finger-pinch"` | `zoom` |  |

## `[gestures.anywhere]`

| Binding | Action | Notes |
| --- | --- | --- |
| `"3-finger-swipe"` | `pan-viewport` | continuous (per-frame dx/dy) |
| `"4-finger-swipe"` | `center-nearest` | threshold (accumulate, detect direction, fire once) |
| `"mod+3-finger-swipe"` | `center-nearest` | mod makes 3-finger swipe navigate too |

**Example: per-direction overrides (threshold only, checked before swipe fallback)**

```toml
"4-finger-swipe-up" = "exec brightnessctl set +5%"
"4-finger-swipe-down" = "exec brightnessctl set 5%-"
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
| `"4-finger-hold"` | `center-window` | fires on release |
| `"mod+3-finger-hold"` | `center-window` |  |

## `[xwayland]`

### `enabled`

Default: `true`

X11 support via xwayland-satellite. driftwm spawns satellite eagerly at startup, exports DISPLAY=:N, and X11 apps connect transparently. If the binary isn't found, X11 support is disabled with a warning; everything else still runs.

default: true

### `path`

Default: `"xwayland-satellite"`

path to xwayland-satellite binary ($PATH lookup works)

## `[backend]`

### `wait_for_frame_completion`

Default: `false`

Hardware stability quirks. All default to false (opt-in). Enable these if you experience flickering, crashes, or rendering issues. Particularly useful on NVIDIA GPUs with proprietary drivers. Note: These flags must be set before launching driftwm. Changing them requires a restart. For additional NVIDIA-specific settings, set these environment variables in your session wrapper script or shell profile before starting driftwm:

```text
export SMITHAY_USE_LEGACY=1          # Use legacy DRM API instead of atomic modesetting
export __GL_GSYNC_ALLOWED=0
export __GL_VRR_ALLOWED=0
export __GL_MaxFramesAllowed=1
export NVD_BACKEND=direct
```

Force GPU-fence wait before every page flip (already done automatically when smithay reports needs_sync — typical case on NVIDIA. Set true only if you still see flicker after defaults.)

### `disable_direct_scanout`

Default: `false`

Force EGL composition (disable direct scanout)

### `disable_hardware_cursor`

Default: `false`

Composite the cursor into the frame instead of using the KMS cursor plane, while keeping direct scanout for fullscreen apps. Fixes a stuttering / tearing hardware cursor on discrete NVIDIA GPUs.

### `max_capture_fps`

Default: `0`

Cap FPS delivered to continuous screen-capture clients (recorders / casts). 0 = unlimited. Each captured frame forces a full-scene re-composite that competes with a fullscreen app behind it; a cap (e.g. 30 or 60) trims that overhead while gaming + streaming. One-shot screenshots (grim) are never throttled.

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

Per-output configuration. Each [[outputs]] entry matches by connector name. Find connector names with wlr-randr or check driftwm logs at startup. Outputs without a matching entry default to scale 1.0. Winit backend ignores [[outputs]] entries.

**Example:**

```toml
[[outputs]]
name = "eDP-1"           # connector name (required)
scale = 1.5              # fractional scale (default: 1.0)
transform = "normal"     # normal, 90, 180, 270, flipped, flipped-90, flipped-180, flipped-270
position = "auto"        # "auto" (left-to-right) or [x, y] in layout coords
mode = "preferred"       # "preferred", "1920x1080", or "2560x1440@144"

[[outputs]]
name = "HDMI-A-1"
scale = 1.0
mode = "1920x1080@60"
```

## Window rules

Window rules: match windows and apply per-window overrides. ALL matching rules are merged in config order (later rules override earlier ones for scalar fields; boolean flags are sticky-on). This lets you compose rules — e.g. one rule sets blur=true, a later one adds opacity=0.85.

Match criteria (at least one required; all specified must match):

- `app_id` — Wayland app_id. X11 apps proxied via xwayland-satellite arrive with app_id set from WM_CLASS instance (typically lowercase).
- `title` — window title

Pattern syntax (applies to all match fields):

- `Plain string` — exact match: "kitty"
- `Glob` — * wildcard:  "steam_app_*"
- `Regex` — wrap in /…/: "/^steam_app_\\d+$/"

To find a window's identifiers, run while the window is open:

```text
`driftwm msg state`
```

Effect fields:

- `position` — [x, y] coordinates (window center, Y-up). Canvas coords, or output-relative (origin = output center) when pinned_to_screen.
- `size` — [width, height] initial window dimensions (one-shot; user/app can resize after)
- `widget` — true: pinned (immovable), below normal windows, excluded from navigation and alt-tab (default: false)
- `pinned_to_screen` — true: lock the window to the output's screen space — ignores pan/zoom, floats above normal windows (PiP, toolbars). `position` becomes output-relative; movable unless widget = true. Toggle live with `toggle-pin-to-screen` (Mod+T). (default: false)
- `decoration` — overrides [decorations] default_mode for matched windows. Omit to inherit default_mode. Values:
  - "client":  CSD — client's own titlebar
  - "server":  SSD — driftwm's titlebar
  - "minimal": SSD — no titlebar, just shadow + corners + border (this is the mode for chrome-on-borderless widgets; border_width / corner_radius / shadow rules apply)
  - "none":    bare client surface — compositor adds zero chrome, and per-window border_width / corner_radius / shadow rules are ignored. Use "minimal" if you want chrome without a titlebar.
- `blur` — true: blur background behind this window (default: false)
- `opacity` — 0.0–1.0: window transparency (default: 1.0, fully opaque)
- `border_width` — per-window border width override (px). Set to 0 to disable border on a window even when [decorations] border_width > 0. Ignored for decoration = "none".
- `border_color` — per-window unfocused border color hex (e.g. "#5c5c5c").
- `border_color_focused` — per-window focused border color hex.
- `corner_radius` — per-window corner radius override (px). Affects content clip, border shape, and shadow. Ignored for decoration = "none".
- `shadow` — per-window shadow toggle. Overrides [decorations] shadow. Ignored for decoration = "none".
- `pass_keys` — controls which compositor keybindings are forwarded to the app:
  - pass_keys = true — forward ALL keys (game-friendly)
  - pass_keys = ["mod+q", "ctrl+q"] — forward ONLY these combos; all other compositor shortcuts stay active
  - pass_keys = false / omit — compositor handles everything (default)
  - VT switching (Ctrl+Alt+F1–F12) — always stays in the compositor

Layer-shell surfaces (panels, notifications, bars like waybar): `decoration` is ignored — layers have no decoration mode. Chrome (border_width, corner_radius, shadow) is field-by-field opt-in on the rule and does NOT inherit from [decorations]. Without explicit values on the rule, a layer surface has no border, no shadow, and no corner clip.

**Example: Desktop widget (pinned clock/calendar)**

```toml
[[window_rules]]
app_id     = "my-widget"
position   = [0, 0]
widget     = true
decoration = "none"
```

**Example: Picture-in-Picture, pinned to the screen, stays put while you pan/zoom**

```toml
[[window_rules]]
title            = "Picture-in-Picture"
pinned_to_screen = true
position         = [0, -300]   # output-relative: 300px below center; movable, drop to center on the output
```

**Example: Blurred transparent terminal**

```toml
[[window_rules]]
app_id  = "kitty"
opacity = 0.85
blur    = true
```

**Example: Game, pass ALL keys to the app (mod+q, ctrl+q etc. reach the game)**

```toml
[[window_rules]]
app_id    = "steam_app_*"
pass_keys = true
```

**Example: Game, only let ctrl+q through, keep everything else (mod+q still closes)**

```toml
[[window_rules]]
app_id    = "factorio"
pass_keys = ["ctrl+q"]
```

**Example: Regex match, any Steam game app**

```toml
[[window_rules]]
app_id    = "/^steam_app_\\d+$/"
pass_keys = true
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

**Example: Iced/libcosmic utility windows that share the main window's app_id**

```toml
[[window_rules]]
title  = "winit window"
widget = true
```
