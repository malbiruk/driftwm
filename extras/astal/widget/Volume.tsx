import Wp from "gi://AstalWp"
import { createBinding, createComputed, With } from "ags"
import Stat from "./Stat"

// Speakers populate asynchronously, so pick the active one reactively: the
// default when it's a real sink, else the first available. A system with no
// default node set would otherwise report a misleading 0%.
export default function Volume() {
  const audio = Wp.get_default()!.audio
  const speakers = createBinding(audio, "speakers")
  const defaultSpeaker = createBinding(audio, "defaultSpeaker")
  const active = createComputed(() => {
    const list = speakers() ?? []
    const def = defaultSpeaker()
    return def && list.some((s) => s.id === def.id) ? def : (list[0] ?? null)
  })

  return (
    <With value={active}>
      {(sp) =>
        sp ? (
          <Stat
            label="vol"
            value={createBinding(sp, "volume").as((v) => `${Math.round(v * 100)}%`)}
          />
        ) : (
          <Stat label="vol" value="—" />
        )
      }
    </With>
  )
}
