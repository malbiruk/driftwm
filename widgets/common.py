"""Shared helpers for driftwm dashboard widgets."""

import json
import os
import select
import subprocess
import sys
import termios
import urllib.request
from collections import deque
from pathlib import Path

# ── Block digits (3x2 each) ──────────────────────────────────

DIGITS = {
    "0": ["█▀█", "█▄█"],
    "1": [" ▀█", " ▄█"],
    "2": ["▀▀█", "█▄▄"],
    "3": ["▀▀█", "▄▄█"],
    "4": ["█ █", "▀▀█"],
    "5": ["█▀▀", "▄▄█"],
    "6": ["█▀▀", "█▄█"],
    "7": ["▀▀█", "  █"],
    "8": ["█▀█", "█▀█"],
    "9": ["█▀█", "▄▄█"],
}


def render_big_time(time_str: str, *, colon_on: bool = True) -> tuple[str, str]:
    """Render HH:MM as two rows of block characters."""
    rows: list[list[str]] = [[], []]
    for ch in time_str:
        if ch == ":":
            rows[0].append(" · " if colon_on else "   ")
            rows[1].append(" · " if colon_on else "   ")
        elif ch in DIGITS:
            for i in range(2):
                rows[i].append(DIGITS[ch][i])
    return " ".join(rows[0]), " ".join(rows[1])


# ── Sparkline ────────────────────────────────────────────────

SPARK = " ▁▂▃▄▅▆▇█"


def sparkline(values: deque | list, width: int = 10) -> str:
    """Render a sparkline from recent values (0-100 absolute scale)."""
    recent = list(values)[-width:]
    if not recent:
        return " " * width
    return "".join(
        SPARK[max(min(int(v / 100 * 8), 8), 1 if v > 0.5 else 0)] for v in recent
    )


def progress_bar(pct: float, width: int = 10) -> str:
    """Render a thin horizontal progress bar."""
    pct = max(0.0, min(100.0, pct))
    filled = round(pct / 100 * width)
    return "━" * filled + " " * (width - filled)


# ── Nerd Font icons ─────────────────────────────────────────

ICON = {
    "cpu": "\uf4bc",
    "ram": "\uefc5",
    "bat_charging": "󰂄",
    "bat_full": "󰁹",
    "bat_high": "󰂂",
    "bat_med": "󰂀",
    "bat_low": "󰁾",
    "bat_empty": "󰁺",
    "vol_high": "󰕾",
    "vol_med": "󰖀",
    "vol_low": "󰕿",
    "vol_mute": "󰖁",
    "wifi_4": "󰤨",
    "wifi_3": "󰤥",
    "wifi_2": "󰤢",
    "wifi_1": "󰤟",
    "wifi_off": "󰤮",
    "wifi_none": "󰤭",
    "kbd": "󰌌",
    "pos": "\uf124",
    "zoom": "\uf00e",
    "bell": "\uf0f3",
    "calendar": "\uf073",
    "weather": "\uf0c2",
    "bright_high": "󰃠",
    "bright_med": "󰃟",
    "bright_low": "󰃞",
    "bright_dim": "󰃝",
    "bt_on": "󰂯",
    "bt_off": "󰂲",
    "bt_connected": "󰂱",
}

# ── Weather icons (Unicode, no Nerd Font needed) ────────────

WEATHER_ICON = {
    "clear": chr(0xF0599),  # 󰖙
    "sunny": chr(0xF0599),  # 󰖙
    "partly": chr(0xF0595),  # 󰖕
    "cloudy": chr(0xF0590),  # 󰖐
    "overcast": chr(0xF0590),  # 󰖐
    "rain": chr(0xF0597),  # 󰖗
    "drizzle": chr(0xF0597),  # 󰖗
    "thunder": chr(0xF0593),  # 󰖓
    "snow": chr(0xF0598),  # 󰖘
    "mist": chr(0xF0591),  # 󰖑
    "fog": chr(0xF0591),  # 󰖑
}


