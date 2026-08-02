# IPC

driftwm exposes a small IPC over a Unix domain socket so external tools and
scripts can query and control the running compositor. The `driftwm msg`
subcommand is the built-in client; the wire protocol is plain line-delimited
JSON, so any language can speak it directly.

## `driftwm msg`

Run `driftwm msg <command>` from inside a driftwm session. The commands —
`camera`, `zoom`, `focus`, `move`, `resize`, `opacity`, `close`, `suspend`,
`relaunch`, `layout`, `action`, `bookmark`, `screenshot`, `state`, `subscribe`,
and `debug-counters` — with their arguments, flags, and JSON reply shapes are
documented in the generated [CLI reference](cli.md); `driftwm msg <command> --help`
prints the same for one command. The conventions they share follow below.

`camera`, `zoom`, `focus`, `move`, `resize`, `opacity`, and `bookmark` read when
given no arguments and write when given arguments. The others don't follow that rule:
`action` requires its arguments, `close`/`suspend`/`relaunch` act on the focused
window when given no selector, and `layout`, `screenshot`, `state`, `subscribe`,
and `debug-counters` need no arguments at all. A command that fails (bad value,
no focused window, no match) prints an error to stderr and exits non-zero, so
scripts can branch on it.

Add `--json` to print the raw JSON reply. The default output is a human-readable
rendering for a terminal, not a stable format — parse `--json`, or the
[state file](#state-file), in scripts.

### Coordinates

Window and camera positions use the same convention as
[window rules](window-rules.md) and the [state file](#state-file): a **center**
point, with **Y pointing up**. `move 0 0` centers the focused window on the
origin, `camera 0 0` centers the viewport there, and positive `y` is above.
Pinned and fullscreen windows live in screen space, not on the canvas, so `move`
refuses to reposition them and `resize` refuses to resize them. Reading either
still works.

### Sizes

`resize` dimensions are the client's own content, in the same canvas units as
`state`'s `size`. They **exclude** the compositor-drawn title bar and border, so
a script laying out a grid from them overlaps by that chrome on server-decorated
windows.

A request is clamped to the client's declared minimum and maximum, and the reply
echoes what was configured — not what the client went on to commit, which is why
the reply is a request rather than a fact (see [Responses](#responses)). Read it
back with a bare `resize` once the window has settled to see what it actually
has. The window keeps its visual center, computed from the size being requested:
a client that only accepts whole character cells therefore lands up to half its
rounding off center. That error stays bounded — repeating a request changes
nothing, and the next one re-derives from the size last asked for.
`preserve_aspect_ratio` does not apply — it governs interactive resizes only.

`resize` is refused while a window is under an interactive move or resize, since
the live grab recomputes its rect on every motion tick and would erase the
result — and for the frame after a drag ends, until the client commits the size
it was dragged to.

One case discards the centering: if a client commits a size *larger* than the one
it was asked for and that footprint collides with a snapped neighbour, the
compositor relocates **the grown window itself** (not the neighbour) beside its
cluster, and pans the camera after it when it is focused and no longer fully
visible.

Stand-ins differ in two ways. They have no client to declare limits, so
`resize` clamps them to a fixed 120x120 floor instead. And a stand-in left by a
client-decorated window stores the body *without* the title bar it draws over
it, so a `relaunch` after `resize --id <stand-in> 800 600` brings the app back at
`800 x (600 + title_bar_height)`.

### Suspended windows

A [suspended window](session.md) — the compositor-drawn stand-in left behind by
`suspend-window` or `suspend_on_close` — appears in the `windows` inventory with
its own `id` and `suspended: true`. `focus`, `move`, `resize`, and `close`
(which dismisses the stand-in rather than asking a nonexistent client to close)
take that id like any window's; `suspend <selector>` turns a live window into a
stand-in, and `relaunch <selector>` starts its app again.

When a live client and a stand-in share an `app_id`, an app_id selector resolves
to the **live client** — target the stand-in by its `id`. `relaunch` is the
exception: it only ever acts on stand-ins, so an app_id selector there resolves
straight to the matching one.

Between a `relaunch` and the new window's first *sized* commit (when it takes
over the stand-in's slot), a snapshot lists both entries — count windows by
`id`, not `app_id`.

### Screenshots

`screenshot` re-renders the canvas rather than copying the framebuffer, so it
reaches off-screen content; its four targets, flags, and caveats are in the
[CLI reference](cli.md#driftwm-msg-screenshot) and their wire forms in
[Requests](#requests).

### Subscribing to changes

`subscribe` turns the connection into a live feed instead of polling: one event
per change (per rendered frame while something animates), each a whole-state
snapshot rather than a granular event type (window-opened, focus-changed, …) —
diff consecutive snapshots if you need the delta. Nothing is pushed while
nothing changes. Mechanics are in the
[CLI reference](cli.md#driftwm-msg-subscribe); the wire-level event shape is
under [Events](#events) below.

A one-liner that prints the focused window's `app_id` whenever anything changes:

```bash
driftwm msg --json subscribe \
  | jq --unbuffered -r '.State.windows[] | select(.is_focused) | .app_id'
```

A small daemon that dims whatever loses focus and restores full opacity to
whatever gains it (a snapshot arrives per rendered frame during a pan, so the
focused id is deduped against the last one seen):

```bash
prev=
driftwm msg --json subscribe \
  | jq --unbuffered -r '.State.windows[] | select(.is_focused) | .id' \
  | while read -r id; do
      [ "$id" = "$prev" ] && continue   # same focus, skip repeats
      [ -n "$prev" ] && driftwm msg opacity 0.7 --id "$prev"
      driftwm msg opacity 1 --id "$id"
      prev=$id
  done
```

### Debug counters

`debug-counters` reports the sizes of the compositor's internal per-window,
per-surface, and per-client collections; its keys are **unstable** internal
field names. See the [CLI reference](cli.md#driftwm-msg-debug-counters).

## Wire protocol

The socket path is `$XDG_RUNTIME_DIR/driftwm/ipc-<WAYLAND_DISPLAY>.sock`
(permissions `0600`). The name is derived from the compositor's `WAYLAND_DISPLAY`,
so each instance owns a distinct socket and a client launched inside a session
automatically targets that session. Set `DRIFTWM_SOCKET` to point a client at an
explicit path.

> [!WARNING]
> The socket is a full control surface, not a read-only one: `action` can run
> `exec`/`spawn`, `quit`, and `reload-config`, and `relaunch` launches a
> suspended window's app. It is `0600` for that reason — don't loosen the
> permissions or bridge it over a network.

The protocol is one JSON **request** per line, answered by one JSON **reply**
per line. A single connection may carry several requests; the connection stays
open until the client closes it.

A reply is `{"Ok": <response>}` on success or `{"Err": "message"}` on failure.

A window can be targeted by a **selector**: a JSON number is its stable `id`
(from `state`), a JSON string is a case-insensitive `app_id` substring.

### Requests

| Request           | JSON to send                                                                        |
| ----------------- | ----------------------------------------------------------------------------------- |
| get / set camera  | `{"Camera":null}` / `{"Camera":[500,300]}`                                          |
| get / set zoom    | `{"Zoom":null}` / `{"Zoom":0.5}`                                                    |
| get / set focus   | `{"Focus":null}` / `{"Focus":"alacritty"}` / `{"Focus":5}`                          |
| get / set move    | `{"Move":{}}` / `{"Move":{"window":5,"to":[100,200]}}` (both optional)              |
| get / set resize  | `{"Resize":{}}` / `{"Resize":{"window":5,"to":[800,600]}}` (both optional)          |
| get / set opacity | `{"Opacity":{}}` / `{"Opacity":{"window":5,"value":0.5}}` (both optional)           |
| close             | `{"Close":null}` / `{"Close":5}` / `{"Close":"alacritty"}`                          |
| suspend           | `{"Suspend":null}` / `{"Suspend":5}` / `{"Suspend":"alacritty"}`                    |
| relaunch          | `{"Relaunch":null}` / `{"Relaunch":5}` / `{"Relaunch":"alacritty"}`                 |
| layout            | `{"Layout":{"short":false}}`                                                        |
| run action        | `{"Action":"switch-layout next"}`                                                   |
| bookmark          | `{"Bookmark":{}}` (list) / `{"Bookmark":{"name":"home"}}` (get) / `{"Bookmark":{"name":"home","to":[0,0]}}` (set) / `{"Bookmark":{"name":"home","delete":true}}` (delete) |
| screenshot        | `{"Screenshot":{"target":"Viewport","scale":1.0,"path":"/abs/shot.png"}}` (all three fields required; `path` must be absolute) |
| screenshot target | `"Viewport"` / `"All"` / `{"Window":{}}` / `{"Window":{"window":5}}` (selector optional) / `{"Region":{"x":0,"y":0,"w":640,"h":480,"from_screen":false}}` (all five required) |
| state             | `"State"`                                                                           |
| subscribe         | `"Subscribe"`                                                                       |
| debug counters    | `"DebugCounters"` (reply keys are unstable — see [Debug counters](#debug-counters)) |

### Responses

```json
{"Ok":{"Camera":{"x":500.0,"y":300.0}}}
{"Ok":{"Zoom":0.5}}
{"Ok":{"Layout":"English (US)"}}    // or "us" for {"Layout":{"short":true}}
{"Ok":{"Focused":{"id":5,"app_id":"alacritty"}}}   // or {"Ok":{"Focused":null}}
{"Ok":{"Position":{"x":100,"y":200}}}
{"Ok":{"Size":{"width":800,"height":600}}}
{"Ok":{"Opacity":0.85}}
{"Ok":{"Bookmark":{"x":500.0,"y":300.0}}}   // bookmark get / set (Y-up)
{"Ok":{"Bookmarks":{"home":[0.0,0.0]}}}     // bookmark list (sorted by name)
{"Ok":{"Screenshot":{"path":"/abs/shot.png","width":1920,"height":1080}}}
{"Ok":"Ok"}                          // action / close / suspend / relaunch / bookmark delete
{"Ok":{"DebugCounters":{"decorations":2,"stage_entries":2}}}   // abridged
{"Ok":{"State":{"camera":[-960.0,-600.0],"zoom":1.0,"layout":"English (US)",
  "layout_short":"us","windows":[
  {"id":3,"app_id":"foot","title":"~","position":[0,0],"size":[800,480],
   "is_focused":true,"is_widget":false,"suspended":false}
]}}}
{"Err":"no focused window"}
```

`Size` is the one reply here that echoes a *request* rather than compositor
state: on a set it reports the clamped size the client was asked for, which the
client is free not to commit. Read it back with a bare `{"Resize":{}}` to see
what the window actually has. Every other setter (`Camera`, `Zoom`, `Position`,
`Opacity`) echoes state the compositor owns outright.

The `windows` array is the same shape driftwm writes to its [state file](#state-file),
focused window first — but only while something is focused; with nothing focused
no entry is promoted, so filter on `is_focused` instead of indexing `windows[0]`.
Each entry's `id` is a stable per-session window handle — pass it back as a
selector to `focus`, `move`, `resize`, `close`, `suspend`, `relaunch`, or
`screenshot window`. `suspended` marks a compositor-drawn stand-in rather than a
live client — see [Suspended windows](#suspended-windows).
The reply also carries `layout` (full XKB name) and `layout_short` (the
configured code for the active group); `fullscreen` and `pinned` (screen-space
windows, each carrying an `id` too — a `pinned` entry's `position`/`size` are in
rule coordinates, output-relative, so they paste straight into a
`pinned_to_screen` rule); `layers` (namespaces of screen-space layer-shell
surfaces); `canvas_layers` (canvas-positioned layers with rule-coordinate
position and size); and `outputs` (per-output `name`, viewport `camera` (center,
Y-up), `zoom`, logical `size`, `active` flag, and `active_bookmark`).

`active_bookmark` (top-level) is the focused output's active bookmark — the
bookmark nearest the viewport's usable center among those currently visible, or
`null` when none is in view. Each `outputs` entry carries its own
`active_bookmark` for that output's viewport. This is the same value the
`ext-workspace-v1` protocol marks `active`, so a bar can highlight the current
bookmark.

### Events

A `subscribe` connection doesn't get `Ok`/`Err` replies after the initial ack;
it gets one-way **event** lines:

```json
{"State":{"camera":[-960.0,-600.0],"zoom":1.0,"layout":"English (US)","layout_short":"us","windows":[...],"outputs":[...]}}
```

The `State` payload is identical to the `state` reply's, so anything that reads
one reads the other.

### Talking to the socket directly

```bash
SOCK="$XDG_RUNTIME_DIR/driftwm/ipc-$WAYLAND_DISPLAY.sock"

echo '"State"'            | socat -t1 - UNIX-CONNECT:"$SOCK"
echo '{"Camera":[500,300]}' | socat -t1 - UNIX-CONNECT:"$SOCK"
```

## State file

For read-only polling (status bars, scripts), driftwm also writes a throttled
(~10 Hz) snapshot to `$XDG_RUNTIME_DIR/driftwm/state` — one `key=value` per
line. Reading that file avoids a socket round-trip when you only need to
observe; when you'd rather be pushed than poll, use
[`subscribe`](#subscribing-to-changes) instead.

| Key                                             | Value                                                          |
| ----------------------------------------------- | -------------------------------------------------------------- |
| `x`, `y`, `zoom`                                | The focused output's viewport                                  |
| `layout`, `layout_short`                        | Full XKB name, and the configured code for the active group    |
| `saved_x`, `saved_y`, `saved_zoom`              | The stored home-return viewport, when there is one             |
| `windows`                                       | JSON array, entries shaped like `state`'s                      |
| `layers`                                        | Comma-separated layer-shell namespaces                         |
| `canvas_layers`                                 | JSON array of canvas-positioned layers                         |
| `outputs.<name>.camera_x`, `.camera_y`, `.zoom` | That output's viewport                                         |
| `outputs.<name>.fullscreen`                     | JSON object: `id`, `app_id`, `title`                           |
| `outputs.<name>.pinned`                         | JSON array: `id`, `app_id`, `title`, `position`, `size`        |

A key with nothing to report is omitted, not written empty: no windows means no
`windows=` line at all, no layer surfaces no `layers=` line, and an output with
nothing fullscreen or pinned gets neither of its screen-space lines. Only `x`,
`y`, `zoom`, `layout`, `layout_short`, and the per-output camera lines are
always present.

Every published coordinate is a **center** with **Y-up**, matching `state` and
window rules. `x=`/`y=`/`zoom=` are the focused output's viewport, and each
output also reports its own under `outputs.<name>.camera_x`,
`outputs.<name>.camera_y`, and `outputs.<name>.zoom` — the same convention as
the `state` reply's per-output `camera` and `zoom`, rounded for the file.

`layers=` namespaces are the `app_id` a window rule matches a layer surface by.
A `canvas_layers` entry's `position` is derived from the surface's current size,
so it can drift from the rule that placed it if the surface resized after
mapping.
