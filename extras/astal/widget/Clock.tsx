import GLib from "gi://GLib"
import { Gtk } from "ags/gtk4"
import { createPoll } from "ags/time"

// GLib formatting avoids spawning `date` every tick.
export default function Clock() {
  const time = createPoll("", 1000, () => GLib.DateTime.new_now_local().format("%H:%M") ?? "")
  const date = createPoll("", 60000, () => GLib.DateTime.new_now_local().format("%A, %B %-d") ?? "")
  return (
    <box orientation={Gtk.Orientation.VERTICAL} class="clock">
      <label class="time" label={time} />
      <label class="date" label={date} />
    </box>
  )
}