def weather_icon(desc: str) -> str:
    desc_lower = desc.lower()
    for key, icon in WEATHER_ICON.items():
        if key in desc_lower:
            return icon
    return chr(0xF0595)  # 󰖕 partly cloudy as default


# ── Data readers ─────────────────────────────────────────────


class CpuTracker:
    """Tracks CPU usage between calls via /proc/stat deltas."""

    def __init__(self) -> None:
        self.prev: tuple[int, int] | None = None

    def read(self) -> float:
        parts = Path("/proc/stat").read_text().split("\n")[0].split()
        idle = int(parts[4]) + int(parts[5])
        total = sum(int(x) for x in parts[1:])
        if self.prev is None:
            self.prev = (idle, total)
            return 0.0
        prev_idle, prev_total = self.prev
        self.prev = (idle, total)
        d_total = total - prev_total
        if d_total == 0:
            return 0.0
        return 100.0 * (1.0 - (idle - prev_idle) / d_total)


cpu_tracker = CpuTracker()


def get_cpu_percent() -> float:
    """CPU usage since last call (delta from /proc/stat)."""
    return cpu_tracker.read()


def get_ram() -> tuple[float, float]:
    """Returns (used_gb, total_gb)."""
    info = {}
    for line in Path("/proc/meminfo").read_text().splitlines():
        parts = line.split()
        info[parts[0].rstrip(":")] = int(parts[1])
    total = info["MemTotal"]
    avail = info["MemAvailable"]
    return (total - avail) / 1048576, total / 1048576


def _parse_upower(output: str) -> tuple[int, str, str] | None:
    """Extract battery info from upower -i output."""
    pct = None
    status = ""
    time_rem = ""
    for line in output.splitlines():
        if "percentage" in line:
            pct = int("".join(c for c in line.split()[-1] if c.isdigit()))
        elif "state:" in line:
            status = line.split()[-1]
        elif "time to empty" in line or "time to full" in line:
            time_rem = line.split(":", 1)[-1].strip()
    if pct is not None:
        return pct, status, time_rem
    return None


def get_battery() -> tuple[int, str, str] | None:
    """Returns (percent, status, time_remaining) or None."""
    # Try upower first (shows time remaining)
    try:
        result = subprocess.run(
            ["upower", "-e"],
            capture_output=True,
            text=True,
            timeout=2,
            check=False,
        )
        bat_dev = next(
            (line for line in result.stdout.splitlines() if "BAT" in line),
            None,
        )
        if bat_dev:
            upower_out = subprocess.run(
                ["upower", "-i", bat_dev],
                capture_output=True,
                text=True,
                timeout=2,
                check=False,
            ).stdout
            parsed = _parse_upower(upower_out)
            if parsed is not None:
                return parsed
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass

    # Fallback to sysfs
    for bat in sorted(Path("/sys/class/power_supply").glob("BAT*")):
        try:
            pct = int((bat / "capacity").read_text().strip())
            status = (bat / "status").read_text().strip().lower()
        except (OSError, ValueError):
            continue
        else:
            return pct, status, ""
    return None


def battery_icon(pct: int, status: str) -> str:
    if status == "charging":
        return ICON["bat_charging"]
    if pct > 75:
        return ICON["bat_high"]
    if pct > 50:
        return ICON["bat_med"]
    if pct > 25:
        return ICON["bat_low"]
    return ICON["bat_empty"]


def get_volume() -> tuple[int, bool]:
    """Returns (percent, is_muted)."""
    try:
        result = subprocess.run(
            ["wpctl", "get-volume", "@DEFAULT_AUDIO_SINK@"],
            capture_output=True,
            text=True,
            timeout=2,
            check=False,
        )
        parts = result.stdout.strip().split()
        vol = int(float(parts[1]) * 100)
        muted = "[MUTED]" in result.stdout
    except (FileNotFoundError, subprocess.TimeoutExpired, IndexError, ValueError):
        return 0, False
    else:
        return vol, muted


