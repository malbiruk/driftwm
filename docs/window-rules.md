# Window Rules

Window rules let you apply per-window overrides based on a window's identity.
Rules are declared as `[[window_rules]]` sections in your config file.

Most rule effects — position, size, opacity, decoration, borders, widget,
pinned, output, … — are resolved **once, when a window maps**: reloading your
config only affects windows opened afterwards, and a window that changes its
title after mapping is **not** re-checked against `title` rules. Two things
re-resolve live against the current config instead: `pass_keys` is evaluated per
keypress (so a config reload — and a title change — takes effect immediately),
and layer-surface chrome is evaluated per frame (so a config reload takes effect
immediately).

## How matching works

**All matching rules are applied, not just the first one.** Rules are processed
in config order and merged together:

- **Scalar fields** (`decoration`, `opacity`, `position`, `size`,
  `border_width`, `border_color`, `border_color_focused`, `corner_radius`,
  `shadow`): last-wins — a later rule overrides an earlier one.
- **Boolean flags** (`widget`, `blur`): sticky-on — once set by
  any matching rule, the flag stays set regardless of later rules.
- **`pass_keys`**: `All` is sticky-on; `Only` lists are unioned across
  rules (see [pass_keys details](#pass_keys-details)).

This lets you compose independent rules for the same window:

```toml
# Rule 1: make kitty blur its background
[[window_rules]]
app_id = "kitty"
blur   = true

# Rule 2: also make it semi-transparent (blur from Rule 1 is kept)
[[window_rules]]
app_id  = "kitty"
opacity = 0.85
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
jq '.Ok.State.windows[] | select(.is_widget == false) | {app_id, title}'
```

## Pattern syntax

All match fields support three syntaxes:

| Syntax       | Example                | Meaning                                 |
| ------------ | ---------------------- | --------------------------------------- |
| Exact string | `"kitty"`              | Exact match (case-sensitive)            |
| Glob         | `"steam_app_*"`        | `*` matches any sequence of chars       |
| Regex        | `"/^steam_app_\\d+$/"` | Full regular expression (wrap in `/…/`) |

Multiple `*` wildcards are allowed in glob patterns: `"*terminal*"`.

Regex patterns use the `regex` crate (RE2-compatible, no backreferences).

## Field reference

Every rule field — its type, default, accepted values, and per-field caveats
(which fields `decoration = "none"` ignores, the blur GPU/VRAM cost, the one-shot
`size`, how layer-shell surfaces opt into chrome) — is documented in the generated
[config reference](config.md#window-rules), whose canonical source is
[`config.reference.toml`](../config.reference.toml). This page is the recipe and
semantics guide; the reference is the field dictionary.

Layer-shell surfaces interpret chrome fields differently — see
[Layer-shell surfaces](#layer-shell-surfaces) below.

### Screen-pinned windows

`pinned_to_screen = true` lifts a window out of the infinite canvas and fixes it
to one output's **screen space**: it does not pan or zoom with the camera, and it
renders **above** normal windows (but below panels / Top & Overlay layer-shell
surfaces). Use it for Picture-in-Picture, video-call toolbars, or any always-on
floating overlay.

- **Coordinates are output-relative.** When pinned, `position` is measured from
  the **output center** (still center-anchored and Y-up): `[0, 0]` centers the
  window on the monitor, `+Y` is up. Drop `position` to center it.
- **Movable and resizable** like a normal window — drag the title bar (or
  `Mod`-drag) to move, drag a border to resize. Dragging across monitors
  reassigns it to that output. Combine with `widget = true` to make it
  **immovable**.
- **Fullscreen round-trips.** A fullscreen request (or `Mod+F`) temporarily
  unpins the window to fill the screen; exiting fullscreen re-pins it in place.
  Any canvas pan/zoom exits fullscreen, just like a normal window.
- **Off the canvas.** Pinned windows are excluded from navigation, alt-tab,
  snapping, fit/center actions, and canvas screenshots
  (`driftwm msg screenshot`). They remain focusable and closable; SSD windows
  show a small dot in the title bar.
- **Toggle at runtime** with the `toggle-pin-to-screen` action (bound to `Mod+T`
  by default), which pins/unpins the focused window in place.

#### Finding a pinned window's position and size

`driftwm msg state` already reports a pinned window's `position`/`size` in rule
coordinates, so the flow is: pin the window live, place it, and copy the numbers
straight into a rule:

1. Open the window (e.g. start Picture-in-Picture), click it to focus, and press
   `Mod+T` to pin it. It's now in screen space — drag it anywhere with the mouse
   and resize to taste.
2. Run `driftwm msg state` and read the `pinned` section: each entry lists its
   output, `app_id`, `title`, `position`, and `size`. Those `position`/`size`
   values are already output-relative rule coordinates.
3. Write the rule with those `position`/`size` values plus
   `pinned_to_screen = true` (and `decoration = "none"` for a chrome-free PiP
   surface).

```toml
[[window_rules]]
title            = "Picture-in-Picture"
pinned_to_screen = true
position         = [540, -350]
size             = [570, 320]
decoration       = "none"
```

Pinned windows stay absent from the canvas `windows=` inventory and from canvas
screenshots (`driftwm msg screenshot`) — like layer-shell panels, they live in
screen space, not on the canvas. They appear in their own per-output `pinned`
section of `driftwm msg state` instead, which is where the copy-ready numbers
come from.

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

### `pass_keys` details

`pass_keys` controls which compositor keybindings are forwarded to the focused
window instead of being handled by the compositor:

| Value                 | Behaviour                                                                         |
| --------------------- | --------------------------------------------------------------------------------- |
| `false` (or omit)     | Compositor handles all keybindings normally (default)                             |
| `true`                | **All** keys forwarded — no compositor shortcuts fire while this window has focus |
| `["mod+q", "ctrl+q"]` | **Only** the listed combos are forwarded; all other shortcuts stay active         |

VT switching (`Ctrl+Alt+F1`–`F12`) **always stays in the compositor** regardless
of `pass_keys`.

Key combo syntax is the same as in `[keybindings]`: `mod+key`, `ctrl+shift+key`, etc.

When multiple rules match the same window:

- `true` is sticky-on: if **any** rule sets `pass_keys = true`, the result is `true`.
- `["combo", …]` lists are **unioned** across all matching rules.
- `true` overrides a list: if one rule says `true` and another says `["mod+q"]`, the result is `true`.

## Examples

### Desktop widget (pinned clock/info panel)

```toml
[[window_rules]]
app_id     = "my-widget"
position   = [0, 0]
widget     = true
decoration = "none"
```

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

### Match any Steam game by regex

```toml
[[window_rules]]
app_id    = "/^steam_app_\\d+$/"
pass_keys = true
```

### Initial size and position for a floating panel

```toml
[[window_rules]]
app_id   = "myapp-panel"
size     = [400, 800]
position = [960, 0]
widget   = true
```

### Composing rules (multi-rule merge)

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

### Widget with a custom border and shadow

`decoration = "minimal"` gives you a titlebar-less window that still participates
in compositor chrome — borders, corner clipping, and shadow all apply. Use it
when you want a widget that isn't fully bare. `decoration = "none"` is the
opposite: a bare client surface where the compositor adds (and ignores) all
chrome overrides.

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

### Suppress iced/libcosmic utility popups

Some apps (cosmic-term, etc.) open small utility windows that share the main
app_id but have a generic title:

```toml
[[window_rules]]
title  = "winit window"
widget = true
```

### On-screen keyboard above other overlays

Overlay layer-shell clients that share a wlr-layer (an on-screen keyboard, a touch
visualizer) otherwise stack by launch order; a higher `layer_order` keeps this one
on top (see [Layer-shell surfaces](#layer-shell-surfaces)):

```toml
[[window_rules]]
app_id      = "wvkbd"
layer_order = 10
```

## Debugging

Enable debug logging to see which rules matched a window at map time:

```sh
RUST_LOG=debug driftwm 2>&1 | grep -i "window rule\|app_id"
```
