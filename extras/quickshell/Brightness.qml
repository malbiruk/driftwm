import QtQuick
import QtQuick.Layouts
import Quickshell.Io

// Reads the first backlight under /sys directly, sparing a process per tick;
// brightnessctl sets it, as the keys do.
Tile {
    id: root

    property string backlight: ""
    property int maximum: 0
    property int current: 0
    readonly property bool present: backlight !== "" && maximum > 0

    label: "bri"
    value: present ? `${Math.round(current / maximum * 100)}%` : "—"
    clickable: present
    onClicked: popover.toggle()

    Process {
        command: ["sh", "-c", "ls /sys/class/backlight | head -n1"]
        running: true
        stdout: StdioCollector {
            onStreamFinished: {
                const name = text.trim()
                if (name) root.backlight = `/sys/class/backlight/${name}`
            }
        }
    }

    FileView {
        path: root.backlight ? `${root.backlight}/max_brightness` : ""
        onLoaded: root.maximum = Number(text().trim())
    }

    FileView {
        id: level
        path: root.backlight ? `${root.backlight}/brightness` : ""
        onLoaded: root.current = Number(text().trim())
    }

    // sysfs gives no change notifications, so poll to catch the keys' changes.
    Timer {
        interval: 3000
        running: root.present
        repeat: true
        onTriggered: level.reload()
    }

    // Coalesce a drag into one brightnessctl call per tick; the tile updates
    // optimistically and re-reads sysfs once the call has landed.
    Timer {
        id: setter
        property real pending: 0
        interval: 50
        onTriggered: {
            if (writer.running) {
                restart()
                return
            }
            root.current = Math.round(pending / 100 * root.maximum)
            writer.command = ["brightnessctl", "-q", "set", `${Math.round(pending)}%`]
            writer.running = true
        }
    }

    Process {
        id: writer
        onExited: level.reload()
    }

    Popover {
        id: popover
        target: root

        RowLayout {
            spacing: 10

            ThinSlider {
                id: slider
                Layout.preferredWidth: 200
                from: 1
                to: 100
                stepSize: 1
                onMoved: {
                    setter.pending = value
                    setter.restart()
                }
            }

            Binding {
                target: slider
                property: "value"
                value: root.present ? root.current / root.maximum * 100 : 0
                when: !slider.pressed
                restoreMode: Binding.RestoreNone
            }

            Text {
                Layout.preferredWidth: 36
                horizontalAlignment: Text.AlignRight
                text: `${Math.round(slider.value)}%`
                color: Theme.fg
                font.pixelSize: Theme.fontSize
                font.family: Theme.fontFamily
            }
        }
    }
}
