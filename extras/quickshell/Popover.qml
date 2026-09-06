import QtQuick
import QtQuick.Layouts
import Quickshell

// A real xdg popup under `target`, so it can overhang the panel; grabFocus
// dismisses it on an outside click and lets a text field inside take the
// keyboard.
PopupWindow {
    id: root

    required property Item target
    property int padding: 12
    default property alias content: inner.data

    anchor.item: target
    anchor.edges: Edges.Bottom
    anchor.gravity: Edges.Bottom
    anchor.margins.top: 6
    grabFocus: true
    color: "transparent"
    implicitWidth: inner.implicitWidth + 2 * padding + 2
    implicitHeight: inner.implicitHeight + 2 * padding + 2

    function toggle() {
        visible = !visible
    }

    onVisibleChanged: {
        if (visible) {
            if (Popups.current && Popups.current !== root) Popups.current.visible = false
            Popups.current = root
        } else if (Popups.current === root) {
            Popups.current = null
        }
    }

    Rectangle {
        anchors.fill: parent
        color: Theme.bg
        border.color: Theme.border
        border.width: 1
        radius: Theme.popupRadius

        ColumnLayout {
            id: inner
            x: root.padding + 1
            y: root.padding + 1
            width: parent.width - 2 * root.padding - 2
            spacing: 6
        }
    }
}
