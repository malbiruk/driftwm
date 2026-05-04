#!/bin/sh
# Listen for GNOME color-scheme changes and apply rice theme.
# Autostart entry — runs forever. Logs to /tmp/driftwm-theme.log.

set -u

LOG=/tmp/driftwm-theme.log
SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"
APPLY="$SCRIPT_DIR/theme-apply.sh"

current_mode() {
    case "$(gsettings get org.gnome.desktop.interface color-scheme 2>/dev/null)" in
        *prefer-dark*) echo dark ;;
        *)             echo light ;;
    esac
}

# Truncate log on each watcher start; re-redirect script output.
: >"$LOG"
exec >>"$LOG" 2>&1

echo "[$(date '+%F %T')] watcher start (pid $$)"

# Startup sync: write files to match current gsettings, but don't restart waybar
# (autostart launches it fresh with the just-written CSS).
mode=$(current_mode)
echo "[$(date '+%F %T')] startup mode=$mode"
"$APPLY" "$mode" --no-restart

# Block on gsettings monitor. One line per change; we re-query gsettings
# rather than parse the line, in case the format ever shifts.
gsettings monitor org.gnome.desktop.interface color-scheme | while read -r _; do
    mode=$(current_mode)
    echo "[$(date '+%F %T')] change mode=$mode"
    "$APPLY" "$mode"
done
