//! Config-syntax rendering of bindings: the inverse of the `parse_*` functions,
//! so a binding read back out of the compositor reads like the config line that
//! produced it.

use std::fmt;

use smithay::input::keyboard::xkb;

use super::types::*;

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Direction::Up => "up",
            Direction::Down => "down",
            Direction::Left => "left",
            Direction::Right => "right",
            Direction::UpLeft => "up-left",
            Direction::UpRight => "up-right",
            Direction::DownLeft => "down-left",
            Direction::DownRight => "down-right",
        })
    }
}

impl fmt::Display for LayoutSwitch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutSwitch::Next => f.write_str("next"),
            LayoutSwitch::Prev => f.write_str("prev"),
            LayoutSwitch::Index(i) => write!(f, "{i}"),
        }
    }
}

impl fmt::Display for TrackpadState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TrackpadState::Toggle => "toggle",
            TrackpadState::On => "on",
            TrackpadState::Off => "off",
        })
    }
}

impl fmt::Display for Action {
    /// Exhaustive on purpose: a new `Action` variant fails to compile here until
    /// it is given its config spelling.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Exec(cmd) => write!(f, "exec {cmd}"),
            Action::ExecTerminal => f.write_str("exec-terminal"),
            Action::ExecLauncher => f.write_str("exec-launcher"),
            Action::Spawn(cmd) => write!(f, "spawn {cmd}"),
            Action::CloseWindow => f.write_str("close-window"),
            Action::SuspendWindow => f.write_str("suspend-window"),
            Action::NudgeWindow(d) => write!(f, "nudge-window {d}"),
            Action::MoveToSnap(d) => write!(f, "move-to-snap {d}"),
            Action::GrowWindow(d) => write!(f, "grow-window {d}"),
            Action::ShrinkWindow(d) => write!(f, "shrink-window {d}"),
            Action::ResizeToSnap(d) => write!(f, "resize-to-snap {d}"),
            Action::PanViewport(d) => write!(f, "pan-viewport {d}"),
            Action::CenterWindow => f.write_str("center-window"),
            Action::CenterNearest(d) => write!(f, "center-nearest {d}"),
            Action::FocusNearest(d) => write!(f, "focus-nearest {d}"),
            Action::CycleWindows { backward: false } => f.write_str("cycle-windows forward"),
            Action::CycleWindows { backward: true } => f.write_str("cycle-windows backward"),
            Action::HomeToggle => f.write_str("home-toggle"),
            Action::GoToBookmark(name) => write!(f, "go-to-bookmark {name}"),
            Action::SetBookmark(name) => write!(f, "set-bookmark {name}"),
            Action::MoveToBookmark(name) => write!(f, "move-to-bookmark {name}"),
            Action::ZoomIn => f.write_str("zoom-in"),
            Action::ZoomOut => f.write_str("zoom-out"),
            Action::ZoomReset => f.write_str("zoom-reset"),
            Action::ZoomToFit => f.write_str("zoom-to-fit"),
            Action::ZoomToFitSnapped => f.write_str("zoom-to-fit-snapped"),
            Action::ToggleFullscreen => f.write_str("toggle-fullscreen"),
            Action::FitWindow => f.write_str("fit-window"),
            Action::FitWindowSnapped => f.write_str("fit-window-snapped"),
            Action::FillWindow => f.write_str("fill-window"),
            Action::SendToOutput(d) => write!(f, "send-to-output {d}"),
            Action::SendCursorToOutput(d) => write!(f, "send-cursor-to-output {d}"),
            Action::FocusCenter => f.write_str("focus-center"),
            Action::TogglePinToScreen => f.write_str("toggle-pin-to-screen"),
            Action::SwitchLayout(target) => write!(f, "switch-layout {target}"),
            Action::ReloadConfig => f.write_str("reload-config"),
            Action::ToggleCursorPan => f.write_str("toggle-cursor-pan"),
            Action::SetTrackpad(state) => write!(f, "set-trackpad {state}"),
            Action::Quit => f.write_str("quit"),
        }
    }
}

impl fmt::Display for MouseAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MouseAction::MoveWindow => f.write_str("move-window"),
            MouseAction::MoveSnappedWindows => f.write_str("move-snapped-windows"),
            MouseAction::ResizeWindow => f.write_str("resize-window"),
            MouseAction::ResizeWindowSnapped => f.write_str("resize-window-snapped"),
            MouseAction::PanViewport => f.write_str("pan-viewport"),
            MouseAction::Zoom => f.write_str("zoom"),
            MouseAction::CenterNearest => f.write_str("center-nearest"),
            MouseAction::FocusNearest => f.write_str("focus-nearest"),
            MouseAction::Action(action) => action.fmt(f),
        }
    }
}

fn modifier_names(m: &Modifiers) -> impl Iterator<Item = &'static str> {
    [
        (m.logo, "super"),
        (m.ctrl, "ctrl"),
        (m.alt, "alt"),
        (m.shift, "shift"),
        (m.mod3, "mod3"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
}

fn write_modifier_prefix(f: &mut fmt::Formatter<'_>, m: &Modifiers) -> fmt::Result {
    modifier_names(m).try_for_each(|name| write!(f, "{name}+"))
}

impl fmt::Display for Modifiers {
    /// A bare modifier chord, as a tap binding is written (`alt+shift`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&modifier_names(self).collect::<Vec<_>>().join("+"))
    }
}

impl fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_modifier_prefix(f, &self.modifiers)?;
        f.write_str(&xkb::keysym_get_name(self.sym))
    }
}

impl fmt::Display for MouseTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MouseTrigger::Button(BTN_LEFT) => f.write_str("left"),
            MouseTrigger::Button(BTN_RIGHT) => f.write_str("right"),
            MouseTrigger::Button(BTN_MIDDLE) => f.write_str("middle"),
            MouseTrigger::Button(code) => write!(f, "button-{code:#x}"),
            MouseTrigger::TrackpadScroll => f.write_str("trackpad-scroll"),
            MouseTrigger::WheelScroll => f.write_str("wheel-scroll"),
            MouseTrigger::WheelUp => f.write_str("wheel-up"),
            MouseTrigger::WheelDown => f.write_str("wheel-down"),
        }
    }
}

impl fmt::Display for MouseBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_modifier_prefix(f, &self.modifiers)?;
        self.trigger.fmt(f)
    }
}
