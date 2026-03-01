#!/usr/bin/env python3
"""Notification bell widget — shows unread count from swaync."""

import os
import time

from rich.console import Console
from rich.live import Live
from rich.text import Text

from common import ICON, get_notifications

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


console.clear()
with Live(render(), console=console, refresh_per_second=1) as live:
    while True:
        live.update(render())
        time.sleep(1)
