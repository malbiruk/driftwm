#!/bin/sh
# Spotlight: unified search over open windows, suspended windows + installed apps.
# Open windows first, suspended stand-ins after (ᶻ prefix), apps last.
# Selection focuses a window, relaunches a stand-in, or launches an app.
# Windows come from `driftwm msg state` (one IPC roundtrip; also the only way
# to see suspended stand-ins — they aren't foreign toplevels). The .desktop
# scan is one awk pass, cached until an applications dir changes.
# Requires: driftwm, fuzzel, jq

XDG_DATA_DIRS="${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
# Reuse fuzzel's own usage cache so mod+d (drun) and spotlight share ranking.
# Format: `<basename>.desktop|<count>` per line.
FUZZEL_CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/fuzzel"
touch "$FUZZEL_CACHE"

# App table: `name \t icon \t exec \t desktop-file \t wmclass \t listable`.
# NoDisplay/Hidden/no-Exec entries stay in the table with listable=0 — they
# don't belong in the app list but often carry the StartupWMClass that names
# a window. `.dirs` alongside records which dirs the table was built from.
APPS_CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/driftwm-spotlight-apps.tsv"

# ~/.local first so it wins the first-seen dedup below.
app_dirs=$(printf '%s' "$HOME/.local/share:$XDG_DATA_DIRS" | tr ':' '\n' | sed 's|$|/applications|')
dirs_now=$(for dir in $app_dirs; do [ -d "$dir" ] && printf '%s\n' "$dir"; done)

display=$(mktemp)
lookup=$(mktemp)
tmp=""
trap 'rm -f "$display" "$lookup" "$tmp"' EXIT

# --- App table (rebuilt only when a .desktop file or applications dir changed) ---
rebuild=0
[ -f "$APPS_CACHE" ] || rebuild=1
# The dir set itself can change with no mtime to catch it (a data dir with
# old contents appearing in XDG_DATA_DIRS, or a dir vanishing wholesale).
if [ "$rebuild" = 0 ]; then
    [ "$dirs_now" = "$(cat "$APPS_CACHE.dirs" 2>/dev/null)" ] || rebuild=1
fi
if [ "$rebuild" = 0 ]; then
    # Dir mtime catches installs/removals, file mtime catches edits.
    stale=$(find $app_dirs -maxdepth 1 \( -type d -o -name '*.desktop' \) \
        -newer "$APPS_CACHE" 2>/dev/null | head -1)
    [ -n "$stale" ] && rebuild=1
fi
if [ "$rebuild" = 1 ]; then
    set --
    for dir in $dirs_now; do
        for f in "$dir"/*.desktop; do
            [ -f "$f" ] && set -- "$@" "$f"
        done
    done
    # Same-dir temp so the mv is an atomic rename, never a truncated copy.
    tmp=$(mktemp "$APPS_CACHE.XXXXXX")
    if [ "$#" -gt 0 ]; then
        # Single pass over every file; [Desktop Entry] section only.
        awk -F= '
            function emit() {
                if (did == "" || name == "") return
                listable = !nodisp && (type == "" || type == "Application") && exec_line != ""
                print name "\t" icon "\t" exec_line "\t" did "\t" wmclass "\t" (listable ? 1 : 0)
            }
            FNR == 1 {
                emit()
                main = 0; nodisp = 0
                type = ""; name = ""; icon = ""; exec_line = ""; wmclass = ""
                did = FILENAME; sub(/.*\//, "", did)
            }
            /^\[Desktop Entry\]/ { main = 1; next }
            /^\[/                { main = 0; next }
            !main                { next }
            /^Name=/           && name == ""      { sub(/^Name=/, "");           name = $0 }
            /^Icon=/           && icon == ""      { sub(/^Icon=/, "");           icon = $0 }
            /^Exec=/           && exec_line == "" { sub(/^Exec=/, "");           exec_line = $0 }
            /^Type=/           && type == ""      { sub(/^Type=/, "");           type = $0 }
            /^StartupWMClass=/ && wmclass == ""   { sub(/^StartupWMClass=/, ""); wmclass = $0 }
            /^NoDisplay=true/ { nodisp = 1 }
            /^Hidden=true/    { nodisp = 1 }
            END { emit() }
        ' "$@" > "$tmp"
    else
        : > "$tmp"
    fi
    mv "$tmp" "$APPS_CACHE"
    printf '%s\n' "$dirs_now" > "$APPS_CACHE.dirs"
fi

# --- Windows (canvas windows focused-first, then fullscreen/pinned, suspended last) ---
driftwm msg state --json 2>/dev/null \
    | jq -r '
        .Ok.State as $s |
        ( [$s.windows[] | select((.is_widget or .suspended) | not)]
          + $s.fullscreen + $s.pinned
          + [$s.windows[] | select(.suspended)] )
        | .[] | [(if .suspended then "s" else "w" end), .id, .app_id, .title] | @tsv' \
    | while IFS='	' read -r kind wid app_id title; do
        # Resolve app_id -> Name/Icon against the table: exact filename, then
        # filename substring, then StartupWMClass. Also tidies the title.
        row=$(TITLE="$title" awk -F'\t' -v id="$app_id" '
            $4 == id ".desktop" && exact == ""            { exact = $1 "\t" $2 }
            id != "" && index($4, id) && subm == ""       { subm = $1 "\t" $2 }
            id != "" && $5 == id && wm == ""              { wm = $1 "\t" $2 }
            END {
                best = exact != "" ? exact : subm != "" ? subm : wm
                if (best == "") best = id "\t" id
                split(best, b, "\t")
                if (b[2] == "") b[2] = id
                t = ENVIRON["TITLE"]
                gsub(/—|–/, "-", t)
                gsub(/‎|‏|⁨|⁩/, "", t)
                print b[1] "\t" b[2] "\t" t
            }' "$APPS_CACHE")
        app_name=${row%%	*}
        rest=${row#*	}
        icon=${rest%%	*}
        display_title=${rest#*	}
        [ "$kind" = "s" ] && mark="ᶻ" || mark="›"
        if [ -n "$display_title" ]; then
            printf '%s %s  %s\0icon\037%s\n' "$mark" "$display_title" "$app_name" "$icon" >> "$display"
        else
            printf '%s %s\0icon\037%s\n' "$mark" "$app_name" "$icon" >> "$display"
        fi
        printf '%s\t%s\n' "$kind" "$wid" >> "$lookup"
done

# --- Apps: dedup by Name (first-seen wins), rank by fuzzel usage count, then name ---
awk -F'\t' '$6 == 1 && !seen[$1]++' "$APPS_CACHE" \
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
