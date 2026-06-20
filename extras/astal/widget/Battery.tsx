import Battery from "gi://AstalBattery"
import { createBinding } from "ags"
import Stat from "./Stat"

export default function BatteryWidget() {
  const bat = Battery.get_default()
  return (
    <Stat
      label="bat"
      value={createBinding(bat, "percentage").as((p) => `${Math.round(p * 100)}%`)}
      visible={createBinding(bat, "isPresent")}
    />
  )
}
