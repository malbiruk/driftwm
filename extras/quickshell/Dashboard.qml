import QtQuick
import QtQuick.Layouts
import Quickshell
import Quickshell.Wayland

// driftwm's `drift-dashboard` window rule pins this to the canvas origin, so
// it carries no anchors. OnDemand keyboard focus lets a popover's text field
// (the wifi password) take typing.
PanelWindow {
    id: root

    WlrLayershell.namespace: "drift-dashboard"
    WlrLayershell.keyboardFocus: WlrKeyboardFocus.OnDemand
    exclusionMode: ExclusionMode.Ignore
    color: "transparent"
    implicitWidth: 270
    // The footer buttons' padding counts toward the bottom margin.
    implicitHeight: content.implicitHeight + 2 * content.anchors.topMargin - notifications.padY

    Rectangle {
        anchors.fill: parent
        color: Theme.bg
        border.color: Theme.border
        border.width: 1
        radius: Theme.radius
    }

    ColumnLayout {
        id: content
        anchors {
            left: parent.left
            right: parent.right
            top: parent.top
            leftMargin: 18
            rightMargin: 18
            topMargin: 18
        }
        spacing: Theme.sectionGap

        Clock { Layout.alignment: Qt.AlignHCenter }

        // The tile block is centered with its column gap equal to the side
        // margins; the tray and the footer buttons' text line up with its edges.
        RowLayout {
            id: tiles
            readonly property real leftLabel: Math.max(battery.labelWidth, network.labelWidth, brightness.labelWidth, cpu.labelWidth)
            readonly property real rightLabel: Math.max(keyboard.labelWidth, bluetooth.labelWidth, volume.labelWidth, ram.labelWidth)
            readonly property real leftWidth: leftLabel + Theme.labelGap + Math.max(battery.valueWidth, network.valueWidth, brightness.valueWidth, cpu.valueWidth)
            readonly property real rightWidth: rightLabel + Theme.labelGap + Math.max(keyboard.valueWidth, bluetooth.valueWidth, volume.valueWidth, ram.valueWidth)

            Layout.alignment: Qt.AlignHCenter
            Layout.fillWidth: false
            spacing: (root.implicitWidth - leftWidth - rightWidth) / 3

            ColumnLayout {
                spacing: 6
                Battery { id: battery; labelColumn: tiles.leftLabel; Layout.preferredWidth: tiles.leftWidth }
                Network { id: network; labelColumn: tiles.leftLabel; Layout.preferredWidth: tiles.leftWidth }
                Brightness { id: brightness; labelColumn: tiles.leftLabel; Layout.preferredWidth: tiles.leftWidth }
                Cpu { id: cpu; labelColumn: tiles.leftLabel; Layout.preferredWidth: tiles.leftWidth }
            }

            ColumnLayout {
                spacing: 6
                KbdLayout { id: keyboard; labelColumn: tiles.rightLabel; Layout.preferredWidth: tiles.rightWidth }
                Bluetooth { id: bluetooth; labelColumn: tiles.rightLabel; Layout.preferredWidth: tiles.rightWidth }
                Volume { id: volume; labelColumn: tiles.rightLabel; Layout.preferredWidth: tiles.rightWidth }
                Ram { id: ram; labelColumn: tiles.rightLabel; Layout.preferredWidth: tiles.rightWidth }
            }
        }

        Mpris {
            Layout.fillWidth: true
            Layout.leftMargin: footer.x + notifications.padX
            Layout.rightMargin: footer.x + power.padX
        }

        Tray { Layout.leftMargin: footer.x + notifications.padX }

        RowLayout {
            id: footer
            Layout.alignment: Qt.AlignHCenter
            Layout.fillWidth: false
            Layout.preferredWidth: tiles.implicitWidth + notifications.padX + power.padX
            // The buttons' padding would otherwise widen this gap.
            Layout.topMargin: -notifications.padY

            // Notifications themselves stay swaync's job; this is the entry point.
            FlatButton {
                id: notifications
                text: "notifications"
                onClicked: Quickshell.execDetached(["swaync-client", "-t", "-sw"])
            }

            Item { Layout.fillWidth: true }

            PowerMenu { id: power }
        }
    }
}
