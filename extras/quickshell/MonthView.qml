import QtQuick
import QtQuick.Layouts

ColumnLayout {
    id: root

    property date today: new Date()
    property date shown: today
    readonly property int year: shown.getFullYear()
    readonly property int month: shown.getMonth()
    // Qt counts Monday=1..Sunday=7, JS Sunday=0..Saturday=6.
    readonly property int firstWeekday: Qt.locale().firstDayOfWeek % 7

    spacing: 6

    function cellDate(index) {
        const offset = (new Date(year, month, 1).getDay() - firstWeekday + 7) % 7
        return new Date(year, month, 1 - offset + index)
    }

    function sameDay(a, b) {
        return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate()
    }

    RowLayout {
        Layout.fillWidth: true

        FlatButton {
            text: "‹"
            padX: 8
            padY: 2
            onClicked: root.shown = new Date(root.year, root.month - 1, 1)
        }

        Text {
            Layout.fillWidth: true
            horizontalAlignment: Text.AlignHCenter
            text: Qt.formatDate(root.shown, "MMMM yyyy")
            color: Theme.fg
            font.pixelSize: Theme.fontSize
            font.family: Theme.fontFamily
            font.bold: true
        }

        FlatButton {
            text: "›"
            padX: 8
            padY: 2
            onClicked: root.shown = new Date(root.year, root.month + 1, 1)
        }
    }

    GridLayout {
        columns: 7
        columnSpacing: 2
        rowSpacing: 2

        Repeater {
            model: 7

            Text {
                required property int index
                Layout.preferredWidth: 30
                horizontalAlignment: Text.AlignHCenter
                text: Qt.locale().dayName(((root.firstWeekday + index + 6) % 7) + 1, Locale.ShortFormat).toLowerCase()
                color: Theme.fg
                opacity: Theme.dim
                font.pixelSize: Theme.smallFontSize
                font.family: Theme.fontFamily
            }
        }

        Repeater {
            model: 42

            Rectangle {
                id: cell
                required property int index
                readonly property date day: root.cellDate(index)
                readonly property bool isToday: root.sameDay(day, root.today)

                Layout.preferredWidth: 30
                Layout.preferredHeight: 26
                radius: 13
                color: isToday ? Theme.fg : "transparent"

                Text {
                    anchors.centerIn: parent
                    text: cell.day.getDate()
                    color: cell.isToday ? Theme.bg : Theme.fg
                    opacity: cell.isToday || cell.day.getMonth() === root.month ? 1 : 0.3
                    font.pixelSize: 13
                    font.family: Theme.fontFamily
                }
            }
        }
    }
}
