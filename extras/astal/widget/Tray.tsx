import Tray from "gi://AstalTray"
import { createBinding, For } from "ags"

// GTK4 tray: a menubutton per item, its DBusMenu exposed as a menu-model plus an
// action group inserted under the "dbusmenu" prefix.
function TrayItem(item: Tray.TrayItem) {
  return (
    <menubutton
      class="tray-item"
      menuModel={createBinding(item, "menuModel")}
      $={(self) => {
        if (item.actionGroup) self.insert_action_group("dbusmenu", item.actionGroup)
      }}
    >
      <image gicon={createBinding(item, "gicon")} />
    </menubutton>
  )
}

export default function TrayWidget() {
  const tray = Tray.get_default()
  const items = createBinding(tray, "items")
  return (
    <box class="tray" spacing={4} visible={items.as((i) => i.length > 0)}>
      <For each={items}>{(item) => TrayItem(item)}</For>
    </box>
  )
}
