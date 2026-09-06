import QtQuick
import QtQuick.Layouts
import Quickshell

Item {
    id: root

    implicitWidth: column.implicitWidth
    implicitHeight: column.implicitHeight

    Rectangle {
        anchors.fill: parent
        anchors.leftMargin: -10
        anchors.rightMargin: -10
        anchors.topMargin: -4
        anchors.bottomMargin: -4
        radius: 8
        color: Theme.fg
        opacity: mouse.containsMouse ? Theme.hoverOpacity : 0
    }

    SystemClock {
        id: clock
        precision: SystemClock.Minutes
    }

    ColumnLayout {
        id: column
        anchors.horizontalCenter: parent.horizontalCenter
        spacing: 0

        Text {
            Layout.alignment: Qt.AlignHCenter
            text: Qt.formatDateTime(clock.date, "HH:mm")
            color: Theme.fg
            font.pixelSize: 44
            font.family: Theme.fontFamily
            font.bold: true
        }

        Text {
            Layout.alignment: Qt.AlignHCenter
            text: Qt.formatDateTime(clock.date, "dddd, MMMM d")
            color: Theme.fg
            opacity: Theme.dim
            font.pixelSize: 13
            font.family: Theme.fontFamily
        }
    }

    MouseArea {
        id: mouse
        anchors.fill: parent
        hoverEnabled: true
        cursorShape: Qt.PointingHandCursor
        onClicked: calendar.toggle()
    }

    Popover {
        id: calendar
        target: root
        onVisibleChanged: if (visible) month.shown = clock.date

        MonthView {
            id: month
            today: clock.date
        }
    }
}
