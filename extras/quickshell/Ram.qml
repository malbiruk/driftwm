import QtQuick
import Quickshell.Io

Tile {
    id: root

    property real totalGb: 0
    property real usedGb: 0
    property real swapTotalGb: 0
    property real swapUsedGb: 0

    label: "ram"
    value: "—"
    clickable: true
    onClicked: popover.toggle()

    function gb(kib) {
        return kib / 1048576
    }

    FileView {
        id: meminfo
        path: "/proc/meminfo"
        onLoaded: {
            const content = text()
            const field = name => {
                const match = content.match(new RegExp(`${name}:\\s+(\\d+)`))
                return match ? Number(match[1]) : 0
            }
            const total = field("MemTotal")
            const available = field("MemAvailable")
            if (total === 0) return
            root.totalGb = root.gb(total)
            root.usedGb = root.gb(total - available)
            root.swapTotalGb = root.gb(field("SwapTotal"))
            root.swapUsedGb = root.gb(field("SwapTotal") - field("SwapFree"))
            root.value = `${Math.round((1 - available / total) * 100)}%`
        }
    }

    Timer {
        interval: 2000
        running: true
        repeat: true
        onTriggered: meminfo.reload()
    }

    Popover {
        id: popover
        target: root

        Text {
            text: `${root.usedGb.toFixed(1)} / ${root.totalGb.toFixed(1)} GB used`
            color: Theme.fg
            font.pixelSize: Theme.fontSize
            font.family: Theme.fontFamily
        }

        Text {
            visible: root.swapTotalGb > 0
            text: `swap ${root.swapUsedGb.toFixed(1)} / ${root.swapTotalGb.toFixed(1)} GB`
            color: Theme.fg
            opacity: Theme.dim
            font.pixelSize: Theme.smallFontSize
            font.family: Theme.fontFamily
        }
    }
}
