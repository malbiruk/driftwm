# IPC

driftwm exposes a small IPC over a Unix domain socket so external tools and
scripts can query and control the running compositor. The `driftwm msg`
subcommand is the built-in client; the wire protocol is plain line-delimited
JSON, so any language can speak it directly.

## `driftwm msg`

Run `driftwm msg <command>` from inside a driftwm session. Each command reads
when given no arguments and writes when given arguments. Add `--json` to print
the raw JSON reply instead of the human-readable form. A command that fails (bad
value, no focused window, no match) prints an error to stderr and exits
non-zero, so scripts can branch on it.

The commands — `camera`, `zoom`, `focus`, `move`, `opacity`, `close`, `layout`,
`action`, `screenshot`, `state`, `subscribe`, and `debug-counters` — with their
arguments, flags, and JSON reply shapes are documented in the generated
[CLI reference](cli.md); `driftwm msg <command> --help` prints the same for one
command. The conventions they share follow below.

### Coordinates

Window and camera positions use the same convention as
[window rules](window-rules.md) and the [state file](#state-file): a **center**
point, with **Y pointing up**. `move 0 0` centers the focused window on the
origin, `camera 0 0` centers the viewport there, and positive `y` is above.
Pinned and fullscreen windows live in screen space, not on the canvas, so `move`
refuses to reposition them.

### Screenshots

`screenshot` is a **canvas capture**, not a screen grab: it re-renders the canvas
at an arbitrary resolution rather than copying the framebuffer, so it reaches
off-screen content but omits panels/layer-shells and blur — use `grim` for a
literal screen grab. Targets, flags, examples, and caveats live in the
[CLI reference](cli.md#driftwm-msg-screenshot) (`driftwm msg screenshot --help`).

### Subscribing to changes

`subscribe` turns the connection into a live feed instead of polling: one event
per change (per rendered frame while something animates), each a whole-state
snapshot. Nothing is pushed while nothing changes — render from the latest
received snapshot rather than in lockstep with events. Mechanics are in the
[CLI reference](cli.md#driftwm-msg-subscribe); the wire-level event shape is
under [Events](#events) below.

A one-liner that prints the focused window's `app_id` whenever anything changes:

```bash
driftwm msg --json subscribe | jq -r '.State.windows[0].app_id'
```

A small daemon that dims whatever loses focus and restores full opacity to
whatever gains it (a snapshot arrives per rendered frame during a pan, so the
focused id is deduped against the last one seen):

```bash
prev=
driftwm msg --json subscribe \
  | jq --unbuffered -r '.State.windows[] | select(.is_focused) | .id' \
  | while read -r id; do
      [ "$id" = "$prev" ] && continue                      # same focus, skip repeats
      [ -n "$prev" ] && driftwm msg opacity 0.7 --id "$prev" # dim the window we left
      driftwm msg opacity 1 --id "$id"                     # full opacity on the new one
      prev=$id
  done
```

### Debug counters

`debug-counters` reports the sizes of the compositor's internal per-window,
per-surface, and per-client collections — a leak-diagnosis endpoint with
**unstable keys** (internal field names that can change between releases). See
the [CLI reference](cli.md#driftwm-msg-debug-counters).

## Wire protocol

The socket path is `$XDG_RUNTIME_DIR/driftwm/ipc-<WAYLAND_DISPLAY>.sock`
(permissions `0600`). The name is derived from the compositor's `WAYLAND_DISPLAY`,
so each instance owns a distinct socket and a client launched inside a session
automatically targets that session. Set `DRIFTWM_SOCKET` to point a client at an
explicit path.

> [!WARNING]
> The IPC socket is a full control surface, not a read-only one: `action` can
> run `exec`/`spawn` (launch programs), `quit`, and `reload-config`. It's safe
> only because the socket is `0600` (your user only) — anything that can open it
> could already run programs as you. So don't loosen the permissions or bridge it
> over a network for "just reading state": that hands arbitrary code execution to
> whoever reaches it.

The protocol is one JSON **request** per line, answered by one JSON **reply**
per line. A single connection may carry several requests; the connection stays
open until the client closes it.

A reply is `{"Ok": <response>}` on success or `{"Err": "message"}` on failure.

A window can be targeted by a **selector**: a JSON number is its stable `id`
(from `state`), a JSON string is a case-insensitive `app_id` substring.

> [!NOTE]
> The `Move` request and the `Window` screenshot target changed shape in this
> release: the old `{"Move":[x,y]}` tuple and bare `"Window"` string forms are
> gone (replaced by the object forms below). The `Focused` reply also grew from
> a bare app_id string to `{"id":…,"app_id":…}`.

### Requests

| Request           | JSON to send                                                                        |
| ----------------- | ----------------------------------------------------------------------------------- |
| get / set camera  | `{"Camera":null}` / `{"Camera":[500,300]}`                                          |
| get / set zoom    | `{"Zoom":null}` / `{"Zoom":0.5}`                                                    |
| get / set focus   | `{"Focus":null}` / `{"Focus":"alacritty"}` / `{"Focus":5}`                          |
| get / set move    | `{"Move":{}}` / `{"Move":{"window":5,"to":[100,200]}}` (both optional)              |
| get / set opacity | `{"Opacity":{}}` / `{"Opacity":{"window":5,"value":0.5}}` (both optional)           |
| close             | `{"Close":null}` / `{"Close":5}` / `{"Close":"alacritty"}`                          |
| layout            | `{"Layout":{"short":false}}`                                                        |
| run action        | `{"Action":"switch-layout next"}`                                                   |
| screenshot        | `{"Screenshot":{"target":"Viewport","scale":1.0,"path":"/abs/shot.png"}}`           |
| screenshot window | `{"Window":{}}` / `{"Window":{"window":5}}` (as the `target`)                       |
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
{"Ok":{"Opacity":0.85}}
{"Ok":"Ok"}                          // action / close
{"Ok":{"DebugCounters":{"decorations":2,"stage_entries":2}}}   // abridged
{"Ok":{"State":{"camera":[-960.0,-600.0],"zoom":1.0,"layout":"English (US)",
  "layout_short":"us","windows":[
  {"id":3,"app_id":"foot","title":"~","position":[0,0],"size":[800,480],
   "is_focused":true,"is_widget":false}
]}}}
{"Err":"no focused window"}
```

The `windows` array is the same shape driftwm writes to its [state file](#state-file),
focused window first. Each entry's `id` is a stable per-session window handle —
pass it back as a selector to `focus`, `move`, `close`, or `screenshot window`.
The reply also carries `layout` (full XKB name) and `layout_short` (the
configured code for the active group); `fullscreen` and `pinned` (screen-space
windows, each carrying an `id` too — a `pinned` entry's `position`/`size` are in
rule coordinates, output-relative, so they paste straight into a
`pinned_to_screen` rule); `layers` (namespaces of screen-space layer-shell
surfaces); `canvas_layers` (canvas-positioned layers with rule-coordinate
position and size); and `outputs` (per-output `name`, viewport `camera` (center,
Y-up), `zoom`, logical `size`, and `active` flag).

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
(~10 Hz) snapshot to `$XDG_RUNTIME_DIR/driftwm/state` — `key=value` lines plus a
`windows=` JSON array using the same window shape as `state`. Reading that file
avoids a socket round-trip when you only need to observe; when you'd rather be
pushed than poll, use [`subscribe`](#subscribing-to-changes) instead.

Layer-shell clients appear too: `layers=` lists the namespaces of screen-space
layer surfaces (bars, OSKs, overlays — useful for finding the `app_id` a
window rule should match), and `canvas_layers=` is a JSON array of
canvas-positioned layers with their namespace, rule-coordinate `position`, and
`size` (the position reflects the current size, so it can drift from the
placing rule if the surface resized after mapping).

## Limitations

- `subscribe` events are whole-state snapshots, not granular event types
  (window-opened, focus-changed, …) — diff consecutive snapshots if you need
  the delta.
