import app from "ags/gtk4/app"
import { Astal, Gtk } from "ags/gtk4"
import Clock from "./Clock"
import Battery from "./Battery"
import Layout from "./Layout"
import Network from "./Network"
import Bluetooth from "./Bluetooth"
import Volume from "./Volume"
import Brightness from "./Brightness"
import Cpu from "./Cpu"
import Ram from "./Ram"
import Mpris from "./Mpris"
import Tray from "./Tray"
import NotifButton from "./NotifButton"
import PowerMenu from "./PowerMenu"

// i3bar/swaybar-style text status, but a square-ish canvas panel with "hidden
// power": tapping a tile opens a GTK popover (volume slider, wifi/bt connect…),
// so no external apps are needed.
export default function Dashboard() {
  return (
    <window
      visible
      namespace="drift-dashboard"
      class="Dashboard"
      exclusivity={Astal.Exclusivity.IGNORE}
      application={app}
    >
      <box orientation={Gtk.Orientation.VERTICAL} class="dashboard" spacing={10}>
        <Clock />

        <box class="grid" orientation={Gtk.Orientation.VERTICAL} spacing={6}>
          <box class="grid-row" homogeneous spacing={18}>
            <Battery />
            <Layout />
          </box>
          <box class="grid-row" homogeneous spacing={18}>
            <Network />
            <Bluetooth />
          </box>
          <box class="grid-row" homogeneous spacing={18}>
            <Volume />
            <Brightness />
          </box>
          <box class="grid-row" homogeneous spacing={18}>
            <Cpu />
            <Ram />
          </box>
        </box>

        <Mpris />
        <Tray />

        <box class="grid-row footer" homogeneous spacing={18}>
          <NotifButton />
          <PowerMenu />
        </box>
      </box>
    </window>
  )
}