def volume_icon(pct: int, *, muted: bool) -> str:
    if muted or pct == 0:
        return ICON["vol_mute"]
    if pct > 66:
        return ICON["vol_high"]
    if pct > 33:
        return ICON["vol_med"]
    return ICON["vol_low"]


def get_wifi() -> tuple[str, int] | None:
    """Returns (ssid, signal_percent) or None."""
    try:
        result = subprocess.run(
            ["nmcli", "-t", "-f", "ACTIVE,SSID,SIGNAL", "dev", "wifi"],
            capture_output=True,
            text=True,
            timeout=3,
            check=False,
        )
        for line in result.stdout.splitlines():
            parts = line.split(":")
            if len(parts) >= 3 and parts[0] == "yes":
                return parts[1], int(parts[2])
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    return None


def wifi_icon(signal: int) -> str:
    if signal >= 75:
        return ICON["wifi_4"]
    if signal >= 50:
        return ICON["wifi_3"]
    if signal >= 25:
        return ICON["wifi_2"]
    return ICON["wifi_1"]


def get_notifications() -> int:
    """Get unread notification count from swaync."""
    try:
        result = subprocess.run(
            ["swaync-client", "-c"],
            capture_output=True,
            text=True,
            timeout=2,
            check=False,
        )
        return int(result.stdout.strip())
    except Exception:
        return 0


def get_weather() -> dict | None:
    """Fetch weather from wttr.in. Returns dict or None on failure."""
    try:
        req = urllib.request.Request(
            "https://wttr.in/?format=j1",
            headers={"User-Agent": "driftwm-widget"},
        )
        with urllib.request.urlopen(req, timeout=5) as resp:  # noqa: S310
            data = json.loads(resp.read())
        cur = data["current_condition"][0]
        today = data["weather"][0]
        tomorrow = data["weather"][1] if len(data["weather"]) > 1 else None
        area = data.get("nearest_area", [{}])[0]
        location = area.get("areaName", [{}])[0].get("value", "")
        return {
            "location": location,
            "temp": cur["temp_C"],
            "feels": cur["FeelsLikeC"],
            "desc": cur["weatherDesc"][0]["value"],
            "humidity": cur["humidity"],
            "high": today["maxtempC"],
            "low": today["mintempC"],
            "tomorrow_temp": tomorrow["avgtempC"] if tomorrow else "?",
            "tomorrow_desc": (
                tomorrow["hourly"][4]["weatherDesc"][0]["value"] if tomorrow else "?"
            ),
        }
    except Exception:
        return None


def read_state_file() -> dict[str, str]:
    """Read driftwm state from $XDG_RUNTIME_DIR/driftwm/state."""
    runtime_dir = Path(environ_get("XDG_RUNTIME_DIR", ""))
    path = runtime_dir / "driftwm" / "state"
    result = {}
    try:
        for line in path.read_text().splitlines():
            if "=" in line:
                k, v = line.split("=", 1)
                result[k.strip()] = v.strip()
    except (FileNotFoundError, PermissionError):
        pass
    return result


def environ_get(key: str, default: str) -> str:
    """Wrapper around os.environ.get for testability."""

    return os.environ.get(key, default)


def get_brightness() -> int | None:
    """Returns screen brightness percent or None if unavailable."""
    try:
        result = subprocess.run(
            ["brightnessctl", "get"],
            capture_output=True,
            text=True,
            timeout=2,
            check=False,
        )
        current = int(result.stdout.strip())
        max_result = subprocess.run(
            ["brightnessctl", "max"],
            capture_output=True,
            text=True,
            timeout=2,
            check=False,
        )
        maximum = int(max_result.stdout.strip())
        if maximum > 0:
            return current * 100 // maximum
    except (FileNotFoundError, subprocess.TimeoutExpired, ValueError):
        pass

    # Fallback to sysfs
    for device in sorted(Path("/sys/class/backlight").glob("*")):
        try:
            current = int((device / "brightness").read_text().strip())
            maximum = int((device / "max_brightness").read_text().strip())
            if maximum > 0:
                return current * 100 // maximum
        except (OSError, ValueError):
            continue
    return None


