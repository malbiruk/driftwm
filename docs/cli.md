# CLI reference

<!-- Generated from the clap command tree — do not edit by hand.
     Regenerate with: UPDATE_CLI_DOCS=1 cargo test docs_cli_md_is_up_to_date -->

driftwm's command-line interface: the root command that starts the compositor,
and every `driftwm msg` subcommand for controlling a running one. For the raw
JSON wire protocol behind `msg`, see [ipc.md](ipc.md).

## `driftwm`

```
driftwm [OPTIONS] [COMMAND]
```

A trackpad-first infinite canvas Wayland compositor.

With no subcommand, starts the compositor, auto-detecting the backend (udev on a TTY, winit when nested). The `msg` subcommand instead talks to an already-running instance over its IPC socket.

- `--backend <udev|winit>` — Backend to use (default: udev on a TTY, winit if nested)
- `--config <PATH>` — Use an alternate config file
- `--check-config` — Validate the config and exit
- `--session-file <PATH>` — Durable session file path. Overrides the default; lets a nested winit dev session opt into session restore (it skips it otherwise)
- `-V, --version` — Print version

```bash
driftwm --backend winit --config ~/dev.toml
```

### `driftwm msg`

```
driftwm msg [OPTIONS] <COMMAND>
```

Send a command to the running compositor over its IPC socket.

Auto-targets the instance named by `WAYLAND_DISPLAY` (override with `DRIFTWM_SOCKET`).

A window command selects its target by `app_id` substring (case-insensitive) or by `--id <n>`, the stable id `state` prints. Widgets match no `app_id` search — reach one by `--id`.

Add `--json` for the raw JSON reply. A command that fails (bad value, no match, no focused window) prints an error to stderr and exits non-zero, so scripts can branch on it.

- `--json` — Print the raw JSON reply

```bash
driftwm msg state
driftwm msg --json focus --id 5
```

