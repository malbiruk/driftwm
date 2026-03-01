#!/usr/bin/env python3
"""Clock + date widget."""

import os
import time
from datetime import datetime

from common import render_big_time
from rich.console import Console
from rich.live import Live
from rich.text import Text

WIDTH = 36
console = Console(width=WIDTH, highlight=False)


def center(line: str) -> str:
    pad = max((WIDTH - len(line)) // 2, 0)
    return " " * pad + line


def render() -> Text:
    text = Text()
    now = datetime.now()  # noqa: DTZ005
    try:
        term_h = os.get_terminal_size().lines
    except OSError:
        term_h = 6
    top_pad = max((term_h - 4) // 2, 0)
    text.append("\n" * top_pad)

    r1, r2 = render_big_time(now.strftime("%H:%M"), colon_on=now.second % 2 == 0)
    text.append(center(r1) + "\n", style="bold")
    text.append(center(r2) + "\n", style="bold")
    text.append("\n")
    date_line = now.strftime("%A · %B %d").lower()
    text.append(center(date_line) + "\n")
    return text


console.clear()
with Live(render(), console=console, refresh_per_second=2) as live:
    while True:
        live.update(render())
        time.sleep(0.5)
