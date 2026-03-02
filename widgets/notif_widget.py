#!/usr/bin/env python3
"""Notification bell widget — shows unread count from swaync. Click to toggle."""

import atexit
import contextlib
import os
import subprocess

from common import ICON, disable_mouse, enable_mouse, get_notifications, poll_click
from rich.console import Console
from rich.live import Live
from rich.text import Text

WIDTH = 19
console = Console(width=WIDTH, highlight=False)


def render() -> Text:
    text = Text()
    try:
        term_h = os.get_terminal_size().lines
    except OSError:
        term_h = 4
    top_pad = max((term_h - 2) // 2, 0)
    text.append("\n" * top_pad)

    count = get_notifications()
    text.append(f"  {ICON['bell']}  ", style="yellow")
    text.append("notifications\n")
    if count > 0:
        text.append(f"     {count} unread\n", style="yellow")
    else:
        text.append("     all clear\n")

    return text


atexit.register(disable_mouse)
enable_mouse()
console.clear()
try:
    with Live(render(), console=console, refresh_per_second=1) as live:
        while True:
            live.update(render())
            if poll_click(1.0) is not None:
                with contextlib.suppress(OSError):
                    subprocess.Popen(
                        ["swaync-client", "-t"],
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
finally:
    disable_mouse()
