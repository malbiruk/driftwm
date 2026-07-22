use smithay::input::keyboard::{keysyms, xkb};

use super::types::*;

fn parse_modifiers(parts: &[&str], mod_key: ModKey) -> Result<Modifiers, String> {
    let mut mods = Modifiers::EMPTY;
    for part in parts {
        match part.to_lowercase().as_str() {
            "mod" => match mod_key {
                ModKey::Alt => mods.alt = true,
                ModKey::Super => mods.logo = true,
            },
            "alt" => mods.alt = true,
            "super" | "logo" => mods.logo = true,
            "ctrl" | "control" => mods.ctrl = true,
            "shift" => mods.shift = true,
            other => return Err(format!("unknown modifier: {other}")),
        }
    }
    Ok(mods)
}

/// True if every `+`-separated token names a modifier — a combo with no keysym,
/// so it's a tap-modifier binding rather than a `parse_key_combo`. Modifier names
/// are never valid keysym names, so this never shadows a real key binding.
fn is_modifier_only(s: &str) -> bool {
    let mut parts = s.split('+').map(str::trim).peekable();
    parts.peek().is_some()
        && parts.all(|p| {
            matches!(
                p.to_lowercase().as_str(),
                "mod" | "alt" | "super" | "logo" | "ctrl" | "control" | "shift"
            )
        })
}

/// Parse a modifier-only combo like "alt+shift" into a `Modifiers` set for a
/// tap-modifier binding. Returns `None` when `s` is not modifier-only, so the
/// caller falls back to `parse_key_combo`.
pub fn parse_tap_combo(s: &str, mod_key: ModKey) -> Option<Result<Modifiers, String>> {
    if !is_modifier_only(s) {
        return None;
    }
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    Some(parse_modifiers(&parts, mod_key))
}

/// Parse a key combo string like "Mod+Shift+Up" into a KeyCombo.
pub fn parse_key_combo(s: &str, mod_key: ModKey) -> Result<KeyCombo, String> {
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    if parts.is_empty() {
        return Err("empty key combo".to_string());
    }

    let (keysym_name, modifier_parts) = parts.split_last().unwrap();
    let mods = parse_modifiers(modifier_parts, mod_key)?;

    let sym = xkb::keysym_from_name(keysym_name, xkb::KEYSYM_CASE_INSENSITIVE);
    if sym.raw() == keysyms::KEY_NoSymbol {
        return Err(format!("unknown keysym: {keysym_name}"));
    }

    Ok(KeyCombo {
        modifiers: mods,
        sym,
    })
}

/// Parse a mouse binding string like "Mod+Shift+Left" into a MouseBinding.
/// Last segment is the trigger: Left, Right, Middle, TrackpadScroll, WheelScroll.
pub fn parse_mouse_binding(s: &str, mod_key: ModKey) -> Result<MouseBinding, String> {
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    if parts.is_empty() {
        return Err("empty mouse binding".to_string());
    }

    let (trigger_name, modifier_parts) = parts.split_last().unwrap();
    let mods = parse_modifiers(modifier_parts, mod_key)?;

    let trigger = match trigger_name.to_lowercase().as_str() {
        "left" => MouseTrigger::Button(BTN_LEFT),
        "right" => MouseTrigger::Button(BTN_RIGHT),
        "middle" => MouseTrigger::Button(BTN_MIDDLE),
        "trackpad-scroll" => MouseTrigger::TrackpadScroll,
        "wheel-scroll" => MouseTrigger::WheelScroll,
        "wheel-up" => MouseTrigger::WheelUp,
        "wheel-down" => MouseTrigger::WheelDown,
        other => return Err(format!("unknown mouse trigger: {other}")),
    };

    Ok(MouseBinding {
        modifiers: mods,
        trigger,
    })
}

