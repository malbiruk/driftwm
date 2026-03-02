#!/usr/bin/env python3
"""System stats + connections widget. Click zones dispatch actions."""

import atexit
import contextlib
import os
import subprocess
from collections import deque

from common import (
    ICON,
    battery_icon,
    brightness_icon,
    disable_mouse,
    enable_mouse,
    get_battery,
    get_bluetooth,
    get_brightness,
    get_cpu_percent,
    get_ram,
    get_volume,
    get_wifi,
    poll_click,
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

# Maps terminal row (1-based) → shell command. Built each render().
click_map: dict[int, list[str]] = {}

# Click actions per section
ACTION_CPU = ["gnome-system-monitor"]
ACTION_RAM = ["gnome-system-monitor"]
ACTION_VOL = ["swayosd-client", "--output-volume", "mute-toggle"]
ACTION_WIFI = ["alacritty", "-e", "nmtui"]
ACTION_BT = ["blueman-manager"]


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


def _render_cpu_ram(text: Text, line: int) -> int:
    cpu = get_cpu_percent()
    cpu_history.append(cpu)
    text.append(f"   {ICON['cpu']}  ", style="cyan")
    info = f"cpu  {cpu:3.0f}%"
    text.append(f"{info:<{PAD}}")
    text.append(f"{sparkline(cpu_history)}\n", style=load_color(cpu))
    click_map[line] = ACTION_CPU
    line += 1

    ram_used, ram_total = get_ram()
    ram_pct = ram_used / ram_total * 100 if ram_total > 0 else 0
    ram_history.append(ram_pct)
    text.append(f"   {ICON['ram']}  ", style="magenta")
    info = f"ram  {ram_used:.1f}/{ram_total:.0f}G"
    text.append(f"{info:<{PAD}}")
    text.append(f"{sparkline(ram_history)}\n", style=load_color(ram_pct))
    click_map[line] = ACTION_RAM
    return line + 1


def _render_battery(text: Text, line: int) -> int:
    bat = get_battery()
    if not bat:
        return line
    pct, status, _time_rem = bat
    icon = battery_icon(pct, status)
    color = bat_color(pct)
    text.append(f"   {icon}  ", style=color)
    info = f"bat  {pct:3d}%"
    text.append(f"{info:<{PAD}}")
    text.append(f"{progress_bar(pct)}\n", style=color)
    return line + 1


def _render_volume(text: Text, line: int) -> int:
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
    click_map[line] = ACTION_VOL
    return line + 1


def _render_brightness(text: Text, line: int) -> int:
    bri = get_brightness()
    if bri is None:
        return line
    bicon = brightness_icon(bri)
    text.append(f"   {bicon}  ", style="yellow")
    info = f"bri  {bri:3d}%"
    text.append(f"{info:<{PAD}}")
    text.append(f"{progress_bar(bri)}\n", style="yellow")
    return line + 1


def _render_connections(text: Text, line: int) -> int:
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
    click_map[line] = ACTION_WIFI
    line += 1

    bt = get_bluetooth()
    if bt:
        text.append(f"   {bt}\n", style="blue")
        click_map[line] = ACTION_BT
    return line + 1


def render() -> Text:
    click_map.clear()
    text = Text()
    try:
        term_h = os.get_terminal_size().lines
    except OSError:
        term_h = 11
    top_pad = max((term_h - 8) // 2, 0)
    text.append("\n" * top_pad)
    line = 1 + top_pad

    line = _render_cpu_ram(text, line)
    text.append("\n")
    line += 1
    line = _render_battery(text, line)
    line = _render_volume(text, line)
    line = _render_brightness(text, line)
    text.append("\n")
    line += 1
    _render_connections(text, line)

    return text


atexit.register(disable_mouse)
enable_mouse()
console.clear()
try:
    with Live(render(), console=console, refresh_per_second=2) as live:
        while True:
            live.update(render())
            click = poll_click(1.0)
            if click is not None:
                _, y = click
                cmd = click_map.get(y)
                if cmd:
                    with contextlib.suppress(OSError):
                        subprocess.Popen(
                            cmd,
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                        )
finally:
    disable_mouse()
