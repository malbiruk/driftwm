import QtQuick
import QtQuick.Layouts
import QtQuick.Controls
import Quickshell.Io

// nmcli rather than Quickshell.Networking: in the tested Quickshell the module
// lists networks only while scanning and never flags the connected or saved
// ones, which the tile and the connect flow need. `nmcli monitor` triggers
// refreshes on changes; scans run only while the popover is open.
Tile {
    id: root

    property bool available: false
    property bool hasWifi: false
    property string device: ""
    property bool radio: false
    property bool wired: false
    property string ssid: ""
    property int signal: 0
    property var networks: []
    property string pending: ""
    property string error: ""
    property bool refreshQueued: false
    property bool rescanQueued: false

    label: "net"
    value: ssid ? `${signal}%`
         : wired ? "wired"
         : !hasWifi ? "—"
         : radio ? "on" : "off"
    clickable: hasWifi
    onClicked: popover.toggle()
    onAvailableChanged: if (available) monitor.running = true

    function refresh(rescan) {
        if (refresher.running) {
            refreshQueued = true
            rescanQueued = rescanQueued || rescan
            return
        }
        refresher.command = ["sh", "-c", `nmcli radio wifi; echo @@; nmcli -t -e no -f TYPE,STATE,DEVICE dev; echo @@; nmcli -t -e no -f TYPE,NAME con show; echo @@; nmcli -t -e no -f ACTIVE,SIGNAL,SECURITY,SSID dev wifi list --rescan ${rescan ? "yes" : "no"}`]
        refresher.running = true
    }

    function parse(text) {
        // Marker lines rather than a joined separator, so an empty section
        // (no devices, no profiles) still yields four parts.
        const sections = text.split(/^@@$/m)
        if (sections.length < 4) return
        const [radioText, devices, connections, list] = sections
        radio = radioText.trim() === "enabled"
        hasWifi = false
        wired = false
        for (const line of devices.trim().split("\n")) {
            const [type, state, name] = line.split(":")
            if (type === "wifi") {
                hasWifi = true
                device = name || ""
            } else if (type === "ethernet" && state && state.startsWith("connected")) {
                wired = true
            }
        }
        // Saved networks by profile name, which nmcli sets to the SSID unless
        // the user renamed the profile.
        const known = new Set()
        for (const line of connections.trim().split("\n")) {
            const type = line.substring(0, line.indexOf(":"))
            if (type === "802-11-wireless") known.add(line.substring(type.length + 1))
        }
        // One row per SSID: the active band, else the strongest.
        const bySsid = new Map()
        for (const line of list.trim().split("\n")) {
            const parts = line.split(":")
            if (parts.length < 4) continue
            const entry = {
                active: parts[0] === "yes",
                signal: Number(parts[1]),
                security: parts[2],
                ssid: parts.slice(3).join(":"),
            }
            if (!entry.ssid) continue
            entry.known = known.has(entry.ssid)
            const seen = bySsid.get(entry.ssid)
            if (!seen || entry.active || (!seen.active && entry.signal > seen.signal)) bySsid.set(entry.ssid, entry)
        }
        const next = [...bySsid.values()].sort((a, b) => (b.active - a.active) || (b.known - a.known) || (b.signal - a.signal))
        // A new array rebuilds the list's rows, so only swap it in on a change.
        if (JSON.stringify(next) !== JSON.stringify(networks)) networks = next
        const active = next.find(network => network.active)
        ssid = active ? active.ssid : ""
        signal = active ? active.signal : 0
    }

    function run(target, command) {
        if (runner.running) return
        error = ""
        pending = ""
        runner.target = target
        runner.command = command
        runner.running = true
    }

    function tap(network) {
        if (network.active) {
            // `connection down` leaves the device free to autoconnect elsewhere,
            // where `device disconnect` would block it until a manual connect.
            run("disconnect", ["nmcli", "connection", "down", "id", network.ssid])
        } else if (network.known || network.security === "" || network.security.includes("OWE")) {
            run(`join ${network.ssid}`, ["nmcli", "device", "wifi", "connect", network.ssid])
        } else {
            error = ""
            pending = network.ssid
        }
    }

    Process {
        id: refresher
        stdout: StdioCollector {
            onStreamFinished: root.parse(text)
        }
        onExited: exitCode => {
            root.available = exitCode === 0
            if (!root.available) {
                root.hasWifi = false
                root.ssid = ""
                root.networks = []
            }
            if (root.refreshQueued) {
                const rescan = root.rescanQueued
                root.refreshQueued = false
                root.rescanQueued = false
                root.refresh(rescan)
            }
        }
    }

    Process {
        id: runner
        property string target
        stdout: StdioCollector {}
        stderr: StdioCollector {}
        onExited: exitCode => {
            if (exitCode !== 0) root.error = `couldn't ${target}`
            root.refresh(popover.visible)
        }
    }

    // nmcli prints a line per NetworkManager event; coalesce bursts. The
    // wrapper holds a stdin pipe from the shell and kills the monitor when
    // that pipe closes, so a killed shell can't leave a monitor behind; it
    // also exits when NetworkManager restarts, and comes back once a refresh
    // succeeds.
    Process {
        id: monitor
        // The watcher reads a dup of stdin: a background job's own stdin is /dev/null.
        command: ["sh", "-c", "exec 3<&0; nmcli monitor & mon=$!; { read -r _ <&3; kill $mon 2>/dev/null; } & wait $mon; kill $! 2>/dev/null"]
        stdinEnabled: true
        stdout: SplitParser {
            onRead: debounce.restart()
        }
        onExited: monitorRetry.restart()
    }

    Timer {
        id: monitorRetry
        interval: 5000
        onTriggered: if (root.available && !monitor.running) monitor.running = true
    }

    Timer {
        id: debounce
        interval: 800
        onTriggered: root.refresh(false)
    }

    Timer {
        interval: popover.visible ? 10000 : 30000
        running: true
        repeat: true
        triggeredOnStart: true
        onTriggered: root.refresh(popover.visible)
    }

    Popover {
        id: popover
        target: root
        onVisibleChanged: {
            root.pending = ""
            root.error = ""
            if (visible) root.refresh(true)
        }

        RowLayout {
            Layout.fillWidth: true
            Layout.preferredWidth: 300

            Text {
                Layout.fillWidth: true
                Layout.leftMargin: 8
                text: "wi-fi"
                color: Theme.fg
                font.pixelSize: Theme.fontSize
                font.family: Theme.fontFamily
                font.bold: true
            }

            FlatButton {
                text: root.radio ? "on" : "off"
                onClicked: root.run("switch wi-fi", ["nmcli", "radio", "wifi", root.radio ? "off" : "on"])
            }
        }

        ListView {
            id: list
            Layout.fillWidth: true
            Layout.preferredHeight: Math.min(contentHeight, 240)
            clip: true
            model: root.networks

            delegate: ListRow {
                id: entry
                required property var modelData
                width: list.width
                text: entry.modelData.ssid
                highlighted: entry.modelData.active
                detail: (entry.modelData.active ? "connected · " : entry.modelData.known ? "saved · " : "") + `${entry.modelData.signal}%`
                action: entry.modelData.known ? "forget" : ""
                onClicked: root.tap(entry.modelData)
                onActionClicked: root.run(`forget ${entry.modelData.ssid}`, ["nmcli", "connection", "delete", "id", entry.modelData.ssid])
            }
        }

        // Outside the list so a refresh rebuilding the rows can't clear it.
        RowLayout {
            visible: root.pending !== ""
            Layout.fillWidth: true
            Layout.leftMargin: 8
            Layout.rightMargin: 8
            spacing: 6

            TextField {
                id: password
                Layout.fillWidth: true
                echoMode: TextInput.Password
                placeholderText: `password for ${root.pending}`
                placeholderTextColor: "#808080"
                color: Theme.fg
                font.pixelSize: Theme.fontSize
                font.family: Theme.fontFamily
                background: Rectangle {
                    color: Theme.track
                    radius: 6
                }
                onVisibleChanged: if (visible) {
                    text = ""
                    forceActiveFocus()
                }
                onAccepted: join.clicked()
            }

            FlatButton {
                id: join
                text: "join"
                onClicked: root.run(`join ${root.pending}`, ["nmcli", "device", "wifi", "connect", root.pending, "password", password.text])
            }
        }

        Text {
            visible: root.networks.length === 0
            Layout.leftMargin: 8
            text: root.radio ? "scanning…" : "wi-fi is off"
            color: Theme.fg
            opacity: Theme.dim
            font.pixelSize: Theme.fontSize
            font.family: Theme.fontFamily
        }

        Text {
            visible: root.error !== ""
            Layout.leftMargin: 8
            text: root.error
            color: Theme.fg
            opacity: Theme.dim
            font.pixelSize: Theme.smallFontSize
            font.family: Theme.fontFamily
        }
    }
}
