#!/bin/sh
# Toggle GNOME color-scheme between default and prefer-dark, fire notification.
# theme-watch.sh listens for the gsettings change and runs theme-apply.sh.

set -eu

key='org.gnome.desktop.interface color-scheme'

case "$(gsettings get $key)" in
    *prefer-dark*) new='default';      label='Light' ;;
    *)             new='prefer-dark';  label='Dark'  ;;
esac

gsettings set $key "$new"

# x-canonical-private-synchronous makes swaync replace prior toggle notifications
# instead of stacking them on rapid switches.
notify-send \
    -h string:x-canonical-private-synchronous:theme \
    'Theme' "$label mode"
