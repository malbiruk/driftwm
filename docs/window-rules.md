# Window Rules

Window rules let you apply per-window overrides based on a window's identity.
Rules are declared as `[[window_rules]]` sections in your config file.

Most rule effects are resolved once, when a window maps — see
[When rules take effect](#when-rules-take-effect).

## How matching works

**All matching rules are applied, not just the first one.** Rules are processed
in config order and merged together:

- **Scalar fields** (`decoration`, `opacity`, `position`, `size`,
  `fullscreen`, `focus_on_open`, `border_width`, `border_color`,
  `border_color_focused`, `corner_radius`, `shadow`, `output`, `layer_order`,
  `suspend_on_close`, `restore_windows`): last-wins — a later rule overrides an
  earlier one.
- **Boolean flags** (`widget`, `pinned_to_screen`, `blur`,
  `preserve_aspect_ratio`): sticky-on — once set by any matching rule, the flag
  stays set regardless of later rules. There is no way to turn one back off:
  writing `blur = false` or `widget = false` in a rule does nothing.
- **`pass_keys`**: `All` is sticky-on; `Only` lists are unioned across
  rules (see [`pass_keys`](#pass_keys)).
- **`pass_mouse`**: same shape — `true` is sticky-on; combo lists are unioned
  across rules (see [`pass_mouse`](#pass_mouse)).

This lets you compose independent rules for the same window:

```toml
# All three rules below apply to the same kitty window and are merged:

[[window_rules]]
app_id = "kitty"
blur   = true        # sticky-on: cannot be unset by later rules

[[window_rules]]
app_id  = "kitty"
opacity = 0.85       # blur from above is preserved

[[window_rules]]
title   = "*nvim*"   # title match narrows to nvim windows only
opacity = 1.0        # override opacity for nvim (blur still applies)
```

## Match criteria

At least one criterion is required. All specified criteria must match.

| Field    | Matches                                                                                                                 |
| -------- | ----------------------------------------------------------------------------------------------------------------------- |
| `app_id` | Wayland app_id (X11 apps via xwayland-satellite arrive with `app_id` set from `WM_CLASS` instance, typically lowercase) |
| `title`  | Window title                                                                                                            |

### Finding a window's identifiers

```sh
driftwm msg state   # camera, zoom, and the window inventory
```

To get the app ids and titles of all current non-widget windows:

```sh
driftwm msg --json state | \
jq '.Ok.State.windows[] | select(.is_widget == false and .suspended != true) | {app_id, title}'
```

## Pattern syntax

All match fields support three syntaxes:

| Syntax       | Example                | Meaning                                 |
| ------------ | ---------------------- | --------------------------------------- |
| Exact string | `"kitty"`              | Exact match (case-sensitive)            |
| Glob         | `"steam_app_*"`        | `*` matches any sequence of chars       |
| Regex        | `"/^steam_app_\\d+$/"` | Full regular expression (wrap in `/…/`) |

Multiple `*` wildcards are allowed in glob patterns: `"*terminal*"`.

Regex patterns don't support backreferences or lookaround.

```toml
# Match any Steam game by regex
[[window_rules]]
app_id    = "/^steam_app_\\d+$/"
pass_keys = true
```

## Coordinates and sizes

`position` and `size` describe a window's **visual frame**: the app's content
plus the title bar and border driftwm draws around it, if it draws any.
`position` is that frame's center, with **Y pointing up**.

This is what makes a layout portable. `size = [800, 600]` gives you an 800x600
window on screen whether it is server-decorated, client-decorated, or bare, and
two windows placed 800 apart sit flush against each other either way. The same
numbers come back out of `driftwm msg state` and the
[state file](ipc.md#state-file), so a rule and a running window always describe
the same rectangle.

The app itself gets whatever is left inside the frame. With
`[decorations] title_bar_height = 25` and `border_width = 2`, a
`size = [800, 600]` rule that also sets `decoration = "server"` hands the client
796x571.

## Field reference

Every rule field — its type, default, accepted values, and per-field caveats
(which fields `decoration = "none"` ignores, the blur GPU/VRAM cost, the one-shot
`size`, how layer-shell surfaces opt into chrome) — is documented in the generated
[config reference](config.md#window-rules), whose canonical source is
[`config.reference.toml`](../config.reference.toml). This page is the recipe and
semantics guide; the reference is the field dictionary.

Layer-shell surfaces interpret chrome fields differently — see
[Layer-shell surfaces](#layer-shell-surfaces) below.

### Transparency

`opacity` below 1.0 makes a window see-through, and that carries into fullscreen:
a translucent fullscreen window shows the **canvas** behind it instead of black.
"Canvas" means the compositor's own [`[background]`](config.md#background) —
the built-in dot grid, your shader, or your `tile`/`wallpaper` image — plus any
canvas layers. Everything else stays hidden: other windows, pinned windows,
suspended stand-ins, and every layer-shell surface, panels included. It is a
window onto the plane, not a way to see the rest of your desktop.

That last part decides what a wallpaper daemon looks like through the window.
swaybg, swww and mpvpaper paint on the `background` wlr-layer, which is a layer
surface like any other and stays culled — so with `[background] type = "none"`
and one of those as your wallpaper, a translucent fullscreen window still shows
black. Point `[background]` at the image or shader instead if you want it
visible through the window.

Widgets divide along the same line, by the protocol they are built on: a widget
that is a layer-shell surface placed at canvas coordinates rides the canvas
layers and shows through, while a widget that is a rule-placed xdg-toplevel is a
window and stays hidden. Two widgets that look identical on the canvas can
differ here.

```toml
[[window_rules]]
app_id  = "mpv"
opacity = 0.85
```

It holds across the fullscreen transitions too, and `driftwm msg opacity` changes
it live on a window that is already fullscreen. The trade: while a translucent
window is fullscreen, that output loses direct scan-out, because the compositor
has to compose the canvas under it every frame. Leave games opaque.

A rule `opacity` is a **pin**: the window draws at that one value whether it is
focused or not, and it opts the window out of the global `opacity` /
`opacity_focused` defaults in [`[decorations]`](config.md#decorations). That
includes `opacity = 1.0` — how a screen-pinned PiP window stays fully opaque
while everything else dims. `driftwm msg opacity <value>` pins the window the
same way.

### Screen-pinned windows

`pinned_to_screen = true` lifts a window out of the infinite canvas and fixes it
to one output's **screen space**: it does not pan or zoom with the camera, and it
renders **above** normal windows (but below panels / Top & Overlay layer-shell
surfaces). Use it for Picture-in-Picture, video-call toolbars, or any always-on
floating overlay.

```toml
[[window_rules]]
title            = "Picture-in-Picture"
pinned_to_screen = true
position         = [540, -350]
size             = [570, 320]
decoration       = "none"
```

- **Coordinates are output-relative.** When pinned, `position` is measured from
  the **output center** (still the visual frame's center, Y-up): `[0, 0]` centers
  the window on the monitor, `+Y` is up. Drop `position` to center it. A position
  that would push the window off the monitor is clamped so the whole frame stays
  visible — a title bar never goes off the top edge.
- **Off the canvas.** Pinned windows are excluded from navigation, alt-tab,
  snapping, fit/center actions, and canvas screenshots
  (`driftwm msg screenshot`). They remain focusable and closable; SSD windows
  show a small dot in the title bar.
- **Fullscreen round-trips.** A fullscreen request (or `Mod+F`) temporarily
  unpins the window to fill the screen; exiting fullscreen re-pins it in place.
  Any canvas pan/zoom exits fullscreen, just like a normal window.
- **Dragging it across monitors** reassigns it to that output. Combine with
  `widget = true` to make it immovable.

To find the numbers for a rule, pin the window live with `toggle-pin-to-screen`
(`Mod+T`), drag and resize it into place, then copy `position`/`size` from its
entry in the per-output `pinned` section of `driftwm msg state` — those are
already output-relative rule coordinates.

### Output selection

On a multi-monitor setup, `output` names a monitor by its output name (e.g.
`"DP-1"` — find names under `outputs.*` in `driftwm msg state`). It governs two
placements:

```toml
[[window_rules]]
app_id = "steam_app_*"
output = "DP-1"
```

- **Fullscreen** — which monitor a window fullscreens onto. Precedence: the
  rule's `output` wins; otherwise the output the client itself requested;
  otherwise the active output (where the pointer is).
- **Screen-pinned** — which monitor a `pinned_to_screen` window *initially* pins
  to. Precedence: the rule's `output` wins; otherwise the active output. The
  rule's `position` is then resolved against that monitor. Afterwards, dragging
  the window across monitors — or `send-to-output` — reassigns it, so `output`
  only seeds the starting display.

An unknown or disconnected output name falls through to the next choice.
`output` does not move a plain windowed (non-fullscreen, non-pinned) window.

### Layer-shell surfaces

Layer-shell surfaces (panels, notifications, bars like waybar) have no decoration
mode — the `decoration` field on a rule matching a layer surface is ignored.

Chrome on layers is **field-by-field opt-in**: set `border_width`,
`corner_radius`, and/or `shadow` directly on the rule. Layers do **not** inherit
`[decorations]` defaults for those three fields — without an explicit value on
the rule, a layer surface has no border, no shadow, and no corner clipping.
`border_color_focused` is also ignored on layers (the focused / unfocused
distinction is window-only); layers always use `border_color`.

```toml
[[window_rules]]
app_id        = "waybar"
widget        = true
corner_radius = 10
shadow        = true
border_width  = 2
```

### `pass_keys`

`pass_keys` forwards compositor keybindings to the focused window instead of
handling them — useful for games and remote-desktop clients. Key combo syntax is
the same as in `[keybindings]`: `mod+key`, `ctrl+shift+key`, etc.

VT switching (`Ctrl+Alt+F1`–`F12`) **always stays in the compositor**, so
`pass_keys = true` can never lock you out of your TTYs.

When multiple rules match the same window, `["combo", …]` lists are **unioned**,
and `true` beats a list: if one rule says `true` and another says `["mod+q"]`,
the result is `true`.

### `pass_mouse`

`pass_mouse` is the same idea for the `[mouse.*]` bindings: a claimed press or
scroll reaches the app instead of running its compositor action. Apps that want
`alt`-drag for themselves — Blender, Krita, GIMP, CAD three-button emulation —
need it, since `alt+left`/`alt+middle`/`alt+right` are the on-window move / fit /
resize defaults.

Combo syntax is the `[mouse.*]` binding syntax: modifiers plus one of `left`,
`right`, `middle`, `trackpad-scroll`, `wheel-scroll`, `wheel-up`, `wheel-down`.
`mod` resolves through `mod_key` as everywhere else. To hand an app the default
`mod+wheel-scroll` zoom, list `"mod+wheel-scroll"`.

The discrete `wheel-up` / `wheel-down` triggers are a separate channel from the
continuous `wheel-scroll` one, so listing `"mod+wheel-up"` claims only the
notch binding — a `mod+wheel-scroll` zoom keeps firing unless it is listed too.

Unlike `pass_keys`, which follows the **focused** window, `pass_mouse` is
resolved against the window **under the pointer** — canvas, pinned or
fullscreen — because that is what a mouse binding's context comes from.

Compositor chrome always stays with the compositor: the SSD title bar, the
close button and the resize borders keep their normal behaviour for every
button, so `alt+left` on a title bar still starts a move and `alt+right` on a
resize border still resizes.

A claimed click behaves exactly as an unbound one, which also means it still
focuses and raises the window — and with `auto_navigate_on_click = true` it
still pans the camera to the window.

Below `[zoom] interact_min` a canvas window has no pointer focus to forward to,
so a *canvas window's* `pass_mouse` claims are ignored there and the compositor
bindings stay live. Pinned and fullscreen windows keep their pointer focus, so
their claims still apply.

When multiple rules match the same window, combo lists are **unioned** and
`true` beats a list.

```toml
[[window_rules]]
app_id     = "blender"
pass_mouse = ["alt+left", "alt+middle", "alt+right"]
```

## Examples

### Desktop widget (pinned clock/info panel)

```toml
[[window_rules]]
app_id     = "my-widget"
position   = [0, 0]
widget     = true
decoration = "none"
```

A widget ignores drags, gestures and nudges, so `position` is where it stays —
but `driftwm msg move <X> <Y> --id <ID>` still repositions one at runtime, which
is how external desktop-icon clients scatter icons across the canvas. That move
lasts only for the session.

### Pictures and text on the canvas (decals)

To pin arbitrary images to canvas spots — hand-drawn shortcut sheets, logos,
region labels — render a transparent PNG/SVG as a borderless window with
[`extras/scripts/driftwm-decal`](../extras/scripts/driftwm-decal) (deps:
python-gobject + gtk4), then pin each one with a `widget` rule. The transparent
parts show the dot grid (or your shader wallpaper) through; decals sit below
normal windows and stay off alt-tab. Each invocation is one decal window,
matched by `--title`:

```toml
autostart = [
    "driftwm-decal ~/decals/shortcuts.svg --title shortcuts",
    "driftwm-decal ~/decals/logo.png      --title logo",
]

[[window_rules]]
title      = "shortcuts"
widget     = true          # pin to canvas, below windows, off alt-tab
decoration = "none"
position   = [1200, -400]  # canvas coords, Y-up, image center
size       = [420, 130]

[[window_rules]]
title      = "logo"
widget     = true
decoration = "none"
position   = [-800, 600]
size       = [256, 256]
```

### Transparent blurred terminal

```toml
[[window_rules]]
app_id  = "kitty"
opacity = 0.85
blur    = true
```

### Game: pass all keys through (Wayland-native)

```toml
[[window_rules]]
app_id    = "steam_app_*"
pass_keys = true
```

### Game: only let specific keys through

Keep `mod+q` and other compositor shortcuts active, but pass `ctrl+q` to the game:

```toml
[[window_rules]]
app_id    = "factorio"
pass_keys = ["ctrl+q", "ctrl+s"]
```

### Blender: let alt+drag reach the app

Blender orbits and pans with `alt`-drag, which collides with the on-window
move / fit / resize defaults. Hand it those three combos and leave every
keybinding — and every other mouse binding — with the compositor:

```toml
[[window_rules]]
app_id     = "blender"
pass_mouse = ["alt+left", "alt+middle", "alt+right"]
```

### Initial size and position for a floating panel

```toml
[[window_rules]]
app_id   = "myapp-panel"
size     = [400, 800]
position = [960, 0]
widget   = true
```

### Widget with a custom border and shadow

`decoration = "minimal"` is the mode for a widget that should keep borders,
corner clipping, and shadow but lose its titlebar — `decoration = "none"`
ignores those overrides entirely.

```toml
[[window_rules]]
app_id               = "my-clock"
widget               = true
decoration           = "minimal"
border_width         = 2
border_color         = "#5c5c5c"
border_color_focused = "#7aa2f7"
corner_radius        = 8
shadow               = true
```

### Disable shadow on a specific app

```toml
[[window_rules]]
app_id = "firefox"
shadow = false
```

### Picture-in-Picture that keeps its aspect ratio

```toml
[[window_rules]]
title                 = "Picture-in-Picture"
pinned_to_screen      = true
preserve_aspect_ratio = true
decoration            = "none"
```

This applies to interactive resizes only — a mouse-border drag, a resize
gesture, a touch resize. The `size` rule, fit/fullscreen, `driftwm msg resize`,
the `grow-window` / `shrink-window` bindings, and client-driven sizes are left
alone.

### Overlay that opens without taking focus

```toml
[[window_rules]]
title            = "my-hud"
pinned_to_screen = true
focus_on_open    = false
```

### Suppress a stray window titled "winit window"

Some iced/libcosmic apps (cosmic-term, etc.) open small utility windows that
share the main app_id but have a generic title:

```toml
[[window_rules]]
title  = "winit window"
widget = true
```

### On-screen keyboard above other overlays

Layer-shell clients on the same wlr-layer stack by launch order. `layer_order`
overrides that; higher is on top (see
[Layer-shell surfaces](#layer-shell-surfaces)):

```toml
[[window_rules]]
app_id      = "wvkbd"
layer_order = 10
```

## When rules take effect

Most rule effects — position, size, opacity, decoration, borders, widget,
pinned, output, … — are resolved **once, when a window maps**: reloading your
config only affects windows opened afterwards, and a window that changes its
title after mapping is **not** re-checked against `title` rules.

A few things re-resolve live against the current config instead: `pass_keys` is
evaluated per keypress and `pass_mouse` per button press and scroll (so a config
reload — and a title change — takes effect immediately), layer-surface chrome is
evaluated per frame (likewise),
`suspend_on_close` is evaluated when a window closes, and `restore_windows` when
the session is saved or loaded. On load, `restore_windows` is matched against a
saved record, which carries an `app_id` but no title, so only the `app_id`
criterion is consulted there (see
[session restore](session.md#restore_windows)).

## Debugging

Enable debug logging to see which rules matched a window at map time:

```sh
RUST_LOG=debug driftwm 2>&1 | grep -i "window rule\|app_id"
```
