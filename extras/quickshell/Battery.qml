import QtQuick
import QtQuick.Layouts
import Quickshell.Services.UPower

Tile {
    id: root

    readonly property var device: UPower.displayDevice
    readonly property bool present: device !== null && device.isPresent

    label: "bat"
    value: present ? `${Math.round(device.percentage * 100)}%` : "—"
    clickable: present
    onClicked: popover.toggle()

    function span(seconds) {
        const minutes = Math.round(seconds / 60)
        const h = Math.floor(minutes / 60)
        const m = minutes % 60
        return h > 0 ? `${h}h ${m}m` : `${m}m`
    }

    function status() {
        if (!present) return ""
        switch (device.state) {
        case UPowerDeviceState.Charging:
            return device.timeToFull > 0 ? `charging, ${span(device.timeToFull)} to full` : "charging"
        case UPowerDeviceState.Discharging:
            return device.timeToEmpty > 0 ? `discharging, ${span(device.timeToEmpty)} left` : "discharging"
        case UPowerDeviceState.FullyCharged:
            return "fully charged"
        case UPowerDeviceState.PendingCharge:
            return "plugged in, not charging"
        default:
            return UPowerDeviceState.toString(device.state).toLowerCase()
        }
    }

    Popover {
        id: popover
        target: root

        Text {
            text: root.status()
            color: Theme.fg
            font.pixelSize: Theme.fontSize
            font.family: Theme.fontFamily
        }

        Text {
            visible: root.present && root.device.healthSupported
            text: root.present ? `health ${Math.round(root.device.healthPercentage)}%` : ""
            color: Theme.fg
            opacity: Theme.dim
            font.pixelSize: Theme.smallFontSize
            font.family: Theme.fontFamily
        }

        // Power profiles, through power-profiles-daemon or tuned-ppd.
        RowLayout {
            Layout.topMargin: 4
            spacing: 2

            Repeater {
                model: [
                    { label: "saver", profile: PowerProfile.PowerSaver, available: true },
                    { label: "balanced", profile: PowerProfile.Balanced, available: true },
                    { label: "performance", profile: PowerProfile.Performance, available: PowerProfiles.hasPerformanceProfile },
                ]

                FlatButton {
                    required property var modelData
                    visible: modelData.available
                    text: modelData.label
                    highlighted: PowerProfiles.profile === modelData.profile
                    padY: 3
                    onClicked: PowerProfiles.profile = modelData.profile
                }
            }
        }
    }
}
