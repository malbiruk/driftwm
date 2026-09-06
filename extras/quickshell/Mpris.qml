import QtQuick
import QtQuick.Layouts
import Quickshell.Services.Mpris

// One line: title over artist on the left, transport controls on the right.
// Follows the playing player. playerctld mirrors whichever player is active,
// so it is skipped rather than shown as a second one. Unicode glyphs keep the
// text-only look.
Item {
    id: root

    readonly property var players: Mpris.players.values.filter(candidate => candidate.dbusName !== "org.mpris.MediaPlayer2.playerctld")
    readonly property var player: players.find(candidate => candidate.isPlaying) || players[0] || null

    visible: player !== null
    implicitWidth: card.implicitWidth
    implicitHeight: card.implicitHeight

    RowLayout {
        id: card
        anchors.left: parent.left
        anchors.right: parent.right
        spacing: 0

        ColumnLayout {
            Layout.fillWidth: true
            Layout.rightMargin: 6
            spacing: 0

            Text {
                Layout.fillWidth: true
                text: root.player ? (root.player.trackTitle || root.player.identity) : ""
                color: Theme.fg
                font.bold: true
                font.pixelSize: Theme.fontSize
                font.family: Theme.fontFamily
                elide: Text.ElideRight
            }

            Text {
                Layout.fillWidth: true
                visible: text !== ""
                text: root.player ? root.player.trackArtist : ""
                color: Theme.fg
                opacity: 0.55
                font.pixelSize: Theme.smallFontSize
                font.family: Theme.fontFamily
                elide: Text.ElideRight
            }
        }

        FlatButton {
            text: "⏮"
            fontSize: 16
            padX: 5
            padY: 2
            onClicked: root.player.previous()
        }

        FlatButton {
            text: root.player && root.player.isPlaying ? "⏸" : "⏵"
            fontSize: 16
            padX: 5
            padY: 2
            onClicked: root.player.togglePlaying()
        }

        FlatButton {
            text: "⏭"
            fontSize: 16
            padX: 5
            padY: 2
            onClicked: root.player.next()
        }
    }
}
