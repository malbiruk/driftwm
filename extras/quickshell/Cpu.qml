import QtQuick
import QtQuick.Layouts
import Quickshell.Io

// Usage = busy delta / total delta between samples of /proc/stat, for the
// whole machine (the tile) and per thread (the popover).
Tile {
    id: root

    property var previous: ({})
    property var threads: []
    property string load: ""

    label: "cpu"
    value: "—"
    clickable: true
    onClicked: popover.toggle()

    function usage(name, parts) {
        const idle = parts[3] + (parts[4] || 0)
        const total = parts.reduce((a, b) => a + b, 0)
        const last = previous[name] || { idle: idle, total: total }
        previous[name] = { idle: idle, total: total }
        const totalDelta = total - last.total
        return totalDelta > 0 ? Math.round((1 - (idle - last.idle) / totalDelta) * 100) : -1
    }

    FileView {
        id: stat
        path: "/proc/stat"
        onLoaded: {
            const perThread = []
            let overall = -1
            for (const line of text().split("\n")) {
                const fields = line.trim().split(/\s+/)
                if (!/^cpu\d*$/.test(fields[0])) continue
                const percent = root.usage(fields[0], fields.slice(1).map(Number))
                if (fields[0] === "cpu") overall = percent
                else perThread.push(percent)
            }
            root.value = overall >= 0 ? `${overall}%` : "—"
            root.threads = perThread
        }
    }

    FileView {
        id: loadavg
        path: "/proc/loadavg"
        onLoaded: root.load = text().split(" ").slice(0, 3).join(" · ")
    }

    Timer {
        interval: 2000
        running: true
        repeat: true
        onTriggered: {
            stat.reload()
            if (popover.visible) loadavg.reload()
        }
    }

    Popover {
        id: popover
        target: root
        onVisibleChanged: if (visible) loadavg.reload()

        GridLayout {
            columns: 4
            columnSpacing: 10
            rowSpacing: 2

            Repeater {
                model: root.threads

                Text {
                    required property int modelData
                    Layout.preferredWidth: 36
                    horizontalAlignment: Text.AlignRight
                    text: modelData >= 0 ? `${modelData}%` : "—"
                    color: Theme.fg
                    font.pixelSize: Theme.fontSize
                    font.family: Theme.fontFamily
                }
            }
        }

        Text {
            Layout.topMargin: 2
            text: `load ${root.load} (1, 5, 15 min)`
            color: Theme.fg
            opacity: Theme.dim
            font.pixelSize: Theme.smallFontSize
            font.family: Theme.fontFamily
        }
    }
}
