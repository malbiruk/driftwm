#!/usr/bin/env python3
"""System stats + connections widget."""

import os
import time
from collections import deque

from common import (
    ICON,
    battery_icon,
    brightness_icon,
    get_battery,
    get_bluetooth,
    get_brightness,
    get_cpu_percent,
    get_ram,
    get_volume,
    get_wifi,
    progress_bar,
    sparkline,
    volume_icon,
    wifi_icon,
)
from rich.console import Console
from rich.live import Live
from rich.text import Text

WIDTH = 36
PAD = 15
console = Console(width=WIDTH, highlight=False)
cpu_history: deque[float] = deque(maxlen=10)
ram_history: deque[float] = deque(maxlen=10)


def load_color(pct: float) -> str:
    if pct < 50:
        return "green"
    if pct < 80:
        return "yellow"
    return "red"


def bat_color(pct: int) -> str:
    if pct > 50:
        return "green"
    if pct > 20:
        return "yellow"
    return "red"


def _render_battery(text: Text) -> None:
    bat = get_battery()
    if not bat:
        return
    pct, status, _time_rem = bat
    icon = battery_icon(pct, status)
    color = bat_color(pct)
    text.append(f"   {icon}  ", style=color)
    info = f"bat  {pct:3d}%"
    text.append(f"{info:<{PAD}}")
    text.append(f"{progress_bar(pct)}\n", style=color)


def _render_volume(text: Text) -> None:
    vol, muted = get_volume()
    vicon = volume_icon(vol, muted=muted)
    if muted:
        text.append(f"   {vicon}  ")
        info = "vol  muted"
        text.append(f"{info:<{PAD}}")
        text.append(f"{progress_bar(vol)}\n")
    else:
        text.append(f"   {vicon}  ", style="blue")
        info = f"vol  {vol:3d}%"
        text.append(f"{info:<{PAD}}")
        text.append(f"{progress_bar(vol)}\n", style="blue")


def _render_brightness(text: Text) -> None:
    bri = get_brightness()
    if bri is None:
        return
    bicon = brightness_icon(bri)
    text.append(f"   {bicon}  ", style="yellow")
    info = f"bri  {bri:3d}%"
    text.append(f"{info:<{PAD}}")
    text.append(f"{progress_bar(bri)}\n", style="yellow")


def _render_connections(text: Text) -> None:
    wifi = get_wifi()
    if wifi:
        ssid, signal = wifi
        wicon = wifi_icon(signal)
        display_ssid = ssid[:14] if len(ssid) > 14 else ssid
        text.append(f"   {wicon}  ", style="cyan")
        text.append(f"{display_ssid} ({signal}%)\n")
    else:
        text.append(f"   {ICON['wifi_off']}  ")
        text.append("offline\n")

    bt = get_bluetooth()
    if bt:
        text.append(f"   {bt}\n", style="blue")


def render() -> Text:
    text = Text()
    try:
        term_h = os.get_terminal_size().lines
    except OSError:
        term_h = 10
    top_pad = max((term_h - 8) // 2, 0)
    text.append("\n" * top_pad)

    cpu = get_cpu_percent()
    cpu_history.append(cpu)
    text.append(f"   {ICON['cpu']}  ", style="cyan")
    info = f"cpu  {cpu:3.0f}%"
    text.append(f"{info:<{PAD}}")
    text.append(f"{sparkline(cpu_history)}\n", style=load_color(cpu))

    ram_used, ram_total = get_ram()
    ram_pct = ram_used / ram_total * 100 if ram_total > 0 else 0
    ram_history.append(ram_pct)
    text.append(f"   {ICON['ram']}  ", style="magenta")
    info = f"ram  {ram_used:.1f}/{ram_total:.0f}G"
    text.append(f"{info:<{PAD}}")
    text.append(f"{sparkline(ram_history)}\n", style=load_color(ram_pct))

    text.append("\n")
    _render_battery(text)
    _render_volume(text)
    _render_brightness(text)
    text.append("\n")
    _render_connections(text)

    return text


console.clear()
with Live(render(), console=console, refresh_per_second=2) as live:
    while True:
        live.update(render())
        time.sleep(1)
