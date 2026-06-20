import Mpris from "gi://AstalMpris"
import Pango from "gi://Pango"
import { createBinding, For } from "ags"
import { Gtk } from "ags/gtk4"

// Controls use unicode media glyphs (font-rendered) so there's no icon-theme
// reliance, matching the text-only look.
function PlayerCard(player: Mpris.Player) {
  const playGlyph = createBinding(player, "playbackStatus").as((s) =>
    s === Mpris.PlaybackStatus.PLAYING ? "⏸" : "⏵",
  )
  return (
    <box orientation={Gtk.Orientation.VERTICAL} class="player" spacing={2}>
      <label
        class="title"
        label={createBinding(player, "title")}
        maxWidthChars={22}
        ellipsize={Pango.EllipsizeMode.END}
      />
      <label
        class="artist"
        label={createBinding(player, "artist")}
        maxWidthChars={22}
        ellipsize={Pango.EllipsizeMode.END}
      />
      <box class="controls" spacing={12} halign={Gtk.Align.CENTER}>
        <button onClicked={() => player.previous()}>
          <label label="⏮" />
        </button>
        <button onClicked={() => player.play_pause()}>
          <label label={playGlyph} />
        </button>
        <button onClicked={() => player.next()}>
          <label label="⏭" />
        </button>
      </box>
    </box>
  )
}

export default function MprisWidget() {
  const mpris = Mpris.get_default()
  const players = createBinding(mpris, "players")
  return (
    <box
      class="mpris"
      orientation={Gtk.Orientation.VERTICAL}
      spacing={8}
      visible={players.as((p) => p.length > 0)}
    >
      <For each={players}>{(player) => PlayerCard(player)}</For>
    </box>
  )
}
