# Window Suspend & Session Restore

One mechanism backs three related things: a keybindable action that leaves a
placeholder behind when you close a window, an option that does the same
automatically for every client-initiated close, and an option that restores
your whole canvas after a restart.

## Suspended windows

`suspend-window` closes the target window but leaves a **suspended window** —
a compositor-drawn stand-in — at its exact canvas position and size: a body
filled with the title bar's background color, with the app's name centered in
it, and rounded corners that follow the border. The chrome matches what the
live window had, so the footprint doesn't shift:

- A **server-decorated** window keeps a title bar (with a close button, but no
  text — the centered name already labels it).
- A **client-decorated** window is body-only, at its exact original geometry.
  It has no compositor close button; dismiss it with `close-window`, a second
  `suspend-window` press, or `msg close`.

It's draggable, resizable, raisable, and focusable like any window. Pressing
`Enter` while it's focused, or clicking/tapping the centered name, relaunches
the app; while the relaunch is pending the name reads `<app> — launching…`. The
new window takes over the stand-in's exact geometry and z-order slot. Firing
`suspend-window` again on a focused suspended window dismisses it — the put-away
gesture, repeated, escalates to "gone".

It's a regular action — bind it in `[keybindings]`, `[mouse]`, `[gestures]`,
or `[touch]` like any other. There's no default binding:

```toml
[keybindings]
"mod+shift+s" = "suspend-window"
```

Under focus-follows-mouse, an on-window binding's target is just the focused
window (hovering already focused it) — no special targeting needed.

A few things are deliberately different about a suspended window:

- **Excluded from Alt-Tab and focus history**, the same as a pinned widget —
  it's still focusable by hovering or clicking, but cycling and MRU never
  land on it, and neither does a taskbar's window list.
- **Excluded from snapping and clusters**, for now — it neither snaps to
  other windows nor participates in a snap cluster's group move/resize/fit.
  Planned to change (see [Limitations](#limitations) below).
- **Unpinnable, unfullscreenable, unfittable** — those actions no-op on it.

If the window was fullscreen or screen-pinned when suspended, it's returned
to the canvas first, at its most recent windowed size.

### The `.desktop` requirement

A suspended window only exists for an app driftwm can relaunch. `suspend-window`
resolves the target's `app_id` against your installed `.desktop` entries (exact
filename match, then `StartupWMClass`, then a case-insensitive filename match);
`Terminal=true` entries don't count — relaunching one would open a bare
terminal, not the app. No match means no suspended window: the action logs why
and closes the window normally instead of leaving a placeholder that could
never come back.

## `suspend_on_close`

```toml
suspend_on_close = true
```

With this on, a client-initiated close converts into a suspended window
instead of the window vanishing. This is the only honest way to cover the
title-bar close button: the compositor never actually sees a CSD `×` click —
the client just destroys its own toplevel, indistinguishable from `Ctrl+Q` or
the app quitting on its own. So the flag covers every client-side close of an
eligible window: SSD `×`, CSD `×`, an in-app quit, a shell exiting in a
terminal. Widgets and dialogs (a toplevel with a parent) are never eligible —
same as `suspend-window` itself.

Escape hatches, for closes you want to stay real closes:

- The `close-window` action, `msg close`, and a taskbar's close button all
  close for real, even with the flag on.
- Closing a suspended window (its own close button, `close-window` while it's
  focused, or `msg close`) dismisses it — it doesn't re-suspend.
- A window rule can override the flag per app:

  ```toml
  [[window_rules]]
  app_id            = "kitty"
  suspend_on_close  = false   # this terminal should always really close
  ```

> [!TIP]
> A crash leaves a suspended window too — the compositor can't tell an app
> crashing from it quitting cleanly. That's a free side effect worth knowing
> about: with `suspend_on_close` on, a crashed app's window and position
> aren't just gone, and `Enter` brings it right back.

## `restore_session`

```toml
restore_session = true
```

On a graceful shutdown — `quit`/`Super+Ctrl+Shift+Q` or a logout that sends
SIGTERM/SIGHUP — every eligible live window is saved. On the next launch they
come back as dormant suspended windows at the positions they were at; nothing
auto-launches, you relaunch each one same as any other suspended window (or
leave it be). A `kill -9`, a crash, or unplugging the machine skips this save
entirely — those aren't "graceful."

Suspended windows themselves are **always** saved and restored, regardless of
this flag — they're already an explicit, user-visible artifact on your canvas.
`restore_session` only decides whether still-_open_ windows get saved too on
the way out. Per-output camera position and zoom are restored across restarts
as well.

The session lives at `~/.local/state/driftwm/session.json` (respects
`XDG_STATE_HOME`). It's written through immediately on anything you'd notice
(suspending, dismissing, relaunching) and debounced (~1s) for continuous
changes like dragging a suspended window. A file that fails to parse (wrong
version, corrupted write) is quarantined to
`session.json.corrupt.<timestamp>` next to it and startup continues with an
empty session — a bad file never blocks driftwm from starting.

## Relaunching & matching

Relaunch (`Enter`, clicking the name, or `msg relaunch`) spawns the app with an
activation token and waits for its window to come back, matching it to the
stand-in by, in order:

1. **The activation token**, if the app presents it back (most native Wayland
   toolkits do).
2. **App identity**, as a 5-second fallback for apps that ignore the token:
   the oldest pending relaunch of the same app_id adopts the next window of
   that app_id to map.

### Limitations

- **Single-instance apps** (Chrome and other apps that focus an existing
  window instead of opening a new one) may never present the token or map a
  new window at all — nothing to adopt. The stand-in reverts to dormant
  (showing the app's name again) after about 30 seconds.
- **The 5-second fallback window is a capture hazard**: if you manually launch
  another window of the same app while a relaunch is pending, it can get
  captured into the suspended window's rect instead of the actual relaunch,
  which then places itself normally.
- **An app that reports a different `app_id` on relaunch** than it was
  suspended under only adopts via the activation token — the identity
  fallback won't recognize it as the same app.
- **Multiple simultaneous relaunches of the same app** match first-come,
  first-served by spawn order — with two pending at once, a token-ignoring
  client can end up adopted into the wrong one's rect.
- Touch only taps a suspended window (focus, raise, relaunch) — drag-move and
  drag-resize by touch aren't wired up yet.

## Nested / dev sessions

A nested (winit) driftwm skips durable session persistence by default, so a
dev session run inside your main one can never clobber it. Opt in with
`--session-file <path>`:

```bash
driftwm --backend winit --session-file /tmp/driftwm-dev-session.json
```

This is unrelated to suspended windows within a single run — those are on the
canvas the moment they're created, in any backend. Durability, though, needs a
store: the udev backend uses the default path, and a winit run persists only
with `--session-file`. Without a store path nothing is written — the flag only
affects whether a _quit_ is saved to (and a _startup_ is restored from) a file
at all.

## IPC

Suspended windows are visible and controllable over the [IPC](ipc.md) socket
too — see [IPC › Suspended windows](ipc.md#suspended-windows).
