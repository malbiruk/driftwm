# Decals

A decal is a still image pinned to the canvas. Use one as a landmark: a sticker
that says what a cluster of windows is about, or an anchor you can spot from a
zoomed-out view.

```bash
driftwm msg decal add ~/stickers/rocket.png      # centered under the pointer
driftwm msg decal add ~/stickers/rocket.png 400 -250 --scale 0.5
driftwm msg decal add ~/stickers/rocket.png --fit 256   # longest side 256 canvas units
driftwm msg decal list
driftwm msg decal remove 3                       # by id
driftwm msg decal remove                         # the topmost one under the pointer
```

## Behaviour

- **Drawn by the compositor.** There is no window and no client process, so
  nothing to launch, restore or close, and it appears at its exact spot with no
  placement animation.
- **Below everything.** Decals draw beneath every window and widget, above the
  background. A transparent PNG shows the dot grid or your shader wallpaper
  through.
- **Immune to the window machinery.** Dragging, snapping, fit, fill, tiling-style
  placement and navigation never see a decal, and it stays out of Alt-Tab. The
  only things that move it are `decal remove` and a new `decal add`.
- **Pans and zooms with the canvas**, like a window, so a decal shrinks as you
  zoom out. Make anchors large and high-contrast if they need to read from far
  away.
- **Persistent.** Decals are saved to `decals.json` next to `session.json`
  (`$XDG_STATE_HOME/driftwm/`) on every change, whatever `[session]` says. A
  nested session saves them only when you pass `--session-file`, like the rest of
  the [session state](session.md#nested-sessions).
- **Captured.** `driftwm msg screenshot` includes decals, except isolated
  `window` captures.

## Coordinates

Positions are canvas points, Y-up, naming the image's **center** — the same
convention as `driftwm msg move` and bookmarks. With no coordinates the center
goes under the pointer. `--scale` multiplies the image's pixel size to give its
size in canvas units; the default `1` draws one image pixel per canvas unit.
`--fit N` sizes the longest side to `N` canvas units instead, so a folder of
images of mixed sizes can be dropped at one consistent size.

## Images

PNG and JPEG, up to 32 MiB and 8192 px on a side. Alpha is honoured. The file is
read when the decal is placed (its size) and again on first draw (its pixels),
so keep it where it is: a decal whose file has gone stays in `decals.json` and
draws nothing until the file returns.

Decoding happens on the render path, so a very large image can stall one frame
the first time it comes into view.

## Limits

- At most 256 decals.
- A decal cannot be selected or dragged. Remove and re-add it to move it.
- `decal remove` with no id hits the topmost decal under the pointer even when a
  window covers it.

For a decal-like image that must live in a window (an animated one, say), see the
[decals recipe](window-rules.md#pictures-and-text-on-the-canvas-decals) in window
rules.
