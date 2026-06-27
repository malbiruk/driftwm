# Window Rules

Window rules let you apply per-window overrides based on a window's identity.
Rules are declared as `[[window_rules]]` sections in your config file.

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

## Effect fields

The table below describes how each field behaves on rules matching regular
windows (xdg-toplevels). Layer-shell surfaces interpret chrome fields
differently — see [Layer-shell surfaces](#layer-shell-surfaces) below.

| Field                  | Type                     | Default   | Description                                                                                                                                  |
| ---------------------- | ------------------------ | --------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `position`             | `[x, y]`                 | —         | Place window at canvas coordinates (window center, Y-up). Output-relative (origin = output center) when `pinned_to_screen` is set.           |
| `size`                 | `[w, h]`                 | —         | Initial window dimensions in pixels (one-shot: the user/app can resize freely afterwards; pair with `widget = true` to keep the size locked) |
| `widget`               | `bool`                   | `false`   | Pin window: immovable, below normal windows, excluded from navigation/alt-tab                                                                |
| `pinned_to_screen`     | `bool`                   | `false`   | Pin to one output's screen space — see [Screen-pinned windows](#screen-pinned-windows)                                                       |
| `decoration`           | string                   | inherited | Override decoration mode (see below)                                                                                                         |
| `blur`                 | `bool`                   | `false`   | Blur compositor background behind this window                                                                                                |
| `opacity`              | `0.0`–`1.0`              | `1.0`     | Window transparency (1.0 = fully opaque)                                                                                                     |
| `border_width`         | px                       | inherited | Border width override. Set to `0` to disable the border even when global width is `> 0`. Ignored for `decoration = "none"`.                  |
| `border_color`         | `"#rrggbb[aa]"`          | inherited | Per-window unfocused border color                                                                                                            |
| `border_color_focused` | `"#rrggbb[aa]"`          | inherited | Per-window focused border color                                                                                                              |
| `corner_radius`        | px                       | inherited | Per-window corner radius override. Affects content clip, border shape, and shadow. Ignored for `decoration = "none"`.                        |
| `shadow`               | `bool`                   | inherited | Per-window shadow toggle. Overrides `[decorations] shadow`. Ignored for `decoration = "none"`.                                               |
| `output`               | string                   | —         | Output name (e.g. `"DP-1"`) this window fullscreens onto — see [Fullscreen output](#fullscreen-output)                                       |
| `pass_keys`            | `bool` or `["combo", …]` | `false`   | Forward keys to the app — see below                                                                                                          |

> [!WARNING]
> Blur has real GPU/VRAM cost. Results are cached and only recomputed when
> the content behind a window changes, but the cost does **not** scale down with
> zoom — a blurred window is processed at full resolution no matter how far you
> zoom out. Many blurred windows, or a few at extreme zoom-out, can stutter and
> consume significant VRAM. Prefer enabling `blur` on a handful of windows over
> applying it globally. There's room to improve this further.

### `decoration` values

| Value       | Description                                                                                              |
| ----------- | -------------------------------------------------------------------------------------------------------- |
| `"client"`  | CSD — client draws its own titlebar (default)                                                            |
| `"server"`  | SSD — driftwm draws a titlebar with the window title and a close button                                  |
| `"minimal"` | SSD — no titlebar; shadow, corner clip, and border still apply per `[decorations]` / per-window rules    |
| `"none"`    | Bare client surface — compositor adds zero chrome; per-window border/corner/shadow rules are ignored too |

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

`position`/`size` are screen-space. The easiest way is to pin the window live,
drag it where you want, then read the numbers back to bake into a rule:

1. Open the window (e.g. start Picture-in-Picture), click it to focus, and press
   `Mod+T` to pin it. It's now in screen space — drag it anywhere with the mouse
   and resize to taste.
2. Once it sits where you want, press `Mod+A` (home) to bring the viewport to the
   canvas origin at zoom 1.0. The pinned window stays put on screen.
3. Press `Mod+T` again to unpin. At home the window drops back onto the canvas at
   the same on-screen spot, and one canvas unit is one screen pixel, so its
   canvas coords now equal its screen position.
4. Run `driftwm msg state` and copy the window's `app_id`, `title`, `position`,
   and `size`.
5. Write the rule with those values plus `pinned_to_screen = true` (and
   `decoration = "none"` for a chrome-free PiP surface).

```toml
[[window_rules]]
title            = "Picture-in-Picture"
pinned_to_screen = true
position         = [540, -350]
size             = [570, 320]
decoration       = "none"
```

You have to unpin before reading (step 3): pinned windows live in screen space,
not on the canvas, so — like layer-shell panels — they're deliberately absent
from `driftwm msg state` and canvas screenshots. If you run top/bottom bars,
`Mod+A` centers the _usable_ area rather than the raw output, so the result can
sit a little off — nudge `position` to taste.

### Fullscreen output

On a multi-monitor setup, `output` chooses which monitor a window fullscreens
onto, by output name (e.g. `"DP-1"` — find names under `outputs.*` in
`driftwm msg state`):

```toml
[[window_rules]]
app_id = "steam_app_*"
output = "DP-1"
```

Precedence when a window goes fullscreen: the rule's `output` wins; otherwise the
output the client itself requested; otherwise the active output (where the
pointer is). An unknown or disconnected output name falls through to the next
choice. `output` only affects fullscreen — it does not move a windowed or
screen-pinned window.

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

## Debugging

Enable debug logging to see which rules matched a window at map time:

```sh
RUST_LOG=debug driftwm 2>&1 | grep -i "window rule\|app_id"
```
