#!/bin/bash
# Launch all driftwm dashboard widgets as Alacritty terminals.
# Each gets its own app_id for window rule matching.

DIR="$(cd "$(dirname "$0")" && pwd)"

launch() {
    local name="$1" cols="$2" lines="$3" script="$4"
    alacritty --class "drift-${name}" \
        -o "window.dimensions.columns=${cols}" \
        -o "window.dimensions.lines=${lines}" \
        -o "window.padding.x=8" \
        -o "window.padding.y=8" \
        -o "window.decorations=\"None\"" \
        -e uv run --project "$DIR" python "$DIR/${script}" &
}

launch clock       34 6  clock_widget.py
launch stats       34 11 stats_widget.py
launch canvas      26 4  canvas_widget.py
launch layout      6 4  layout_widget.py
launch calendar    22 11  calendar_widget.py
launch weather     22 6  weather_widget.py
launch notif       21 4  notif_widget.py

wait
