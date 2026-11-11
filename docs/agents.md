# Driving driftwm with AI agents

driftwm is unusually well suited to computer-use agents: the canvas is queryable and
renderable over IPC, and windows carry a stable id plus their client pid. This page
describes the integration and the pieces added for it.

## What the IPC gives an agent

| Need | Command |
|---|---|
| Window inventory (id, app_id, title, canvas position/size, pid, focus) | `driftwm msg --json state` |
| Canvas screenshot at arbitrary scale, including off-screen content | `driftwm msg screenshot -o - --scale N` |
| Spatial navigation | `driftwm msg camera <x> <y>`, `driftwm msg zoom <level>`, `driftwm msg focus <app_id|--id>` |
| Pointer input without uinput | `driftwm msg pointer <x> <y> [--button left|right|middle]` |
| Stable window identity | `state.windows[].id` / `[].pid` |
| Bookmarks | `driftwm msg bookmark ...` |

A screenshot is a **canvas re-render**, not a framebuffer copy: an agent does not need the
window on screen. `--scale 2` on a 1600×900 output yields 3200×1800, which is useful for
OCR/vision stages.

`pointer` is deliberately part of the IPC rather than a Wayland protocol: it works in
nested sessions (where uinput events would go to the host compositor) and needs no extra
device permissions. The socket is user-only (0600); treat it as an input-injection
surface and keep it inside the session.

## Glue for AT-SPI agents

`agent-desktop` (AT-SPI) can see app windows inside driftwm and click them semantically
(no pointer). Pair it with driftwm's IPC:

- map `agent-desktop` windows to driftwm ids by `pid`;
- use `driftwm msg focus <app_id>` to bring a window into view before snapshotting;
- use `driftwm msg screenshot` for observation (agent-desktop's own Wayland capture only
  supports GNOME/Mutter);
- use `driftwm msg pointer ... --button left` when a coordinate click is required.

A reference implementation of this glue (CLI + WebSocket viewer) lives in
[`contrib/driftwm-agent`](../contrib/driftwm-agent).

## Remaining gaps

1. **Video streaming.** Each `msg screenshot` re-renders; a remote viewer should throttle
   (5–10 fps) or capture through `zwlr_screencopy`/portal for H.264/WebRTC.
2. **Security.** Input IPC is a deliberate capability; if it ever becomes configurable,
   default it on only for the session owner.