| Command | Description |
| --- | --- |
| [`state`](#driftwm-msg-state) | Dump camera, zoom, and the window inventory |
| [`subscribe`](#driftwm-msg-subscribe) | Stream state snapshots as they change (one JSON line per event with --json) |
| [`focus`](#driftwm-msg-focus) | Print the focused window, or focus one by `app_id` substring or `--id` |
| [`move`](#driftwm-msg-move) | Get a window's position, or move it to `<x> <y>` (visible-frame center, Y-up) |
| [`resize`](#driftwm-msg-resize) | Get a window's size, or resize it to `<width> <height>` |
| [`close`](#driftwm-msg-close) | Close the focused window, or one by `app_id` substring or `--id` |
| [`opacity`](#driftwm-msg-opacity) | Get a window's opacity, or set it with `<value>` — `0` transparent, `1` opaque |
| [`suspend`](#driftwm-msg-suspend) | Suspend the focused window, or one by `app_id` substring or `--id` |
| [`relaunch`](#driftwm-msg-relaunch) | Relaunch a suspended window: the focused stand-in, or one by `app_id` substring or `--id` |
| [`camera`](#driftwm-msg-camera) | Get the camera position, or pan the viewport to `<x> <y>` (canvas point, Y-up) |
| [`zoom`](#driftwm-msg-zoom) | Get the zoom level, or set it with `<level>` |
| [`bookmark`](#driftwm-msg-bookmark) | List bookmarks, get or set one by `<name>`, or delete with `--delete` |
| [`layout`](#driftwm-msg-layout) | Print the active keyboard layout (full XKB name, e.g. `English (US)`) |
| [`action`](#driftwm-msg-action) | Run a config action, e.g. `action close-window`, `action switch-layout next` |
| [`screenshot`](#driftwm-msg-screenshot) | Capture a canvas PNG. With no subcommand, captures the active output's current view of the canvas |
| [`debug-counters`](#driftwm-msg-debug-counters) | Print internal collection sizes for leak diagnosis (unstable keys) |

#### `driftwm msg state`

```
driftwm msg state
```

Dump camera, zoom, and the window inventory.

Also prints the keyboard layout, the fullscreen and pinned screen-space inventories, layer-shell namespaces, and each output's viewport. Every window entry carries the stable `id` other commands take as a selector.

`--json` reply: `{"Ok":{"State":{"camera":[..],"zoom":1.0,"windows":[..],"outputs":[..]}}}`.

```bash
driftwm msg --json state | jq '.Ok.State.windows'
```

#### `driftwm msg subscribe`

```
driftwm msg subscribe
```

Stream state snapshots as they change (one JSON line per event with --json).

The server acks, pushes the current state immediately, then pushes a fresh snapshot on any change to it. While something animates that is one event per rendered frame (not throttled like the state file), so a pan or drag streams at the compositor's frame rate. Runs until interrupted.

Each event is `{"State":{..}}` — the whole snapshot, same shape as the `state` reply, and not wrapped in `Ok`/`Err`. A slow subscriber never blocks the compositor: it drops snapshots and catches up in full on the next change.

```bash
driftwm msg --json subscribe | jq --unbuffered -r '.State.zoom'
```

#### `driftwm msg focus`

```
driftwm msg focus [OPTIONS] [APP_ID]
```

Print the focused window, or focus one by `app_id` substring or `--id`.

Focusing pans the camera to the window unless it is already fully visible. Widgets cannot be focused.

`--json` reply: `{"Ok":{"Focused":{"id":5,"app_id":"alacritty"}}}` (or `{"Ok":{"Focused":null}}`).

- `--id <ID>` — Target this window id

```bash
driftwm msg focus firefox
driftwm msg focus --id 5
```

#### `driftwm msg move`

```
driftwm msg move [OPTIONS] [X] [Y]
```

Get a window's position, or move it to `<x> <y>` (visible-frame center, Y-up).

Pinned and fullscreen windows live in screen space, not on the canvas, so `move` refuses to reposition them.

`--json` reply: `{"Ok":{"Position":{"x":100,"y":200}}}`.

- `--id <ID>` — Target this window id

```bash
driftwm msg move
driftwm msg move -400 200 --id 5
```

#### `driftwm msg resize`

```
driftwm msg resize [OPTIONS] [WIDTH] [HEIGHT]
```

Get a window's size, or resize it to `<width> <height>`.

Dimensions are the visible frame, including any compositor-drawn title bar and border. A request is clamped to the client's declared limits — which describe the content inside that frame — and the reply echoes what was configured, not what the client went on to commit. Refused for pinned and fullscreen windows as with `move`, and while the window is under an interactive move or resize.

`--json` reply: `{"Ok":{"Size":{"width":800,"height":600}}}`.

- `--id <ID>` — Target this window id

```bash
driftwm msg resize
driftwm msg resize 800 600 --id 5
```

#### `driftwm msg close`

```
driftwm msg close [OPTIONS] [APP_ID]
```

Close the focused window, or one by `app_id` substring or `--id`.

Errors when nothing matches.

`--json` reply: `{"Ok":"Ok"}`.

- `--id <ID>` — Target this window id

```bash
driftwm msg close firefox
```

#### `driftwm msg opacity`

```
driftwm msg opacity [OPTIONS] [VALUE]
```

Get a window's opacity, or set it with `<value>` — `0` transparent, `1` opaque.

Runtime-only: seeded from an `opacity` window rule, lost when the window or the compositor restarts. Out-of-range values are rejected. Default `1`.

`--json` reply: `{"Ok":{"Opacity":0.85}}`.

- `--id <ID>` — Target this window id

```bash
driftwm msg opacity 0.85 --id 5
```

#### `driftwm msg suspend`

```
driftwm msg suspend [OPTIONS] [APP_ID]
```

Suspend the focused window, or one by `app_id` substring or `--id`.

The same conversion as the `suspend-window` action: the client goes away and a compositor-drawn stand-in holds its place, to be brought back with `relaunch`, `Enter`, or a click.

`--json` reply: `{"Ok":"Ok"}`.

- `--id <ID>` — Target this window id

```bash
driftwm msg suspend firefox
```

#### `driftwm msg relaunch`

```
driftwm msg relaunch [OPTIONS] [APP_ID]
```

Relaunch a suspended window: the focused stand-in, or one by `app_id` substring or `--id`.

Spawns the app from its `.desktop` entry and adopts the new window into the stand-in's slot on its first sized commit. Acts only on stand-ins, so an `app_id` substring never resolves to a live client. Errors when nothing matches.

`--json` reply: `{"Ok":"Ok"}`.

- `--id <ID>` — Target this window id

```bash
driftwm msg relaunch firefox
```

#### `driftwm msg camera`

```
driftwm msg camera [X] [Y]
```

Get the camera position, or pan the viewport to `<x> <y>` (canvas point, Y-up).

Panning is animated, and takes both coordinates or neither.

`--json` reply: `{"Ok":{"Camera":{"x":500.0,"y":300.0}}}`.

```bash
driftwm msg camera
driftwm msg camera 500 300
```

#### `driftwm msg zoom`

```
driftwm msg zoom [LEVEL]
```

Get the zoom level, or set it with `<level>`.

Setting is animated and clamped: out to fit-all, in to native resolution (no magnification).

`--json` reply: `{"Ok":{"Zoom":0.5}}`.

```bash
driftwm msg zoom 0.5
```

#### `driftwm msg bookmark`

```
driftwm msg bookmark [OPTIONS] [NAME] [X] [Y]
```

List bookmarks, get or set one by `<name>`, or delete with `--delete`.

Coordinates are canvas points, Y-up and window-center, the same convention as `move`; setting an existing name overwrites it. A bookmark stores a position only, never zoom — jump to one with the `go-to-bookmark` action or a `mod+<n>` keybinding.

`--json` reply: `{"Ok":{"Bookmark":{"x":500.0,"y":300.0}}}` (get/set), or `{"Ok":{"Bookmarks":{"home":[0.0,0.0]}}}` (list), or `{"Ok":"Ok"}` (delete).

- `[NAME]` — Bookmark name. Omit to list every bookmark
- `[X]` — X coordinate (Y-up). Requires `<y>`
- `[Y]` — Y coordinate (Y-up)
- `--delete` — Delete the named bookmark

```bash
driftwm msg bookmark
driftwm msg bookmark inbox 500 300
driftwm msg bookmark inbox --delete
```

#### `driftwm msg layout`

```
driftwm msg layout [OPTIONS]
```

Print the active keyboard layout (full XKB name, e.g. `English (US)`).

`--json` reply: `{"Ok":{"Layout":"English (US)"}}` (or `"us"` with `--short`).

- `--short` — Print the configured code for the active group instead (e.g. `us`, `ru`)

```bash
driftwm msg layout --short
```

#### `driftwm msg action`

```
driftwm msg action <SPEC>...
```

Run a config action, e.g. `action close-window`, `action switch-layout next`.

Takes the same string you would write in a config keybinding, parsed with the config parser, so every keybindable action is reachable here. Replies `Ok` whenever the spec parses — even when it had no effect (e.g. `close-window` with nothing focused); only an unparseable spec errors.

Window actions act on the focused window, so `focus` the target first, or pass `--id` to a command that takes it.

The socket is a full control surface: `action` can `exec`/`spawn`, `quit`, and `reload-config`. It is safe only because the socket is `0600`.

`--json` reply: `{"Ok":"Ok"}`.

- `<SPEC>...` — Action and arguments, exactly as written in config (e.g. `nudge-window up`)

```bash
driftwm msg action switch-layout next
driftwm msg action toggle-fullscreen
```

#### `driftwm msg screenshot`

```
driftwm msg screenshot [OPTIONS] [COMMAND]
```

Capture a canvas PNG. With no subcommand, captures the active output's current view of the canvas.

A canvas capture, not a screen grab: it re-renders a virtual viewport onto the canvas, reaching off-screen content at any resolution. Windows get full chrome (title bar, border, shadow); panels/layer-shells and blur are not drawn (use `grim` for a literal grab). `-o -` streams the PNG to stdout.

Blur caveat: a scene capture (viewport/`all`/`region`) shows a translucent window over a sharp backdrop, never a blurred one; a `window` capture keeps the translucency over transparent pixels. A gigapixel TIFF wallpaper uses a coarse pyramid level, softening at extreme `--scale`. Captures tile internally but cap at 16384 px/side.

`--json` reply: `{"Ok":{"Screenshot":{"path":"/abs/shot.png","width":1920,"height":1080}}}`.

- `--scale <SCALE>` — Pixels per canvas unit — higher captures more detail than the screen shows, independent of zoom (default: `1`)
- `-o, --output <OUTPUT>` — Output PNG path, or `-` for stdout (default: `./driftwm-screenshot-<time>.png`)

```bash
driftwm msg screenshot --scale 2 -o ~/canvas.png
```

##### `driftwm msg screenshot window`

```
driftwm msg screenshot window [OPTIONS] [APP_ID]
```

The focused window, or one by `app_id` substring or `--id`.

Composed alone on transparency, so overlapping windows never appear; pinned and fullscreen windows capture like any other (a fullscreen window has no chrome). `--json` reply shape is the shared `Screenshot` reply above.

- `--id <ID>` — Target this window id

```bash
driftwm msg screenshot window -o - | wl-copy
```

##### `driftwm msg screenshot all`

```
driftwm msg screenshot all
```

The bounding box of all non-widget windows.

A scene with the canvas background plus every window's chrome, framed with a `[zoom] fit_padding` margin. `--json` reply shape is the shared `Screenshot` reply above.

```bash
driftwm msg screenshot all --scale 0.5
```

##### `driftwm msg screenshot region`

```
driftwm msg screenshot region [OPTIONS] <COORDS>...
```

A rectangle — `X Y W H` (canvas coords, center/Y-up) or slurp's native `X,Y WxH`. Commas and the `x` separator are tolerated, so `$(slurp)` drops in directly. Treated as output-screen pixels with `--from-screen`.

Captures a scene (canvas background plus window chrome) over the rectangle. `--json` reply shape is the shared `Screenshot` reply above.

- `<COORDS>...` — Four ints `X Y W H`, or slurp's `X,Y WxH` (quoted or not)
- `--from-screen` — Treat the rectangle as output-screen pixels mapped via the active viewport

```bash
driftwm msg screenshot region -1000 -500 2000 1000
driftwm msg screenshot region "$(slurp)" --from-screen
```

#### `driftwm msg debug-counters`

```
driftwm msg debug-counters
```

Print internal collection sizes for leak diagnosis (unstable keys).

Keys are internal field names and change between releases; don't script against them. A window/surface/client-keyed count should return to its idle baseline once the windows and clients that raised it are gone (output-keyed counters follow output lifetimes instead and can persist across hotplug).

`--json` reply: `{"Ok":{"DebugCounters":{"decorations":2,"stage_entries":2}}}`.

```bash
driftwm msg debug-counters
```
