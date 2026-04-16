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
        "close-window" => Ok(Action::CloseWindow),
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
        "zoom-in" => Ok(Action::ZoomIn),
        "zoom-out" => Ok(Action::ZoomOut),
        "zoom-reset" => Ok(Action::ZoomReset),
        "zoom-to-fit" => Ok(Action::ZoomToFit),
        "toggle-fullscreen" => Ok(Action::ToggleFullscreen),
        "fit-window" => Ok(Action::FitWindow),
        "send-to-output" => {
            let dir = parse_direction(arg.ok_or("send-to-output requires a direction")?)?;
            Ok(Action::SendToOutput(dir))
        }
        "reload-config" => Ok(Action::ReloadConfig),
        "quit" => Ok(Action::Quit),
        other => Err(format!("unknown action: {other}")),
    }
}

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
        "resize-window" => Some(ContinuousAction::ResizeWindow),
        "resize-window-snapped" => Some(ContinuousAction::ResizeWindowSnapped),
        _ => None,
    }
}

/// Check if an action string names a threshold action.
fn parse_threshold_action(s: &str) -> Result<Option<ThresholdAction>, String> {
    match s {
        "center-nearest" => Ok(Some(ThresholdAction::CenterNearest)),
        "center-window" | "focus-center" | "home-toggle" | "zoom-to-fit" | "zoom-in"
        | "zoom-out" | "zoom-reset" | "toggle-fullscreen" | "fit-window" | "reload-config"
        | "quit" | "close-window" => {
            let action = parse_action(s)?;
            Ok(Some(ThresholdAction::Fixed(action)))
        }
        s if s.starts_with("exec ")
            || s.starts_with("spawn ")
            || s.starts_with("send-to-output ") =>
        {
            let action = parse_action(s)?;
            Ok(Some(ThresholdAction::Fixed(action)))
        }
        _ => Ok(None),
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
        GestureTrigger::DoubletapSwipe { .. } => {
            match is_continuous {
                Some(ContinuousAction::MoveWindow) => {
                    return Ok(GestureConfigEntry::Continuous(ContinuousAction::MoveWindow));
                }
                Some(ContinuousAction::ResizeWindow) => {
                    return Ok(GestureConfigEntry::Continuous(
                        ContinuousAction::ResizeWindow,
                    ));
                }
                Some(ContinuousAction::ResizeWindowSnapped) => {
                    return Ok(GestureConfigEntry::Continuous(
                        ContinuousAction::ResizeWindowSnapped,
                    ));
                }
                Some(_) => {
                    return Err(
                        "doubletap-swipe only supports move-window, resize-window, and resize-window-snapped"
                            .to_string(),
                    );
                }
                None => {}
            }
            if is_threshold.is_some() {
                Err(
                    "doubletap-swipe only supports move-window, resize-window, and resize-window-snapped"
                        .to_string(),
                )
            } else {
                Err(format!("unknown gesture action: '{action_str}'"))
            }
        }
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
            // Continuous only
            if let Some(ca) = is_continuous {
                Ok(GestureConfigEntry::Continuous(ca))
            } else if is_threshold.is_some() {
                Err(
                    "pinch trigger only accepts continuous actions (pan-viewport, zoom, \
                     move-window, resize-window); use pinch-in or pinch-out for discrete actions"
                        .to_string(),
                )
            } else {
                Err(format!("unknown gesture action: '{action_str}'"))
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
    }
}
