import GLib from "gi://GLib"
import { createState, onCleanup } from "ags"
import { readFile, monitorFile } from "ags/file"
import Stat from "./Stat"

// driftwm writes the active keyboard layout's short code (e.g. `us`) to its state
// file; we read + watch it rather than polling `driftwm msg layout`.
const STATE = `${GLib.getenv("XDG_RUNTIME_DIR")}/driftwm/state`

function readLayout(): string {
  try {
    const m = readFile(STATE).match(/^layout_short=(.*)$/m)
    return m ? m[1].trim().toUpperCase() : ""
  } catch {
    return ""
  }
}

export default function Layout() {
  const [layout, setLayout] = createState(readLayout())
  const monitor = monitorFile(STATE, () => setLayout(readLayout()))
  onCleanup(() => monitor.cancel())
  return <Stat label="kbd" value={layout} />
}