/// Parse a keyboard action string like "exec foot" or "center-nearest up".
pub fn parse_action(s: &str) -> Result<Action, String> {
    let s = s.trim();
    let (name, arg) = match s.split_once(char::is_whitespace) {
        Some((n, a)) => (n, Some(a.trim())),
        None => (s, None),
    };
    match name {
        "exec" => {
            let cmd = arg.ok_or("exec requires a command argument")?;
            Ok(Action::Exec(cmd.to_string()))
        }
        "spawn" => {
            let cmd = arg.ok_or("spawn requires a command argument")?;
            Ok(Action::Spawn(cmd.to_string()))
        }
        "exec-terminal" => Ok(Action::ExecTerminal),
        "exec-launcher" => Ok(Action::ExecLauncher),
        "close-window" => Ok(Action::CloseWindow),
        "suspend-window" => Ok(Action::SuspendWindow),
        "nudge-window" => {
            let dir = parse_direction(arg.ok_or("nudge-window requires a direction")?)?;
            Ok(Action::NudgeWindow(dir))
        }
        "pan-viewport" => {
            let dir = parse_direction(arg.ok_or("pan-viewport requires a direction")?)?;
            Ok(Action::PanViewport(dir))
        }
        "center-window" => Ok(Action::CenterWindow),
        "focus-center" => Ok(Action::FocusCenter),
        "center-nearest" => {
            let dir = parse_direction(arg.ok_or("center-nearest requires a direction")?)?;
            Ok(Action::CenterNearest(dir))
        }
        "cycle-windows" => {
            let dir_str = arg.ok_or("cycle-windows requires forward or backward")?;
            match dir_str {
                "forward" => Ok(Action::CycleWindows { backward: false }),
                "backward" => Ok(Action::CycleWindows { backward: true }),
                other => Err(format!(
                    "cycle-windows: expected forward or backward, got '{other}'"
                )),
            }
        }
        "home-toggle" => Ok(Action::HomeToggle),
        "go-to" => {
            let arg = arg.ok_or("go-to requires <x> <y> coordinates")?;
            let parts: Vec<&str> = arg.split_whitespace().collect();
            if parts.len() != 2 {
                return Err("go-to requires exactly two coordinates: go-to <x> <y>".to_string());
            }
            let x: f64 = parts[0]
                .parse()
                .map_err(|_| format!("invalid x coordinate: {}", parts[0]))?;
            let y: f64 = parts[1]
                .parse()
                .map_err(|_| format!("invalid y coordinate: {}", parts[1]))?;
            Ok(Action::GoToPosition(x, y))
        }
        "go-to-bookmark" => {
            let name = arg
                .filter(|a| !a.is_empty())
                .ok_or("go-to-bookmark requires a bookmark name")?;
            Ok(Action::GoToBookmark(name.to_string()))
        }
        "set-bookmark" => {
            let name = arg
                .filter(|a| !a.is_empty())
                .ok_or("set-bookmark requires a bookmark name")?;
            Ok(Action::SetBookmark(name.to_string()))
        }
        "move-to-bookmark" => {
            let name = arg
                .filter(|a| !a.is_empty())
                .ok_or("move-to-bookmark requires a bookmark name")?;
            Ok(Action::MoveToBookmark(name.to_string()))
        }
        "zoom-in" => Ok(Action::ZoomIn),
        "zoom-out" => Ok(Action::ZoomOut),
        "zoom-reset" => Ok(Action::ZoomReset),
        "zoom-to-fit" => Ok(Action::ZoomToFit),
        "zoom-to-fit-snapped" => Ok(Action::ZoomToFitSnapped),
        "toggle-fullscreen" => Ok(Action::ToggleFullscreen),
        "fit-window" => Ok(Action::FitWindow),
        "fit-window-snapped" => Ok(Action::FitWindowSnapped),
        "fill-window" => Ok(Action::FillWindow),
        "send-to-output" => {
            let dir = parse_direction(arg.ok_or("send-to-output requires a direction")?)?;
            Ok(Action::SendToOutput(dir))
        }
        "send-cursor-to-output" => {
            let dir = parse_direction(arg.ok_or("send-cursor-to-output requires a direction")?)?;
            Ok(Action::SendCursorToOutput(dir))
        }
        "switch-layout" => {
            let arg = arg.ok_or("switch-layout requires next, prev, or a layout index")?;
            let target = match arg.trim().to_lowercase().as_str() {
                "next" => LayoutSwitch::Next,
                "prev" | "previous" => LayoutSwitch::Prev,
                other => {
                    let idx: usize = other.parse().map_err(|_| {
                        format!("switch-layout: expected next, prev, or an index, got '{other}'")
                    })?;
                    LayoutSwitch::Index(idx)
                }
            };
            Ok(Action::SwitchLayout(target))
        }
        "toggle-pin-to-screen" => Ok(Action::TogglePinToScreen),
        "reload-config" => Ok(Action::ReloadConfig),
        "toggle-cursor-pan" => Ok(Action::ToggleCursorPan),
        "quit" => Ok(Action::Quit),
        other => Err(format!("unknown action: {other}")),
    }
}

