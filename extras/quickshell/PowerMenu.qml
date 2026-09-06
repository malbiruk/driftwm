import QtQuick
import QtQuick.Layouts
import Quickshell

// Session actions sit behind a popover rather than firing on one click.
FlatButton {
    id: root

    readonly property var actions: [
        { label: "lock", command: ["swaylock", "-f"] },
        { label: "log out", command: ["driftwm", "msg", "action", "quit"] },
        { label: "suspend", command: ["systemctl", "suspend"] },
        { label: "reboot", command: ["systemctl", "reboot"] },
        { label: "power off", command: ["systemctl", "poweroff"] },
    ]

    text: "power"
    onClicked: popover.toggle()

    Popover {
        id: popover
        target: root

        Repeater {
            model: root.actions

            ListRow {
                required property var modelData
                Layout.fillWidth: true
                Layout.preferredWidth: metrics.width + 24
                text: modelData.label

                TextMetrics {
                    id: metrics
                    text: modelData.label
                    font.pixelSize: Theme.fontSize
                    font.family: Theme.fontFamily
                }
                onClicked: {
                    popover.visible = false
                    Quickshell.execDetached(modelData.command)
                }
            }
        }
    }
}
