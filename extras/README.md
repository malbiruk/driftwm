# Extras

A curated, install-and-go starting point for driftwm: a single
[Quickshell](https://quickshell.org/) dashboard pinned at the canvas origin
(your "home"), plus opinionated power-canvas defaults. It's an alternative to
bare driftwm rather than a copy of the built-in defaults — tweak or strip freely.

## Install

```sh
./install.sh
```

Backs up any existing config (timestamped), then copies the config, helper
scripts, shader wallpapers, and the dashboard into `~/.config/driftwm`, plus
matching fuzzel and swaync themes into `~/.config/fuzzel` and
`~/.config/swaync`. Safe to re-run.

## The home dashboard

Pinned at the canvas origin — press `mod+a` (or 4-finger pinch-out) to jump
there. Text-only, i3bar style: time/date, keyboard layout, network, Bluetooth,
battery, volume, brightness, CPU/RAM, media controls, the system tray, a
notifications button, and a power menu. Tiles open their controls on click:

- **time** — a calendar
- **kbd** — switches to the next keyboard layout
- **net** — Wi-Fi toggle and the networks in range; tap one to connect (a
  password field appears for a new secured network), tap the connected one to
  disconnect, forget a saved one with its button
- **blu** — Bluetooth toggle, scan, and the paired/discovered devices; tap to
  connect, pair, or disconnect, forget a paired one with its button
- **vol** — output volume, mute, and device picker; the same for the
  microphone
- **bri** — brightness slider
- **bat** — charge state, time remaining, and the power profile (via
  power-profiles-daemon or tuned-ppd)
- **tray** — left click activates an item, right click opens its menu
- **power** — lock, log out, suspend, reboot, power off

Media controls follow the playing player. Tested with Quickshell 0.2.1 (git
2026-02-09); run it by hand with `qs -p ~/.config/driftwm/quickshell`.

## Contents

- `config.toml` — the compositor config (installed)
- `quickshell/` — the home dashboard (installed)
- `scripts/` — spotlight (open windows, suspended windows, and apps in one
  fuzzel search), the low-battery alert, and `driftwm-decal` for pinning
  transparent images to the canvas (installed)
- `wallpapers/` — GLSL shader wallpapers (installed; point `[background]` at
  one to use it)
- `fuzzel/` — minimal launcher theme (swaync's gray, no icons), frosted via blur
  (installed to `~/.config/fuzzel`)
- `swaync/` — swaync's defaults, with the panel's outer corners squared
  (installed to `~/.config/swaync`)
- `waybar/` — an optional touchscreen bar with an on-screen-keyboard toggle
  (not installed; the launch line is at the top of `touchbar.jsonc`)

## Dependencies

The config wires up standard Wayland tools and degrades gracefully if any are
missing. For the full experience:

- **[Quickshell](https://quickshell.org/)** (`qs`) — renders the dashboard;
  its tiles talk to NetworkManager (`nmcli`), BlueZ, UPower, and PipeWire
- **swaync** — notifications · **swayosd** — volume/brightness OSD ·
  **fuzzel** + **jq** — spotlight
- **swaylock** — lock · **swayidle** — lock before sleep
- **brightnessctl** — brightness slider · **playerctl** — media keys ·
  **wl-clipboard** — window screenshot to clipboard · **libnotify**
  (`notify-send`) — battery alerts

swaync keeps its default look (just the panel's outer corners squared); fuzzel
uses a minimal gray theme matching swaync's panel. Both are frosted via the
compositor's blur. swayosd runs on its own defaults.

## Customizing

`config.toml` is a starting point. Every option is documented in
[`config.reference.toml`](../config.reference.toml), and partial configs merge
with built-in defaults, so trim to taste.
