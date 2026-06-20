import { execAsync } from "ags/process"

// Toggles the swaync notification panel (notifications themselves stay swaync's
// job; this is just the entry point).
export default function NotifButton() {
  return (
    <button class="footer-btn" onClicked={() => execAsync(["swaync-client", "-t", "-sw"])}>
      <label label="notifications" />
    </button>
  )
}