/// Every action name `parse_action` accepts, with a canonical sample binding
/// value that must parse. The config-reference exhaustiveness test keeps this
/// list and the documented `Actions:` catalog in lockstep.
pub const ACTION_NAMES: &[(&str, &str)] = &[
    ("center-nearest", "center-nearest up"),
    ("center-window", "center-window"),
    ("close-window", "close-window"),
    ("cycle-windows", "cycle-windows forward"),
    ("exec", "exec foo"),
    ("exec-launcher", "exec-launcher"),
    ("exec-terminal", "exec-terminal"),
    ("fill-window", "fill-window"),
    ("fit-window", "fit-window"),
    ("fit-window-snapped", "fit-window-snapped"),
    ("focus-center", "focus-center"),
    ("go-to", "go-to 0 0"),
    ("go-to-bookmark", "go-to-bookmark 1"),
    ("home-toggle", "home-toggle"),
    ("move-to-bookmark", "move-to-bookmark 1"),
    ("nudge-window", "nudge-window up"),
    ("pan-viewport", "pan-viewport up"),
    ("quit", "quit"),
    ("reload-config", "reload-config"),
    ("send-cursor-to-output", "send-cursor-to-output up"),
    ("send-to-output", "send-to-output up"),
    ("set-bookmark", "set-bookmark 1"),
    ("spawn", "spawn foo"),
    ("suspend-window", "suspend-window"),
    ("switch-layout", "switch-layout next"),
    ("toggle-cursor-pan", "toggle-cursor-pan"),
    ("toggle-fullscreen", "toggle-fullscreen"),
    ("toggle-pin-to-screen", "toggle-pin-to-screen"),
    ("zoom-in", "zoom-in"),
    ("zoom-out", "zoom-out"),
    ("zoom-reset", "zoom-reset"),
    ("zoom-to-fit", "zoom-to-fit"),
    ("zoom-to-fit-snapped", "zoom-to-fit-snapped"),
];

/// Parse a mouse action string like "move-window" or "zoom".
/// Continuous/grab actions are matched first; anything else falls through
/// to `parse_action` so that any keyboard action works for click triggers.
pub fn parse_mouse_action(s: &str) -> Result<MouseAction, String> {
    match s.trim() {
        "move-window" => Ok(MouseAction::MoveWindow),
        "move-snapped-windows" => Ok(MouseAction::MoveSnappedWindows),
        "resize-window" => Ok(MouseAction::ResizeWindow),
        "resize-window-snapped" => Ok(MouseAction::ResizeWindowSnapped),
        "pan-viewport" => Ok(MouseAction::PanViewport),
        "zoom" => Ok(MouseAction::Zoom),
        "center-nearest" => Ok(MouseAction::CenterNearest),
        other => {
            let action = parse_action(other)?;
            Ok(MouseAction::Action(action))
        }
    }
}

