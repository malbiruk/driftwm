pragma Singleton
import QtQuick
import Quickshell
import Quickshell.Io

// i3bar-style text status: no icons, and no hardcoded font (the system sans
// is fine).
Singleton {
    id: theme

    // The GTK interface font, so the panel matches the rest of the desktop;
    // empty (Qt's default) when gsettings is missing.
    property string fontFamily: ""

    Process {
        command: ["gsettings", "get", "org.gnome.desktop.interface", "font-name"]
        running: true
        stdout: StdioCollector {
            onStreamFinished: {
                const match = text.trim().match(/^'(.+?)(?: \d+(?:\.\d+)?)?'$/)
                if (match) theme.fontFamily = match[1]
            }
        }
    }

    readonly property color bg: "#0a0a0a"
    readonly property color border: "#3a3a3a"
    readonly property color fg: "#ffffff"
    readonly property color track: "#2a2a2a"
    readonly property real dim: 0.5
    readonly property real hoverOpacity: 0.1
    readonly property int fontSize: 14
    readonly property int smallFontSize: 12
    readonly property int radius: 14
    readonly property int popupRadius: 10
    readonly property int labelGap: 6
    readonly property int sectionGap: 20
}
