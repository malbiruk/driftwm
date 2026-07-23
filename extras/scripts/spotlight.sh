#!/bin/sh
# Spotlight: unified search over open windows, suspended windows + installed apps.
# Open windows first, suspended stand-ins after (ᶻ prefix), apps last.
# Selection focuses a window, relaunches a stand-in, or launches an app.
# Windows come from `driftwm msg state` (one IPC roundtrip; also the only way
# to see suspended stand-ins — they aren't foreign toplevels).
# Requires: driftwm, fuzzel, jq

XDG_DATA_DIRS="${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
# Reuse fuzzel's own usage cache so mod+d (drun) and spotlight share ranking.
# Format: `<basename>.desktop|<count>` per line.
FUZZEL_CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/fuzzel"
touch "$FUZZEL_CACHE"

# Maps a window's app_id to .desktop Name/Icon.
lookup_desktop() {
    id="$1"
    for dir in "$HOME/.local/share/applications" $(printf '%s' "$XDG_DATA_DIRS" | tr ':' '\n' | sed 's|$|/applications|'); do
        for f in "$dir/$id.desktop" "$dir"/*"$id"*.desktop; do
            [ -f "$f" ] || continue
            name=$(grep -m1 '^Name=' "$f" | cut -d= -f2-)
            icon=$(grep -m1 '^Icon=' "$f" | cut -d= -f2-)
            [ -n "$name" ] && printf '%s\t%s' "$name" "${icon:-$id}" && return
        done
    done
    for dir in "$HOME/.local/share/applications" $(printf '%s' "$XDG_DATA_DIRS" | tr ':' '\n' | sed 's|$|/applications|'); do
        [ -d "$dir" ] || continue
        f=$(grep -rl "^StartupWMClass=$id$" "$dir"/*.desktop 2>/dev/null | head -1)
        if [ -n "$f" ]; then
            name=$(grep -m1 '^Name=' "$f" | cut -d= -f2-)
            icon=$(grep -m1 '^Icon=' "$f" | cut -d= -f2-)
            [ -n "$name" ] && printf '%s\t%s' "$name" "${icon:-$id}" && return
        fi
    done
    printf '%s\t%s' "$id" "$id"
}

display=$(mktemp)
lookup=$(mktemp)
apps_tmp=$(mktemp)
tmp=""
trap 'rm -f "$display" "$lookup" "$apps_tmp" "$tmp"' EXIT

# --- Windows (canvas windows focused-first, then fullscreen/pinned, suspended last) ---
driftwm msg state --json 2>/dev/null \
    | jq -r '
        .Ok.State as $s |
        ( [$s.windows[] | select((.is_widget or .suspended) | not)]
          + $s.fullscreen + $s.pinned
          + [$s.windows[] | select(.suspended)] )
        | .[] | [(if .suspended then "s" else "w" end), .id, .app_id, .title] | @tsv' \
    | while IFS='	' read -r kind wid app_id title; do
        display_title=$(printf '%s' "$title" | sed -e 's/—/-/g' -e 's/–/-/g' -e 's/‎//g' -e 's/‏//g' -e 's/⁨//g' -e 's/⁩//g')
        desktop=$(lookup_desktop "$app_id")
        app_name="${desktop%%	*}"
        icon="${desktop#*	}"
        [ "$kind" = "s" ] && mark="ᶻ" || mark="›"
        if [ -n "$display_title" ]; then
            printf '%s %s  %s\0icon\037%s\n' "$mark" "$display_title" "$app_name" "$icon" >> "$display"
        else
            printf '%s %s\0icon\037%s\n' "$mark" "$app_name" "$icon" >> "$display"
        fi
        printf '%s\t%s\n' "$kind" "$wid" >> "$lookup"
done

# --- Apps (parse .desktop in [Desktop Entry] section only) ---
for dir in "$HOME/.local/share/applications" $(printf '%s' "$XDG_DATA_DIRS" | tr ':' '\n' | sed 's|$|/applications|'); do
    [ -d "$dir" ] || continue
    for f in "$dir"/*.desktop; do
        [ -f "$f" ] || continue
        did="${f##*/}"
        awk -F= -v did="$did" '
            BEGIN { main=0; nodisp=0; hidden=0; type=""; name=""; icon=""; exec_line="" }
            /^\[Desktop Entry\]/ { main=1; next }
            /^\[/                { main=0; next }
            !main                { next }
            /^Name=/      && name==""      { sub(/^Name=/, "");      name=$0 }
            /^Icon=/      && icon==""      { sub(/^Icon=/, "");      icon=$0 }
            /^Exec=/      && exec_line=="" { sub(/^Exec=/, "");      exec_line=$0 }
            /^Type=/      && type==""      { sub(/^Type=/, "");      type=$0 }
            /^NoDisplay=true/              { nodisp=1 }
            /^Hidden=true/                 { hidden=1 }
            END {
                if (nodisp || hidden)               exit 1
                if (type != "" && type != "Application") exit 1
                if (name == "" || exec_line == "")  exit 1
                print name "\t" icon "\t" exec_line "\t" did
            }
        ' "$f" >> "$apps_tmp"
    done
done

# Dedup by Name (first-seen wins, so ~/.local overrides /usr/share since we listed it first),
# annotate with usage count from fuzzel's cache (keyed by .desktop filename),
# then sort by count desc then name asc (case-insensitive).
awk -F'\t' '!seen[$1]++' "$apps_tmp" \
  | awk -F'\t' -v cache="$FUZZEL_CACHE" '
        BEGIN {
            while ((getline line < cache) > 0) {
                n = split(line, a, "|")
                if (n == 2) count[a[1]] = a[2]
            }
        }
        { printf "%d\t%s\t%s\t%s\t%s\n", (count[$4]+0), $1, $2, $3, $4 }
    ' \
  | sort -t '	' -k1,1nr -k2,2f \
  | while IFS='	' read -r _count name icon exec_line did; do
      printf '%s\0icon\037%s\n' "$name" "$icon" >> "$display"
      printf 'a\t%s\t%s\n' "$did" "$exec_line" >> "$lookup"
done

[ -s "$display" ] || exit 0

selected=$(fuzzel --dmenu \
    --width=50 \
    --no-run-if-empty \
    --index \
    < "$display")

[ -z "$selected" ] && exit 0

line_num=$((selected + 1))
match=$(sed -n "${line_num}p" "$lookup")
kind=$(printf '%s' "$match" | cut -f1)

if [ "$kind" = "w" ]; then
    sel_id=$(printf '%s' "$match" | cut -f2)
    driftwm msg focus --id "$sel_id"
elif [ "$kind" = "s" ]; then
    sel_id=$(printf '%s' "$match" | cut -f2)
    # Pan to the stand-in first so the relaunch adopts in view.
    driftwm msg focus --id "$sel_id"
    driftwm msg relaunch --id "$sel_id"
else
    sel_did=$(printf '%s' "$match" | cut -f2)
    exec_line=$(printf '%s' "$match" | cut -f3-)
    # Bump count in fuzzel's cache so mod+d ranking stays in sync.
    tmp=$(mktemp)
    awk -F'|' -v d="$sel_did" '
        $1 == d { print $1 "|" ($2+1); found=1; next }
        { print }
        END { if (!found) print d "|1" }
    ' "$FUZZEL_CACHE" > "$tmp" && mv "$tmp" "$FUZZEL_CACHE"
    # Strip Exec field codes (%f %F %u %U %d %D %n %N %i %c %k %v %m) per Desktop Entry spec.
    exec_clean=$(printf '%s' "$exec_line" | sed -E 's/%[fFuUdDnNickvm]//g; s/  +/ /g')
    setsid sh -c "$exec_clean" </dev/null >/dev/null 2>&1 &
fi
