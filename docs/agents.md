# Driving driftwm with AI agents

driftwm is well suited to computer-use agents: the canvas is queryable and renderable over
its own IPC, and windows carry a stable id plus their client pid. Everything an agent needs
ships in the compositor — there is no adapter process, no shim and no second daemon.

## What the IPC gives an agent

| Need | Command |
|---|---|
| Window inventory (id, app_id, title, canvas position/size, pid, focus) | `driftwm msg --json state` |
| Canvas screenshot at arbitrary scale, including off-screen content | `driftwm msg screenshot -o - --scale N` |
| Spatial navigation | `driftwm msg camera <x> <y>`, `driftwm msg zoom <level>`, `driftwm msg focus <app_id|--id>` |
| Pointer input without uinput | `driftwm msg pointer <x> <y> [--button left|right|middle]` |
| Scroll | `driftwm msg axis <dx> <dy>` |
| Stable window identity | `state.windows[].id` / `[].pid` |
| Bookmarks | `driftwm msg bookmark ...` |

A screenshot is a **canvas re-render**, not a framebuffer copy: an agent does not need the
window on screen. `--scale 2` on a 1600×900 output yields 3200×1800, which is useful for
OCR/vision stages.

`pointer` is deliberately part of the IPC rather than a Wayland protocol: it works in
nested sessions (where uinput events would go to the host compositor) and needs no extra
device permissions. The socket is user-only (0600); treat it as an input-injection surface
and keep it inside the session.

## Pairing with agent-desktop

`agent-desktop` is a standard AT-SPI client: it reads the accessibility tree and drives
elements semantically. It needs no driftwm-specific glue.

- map its windows to driftwm ids by `pid`, which `state` and the `focus` reply both carry;
- `driftwm msg focus <app_id>` activates and reveals a window (foreign-toplevel `activate`
  and `xdg-activation` both land on the same path), then its snapshot sees the window;
- `driftwm msg screenshot` is the observation channel. agent-desktop's own Wayland capture
  has two known limits: display *listing* is Mutter-only, and the wlr portal's `Screenshot`
  implementation writes to a hardcoded `/tmp/out.png`, which an agent that requires
  home-relative files will reject. `ScreenCast` through `xdg-desktop-portal-wlr` works
  against driftwm's `zwlr_screencopy` surface, which is what remote viewers use.

## Remaining gaps

1. **Video streaming.** Each `msg screenshot` re-renders; a remote viewer should throttle
   (5–10 fps) or capture through `zwlr_screencopy`/portal for H.264/WebRTC.
2. **Security.** Input IPC is a deliberate capability; if it ever becomes configurable,
   default it on only for the session owner.
