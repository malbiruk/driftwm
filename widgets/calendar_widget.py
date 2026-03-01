#!/usr/bin/env python3
"""Monthly calendar widget with today highlighted."""

import calendar
import os
import time
from datetime import datetime

from common import ICON
from rich.console import Console
from rich.live import Live
from rich.text import Text

console = Console(width=22, highlight=False)


def render() -> Text:
    now = datetime.now()  # noqa: DTZ005
    year, month, day = now.year, now.month, now.day

    cal = calendar.monthcalendar(year, month)
    content_lines = 2 + len(cal)  # header + day names + weeks

    try:
        term_h = os.get_terminal_size().lines
    except OSError:
        term_h = 11
    top_pad = max((term_h - content_lines) // 2, 0)

    text = Text()
    text.append("\n" * top_pad)
    header = f"{calendar.month_name[month].lower()} {year}"
    text.append(f" {ICON['calendar']} {header}\n", style="bold")
    text.append(" Mo Tu We Th Fr Sa Su\n")

    for week in cal:
        line = Text(" ")
        for d in week:
            if d == 0:
                line.append("   ")
            elif d == day:
                line.append(f"{d:2d} ", style="bold reverse")
            else:
                line.append(f"{d:2d} ")
        line.append("\n")
        text.append(line)

    return text


console.clear()
with Live(render(), console=console, refresh_per_second=1) as live:
    while True:
        live.update(render())
        time.sleep(30)
