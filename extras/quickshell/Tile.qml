import QtQuick

// The label sits in a fixed-width column so labels and values line up across
// tiles.
Item {
    id: root

    property string label
    property string value
    property bool clickable: false
    // The label column's width, shared across a column of tiles so values line up.
    property real labelColumn: labelWidth
    readonly property real labelWidth: labelText.implicitWidth
    signal clicked()

    implicitHeight: 24
    implicitWidth: labelColumn + Theme.labelGap + valueText.implicitWidth

    Rectangle {
        anchors.fill: parent
        anchors.leftMargin: -6
        anchors.rightMargin: -6
        radius: 6
        color: Theme.fg
        opacity: root.clickable && mouse.containsMouse ? Theme.hoverOpacity : 0
    }

    Text {
        id: labelText
        width: root.labelColumn
        anchors.verticalCenter: parent.verticalCenter
        text: root.label
        color: Theme.fg
        opacity: Theme.dim
        font.pixelSize: Theme.fontSize
        font.family: Theme.fontFamily
    }

    Text {
        id: valueText
        anchors.left: labelText.right
        anchors.leftMargin: Theme.labelGap
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        text: root.value
        color: Theme.fg
        elide: Text.ElideRight
        font.pixelSize: Theme.fontSize
        font.family: Theme.fontFamily
    }

    MouseArea {
        id: mouse
        anchors.fill: parent
        anchors.leftMargin: -6
        anchors.rightMargin: -6
        hoverEnabled: true
        enabled: root.clickable
        cursorShape: root.clickable ? Qt.PointingHandCursor : Qt.ArrowCursor
        onClicked: root.clicked()
    }
}
