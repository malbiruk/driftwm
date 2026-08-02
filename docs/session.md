# Window Suspend & Session Restore

One mechanism backs the `suspend-window` action and the four `[session]`
options: `suspend_on_close` leaves a placeholder behind on every
client-initiated close, `restore_windows` brings windows that were still open
at logout back after a restart, and `restore_camera` and `restore_bookmarks`
restore where the canvas was framed and the bookmarks you set.

## Suspended windows

`suspend-window` closes the target window but leaves a **suspended window** —
a compositor-drawn stand-in — at its exact canvas position and size, with the
app's name centered in it.

**A suspended window only exists for an app driftwm can relaunch.**
`suspend-window` resolves the target's `app_id` against your installed
`.desktop` entries (exact filename match, then `StartupWMClass`, then a
case-insensitive filename match); `Terminal=true` entries don't count —
relaunching one would open a bare terminal, not the app. No match means no
suspended window: the action logs why and closes the window normally instead of
leaving a placeholder that could never come back.

Every stand-in wears the same chrome: a textless title bar (the centered name
already labels it) carrying a close button. A **server-decorated** window's
stand-in reuses the bar it already had; a **client-decorated** window's
stand-in keeps its exact original footprint by shrinking the body under the
bar, and is handed the full size back when it relaunches. The close button
dismisses the stand-in — as do `close-window`, a second `suspend-window`
press, and `msg close`.

It's draggable, resizable, raisable, and focusable like any window. Pressing
`Enter` while it's focused, or clicking/tapping the centered name, relaunches
the app; while the relaunch is pending the name reads `<app> launching…`. The
new window takes over the stand-in's exact geometry and z-order slot.

It's a regular action — bind it in `[keybindings]`, `[mouse]`, `[gestures]`,
or `[touch]` like any other. There's no default binding:

```toml
[keybindings]
"mod+shift+s" = "suspend-window"
```

A suspended window differs from a live window in two ways:

- **Excluded from Alt-Tab and focus history**, the same as a pinned widget —
  it's still focusable by hovering or clicking, but cycling and MRU never
  land on it, and neither does a taskbar's window list.
- **Unpinnable, unfullscreenable, unfittable** — `toggle-pin-to-screen`,
  `toggle-fullscreen`, `fit-window`, `fit-window-snapped` and `fill-window`
  no-op on it: each needs a client to configure or a screen slot to pin to.
  `zoom-to-fit-snapped` is not one of them — it only moves the camera, so it
  frames a suspended window's cluster like any other.

If the window was fullscreen or screen-pinned when suspended, it's returned
to the canvas first, at its most recent windowed size.

## `suspend_on_close`

```toml
[session]
suspend_on_close = true
```

With this on, a client-initiated close converts into a suspended window
instead of the window vanishing. The compositor can't tell one client-initiated
close from another — a CSD `×` click and `Ctrl+Q` both just destroy the
toplevel — so the flag covers all of them for an eligible window: SSD `×`, CSD
`×`, an in-app quit, a shell exiting in a terminal. Widgets, dialogs (a
toplevel with a parent), and modal toplevels are never eligible — same as
`suspend-window` itself.

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
> crashing from it quitting cleanly. With `suspend_on_close` on, a crashed
> app's window and position survive, and `Enter` brings it right back.

## `restore_windows`

```toml
[session]
restore_windows = true
```

On a graceful shutdown — `quit`/`Super+Ctrl+Shift+Q` or a logout that sends
SIGTERM/SIGHUP — every eligible live window is saved. On the next launch they
come back as dormant suspended windows at the positions they were at; nothing
auto-launches, you relaunch each one same as any other suspended window (or
leave it be). A `kill -9`, a crash, or unplugging the machine skips the save
entirely.

Suspended windows themselves are **always** saved and restored, regardless of
this flag; `restore_windows` only decides whether still-_open_ windows are
saved too.

The window you had focused comes back focused, as focus on its stand-in — so it
wears a focus ring, `Enter` relaunches it, and `placement = "auto"` puts your
first new window beside it. The restored focus is applied only when the stand-in
is visible at launch; otherwise the canvas starts unfocused. A focused
_suspended_ window carries its focus across a restart whether or not this flag
is on.

