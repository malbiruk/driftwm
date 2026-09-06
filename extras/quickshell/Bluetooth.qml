import QtQuick
import QtQuick.Layouts
import Quickshell.Bluetooth

// Discovery runs only while the popover is open and the adapter is on — via a
// Binding, since powering the adapter is async and StartDiscovery fails until
// it's on. Tapping an unpaired device pairs then connects: "just works" only;
// a device that wants a PIN needs an agent (blueman).
Tile {
    id: root

    readonly property var adapter: Bluetooth.defaultAdapter
    readonly property var devices: adapter
        ? [...adapter.devices.values]
            .filter(device => device.paired || device.connected || !/^([0-9A-F]{2}-){5}[0-9A-F]{2}$/i.test(device.name))
            .sort((a, b) => (b.connected - a.connected) || (b.paired - a.paired) || a.name.localeCompare(b.name))
        : []
    property var justPaired: null

    label: "blu"
    value: !adapter ? "—" : adapter.enabled ? "on" : "off"
    clickable: adapter !== null
    onClicked: popover.toggle()

    function tap(device) {
        if (device.connected) {
            device.disconnect()
        } else if (device.paired) {
            device.connect()
        } else {
            justPaired = device
            device.pair()
        }
    }

    function detail(device) {
        if (device.pairing) return "pairing…"
        if (device.state === BluetoothDeviceState.Connecting) return "connecting…"
        if (device.connected) return device.batteryAvailable ? `connected · ${Math.round(device.battery * 100)}%` : "connected"
        return device.paired ? "paired" : ""
    }

    // Connect as soon as a tapped device finishes pairing.
    Connections {
        target: root.justPaired
        function onPairedChanged() {
            const device = root.justPaired
            if (!device.paired) return
            root.justPaired = null
            device.trusted = true
            device.connect()
        }
    }

    Binding {
        target: root.adapter
        property: "discovering"
        value: popover.visible && root.adapter !== null && root.adapter.enabled
        when: root.adapter !== null
        restoreMode: Binding.RestoreNone
    }

    Popover {
        id: popover
        target: root

        RowLayout {
            Layout.fillWidth: true
            Layout.preferredWidth: 260

            Text {
                Layout.fillWidth: true
                Layout.leftMargin: 8
                text: "bluetooth"
                color: Theme.fg
                font.pixelSize: Theme.fontSize
                font.family: Theme.fontFamily
                font.bold: true
            }

            FlatButton {
                text: root.adapter && root.adapter.enabled ? "on" : "off"
                onClicked: root.adapter.enabled = !root.adapter.enabled
            }
        }

        ListView {
            id: list
            Layout.fillWidth: true
            Layout.preferredHeight: Math.min(contentHeight, 240)
            clip: true
            model: root.devices

            delegate: ListRow {
                id: entry
                required property var modelData
                width: list.width
                text: entry.modelData.name
                highlighted: entry.modelData.connected
                detail: root.detail(entry.modelData)
                action: entry.modelData.paired ? "forget" : ""
                onClicked: root.tap(entry.modelData)
                onActionClicked: entry.modelData.forget()
            }
        }

        Text {
            visible: root.devices.length === 0
            Layout.leftMargin: 8
            text: root.adapter && root.adapter.enabled ? "scanning…" : "bluetooth is off"
            color: Theme.fg
            opacity: Theme.dim
            font.pixelSize: Theme.fontSize
            font.family: Theme.fontFamily
        }
    }
}
