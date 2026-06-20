import { createPoll } from "ags/time"
import { readFile } from "ags/file"
import Stat from "./Stat"

function ramUsage() {
  const txt = readFile("/proc/meminfo")
  const total = Number(txt.match(/MemTotal:\s+(\d+)/)?.[1] ?? 0)
  const avail = Number(txt.match(/MemAvailable:\s+(\d+)/)?.[1] ?? 0)
  const used = total > 0 ? 1 - avail / total : 0
  return `${Math.round(used * 100)}%`
}

export default function Ram() {
  return <Stat label="ram" value={createPoll("", 2000, ramUsage)} />
}
