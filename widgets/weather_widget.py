#!/usr/bin/env python3
"""Weather widget — fetches from wttr.in, caches for 10 minutes."""

import time

from rich.console import Console
from rich.live import Live
from rich.text import Text

from common import get_weather, weather_icon

console = Console(width=22, highlight=False)

REFRESH_INTERVAL = 600  # 10 minutes
cached_weather: dict | None = None
last_fetch: float = 0


def fetch_if_stale() -> dict | None:
    global cached_weather, last_fetch  # noqa: PLW0603
    now = time.time()
    if now - last_fetch > REFRESH_INTERVAL:
        last_fetch = now
        result = get_weather()
        if result is not None:
            cached_weather = result
    return cached_weather


def render() -> Text:
    w = fetch_if_stale()

    text = Text()
    text.append("\n")

    if w is None:
        text.append("  offline\n\n\n")
        return text

    location = w.get("location", "")
    if location:
        text.append(f"   {location.lower()}\n")
    text.append(f"   {w['temp']}\u00b0C", style="bold")
    text.append(f" {w['desc'].lower()}\n")
    text.append(f"   H:{w['high']}\u00b0  L:{w['low']}\u00b0\n")
    text.append(
        f"   tmrw: {w['tomorrow_temp']}\u00b0C\n",
    )

    return text


console.clear()
with Live(render(), console=console, refresh_per_second=1) as live:
    while True:
        live.update(render())
        time.sleep(60)
