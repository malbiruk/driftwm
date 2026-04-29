# xwayland-satellite follow-ups

Open items to polish satellite integration before it can be considered finished.

## Spawn on demand

Currently spawned eagerly at compositor startup, costs ~30MB resident even when
no X11 client ever connects. Should bind X11 sockets ourselves, watch them via
calloop, and only `fork+exec` satellite when a client actually connects.

The on-demand `-listenfd` pattern niri uses races with multi-layout XKB configs
(`layout = "us,ru"` + `grp:win_space_toggle`) under Xwayland 24.x — the queued
X11 connection on the pre-bound socket triggers Xwayland's keyboard init before
`wl_keyboard.keymap` arrives, satellite panics. Need to either work around the
race or wait for an upstream fix.

Open question: shut satellite down again when the last X11 window closes? Or
keep it resident once spawned? Niri keeps it resident.

## Auto-restart on crash

If satellite dies mid-session, X11 stays dead until driftwm restart. Watch the
child process exit (calloop channel from a spawn-watcher thread, or SIGCHLD
hook) and re-arm the on-demand watch — next X11 client will respawn satellite.

Niri does this for free with the listenfd pattern: satellite exits → sockets
become readable again → next connection triggers respawn.

## Fix clipboard X11 → Wayland

Both `PRIMARY` and `CLIPBOARD` selections fail to bridge from X11 apps into
Wayland apps; the reverse direction works.

niri's docs claim X11→Wayland clipboard "works well" via satellite, so this is
likely driftwm-specific rather than a satellite or smithay limit. Most likely
candidates: focus-handling difference (timing of `wl_keyboard.enter` vs
satellite's `last_kb_serial` update), or some data_device init ordering issue.

Investigate with `RUST_LOG=smithay::wayland::selection=debug` while reproducing
`xclip → wl-paste` to see whether smithay denies the `set_selection` call or
satellite never makes it.

## jgmenu submenu positions

Submenus from jgmenu (and possibly other lightweight X11 menu programs) appear
slightly off from where they should. Rare in daily use, low priority. May be a
satellite popup positioning issue (out of our control) — verify upstream first
before spending time.

## README note on maximize limitation

Once the above is done and the merge happens, add a single line to README
explaining the X11 maximize limitation:

> X11 apps' own CSD maximize buttons / titlebar double-click don't trigger
> driftwm's fit-to-viewport (xwayland-satellite limitation, see
> [Supreeeme/xwayland-satellite#269](https://github.com/Supreeeme/xwayland-satellite/issues/269)).
> driftwm's own key, mouse, and gesture bindings for fit/maximize work on X11
> apps as expected.
