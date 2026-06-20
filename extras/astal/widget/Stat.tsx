import { Gtk } from "ags/gtk4"

// One consistent text tile: dim label + white value, no icons.
export default function Stat(props: { label: string; value: unknown; visible?: unknown }) {
  return (
    <box class="stat" spacing={8} visible={(props.visible ?? true) as never}>
      <label class="stat-label" label={props.label} />
      <label
        class="stat-value"
        label={props.value as never}
        hexpand
        halign={Gtk.Align.START}
      />
    </box>
  )
}
