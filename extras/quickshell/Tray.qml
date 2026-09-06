import QtQuick
import QtQuick.Layouts
import Quickshell
import Quickshell.Widgets
import Quickshell.Services.SystemTray

RowLayout {
    id: root

    visible: SystemTray.items.values.length > 0
    spacing: 6

    Repeater {
        model: SystemTray.items

        Item {
            id: entry
            required property var modelData

            implicitWidth: 16
            implicitHeight: 16

            IconImage {
                anchors.fill: parent
                source: entry.modelData.icon
            }

            QsMenuAnchor {
                id: menu
                menu: entry.modelData.menu
                anchor.item: entry
                anchor.edges: Edges.Bottom
                anchor.gravity: Edges.Bottom
            }

            MouseArea {
                anchors.fill: parent
                acceptedButtons: Qt.LeftButton | Qt.RightButton | Qt.MiddleButton
                cursorShape: Qt.PointingHandCursor
                onClicked: mouse => {
                    if (mouse.button === Qt.RightButton || entry.modelData.onlyMenu) {
                        if (entry.modelData.hasMenu) menu.open()
                    } else if (mouse.button === Qt.MiddleButton) {
                        entry.modelData.secondaryActivate()
                    } else {
                        entry.modelData.activate()
                    }
                }
            }
        }
    }
}
