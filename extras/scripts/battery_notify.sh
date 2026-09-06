#!/bin/bash
# Battery notification daemon — polls every 60s, notifies at threshold crossings.

BATTERY_LOW=15
BATTERY_CRITICAL=5
COOLDOWN=300
STATE_DIR="/tmp/driftwm-battery-notify"
mkdir -p "$STATE_DIR"

check_cooldown() {
    local key="$1"
    local now=$(date +%s)
    local state_file="$STATE_DIR/$key"
    if [ -f "$state_file" ]; then
        local last=$(cat "$state_file")
        [ $((now - last)) -lt $COOLDOWN ] && return 1
    fi
    echo "$now" > "$state_file"
    return 0
}

trap 'rm -rf "$STATE_DIR"; exit 0' EXIT INT TERM

while true; do
    bat=$(cat /sys/class/power_supply/BAT*/capacity 2>/dev/null | head -1)
    status=$(cat /sys/class/power_supply/BAT*/status 2>/dev/null | head -1)

    if [ -n "$bat" ] && [ "$status" = "Discharging" ]; then
        if [ "$bat" -le "$BATTERY_CRITICAL" ]; then
            check_cooldown critical && \
                notify-send -u critical "Critical Battery" "${bat}% — plug in immediately"
        elif [ "$bat" -le "$BATTERY_LOW" ]; then
            check_cooldown low && \
                notify-send -u normal "Low Battery" "${bat}% — consider charging soon"
        fi
    fi

    sleep 60
done
