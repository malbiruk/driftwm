# driftwm-agent

Thin adapter between a **driftwm** Wayland session and agent tooling. Not a new agent framework:
it exposes driftwm's canvas (state / camera / zoom / canvas screenshot) next to
`agent-desktop`'s AT-SPI tree (snapshot / click), plus a browser viewer (noVNC-like).

```
driftwm IPC  ──►  driftwm-agent  ──►  CLI / WebSocket
   state            state, shot         agents, browser
   camera/zoom      focus, center
   screenshot       snap, click, type
```

## Requirements

- `driftwm` (built from source; set `DRIFTWM_BIN` if not in PATH)
- `agent-desktop` (npm, for AT-SPI snapshot/click)
- `wtype` (keyboard via virtual-keyboard protocol)
- `ydotool` + `ydotoold` (pointer; only works when driftwm owns libinput — see Limits)
- Bun 1.2+

## CLI

```bash
bun run src/cli.ts state                 # camera, zoom, windows (id, app_id, pid, pos, size)
bun run src/cli.ts center thunar         # pan camera to a window
bun run src/cli.ts shot --scale 1        # canvas PNG (1600x900 on a 1600x900 output)
bun run src/cli.ts shot --scale 2        # 3200x1800 - arbitrary capture resolution
bun run src/cli.ts snap thunar           # agent-desktop AT-SPI snapshot (JSON)
bun run src/cli.ts click @s...:e7        # semantic AT-SPI click (no pointer needed)
bun run src/cli.ts type "hello"          # wtype into the focused surface
bun run src/cli.ts msg zoom 1.5          # raw IPC passthrough
```

## Browser viewer (noVNC-like)

```bash
bun run src/serve.ts --port 6081 --scale 1 [--token SECRET] [--view-only] [--host 0.0.0.0]
# open http://127.0.0.1:6081/
```

- Frames: `driftwm msg screenshot -o -` (PNG, ~6 fps, canvas re-render — not a framebuffer grab).
- Input: pointer via ydotool, text via wtype, clicks via ydotool or AT-SPI.
- State overlay: camera / zoom / output size / window list.

## Limits (measured 2026-11-11)

- **Pointer input** goes through the `msg pointer` IPC command added on this branch
  (`driftwm msg pointer <x> <y> [--button left]`), so it also works in nested sessions
  where uinput events would go to the host compositor. `ydotool` remains a fallback for
  native sessions.
- `agent-desktop`'s own `screenshot` refuses on Wayland (only GNOME/Mutter is measured);
  the compositor's canvas screenshot replaces it.
- Canvas screenshots do not draw layer-shell panels (use `grim` for the literal output).
- Nested winit + llvmpipe costs ~172 MiB PSS idle; native udev on a GPU should be lower.

## Remote options considered

| Option | Verdict |
|---|---|
| this WebSocket streamer | works today, PNG frames, ~6 fps; best for agent/human on LAN |
| [Selkies](https://docs.selkies.io) (pixelflux + GStreamer/WebRTC) | production path: portal/PipeWire capture, H.264, browser; heavier stack |
| Sunshine + Moonlight | lowest latency (NVENC), native client, weak clipboard |
| wayvnc | **not compatible** (requires wlroots protocols; driftwm is Smithay) |
| KasmVNC / noVNC | X11 only |

## Security

Bind `127.0.0.1` (default) or a tailnet address; set `--token` before exposing it; `--view-only`
disables input entirely.
