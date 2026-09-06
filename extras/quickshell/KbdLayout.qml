import QtQuick
import Quickshell
import Quickshell.Io

// Watching driftwm's state file for the layout's short code beats polling
// `driftwm msg layout`.
Tile {
    id: root

    label: "kbd"
    value: "—"
    clickable: true
    onClicked: Quickshell.execDetached(["driftwm", "msg", "action", "switch-layout next"])

    FileView {
        path: `${Quickshell.env("XDG_RUNTIME_DIR")}/driftwm/state`
        watchChanges: true
        onFileChanged: reload()
        onLoaded: {
            const match = text().match(/^layout_short=(.*)$/m)
            root.value = match ? match[1].trim().toLowerCase() : "—"
        }
    }
}
