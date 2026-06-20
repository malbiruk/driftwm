import Bluetooth from "gi://AstalBluetooth"
import { createBinding } from "ags"
import Stat from "./Stat"

export default function BluetoothWidget() {
  const bt = Bluetooth.get_default()
  return <Stat label="bt" value={createBinding(bt, "isPowered").as((p) => (p ? "on" : "off"))} />
}
