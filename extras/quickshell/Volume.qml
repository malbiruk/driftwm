import QtQuick
import QtQuick.Layouts
import Quickshell.Services.Pipewire

// The default sink and source, since that's what the volume keys and apps use.
Tile {
    id: root

    readonly property var sink: Pipewire.defaultAudioSink
    readonly property var source: Pipewire.defaultAudioSource
    readonly property bool present: sink !== null && sink.audio !== null
    readonly property bool micPresent: source !== null && source.audio !== null
    readonly property var sinks: Pipewire.nodes.values.filter(node => node.isSink && !node.isStream && (node.type & PwNodeType.Audio))
    readonly property var sources: Pipewire.nodes.values.filter(node => !node.isSink && !node.isStream && (node.type & PwNodeType.Audio))

    label: "vol"
    // "mut" keeps the column as narrow as a percentage.
    value: !present ? "—" : sink.audio.muted ? "mut" : `${Math.round(sink.audio.volume * 100)}%`
    clickable: present
    onClicked: popover.toggle()

    function nodeName(node) {
        return node.description || node.nickname || node.name
    }

    // Nodes must be tracked before their audio properties exist.
    PwObjectTracker {
        objects: [root.sink, root.source].filter(node => node !== null)
    }

    Popover {
        id: popover
        target: root

        RowLayout {
            spacing: 6

            ThinSlider {
                id: slider
                Layout.preferredWidth: 170
                from: 0
                to: 1
                onMoved: if (root.present) root.sink.audio.volume = value
            }

            Binding {
                target: slider
                property: "value"
                value: root.present ? root.sink.audio.volume : 0
                when: !slider.pressed
                restoreMode: Binding.RestoreNone
            }

            FlatButton {
                text: root.present && root.sink.audio.muted ? "unmute" : "mute"
                onClicked: if (root.present) root.sink.audio.muted = !root.sink.audio.muted
            }
        }

        ColumnLayout {
            visible: root.sinks.length > 1
            Layout.fillWidth: true
            spacing: 2

            Text {
                Layout.leftMargin: 8
                Layout.topMargin: 4
                text: "output"
                color: Theme.fg
                opacity: Theme.dim
                font.pixelSize: Theme.smallFontSize
                font.family: Theme.fontFamily
            }

            Repeater {
                model: root.sinks

                ListRow {
                    required property var modelData
                    Layout.fillWidth: true
                    text: root.nodeName(modelData)
                    highlighted: modelData === root.sink
                    detail: modelData === root.sink ? "active" : ""
                    onClicked: Pipewire.preferredDefaultAudioSink = modelData
                }
            }
        }

        ColumnLayout {
            visible: root.micPresent
            Layout.fillWidth: true
            spacing: 2

            Text {
                Layout.leftMargin: 8
                Layout.topMargin: 4
                text: "input"
                color: Theme.fg
                opacity: Theme.dim
                font.pixelSize: Theme.smallFontSize
                font.family: Theme.fontFamily
            }

            RowLayout {
                spacing: 6

                ThinSlider {
                    id: micSlider
                    Layout.preferredWidth: 170
                    from: 0
                    to: 1
                    onMoved: if (root.micPresent) root.source.audio.volume = value
                }

                Binding {
                    target: micSlider
                    property: "value"
                    value: root.micPresent ? root.source.audio.volume : 0
                    when: !micSlider.pressed
                    restoreMode: Binding.RestoreNone
                }

                FlatButton {
                    text: root.micPresent && root.source.audio.muted ? "unmute" : "mute"
                    onClicked: if (root.micPresent) root.source.audio.muted = !root.source.audio.muted
                }
            }

            Repeater {
                model: root.sources.length > 1 ? root.sources : []

                ListRow {
                    required property var modelData
                    Layout.fillWidth: true
                    text: root.nodeName(modelData)
                    highlighted: modelData === root.source
                    detail: modelData === root.source ? "active" : ""
                    onClicked: Pipewire.preferredDefaultAudioSource = modelData
                }
            }
        }
    }
}