def brightness_icon(pct: int) -> str:
    if pct > 66:
        return ICON["bright_high"]
    if pct > 33:
        return ICON["bright_med"]
    return ICON["bright_low"]


def get_bluetooth() -> str | None:
    """Returns formatted bluetooth status string with icon, or None."""
    try:
        result = subprocess.run(
            ["bluetoothctl", "show"],
            capture_output=True,
            text=True,
            timeout=2,
            check=False,
        )
        powered = any("Powered: yes" in line for line in result.stdout.splitlines())
        if not powered:
            return f"{ICON['bt_off']}  off"

        connected = subprocess.run(
            ["bluetoothctl", "devices", "Connected"],
            capture_output=True,
            text=True,
            timeout=2,
            check=False,
        )
        devices = [
            line.split(" ", 2)[2]
            for line in connected.stdout.strip().splitlines()
            if line.startswith("Device ")
        ]
        if not devices:
            return f"{ICON['bt_on']}  on"
        name = devices[0][:16]
        count = len(devices)
        return f"{ICON['bt_connected']}  {name} ({count})"
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None


def get_wttr() -> str | None:
    """Fetch wttr.in compact output with location. Returns raw text or None."""
    try:
        req = urllib.request.Request(
            "https://wttr.in/?0QnT",
            headers={"User-Agent": "curl/8.0"},
        )
        with urllib.request.urlopen(req, timeout=5) as resp:  # noqa: S310
            return resp.read().decode("utf-8")
    except Exception:
        return None


# ── Mouse click handling ────────────────────────────────────

_orig_termios: list | None = None


def enable_mouse() -> None:
    global _orig_termios  # noqa: PLW0603
    fd = sys.stdin.fileno()
    _orig_termios = termios.tcgetattr(fd)
    new = termios.tcgetattr(fd)
    new[3] &= ~(termios.ICANON | termios.ECHO)
    termios.tcsetattr(fd, termios.TCSANOW, new)
    sys.stdout.write("\033[?1000h")
    sys.stdout.flush()


def disable_mouse() -> None:
    sys.stdout.write("\033[?1000l")
    sys.stdout.flush()
    if _orig_termios is not None:
        termios.tcsetattr(sys.stdin.fileno(), termios.TCSANOW, _orig_termios)


def poll_click(timeout: float) -> tuple[int, int] | None:
    """Wait up to timeout seconds for a mouse press. Returns (x, y) 1-based or None."""
    ready, _, _ = select.select([sys.stdin], [], [], timeout)
    if not ready:
        return None
    data = os.read(sys.stdin.fileno(), 64)
    idx = data.find(b"\033[M")
    if idx < 0 or idx + 5 >= len(data):
        return None
    btn = data[idx + 3] - 32
    if btn & 3 == 3:  # release, not press
        return None
    x = data[idx + 4] - 32
    y = data[idx + 5] - 32
    return x, y


def read_keyboard_layout() -> str:
    """Read keyboard layout from driftwm config."""
    config_dir = Path(environ_get("XDG_CONFIG_HOME", str(Path.home() / ".config")))
    config_path = config_dir / "driftwm" / "config.toml"
    try:
        for line in config_path.read_text().splitlines():
            stripped = line.strip()
            if stripped.startswith("layout"):
                val = stripped.split("=", 1)[1].strip().strip('"').strip("'")
                return val.upper()
    except (FileNotFoundError, PermissionError):
        pass
    return "US"
