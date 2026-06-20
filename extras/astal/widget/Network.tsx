import Network from "gi://AstalNetwork"
import { createBinding, createComputed } from "ags"
import Stat from "./Stat"

// Shows the wifi SSID (or wired/off). Toggle + saved-network list go in a popover.
export default function NetworkWidget() {
  const net = Network.get_default()
  const primary = createBinding(net, "primary")
  const ssid = net.wifi ? createBinding(net.wifi, "ssid") : null

  const value = createComputed(() => {
    switch (primary()) {
      case Network.Primary.WIFI:
        return ssid && ssid() ? ssid()! : "on"
      case Network.Primary.WIRED:
        return "wired"
      default:
        return "off"
    }
  })

  return <Stat label="net" value={value} />
}
