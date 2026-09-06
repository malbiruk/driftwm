import QtQuick

Item {
    id: root

    property string text
    property string detail
    property bool highlighted: false
    // An optional small button at the right edge, e.g. "forget".
    property string action: ""
    signal clicked()
    signal actionClicked()

    implicitHeight: 30

    Rectangle {
        anchors.fill: parent
        radius: 6
        color: Theme.fg
        opacity: mouse.containsMouse ? Theme.hoverOpacity : 0
    }

    Text {
        anchors.left: parent.left
        anchors.leftMargin: 8
        anchors.right: detailText.left
        anchors.rightMargin: 8
        anchors.verticalCenter: parent.verticalCenter
        text: root.text
        color: Theme.fg
        elide: Text.ElideRight
        font.pixelSize: Theme.fontSize
        font.family: Theme.fontFamily
        font.bold: root.highlighted
    }

    Text {
        id: detailText
        anchors.right: actionButton.visible ? actionButton.left : parent.right
        anchors.rightMargin: 8
        anchors.verticalCenter: parent.verticalCenter
        text: root.detail
        color: Theme.fg
        opacity: Theme.dim
        font.pixelSize: Theme.smallFontSize
        font.family: Theme.fontFamily
    }

    MouseArea {
        id: mouse
        anchors.fill: parent
        hoverEnabled: true
        cursorShape: Qt.PointingHandCursor
        onClicked: root.clicked()
    }

    FlatButton {
        id: actionButton
        visible: root.action !== ""
        anchors.right: parent.right
        anchors.rightMargin: 2
        anchors.verticalCenter: parent.verticalCenter
        text: root.action
        fontSize: Theme.smallFontSize
        textOpacity: Theme.dim
        padX: 6
        padY: 2
        onClicked: root.actionClicked()
    }
}
