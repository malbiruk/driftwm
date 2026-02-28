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
/// Last segment is the trigger: Left, Right, Middle, Scroll.
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
        "scroll" => MouseTrigger::Scroll,
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
        "center-nearest" => {
            let dir = parse_direction(arg.ok_or("center-nearest requires a direction")?)?;
            Ok(Action::CenterNearest(dir))
        }
        "cycle-windows" => {
            let dir_str = arg.ok_or("cycle-windows requires forward or backward")?;
            match dir_str {
                "forward" => Ok(Action::CycleWindows { backward: false }),
                "backward" => Ok(Action::CycleWindows { backward: true }),
                other => Err(format!("cycle-windows: expected forward or backward, got '{other}'")),
            }
        }
        "home-toggle" => Ok(Action::HomeToggle),
        "zoom-in" => Ok(Action::ZoomIn),
        "zoom-out" => Ok(Action::ZoomOut),
        "zoom-reset" => Ok(Action::ZoomReset),
        "zoom-to-fit" => Ok(Action::ZoomToFit),
        "toggle-fullscreen" => Ok(Action::ToggleFullscreen),
        "reload-config" => Ok(Action::ReloadConfig),
        "quit" => Ok(Action::Quit),
        other => Err(format!("unknown action: {other}")),
    }
}

/// Parse a mouse action string like "move-window" or "zoom".
pub fn parse_mouse_action(s: &str) -> Result<MouseAction, String> {
    match s.trim() {
        "move-window" => Ok(MouseAction::MoveWindow),
        "resize-window" => Ok(MouseAction::ResizeWindow),
        "pan-viewport" => Ok(MouseAction::PanViewport),
        "zoom" => Ok(MouseAction::Zoom),
        "navigate" => Ok(MouseAction::Navigate),
        other => Err(format!("unknown mouse action: {other}")),
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
