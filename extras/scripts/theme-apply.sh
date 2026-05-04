#!/bin/sh
# Apply rice theme to driftwm + waybar configs.
# Usage: theme-apply.sh light|dark [--no-restart]
#
# driftwm picks up config.toml changes via mtime poll (~500ms).
# waybar is killed and relaunched in autostart order — unless --no-restart.

set -eu

mode="${1:-}"
no_restart="${2:-}"

case "$mode" in
    light|dark) ;;
    *) echo "usage: $0 light|dark [--no-restart]" >&2; exit 2 ;;
esac

EXTRAS="$HOME/Documents/work/scripts/driftwm/extras"

if [ "$mode" = "light" ]; then
    BG="#FDF6E3"
    FG="#5C6A72"
    SHADER="pink_cloud.glsl"
    GTK_THEME="Everforest-Light"
    GTK_PREFER_DARK=0
    COSMIC_DARK=false
else
    BG="#272E33"
    FG="#D3C6AA"
    SHADER="dark_sea.glsl"
    GTK_THEME="Everforest-Dark"
    GTK_PREFER_DARK=1
    COSMIC_DARK=true
fi

# driftwm: decorations.bg_color, decorations.fg_color, background.path
# (output.outline.color stays light — same in both modes by design)
sed -i \
    -e "s|^bg_color = \".*\"|bg_color = \"$BG\"|" \
    -e "s|^fg_color = \".*\"|fg_color = \"$FG\"|" \
    -e "s|^path = \".*static/[^\"]*\\.glsl\"|path = \"/usr/local/share/driftwm/wallpapers/static/$SHADER\"|" \
    "$EXTRAS/config.toml"

# alacritty: swap import to colors-{light,dark}.toml. Live-reload picks it up.
sed -i "s|colors-[a-z]*\\.toml|colors-${mode}.toml|" "$EXTRAS/alacritty/alacritty.toml"

# GTK 3 + GTK 4: settings.ini is the file-based fallback for apps that don't
# subscribe to gsettings. gsettings broadcast below covers the rest.
for f in "$HOME/.config/gtk-3.0/settings.ini" "$HOME/.config/gtk-4.0/settings.ini"; do
    [ -f "$f" ] || continue
    sed -i \
        -e "s|^gtk-theme-name=.*|gtk-theme-name=$GTK_THEME|" \
        -e "s|^gtk-application-prefer-dark-theme=.*|gtk-application-prefer-dark-theme=$GTK_PREFER_DARK|" \
        "$f"
done
# Live broadcast for running GTK apps subscribed via dconf.
gsettings set org.gnome.desktop.interface gtk-theme "$GTK_THEME"

# COSMIC apps: cosmic-config inotify-watches Mode/v1/is_dark and propagates live.
COSMIC_MODE_FILE="$HOME/.config/cosmic/com.system76.CosmicTheme.Mode/v1/is_dark"
[ -f "$COSMIC_MODE_FILE" ] && printf '%s' "$COSMIC_DARK" > "$COSMIC_MODE_FILE"

# waybar CSS: only `background:` inside window#waybar and tooltip blocks.
# Leaves button.active background (accent) and tooltip color (fg, mode-constant
# per current spec) untouched.
for f in "$EXTRAS/waybar/taskbar.css" "$EXTRAS/waybar/tray.css"; do
    awk -v bg="$BG" '
        /^window#waybar \{/ { in_block = 1 }
        /^tooltip \{/       { in_block = 1 }
        /^\}/               { in_block = 0 }
        in_block && /background:/ { sub(/#[A-Fa-f0-9]+/, bg) }
        { print }
    ' "$f" > "$f.tmp" && mv "$f.tmp" "$f"
done

if [ "$no_restart" = "--no-restart" ]; then
    exit 0
fi

# Restart waybar in autostart order (taskbar first, then tray after 1s).
pkill -x waybar 2>/dev/null || true
sleep 0.3
waybar -c "$HOME/.config/waybar/taskbar.jsonc" -s "$HOME/.config/waybar/taskbar.css" &
sleep 1
waybar -c "$HOME/.config/waybar/tray.jsonc" -s "$HOME/.config/waybar/tray.css" &

# swayosd-server snapshots the GTK theme at startup; restart for OSD popups
# (volume, brightness) to render in the new palette.
pkill -x swayosd-server 2>/dev/null || true
sleep 0.2
swayosd-server --top-margin 0.95 &
