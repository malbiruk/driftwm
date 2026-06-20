import GLib from "gi://GLib"
import { createPoll } from "ags/time"
import { readFile } from "ags/file"
import Stat from "./Stat"

// First backlight under /sys (hidden when there's none). brightnessctl is the
// rice's setter for the keys; reading /sys avoids spawning a process per tick.
function findBacklight(): string | null {
  try {
    const dir = GLib.Dir.open("/sys/class/backlight", 0)
    const name = dir.read_name()
    return name ? `/sys/class/backlight/${name}` : null
  } catch {
    return null
  }
}

const BL = findBacklight()

function brightnessPct(): string {
  if (!BL) return ""
  try {
    const cur = Number(readFile(`${BL}/brightness`).trim())
    const max = Number(readFile(`${BL}/max_brightness`).trim())
    return max > 0 ? `${Math.round((cur / max) * 100)}%` : ""
  } catch {
    return ""
  }
}

export default function Brightness() {
  if (!BL) return <box visible={false} />
  return <Stat label="bri" value={createPoll("", 3000, brightnessPct)} />
}