A window rule can override the flag per app, in either direction: `false` keeps
one app off the canvas after a restart while the rest of your session comes back,
and `true` brings one app back while the flag stays off for everything else.

```toml
[[window_rules]]
app_id          = "footclient"
restore_windows = false
```

Two things to keep in mind:

- The flag is **independent of `suspend_on_close`**: that one governs closes,
  this one the logout save. Set *both* to `false` for an app that should never
  leave a stand-in behind.
- **Key the rule on `app_id`.** Saved records carry no title, so a `title`
  criterion narrows only what gets *saved*; on the way back the rule is read
  off `app_id` alone, and decides for every saved window of that app. The
  [config reference](config.md#window-rules) has the rest.

## `restore_camera`

```toml
[session]
restore_camera = true
```

With this on, each output's camera position and zoom are restored across
restarts too, so your canvas comes back framed exactly where you left it. It's
off by default — the default config starts every output at its centered camera.
The flag is read at launch, so flipping it mid-session takes effect on the next
launch, not immediately. (Cameras are always saved regardless, so turning it on
later restores what your session had.)

## `restore_bookmarks`

```toml
[session]
restore_bookmarks = true
```

Bookmarks (named canvas points — see `[navigation.bookmarks]` and the
`go-to-bookmark` / `set-bookmark` / `move-to-bookmark` actions) are a runtime
registry seeded from config. With this off (the default), the registry resets to
the config seeds on every launch, so a `set-bookmark` or `driftwm msg bookmark`
edit lasts only for the session. Turn it on to overlay the saved registry on top
of the config seeds at launch, so a restored bookmark wins per name and config
seeds fill the names the save lacks. Like the camera flag, it's read at launch.

## The session file

The session lives at `~/.local/state/driftwm/session.json` (respects
`XDG_STATE_HOME`). It's written through immediately on anything you'd notice
(suspending, dismissing, relaunching) and debounced (~1s) for continuous
changes like dragging a suspended window. A file that fails to parse (wrong
version, corrupted write) or can't be read at all is quarantined next to it as
`session.json.corrupt.<timestamp>` or `session.json.unreadable.<timestamp>`,
and startup continues with an empty session.

## Relaunching & matching

Relaunch (`Enter`, clicking the name, or `msg relaunch`) spawns the app with an
activation token and waits for its window to come back, matching it to the
stand-in by, in order:

1. **The activation token**, if the app presents it back (most native Wayland
   toolkits do). This works whether the app maps a fresh window or — as a
   single-instance app does — forwards the token to its already-running window;
   either way that window moves into the stand-in's slot, sized to fit. Note
   that the app picks the answering window: one with several windows open that
   forwards the token to an existing sibling will have *that* sibling moved,
   abandoning its old spot.
2. **App identity**, as a 5-second fallback for apps that ignore the token:
   the oldest pending relaunch of the same app_id adopts the next window of
   that app_id to map.

### Limitations

- **Apps that never present the token back** and map no new window (some
  single-instance apps just focus an existing window without forwarding the
  activation token) leave nothing to adopt. The stand-in reverts to dormant
  (showing the app's name again) after about 30 seconds.
- **The 5-second fallback window is a capture hazard.** While a relaunch is
  pending, any window of that `app_id` can be captured into the stand-in's rect
  — one you launched by hand, or the answer to a second pending relaunch of the
  same app (multiple relaunches match first-come, first-served by spawn order).
  The window that was meant for the slot then places itself normally.
- **An app that reports a different `app_id` on relaunch** than it was
  suspended under only adopts via the activation token — the identity
  fallback won't recognize it as the same app.

## Nested sessions

A nested (winit) driftwm doesn't persist a session by default, so it can't
clobber the session file of the compositor it's running inside. Opt in with
`--session-file <path>`:

```bash
driftwm --backend winit --session-file /tmp/driftwm-nested-session.json
```

Suspended windows work within the run either way — they're on the canvas the
moment they're created, in any backend. The path is what makes them durable:
the udev backend uses the default one, and a winit run persists only with
`--session-file`.

## IPC

Suspended windows are visible and controllable over the [IPC](ipc.md) socket
too — see [IPC › Suspended windows](ipc.md#suspended-windows).