/// Parse a direction string (case-insensitive).
pub fn parse_direction(s: &str) -> Result<Direction, String> {
    match s.trim().to_lowercase().as_str() {
        "up" => Ok(Direction::Up),
        "down" => Ok(Direction::Down),
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "up-left" => Ok(Direction::UpLeft),
        "up-right" => Ok(Direction::UpRight),
        "down-left" => Ok(Direction::DownLeft),
        "down-right" => Ok(Direction::DownRight),
        other => Err(format!("unknown direction: {other}")),
    }
}

// ── Gesture parsing ──────────────────────────────────────────────────

/// Parse a gesture binding string like "mod+3-finger-swipe" into a GestureBinding.
/// Last segment(s) are the gesture trigger, preceding parts are modifiers.
pub fn parse_gesture_binding(s: &str, mod_key: ModKey) -> Result<GestureBinding, String> {
    let s = s.trim().to_lowercase();

    // Find the split: everything before the N-finger part is modifiers.
    // Strategy: scan for "N-finger" pattern to split modifiers from trigger.
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    if parts.is_empty() {
        return Err("empty gesture binding".to_string());
    }

    // Find the first part that starts with a digit (the finger count).
    let trigger_idx = parts
        .iter()
        .position(|p| p.starts_with(|c: char| c.is_ascii_digit()))
        .ok_or_else(|| format!("no gesture trigger found in '{s}' (expected N-finger-...)"))?;

    let modifier_parts = &parts[..trigger_idx];
    let mods = parse_modifiers(modifier_parts, mod_key)?;

    // Rejoin the trigger parts (e.g. ["3", "finger", "swipe"] from "3-finger-swipe")
    let trigger_str = parts[trigger_idx..].join("+");
    let trigger = parse_gesture_trigger(&trigger_str)?;

    Ok(GestureBinding {
        modifiers: mods,
        trigger,
    })
}

/// Parse a gesture trigger string like "3-finger-swipe" or "4-finger-pinch-in".
pub fn parse_gesture_trigger(s: &str) -> Result<GestureTrigger, String> {
    let s = s.trim().to_lowercase();
    let s = &s;

    // Extract finger count: "N-finger-..."
    let Some((fingers_str, gesture_type)) = s.split_once("-finger-") else {
        return Err(format!(
            "invalid gesture trigger '{s}' (expected N-finger-<type>)"
        ));
    };

    let fingers: u32 = fingers_str
        .parse()
        .map_err(|_| format!("invalid finger count: '{fingers_str}'"))?;
    if !(2..=5).contains(&fingers) {
        return Err(format!("finger count must be 2-5, got {fingers}"));
    }

    match gesture_type {
        "swipe" => Ok(GestureTrigger::Swipe { fingers }),
        "swipe-up" => Ok(GestureTrigger::SwipeUp { fingers }),
        "swipe-down" => Ok(GestureTrigger::SwipeDown { fingers }),
        "swipe-left" => Ok(GestureTrigger::SwipeLeft { fingers }),
        "swipe-right" => Ok(GestureTrigger::SwipeRight { fingers }),
        "doubletap-swipe" => Ok(GestureTrigger::DoubletapSwipe { fingers }),
        "pinch" => Ok(GestureTrigger::Pinch { fingers }),
        "pinch-in" => Ok(GestureTrigger::PinchIn { fingers }),
        "pinch-out" => Ok(GestureTrigger::PinchOut { fingers }),
        "hold" => Ok(GestureTrigger::Hold { fingers }),
        other => Err(format!("unknown gesture type: '{other}'")),
    }
}

/// Check if an action string names a continuous action.
fn parse_continuous_action(s: &str) -> Option<ContinuousAction> {
    match s {
        "pan-viewport" => Some(ContinuousAction::PanViewport),
        "zoom" => Some(ContinuousAction::Zoom),
        "move-window" => Some(ContinuousAction::MoveWindow),
        "move-snapped-windows" => Some(ContinuousAction::MoveSnappedWindows),
        "resize-window" => Some(ContinuousAction::ResizeWindow),
        "resize-window-snapped" => Some(ContinuousAction::ResizeWindowSnapped),
        _ => None,
    }
}

