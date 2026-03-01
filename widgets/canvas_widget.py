#!/usr/bin/env python3
"""Canvas position widget — shows saved (home-toggle) viewport coords."""

import os
import time

from common import ICON, read_state_file
from rich.console import Console
from rich.live import Live
from rich.text import Text

WIDTH = 25
console = Console(width=WIDTH, highlight=False)


def render() -> Text:
    text = Text()
    try:
        term_h = os.get_terminal_size().lines
    except OSError:
        term_h = 4
    top_pad = max((term_h - 2) // 2, 0)
    text.append("\n" * top_pad)

    state = read_state_file()
    x = state.get("saved_x", "—")
    y = state.get("saved_y", "—")
    zoom = state.get("saved_zoom", "—")

    text.append(f"   {ICON['pos']}  ", style="cyan")
    text.append(f"x: {x}  y: {y}\n")
    text.append(f"   {ICON['zoom']}  ", style="yellow")
    text.append(f"zoom: {zoom}\n")

    return text


console.clear()
with Live(render(), console=console, refresh_per_second=2) as live:
    while True:
        live.update(render())
        time.sleep(1)
