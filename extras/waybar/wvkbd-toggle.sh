#!/bin/sh
# Toggle the wvkbd on-screen keyboard from the waybar touch button.
# First tap launches it (visible); each later tap toggles show/hide via
# wvkbd's own SIGRTMIN toggle signal, sent to the exact PID we launched.

BIN=wvkbd-deskintl
command -v "$BIN" >/dev/null 2>&1 || BIN="$HOME/wvkbd/wvkbd-deskintl"
PIDFILE="${XDG_RUNTIME_DIR:-/tmp}/wvkbd.pid"

if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
    kill -s RTMIN "$(cat "$PIDFILE")"
else
    "$BIN" & echo $! > "$PIDFILE"
fi