/// Classify `s` as a threshold action. Every keyboard action qualifies, plus
/// bare `center-nearest` (its direction comes from the swipe); continuous-only
/// actions don't. A known action name with a bad argument keeps its specific
/// parse error; an unknown name yields `None` so the caller's generic "unknown
/// action" message applies.
fn parse_threshold_action(s: &str) -> Result<Option<ThresholdAction>, String> {
    if s == "center-nearest" {
        return Ok(Some(ThresholdAction::CenterNearest));
    }
    if parse_continuous_action(s).is_some() {
        return Ok(None);
    }
    match parse_action(s) {
        Ok(action) => Ok(Some(ThresholdAction::Fixed(action))),
        Err(e) => {
            let first = s.split_whitespace().next().unwrap_or("");
            if ACTION_NAMES.iter().any(|(name, _)| *name == first) {
                Err(e)
            } else {
                Ok(None)
            }
        }
    }
}

/// Validate trigger + action combination per the validation table.
/// Returns a GestureConfigEntry or error with a specific message.
pub fn parse_gesture_config_entry(
    trigger: &GestureTrigger,
    action_str: &str,
) -> Result<GestureConfigEntry, String> {
    let action_str = action_str.trim();
    let is_continuous = parse_continuous_action(action_str);
    let is_threshold = parse_threshold_action(action_str)?;

    match trigger {
        GestureTrigger::Swipe { .. } => {
            if let Some(ContinuousAction::Zoom) = is_continuous {
                return Err("zoom requires a pinch trigger (needs scale from input)".to_string());
            }
            if let Some(ca) = is_continuous {
                Ok(GestureConfigEntry::Continuous(ca))
            } else if let Some(ta) = is_threshold {
                Ok(GestureConfigEntry::Threshold(ta))
            } else {
                Err(format!("unknown gesture action: '{action_str}'"))
            }
        }
        GestureTrigger::DoubletapSwipe { .. }
        | GestureTrigger::HoldSwipe { .. }
        | GestureTrigger::DoubletapHoldSwipe { .. } => match (is_continuous, is_threshold) {
            (
                Some(
                    ca @ (ContinuousAction::MoveWindow
                    | ContinuousAction::MoveSnappedWindows
                    | ContinuousAction::ResizeWindow
                    | ContinuousAction::ResizeWindowSnapped),
                ),
                _,
            ) => Ok(GestureConfigEntry::Continuous(ca)),
            (Some(_), _) | (None, Some(_)) => Err(
                "doubletap-swipe/hold-swipe/doubletap-hold-swipe only support the window-grab \
                     actions: move-window, move-snapped-windows, resize-window, resize-window-snapped"
                    .to_string(),
            ),
            (None, None) => Err(format!("unknown gesture action: '{action_str}'")),
        },
        GestureTrigger::SwipeUp { .. }
        | GestureTrigger::SwipeDown { .. }
        | GestureTrigger::SwipeLeft { .. }
        | GestureTrigger::SwipeRight { .. } => {
            // Threshold only
            if is_continuous.is_some() {
                return Err(format!(
                    "per-direction swipe triggers only accept threshold actions, \
                     not '{action_str}'"
                ));
            }
            if let Some(ta) = is_threshold {
                Ok(GestureConfigEntry::Threshold(ta))
            } else {
                Err(format!("unknown gesture action: '{action_str}'"))
            }
        }
        GestureTrigger::Pinch { .. } => {
            // A bare pinch always resolves to zoom at runtime, so parsing
            // rejects any other continuous action.
            if let Some(ContinuousAction::Zoom) = is_continuous {
                Ok(GestureConfigEntry::Continuous(ContinuousAction::Zoom))
            } else {
                Err(
                    "pinch only supports zoom; use pinch-in/pinch-out for discrete actions"
                        .to_string(),
                )
            }
        }
        GestureTrigger::PinchIn { .. } | GestureTrigger::PinchOut { .. } => {
            if is_continuous.is_some() {
                return Err(format!(
                    "pinch-in/pinch-out triggers only accept threshold actions, \
                     not '{action_str}'"
                ));
            }
            if matches!(is_threshold, Some(ThresholdAction::CenterNearest)) {
                return Err(
                    "center-nearest requires a swipe trigger (needs direction from input)"
                        .to_string(),
                );
            }
            if let Some(ta) = is_threshold {
                Ok(GestureConfigEntry::Threshold(ta))
            } else {
                Err(format!("unknown gesture action: '{action_str}'"))
            }
        }
        GestureTrigger::Hold { .. } => {
            if is_continuous.is_some() {
                return Err(format!(
                    "hold trigger only accepts threshold actions, not '{action_str}'"
                ));
            }
            if matches!(is_threshold, Some(ThresholdAction::CenterNearest)) {
                return Err(
                    "center-nearest requires a swipe trigger (needs direction from input)"
                        .to_string(),
                );
            }
            if let Some(ta) = is_threshold {
                Ok(GestureConfigEntry::Threshold(ta))
            } else {
                Err(format!("unknown gesture action: '{action_str}'"))
            }
        }
        GestureTrigger::Tap { .. } | GestureTrigger::Doubletap { .. } => {
            Err("tap/doubletap are touch-only triggers".to_string())
        }
    }
}

