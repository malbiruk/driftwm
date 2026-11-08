# Scratchpad

A scratchpad is a window you summon and dismiss with one key: a dropdown
terminal, a mail client, a music player. Pressing the key **spawns** the app if
it isn't running, **stows** its window when it is showing, and **shows** it
again when it is stowed. Stowing is instant and animation-free, the window
stays mapped and its app keeps running (a terminal keeps its shell and
scrollback), and showing it puts it back exactly where, and at the size, you
left it.

driftwm has no minimize, so this is the way to get a Quake/Yakuake-style
dropdown without an external script.

## Set one up

Three steps: bind a key, give the window a place, reload.

**1. Find the app's `app_id`.** Start the app and run `driftwm msg state`; each
window is listed with its `app_id`. The binding matches it **exactly**
(`kitty` and `Kitty` are different). Many terminals let you choose it, which
keeps a dropdown terminal apart from your ordinary ones:

| Terminal  | Flag                     |
| --------- | ------------------------ |
| kitty     | `--class dropdown-term`  |
| foot      | `--app-id dropdown-term` |
| alacritty | `--class dropdown-term`  |

**2. Bind a key.** `toggle-scratchpad <app_id> <command...>` takes the
`app_id` to look for and the command that spawns it when no such window exists:

```toml
[keybindings]
"mod+w" = "toggle-scratchpad dropdown-term kitty --class dropdown-term"
```

Any binding type works: a key, a tap-modifier chord, a mouse button or a
gesture.

**3. Give it a place.** Where the window appears is decided by an ordinary
[window rule](window-rules.md). Pin it to the screen so it floats above
everything and always comes back on screen, wherever the canvas is panned:

```toml
[[window_rules]]
app_id           = "dropdown-term"
pinned_to_screen = true
position         = [0, 185]    # output-relative, Y-up from the output centre
size             = [1200, 450]
```

See [screen-pinned windows](window-rules.md#screen-pinned-windows) for how
`position` works. To find the numbers, run the app, pin it with
`toggle-pin-to-screen` (`Mod+T`), drag and resize it into place, and copy
`position`/`size` from the `pinned` section of `driftwm msg state`.

Reload with `driftwm msg action reload-config` (rules apply to windows opened
afterwards, so close any copy that is already running), then press the key.

## How it behaves

- **It remembers.** The rule only sets the *first* placement. If you move or
  resize the window while it is up, the next show restores that geometry.
- **Focus.** Showing raises and focuses the window. Stowing the focused window
  hands focus to the most recently used other window.
- **Hidden means gone.** A stowed window is not drawn, takes no clicks, and is
  left out of Alt-Tab, the overview and focus-nearest. It still gets frame
  callbacks at a throttled rate, so a terminal keeps processing output.
- **Everything of the app toggles together.** All windows with that `app_id`
  (a mail client's compose windows, say) and their dialogs are stowed and shown
  as one.
- **Fullscreen.** Stowing a fullscreen window leaves fullscreen so you see the
  desktop, and showing it puts it back into fullscreen. A fullscreen or maximize
  the app asks for while stowed is applied when it is shown.
- **Slow starters.** A repeat press within 3 seconds of a spawn does not launch
  the app twice, and a window is never stowed before its first placement, so
  its rules still apply.
- **No placeholders.** A scratchpad app is not [session-restored](session.md):
  no stand-in comes back after a restart, since the key respawns the app. A
  `restore_windows = true` rule for the app overrides this.
- **Reload.** If a reload leaves a stowed window with no binding that can reach
  it, it is shown again instead of being stranded.

You can drive one without a binding too:
`driftwm msg action toggle-scratchpad dropdown-term kitty --class dropdown-term`.

## Limitations

- **Pin it.** Scratchpads are meant for `pinned_to_screen` windows. An unpinned
  one works, but showing it does not pan the camera to it, so it may be off
  screen.
- **Monitor changes.** Unplugging the monitor a pinned window lives on moves it
  to a remaining monitor, clamped to fit, and plugging the monitor back in does
  not restore the original spot. This is how pinned windows behave in general.
- **IPC wording.** `msg focus`, `move`, `pin` and `suspend` refuse a stowed
  window, with a "mid-relaunch" error that does not describe the situation.
