import QtQuick
import QtQuick.Controls

Slider {
    id: root

    implicitWidth: 220
    implicitHeight: 20

    background: Rectangle {
        x: root.leftPadding
        y: root.topPadding + root.availableHeight / 2 - height / 2
        width: root.availableWidth
        height: 4
        radius: 2
        color: Theme.track

        Rectangle {
            width: root.visualPosition * parent.width
            height: parent.height
            radius: 2
            color: Theme.fg
        }
    }

    handle: Rectangle {
        x: root.leftPadding + root.visualPosition * (root.availableWidth - width)
        y: root.topPadding + root.availableHeight / 2 - height / 2
        width: 14
        height: 14
        radius: 7
        color: Theme.fg
    }
}