/// Parse a touch trigger string like "1-finger-swipe", "3-finger-tap", or
/// "3-finger-hold-swipe". Touch has no modifiers (the whole string is the trigger)
/// and 1-finger gestures are valid, unlike the trackpad's 2-finger floor.
pub fn parse_touch_trigger(s: &str) -> Result<GestureTrigger, String> {
    let s = s.trim().to_lowercase();
    let s = &s;

    let Some((fingers_str, gesture_type)) = s.split_once("-finger-") else {
        return Err(format!(
            "invalid touch trigger '{s}' (expected N-finger-<type>)"
        ));
    };

    let fingers: u32 = fingers_str
        .parse()
        .map_err(|_| format!("invalid finger count: '{fingers_str}'"))?;
    if !(1..=5).contains(&fingers) {
        return Err(format!("finger count must be 1-5, got {fingers}"));
    }

    match gesture_type {
        "swipe" => Ok(GestureTrigger::Swipe { fingers }),
        "swipe-up" => Ok(GestureTrigger::SwipeUp { fingers }),
        "swipe-down" => Ok(GestureTrigger::SwipeDown { fingers }),
        "swipe-left" => Ok(GestureTrigger::SwipeLeft { fingers }),
        "swipe-right" => Ok(GestureTrigger::SwipeRight { fingers }),
        "doubletap-swipe" => Ok(GestureTrigger::DoubletapSwipe { fingers }),
        "hold-swipe" => Ok(GestureTrigger::HoldSwipe { fingers }),
        "doubletap-hold-swipe" => Ok(GestureTrigger::DoubletapHoldSwipe { fingers }),
        "pinch" => Ok(GestureTrigger::Pinch { fingers }),
        "pinch-in" => Ok(GestureTrigger::PinchIn { fingers }),
        "pinch-out" => Ok(GestureTrigger::PinchOut { fingers }),
        "tap" => Ok(GestureTrigger::Tap { fingers }),
        "doubletap" => Ok(GestureTrigger::Doubletap { fingers }),
        other => Err(format!("unknown touch gesture type: '{other}'")),
    }
}

