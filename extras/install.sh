#!/bin/sh
# Install the driftwm "home dashboard" rice into your config dir.
#
# Idempotent and non-destructive: existing config/scripts/dashboard are backed
# up (timestamped, never clobbered) before the rice's versions are copied into
# ~/.config/driftwm. See README.md for the tools this setup expects.
set -eu

src="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
cfg="${XDG_CONFIG_HOME:-$HOME/.config}/driftwm"
ts="$(date +%Y%m%d-%H%M%S)"

mkdir -p "$cfg"

# Move an existing target aside (timestamped) so a customized one is never lost.
backup() {
    if [ -e "$cfg/$1" ]; then
        mv -- "$cfg/$1" "$cfg/$1.$ts.bak"
        echo "backed up existing $1 -> $1.$ts.bak"
    fi
}

backup config.toml
cp -- "$src/config.toml" "$cfg/config.toml"

# Scripts: config.toml references them by absolute path (~/.config/driftwm/...)
# rather than bare name, because a systemd user service launched by a display
# manager gets a minimal PATH that often excludes ~/.local/bin.
backup scripts
cp -r -- "$src/scripts" "$cfg/scripts"
chmod +x "$cfg"/scripts/*.sh

# Astal home dashboard (the only part driftwm itself doesn't provide).
if [ -d "$src/astal" ]; then
    backup astal
    cp -r -- "$src/astal" "$cfg/astal"
fi

echo "installed driftwm rice -> $cfg"

# The rice still runs without these — missing pieces just no-op — but warn so the
# experience isn't silently incomplete.
missing=""
for tool in ags swaync swayosd-server fuzzel swaylock swayidle \
            sway-audio-idle-inhibit wlrctl brightnessctl playerctl notify-send; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
    echo
    echo "note: these tools the rice uses are not on PATH (install for the full experience):"
    echo "  $missing"
fi
