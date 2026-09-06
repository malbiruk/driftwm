import QtQuick

Item {
    id: root

    property string text
    property bool highlighted: false
    property int fontSize: Theme.fontSize
    property int padX: 10
    property int padY: 6
    property real textOpacity: 1
    signal clicked()

    implicitWidth: label.implicitWidth + 2 * padX
    implicitHeight: label.implicitHeight + 2 * padY

    Rectangle {
        anchors.fill: parent
        radius: 8
        color: Theme.fg
        opacity: mouse.containsMouse ? Theme.hoverOpacity : root.highlighted ? 0.06 : 0
    }

    Text {
        id: label
        anchors.centerIn: parent
        text: root.text
        color: Theme.fg
        opacity: root.textOpacity
        font.pixelSize: root.fontSize
        font.family: Theme.fontFamily
        font.bold: root.highlighted
    }

    MouseArea {
        id: mouse
        anchors.fill: parent
        hoverEnabled: true
        cursorShape: Qt.PointingHandCursor
        onClicked: root.clicked()
    }
}
