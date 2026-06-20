import { Gtk } from "ags/gtk4"
import { execAsync } from "ags/process"

// Session actions behind a popover (not one-click). Logout exits the compositor
// via its IPC; the rest are systemctl / swaylock.
const ACTIONS = [
  { label: "lock", cmd: ["swaylock", "-f"] },
  { label: "log out", cmd: ["driftwm", "msg", "action", "quit"] },
  { label: "suspend", cmd: ["systemctl", "suspend"] },
  { label: "reboot", cmd: ["systemctl", "reboot"] },
  { label: "power off", cmd: ["systemctl", "poweroff"] },
]

export default function PowerMenu() {
  return (
    <menubutton class="footer-btn">
      <label label="power" />
      <popover>
        <box orientation={Gtk.Orientation.VERTICAL} spacing={2} class="menu-list">
          {ACTIONS.map((a) => (
            <button class="menu-item" onClicked={() => execAsync(a.cmd)}>
              <label label={a.label} halign={Gtk.Align.START} hexpand />
            </button>
          ))}
        </box>
      </popover>
    </menubutton>
  )
}
