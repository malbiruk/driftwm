import { createPoll } from "ags/time"
import { readFile } from "ags/file"
import Stat from "./Stat"

// Usage = busy delta / total delta between samples, from /proc/stat.
function cpuPoll() {
  let prevIdle = 0
  let prevTotal = 0
  return () => {
    const parts = readFile("/proc/stat")
      .split("\n")[0]
      .trim()
      .split(/\s+/)
      .slice(1)
      .map(Number)
    const idle = parts[3] + (parts[4] || 0)
    const total = parts.reduce((a, b) => a + b, 0)
    const dIdle = idle - prevIdle
    const dTotal = total - prevTotal
    prevIdle = idle
    prevTotal = total
    const usage = dTotal > 0 ? 1 - dIdle / dTotal : 0
    return `${Math.round(usage * 100)}%`
  }
}

export default function Cpu() {
  return <Stat label="cpu" value={createPoll("", 2000, cpuPoll())} />
}
