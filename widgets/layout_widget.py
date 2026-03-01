#!/usr/bin/env python3
"""Keyboard layout widget — shows active XKB layout."""

import os
import time

from rich.console import Console
from rich.live import Live
from rich.text import Text

from common import ICON, read_state_file

LAYOUT_SHORT = {
    "English": "EN",
    "Russian": "RU",
    "German": "DE",
    "French": "FR",
    "Spanish": "ES",
    "Ukrainian": "UA",
}


def _short_layout(name: str) -> str:
    for key, short in LAYOUT_SHORT.items():
        if key in name:
            return short
    return name[:2].upper() if name else "—"


WIDTH = 6
console = Console(width=WIDTH, highlight=False)


def render() -> Text:
    text = Text()
    try:
        term_h = os.get_terminal_size().lines
    except OSError:
        term_h = 3
    top_pad = max((term_h - 2) // 2, 0)
    text.append("\n" * top_pad)

    state = read_state_file()
    layout = _short_layout(state.get("layout", "")).lower()

    text.append(f"  {ICON['kbd']}\n", style="magenta")
    text.append(f"  {layout}\n")

    return text


console.clear()
with Live(render(), console=console, refresh_per_second=2) as live:
    while True:
        live.update(render())
        time.sleep(0.5)
