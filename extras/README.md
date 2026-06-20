# Extras

A curated, install-and-go starting point for driftwm: a single
[Astal](https://aylur.github.io/astal/) dashboard pinned at the canvas origin
(your "home"), plus opinionated power-canvas defaults. It's an alternative to
bare driftwm rather than a copy of the built-in defaults — tweak or strip freely.

## Install

```sh
./install.sh
```

Backs up any existing `~/.config/driftwm/config.toml` (timestamped), then copies
the config, helper scripts, and the Astal dashboard into `~/.config/driftwm`.
Safe to re-run.

## The home dashboard

Pinned at the canvas origin — press `mod+a` (or 4-finger pinch-out) to jump there.
Shows time/date, keyboard layout, network, battery, volume, brightness, CPU/RAM,
power profiles, the system tray, a notifications button, and a power menu.

## Contents

- `config.toml` — the compositor config (installed)
- `astal/` — the home dashboard (installed)
- `scripts/` — window search + low-battery alert (installed)
- `wallpapers/` — GLSL shader wallpapers (examples; point `[background]` at one to use it)

## Dependencies

The config wires up standard Wayland tools and degrades gracefully if any are
missing. For the full experience:

- **[Astal](https://aylur.github.io/astal/) / AGS** — renders the dashboard
- **swaync** — notifications · **swayosd** — volume/brightness OSD · **fuzzel** — launcher + window search
- **swaylock** — lock · **swayidle** + **sway-audio-idle-inhibit** — idle lock/suspend, paused while audio plays
- **wlrctl** — window search · **brightnessctl** — idle dim · **playerctl** — media keys · **libnotify** (`notify-send`) — battery alerts

swaync, swayosd, and fuzzel run on their own defaults — no themes are shipped
here.

## Customizing

`config.toml` is a starting point. Every option is documented in
[`config.reference.toml`](../config.reference.toml), and partial configs merge
with built-in defaults, so trim to taste.
