# IPC

driftwm exposes a small IPC over a Unix domain socket so external tools and
scripts can query and control the running compositor. The `driftwm msg`
subcommand is the built-in client; the wire protocol is plain line-delimited
JSON, so any language can speak it directly.

## `driftwm msg`

Run `driftwm msg <command>` from inside a driftwm session. Each command reads
when given no arguments and writes when given arguments.

| Command          | Example                       | Description                                                                                          |
| ---------------- | ----------------------------- | ---------------------------------------------------------------------------------------------------- |
| `camera`         | `driftwm msg camera`          | Print the camera position (viewport center)                                                          |
| `camera <x> <y>` | `driftwm msg camera 500 300`  | Center the viewport on `(x, y)` (animated)                                                           |
| `zoom`           | `driftwm msg zoom`            | Print the zoom level                                                                                 |
| `zoom <level>`   | `driftwm msg zoom 0.5`        | Set zoom (animated); clamped to the supported range (out to fit-all, in to native)                   |
| `focus`          | `driftwm msg focus`           | Print the focused window's `app_id`                                                                  |
| `focus <app_id>` | `driftwm msg focus alacritty` | Focus a window by `app_id` substring (case-insensitive); navigates to it only if it's off-screen     |
| `move`           | `driftwm msg move`            | Print the focused window's position                                                                  |
| `move <x> <y>`   | `driftwm msg move 100 200`    | Move the focused window                                                                              |
| `layout`         | `driftwm msg layout`          | Print the active keyboard layout (full XKB name); `--short` prints the configured code (e.g. `ru`)   |
| `action <spec>`  | `driftwm msg action zoom-in`  | Run any config action (see [Actions](#actions))                                                      |
| `screenshot ...` | `driftwm msg screenshot`      | Capture the canvas to a PNG — current view, window, region, or all (see [Screenshots](#screenshots)) |
| `state`          | `driftwm msg state`           | Dump camera, zoom, and the window inventory                                                          |

Add `--json` to print the raw JSON reply instead of the human-readable form:

```bash
driftwm msg --json state
```

A command that fails (bad value, no focused window, no match) prints an error to
stderr and exits non-zero, so scripts can branch on it.

### Actions

`action` runs any compositor action by the same string you'd write in a config
keybinding — arguments and all. The compositor parses it with the exact parser
the config file uses, so every keybindable action is reachable:

```bash
driftwm msg action zoom-in
driftwm msg action nudge-window up
driftwm msg action go-to 100 200
driftwm msg action switch-layout next     # next | prev | <index>
driftwm msg action close-window           # close the focused window
driftwm msg action quit                   # shut the compositor down
```

The dedicated commands above are the state you can **read or set**; every
**one-shot** operation (closing a window, quitting, zooming a step) lives under
`action` rather than as its own command.

Window actions operate on the **focused** window, so to act on a specific one,
select it first: `driftwm msg focus alacritty && driftwm msg action close-window`.

Switching the keyboard layout is `action switch-layout next|prev|<index>`; read
the current layout with `layout` (full XKB name, e.g. `Russian`) or `layout
--short` for the configured code (e.g. `ru`) — what most status bars want. The
short form indexes the `input.keyboard.layout` list by the active group, so it
mirrors exactly what you configured.

`action` replies `Ok` whenever the spec **parses** — even if the action had no
effect (e.g. `close-window` with nothing focused). Only an unparseable spec
returns `Err`. The dedicated query/set commands above, by contrast, can report a
failed lookup or bad value.

> [!WARNING]
> The IPC socket is a full control surface, not a read-only one: `action` can
> run `exec`/`spawn` (launch programs), `quit`, and `reload-config`. It's safe
> only because the socket is `0600` (your user only) — anything that can open it
> could already run programs as you. So don't loosen the permissions or bridge it
> over a network for "just reading state": that hands arbitrary code execution to
> whoever reaches it.

### Coordinates

Window and camera positions use the same convention as
[window rules](window-rules.md) and the [state file](#state-file): positions are
a **center** point, with **Y pointing up**. So `move 0 0` centers the focused
window on the canvas origin, `camera 0 0` centers the _viewport_ on the origin,
and positive `y` is above it.

### Screenshots

`screenshot` is a **canvas capture**, not a screen grab: it re-renders a virtual
viewport onto the canvas (reaching off-screen content at any resolution) instead
of copying the framebuffer like `grim`. Windows get full chrome (title bar,
border, rounded corners, shadow). **Panels/layer-shells and blur aren't drawn** —
use `grim` for a literal screen grab.

```bash
driftwm msg screenshot                                # active output's current view
driftwm msg screenshot window                         # focused window, isolated
driftwm msg screenshot all --scale 2                  # all windows + background, 2× detail
driftwm msg screenshot region 0 0 2000 1500           # canvas rect (center, Y-up)
driftwm msg screenshot region $(slurp) --from-screen  # pick a region with slurp
driftwm msg screenshot window -o - | wl-copy          # capture window to clipboard
```

Targets: **no subcommand** = the active output's viewport (what you see, minus
panels); **`window`** = the focused window isolated on transparency; **`all`** /
**`region`** = a scene with the canvas background + every window's chrome (`all`
adds a `[zoom] fit_padding` margin).

- `--scale N` — pixels per canvas unit (default `1`); higher captures more detail
  than the screen shows, independent of zoom.
- `region X Y W H` — canvas coords (center, Y-up). With `--from-screen` they're
  screen pixels; slurp's native `X,Y WxH` is accepted, so `region $(slurp)` works.
- `-o PATH` — destination, or `-` for stdout (default
  `./driftwm-screenshot-<time>.png`); the written path is printed.

> [!NOTE]
> Caveats: no blur (a translucent window shows a sharp backdrop, not a blurred
> one); a gigapixel TIFF wallpaper uses a coarse pyramid level (softens at extreme
> `--scale`); captures tile internally but cap at 16384 px/side.

## Wire protocol

The socket path is `$XDG_RUNTIME_DIR/driftwm/ipc-<WAYLAND_DISPLAY>.sock`
(permissions `0600`). The name is derived from the compositor's `WAYLAND_DISPLAY`,
so each instance owns a distinct socket and a client launched inside a session
automatically targets that session. Set `DRIFTWM_SOCKET` to point a client at an
explicit path.

The protocol is one JSON **request** per line, answered by one JSON **reply**
per line. A single connection may carry several requests; the connection stays
open until the client closes it.

A reply is `{"Ok": <response>}` on success or `{"Err": "message"}` on failure.

### Requests

| Request          | JSON to send                                                              |
| ---------------- | ------------------------------------------------------------------------- |
| get / set camera | `{"Camera":null}` / `{"Camera":[500,300]}`                                |
| get / set zoom   | `{"Zoom":null}` / `{"Zoom":0.5}`                                          |
| get / set focus  | `{"Focus":null}` / `{"Focus":"alacritty"}`                                |
| get / set move   | `{"Move":null}` / `{"Move":[100,200]}`                                    |
| layout           | `{"Layout":{"short":false}}`                                              |
| run action       | `{"Action":"switch-layout next"}`                                         |
| screenshot       | `{"Screenshot":{"target":"Viewport","scale":1.0,"path":"/abs/shot.png"}}` |
| state            | `"State"`                                                                 |

### Responses

```json
{"Ok":{"Camera":{"x":500.0,"y":300.0}}}
{"Ok":{"Zoom":0.5}}
{"Ok":{"Layout":"English (US)"}}    // or "us" for {"Layout":{"short":true}}
{"Ok":{"Focused":"alacritty"}}      // or {"Ok":{"Focused":null}}
{"Ok":{"Position":{"x":100,"y":200}}}
{"Ok":"Ok"}                          // camera-set / move-set / action etc.
{"Ok":{"State":{"camera":[-960.0,-600.0],"zoom":1.0,"windows":[
  {"app_id":"foot","title":"~","position":[0,0],"size":[800,480],
   "is_focused":true,"is_widget":false}
]}}}
{"Err":"no focused window"}
```

The `windows` array is the same shape driftwm writes to its [state file](#state-file),
focused window first.

### Talking to the socket directly

```bash
SOCK="$XDG_RUNTIME_DIR/driftwm/ipc-$WAYLAND_DISPLAY.sock"

echo '"State"'            | socat -t1 - UNIX-CONNECT:"$SOCK"
echo '{"Camera":[500,300]}' | socat -t1 - UNIX-CONNECT:"$SOCK"
```

## State file

For read-only polling (status bars, scripts), driftwm also writes a throttled
(~10 Hz) snapshot to `$XDG_RUNTIME_DIR/driftwm/state` — `key=value` lines plus a
`windows=` JSON array using the same window shape as `state`. Reading that file
avoids a socket round-trip when you only need to observe.

Layer-shell clients appear too: `layers=` lists the namespaces of screen-space
layer surfaces (bars, OSKs, overlays — useful for finding the `app_id` a
window rule should match), and `canvas_layers=` is a JSON array of
canvas-positioned layers with their namespace, rule-coordinate `position`, and
`size` (the position reflects the current size, so it can drift from the
placing rule if the surface resized after mapping).

## Limitations

- There is no event/subscription stream yet — poll `state` or the state file.