/// Validate a touch trigger + action combination. Touch adds its own tap /
/// doubletap arms; every other trigger shares the trackpad rules
/// (`parse_gesture_config_entry`).
pub fn parse_touch_config_entry(
    trigger: &GestureTrigger,
    action_str: &str,
) -> Result<GestureConfigEntry, String> {
    let action_str = action_str.trim();
    match trigger {
        GestureTrigger::Tap { .. } | GestureTrigger::Doubletap { .. } => {
            if parse_continuous_action(action_str).is_some() {
                return Err(format!(
                    "tap/doubletap triggers only accept threshold actions, not '{action_str}'"
                ));
            }
            match parse_threshold_action(action_str)? {
                Some(ThresholdAction::CenterNearest) => Err(
                    "center-nearest requires a swipe trigger (needs direction from input)"
                        .to_string(),
                ),
                Some(ta) => Ok(GestureConfigEntry::Threshold(ta)),
                None => Err(format!("unknown touch action: '{action_str}'")),
            }
        }
        _ => parse_gesture_config_entry(trigger, action_str),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// exhaustive on purpose — a new Action variant fails to compile here until
    /// it's named, and its name must join ACTION_NAMES.
    fn action_name(action: &Action) -> &'static str {
        match action {
            Action::Exec(_) => "exec",
            Action::ExecTerminal => "exec-terminal",
            Action::ExecLauncher => "exec-launcher",
            Action::Spawn(_) => "spawn",
            Action::CloseWindow => "close-window",
            Action::SuspendWindow => "suspend-window",
            Action::NudgeWindow(_) => "nudge-window",
            Action::PanViewport(_) => "pan-viewport",
            Action::CenterWindow => "center-window",
            Action::CenterNearest(_) => "center-nearest",
            Action::CycleWindows { .. } => "cycle-windows",
            Action::HomeToggle => "home-toggle",
            Action::GoToPosition(..) => "go-to",
            Action::GoToBookmark(_) => "go-to-bookmark",
            Action::SetBookmark(_) => "set-bookmark",
            Action::MoveToBookmark(_) => "move-to-bookmark",
            Action::ZoomIn => "zoom-in",
            Action::ZoomOut => "zoom-out",
            Action::ZoomReset => "zoom-reset",
            Action::ZoomToFit => "zoom-to-fit",
            Action::ZoomToFitSnapped => "zoom-to-fit-snapped",
            Action::ToggleFullscreen => "toggle-fullscreen",
            Action::FitWindow => "fit-window",
            Action::FitWindowSnapped => "fit-window-snapped",
            Action::FillWindow => "fill-window",
            Action::SendToOutput(_) => "send-to-output",
            Action::SendCursorToOutput(_) => "send-cursor-to-output",
            Action::FocusCenter => "focus-center",
            Action::TogglePinToScreen => "toggle-pin-to-screen",
            Action::SwitchLayout(_) => "switch-layout",
            Action::ReloadConfig => "reload-config",
            Action::ToggleCursorPan => "toggle-cursor-pan",
            Action::Quit => "quit",
        }
    }

    #[test]
    fn bookmark_actions_parse_with_name() {
        assert_eq!(
            parse_action("go-to-bookmark 1"),
            Ok(Action::GoToBookmark("1".into()))
        );
        assert_eq!(
            parse_action("set-bookmark home"),
            Ok(Action::SetBookmark("home".into()))
        );
        // The whole trimmed remainder is the name, so spaces are allowed in it.
        assert_eq!(
            parse_action("move-to-bookmark my desk"),
            Ok(Action::MoveToBookmark("my desk".into()))
        );
    }

    #[test]
    fn bookmark_actions_reject_missing_name() {
        assert!(parse_action("go-to-bookmark").is_err());
        assert!(parse_action("set-bookmark").is_err());
        assert!(parse_action("move-to-bookmark").is_err());
        // Whitespace-only argument is empty after trimming.
        assert!(parse_action("go-to-bookmark   ").is_err());
    }

    #[test]
    fn action_names_round_trip_to_their_variant() {
        for (name, sample) in ACTION_NAMES {
            let parsed = parse_action(sample)
                .unwrap_or_else(|e| panic!("sample {sample:?} for {name} failed to parse: {e}"));
            assert_eq!(
                action_name(&parsed),
                *name,
                "sample {sample:?} parsed to a variant whose name is not {name:?}"
            );
        }
    }
}
