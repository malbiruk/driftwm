use std::collections::HashMap;

use smithay::input::keyboard::{Keysym, keysyms};

use super::types::*;

pub(super) fn default_bindings(
    mod_key: ModKey,
    cycle_mod: CycleModifier,
) -> HashMap<KeyCombo, Action> {
    let terminal = detect_terminal();
    let launcher = detect_launcher();

    let m = mod_key.base();
    let m_shift = Modifiers {
        shift: true,
        ..m.clone()
    };
    let m_ctrl = Modifiers {
        ctrl: true,
        ..m.clone()
    };
    let cyc = cycle_mod.base();
    let cyc_shift = Modifiers {
        shift: true,
        ..cyc.clone()
    };

    let mut bindings = HashMap::from([
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_Return),
            },
            Action::Exec(terminal),
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_d),
            },
            Action::Exec(launcher),
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_q),
            },
            Action::CloseWindow,
        ),
        (
            KeyCombo {
                modifiers: m_shift.clone(),
                sym: Keysym::from(keysyms::KEY_Up),
            },
            Action::NudgeWindow(Direction::Up),
        ),
        (
            KeyCombo {
                modifiers: m_shift.clone(),
                sym: Keysym::from(keysyms::KEY_Down),
            },
            Action::NudgeWindow(Direction::Down),
        ),
        (
            KeyCombo {
                modifiers: m_shift.clone(),
                sym: Keysym::from(keysyms::KEY_Left),
            },
            Action::NudgeWindow(Direction::Left),
        ),
        (
            KeyCombo {
                modifiers: m_shift.clone(),
                sym: Keysym::from(keysyms::KEY_Right),
            },
            Action::NudgeWindow(Direction::Right),
        ),
        (
            KeyCombo {
                modifiers: m_ctrl.clone(),
                sym: Keysym::from(keysyms::KEY_Up),
            },
            Action::PanViewport(Direction::Up),
        ),
        (
            KeyCombo {
                modifiers: m_ctrl.clone(),
                sym: Keysym::from(keysyms::KEY_Down),
            },
            Action::PanViewport(Direction::Down),
        ),
        (
            KeyCombo {
                modifiers: m_ctrl.clone(),
                sym: Keysym::from(keysyms::KEY_Left),
            },
            Action::PanViewport(Direction::Left),
        ),
        (
            KeyCombo {
                modifiers: m_ctrl,
                sym: Keysym::from(keysyms::KEY_Right),
            },
            Action::PanViewport(Direction::Right),
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_a),
            },
            Action::HomeToggle,
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_1),
            },
            Action::GoToPosition(-1750.0, 1750.0),
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_2),
            },
            Action::GoToPosition(1750.0, 1750.0),
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_3),
            },
            Action::GoToPosition(1750.0, -1750.0),
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_4),
            },
            Action::GoToPosition(-1750.0, -1750.0),
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_c),
            },
            Action::CenterWindow,
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_x),
            },
            Action::FocusCenter,
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_Up),
            },
            Action::CenterNearest(Direction::Up),
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_Down),
            },
            Action::CenterNearest(Direction::Down),
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_Left),
            },
            Action::CenterNearest(Direction::Left),
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_Right),
            },
            Action::CenterNearest(Direction::Right),
        ),
        (
            KeyCombo {
                modifiers: cyc,
                sym: Keysym::from(keysyms::KEY_Tab),
            },
            Action::CycleWindows { backward: false },
        ),
        (
            KeyCombo {
                modifiers: cyc_shift,
                sym: Keysym::from(keysyms::KEY_Tab),
            },
            Action::CycleWindows { backward: true },
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_equal),
            },
            Action::ZoomIn,
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_minus),
            },
            Action::ZoomOut,
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_0),
            },
            Action::ZoomReset,
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_z),
            },
            Action::ZoomReset,
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_w),
            },
            Action::ZoomToFit,
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_f),
            },
            Action::ToggleFullscreen,
        ),
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_m),
            },
            Action::FitWindow,
        ),
        (
            KeyCombo {
                modifiers: Modifiers {
                    ctrl: true,
                    shift: true,
                    ..m.clone()
                },
                sym: Keysym::from(keysyms::KEY_q),
            },
            Action::Quit,
        ),
        // Media keys
        (
            KeyCombo {
                modifiers: Modifiers::EMPTY,
                sym: Keysym::from(keysyms::KEY_XF86AudioRaiseVolume),
            },
            Action::Spawn("wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+".into()),
        ),
        (
            KeyCombo {
                modifiers: Modifiers::EMPTY,
                sym: Keysym::from(keysyms::KEY_XF86AudioLowerVolume),
            },
            Action::Spawn("wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-".into()),
        ),
        (
            KeyCombo {
                modifiers: Modifiers::EMPTY,
                sym: Keysym::from(keysyms::KEY_XF86AudioMute),
            },
            Action::Spawn("wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle".into()),
        ),
        (
            KeyCombo {
                modifiers: Modifiers::EMPTY,
                sym: Keysym::from(keysyms::KEY_XF86MonBrightnessUp),
            },
            Action::Spawn("brightnessctl set +5%".into()),
        ),
        (
            KeyCombo {
                modifiers: Modifiers::EMPTY,
                sym: Keysym::from(keysyms::KEY_XF86MonBrightnessDown),
            },
            Action::Spawn("brightnessctl set 5%-".into()),
        ),
        // Screenshot
        (
            KeyCombo {
                modifiers: Modifiers::EMPTY,
                sym: Keysym::from(keysyms::KEY_Print),
            },
            Action::Spawn("grim - | wl-copy".into()),
        ),
        (
            KeyCombo {
                modifiers: Modifiers {
                    shift: true,
                    ..Modifiers::EMPTY
                },
                sym: Keysym::from(keysyms::KEY_Print),
            },
            Action::Spawn("grim -g \"$(slurp -d)\" - | wl-copy".into()),
        ),
        // Lock screen
        (
            KeyCombo {
                modifiers: m.clone(),
                sym: Keysym::from(keysyms::KEY_l),
            },
            Action::Spawn("swaylock -f -c 000000 -kl".into()),
        ),
    ]);

    // Send-to-output bindings (Mod+Alt+Arrow) — only for Super mod_key
    // to avoid conflict with Alt-based bindings
    if mod_key == ModKey::Super {
        let m_alt = Modifiers {
            alt: true,
            ..m.clone()
        };
        bindings.extend([
            (
                KeyCombo {
                    modifiers: m_alt.clone(),
                    sym: Keysym::from(keysyms::KEY_Up),
                },
                Action::SendToOutput(Direction::Up),
            ),
            (
                KeyCombo {
                    modifiers: m_alt.clone(),
                    sym: Keysym::from(keysyms::KEY_Down),
                },
                Action::SendToOutput(Direction::Down),
            ),
            (
                KeyCombo {
                    modifiers: m_alt.clone(),
                    sym: Keysym::from(keysyms::KEY_Left),
                },
                Action::SendToOutput(Direction::Left),
            ),
            (
                KeyCombo {
                    modifiers: m_alt,
                    sym: Keysym::from(keysyms::KEY_Right),
                },
                Action::SendToOutput(Direction::Right),
            ),
        ]);
    }

    bindings
}

pub(super) fn default_mouse_bindings(
    mod_key: ModKey,
) -> ContextBindings<MouseBinding, MouseAction> {
    let m = mod_key.base();
    let alt_only = Modifiers {
        alt: true,
        ..Modifiers::EMPTY
    };
    let alt_shift = Modifiers {
        alt: true,
        shift: true,
        ..Modifiers::EMPTY
    };
    let m_ctrl = Modifiers {
        ctrl: true,
        ..m.clone()
    };

    // Defaults use the canonical action names; `resize_snapped_default` is
    // applied as a post-parse swap in `Config::from_raw` so that it also
    // flips user-defined bindings, not just these defaults.
    let on_window = HashMap::from([
        (
            MouseBinding {
                modifiers: alt_only.clone(),
                trigger: MouseTrigger::Button(BTN_LEFT),
            },
            MouseAction::MoveWindow,
        ),
        (
            MouseBinding {
                modifiers: alt_shift.clone(),
                trigger: MouseTrigger::Button(BTN_LEFT),
            },
            MouseAction::MoveSnappedWindows,
        ),
        (
            MouseBinding {
                modifiers: alt_only.clone(),
                trigger: MouseTrigger::Button(BTN_RIGHT),
            },
            MouseAction::ResizeWindow,
        ),
        (
            MouseBinding {
                modifiers: alt_shift,
                trigger: MouseTrigger::Button(BTN_RIGHT),
            },
            MouseAction::ResizeWindowSnapped,
        ),
        (
            MouseBinding {
                modifiers: alt_only.clone(),
                trigger: MouseTrigger::Button(BTN_MIDDLE),
            },
            MouseAction::Action(Action::FitWindow),
        ),
        (
            MouseBinding {
                modifiers: m.clone(),
                trigger: MouseTrigger::Button(BTN_MIDDLE),
            },
            MouseAction::Action(Action::ToggleFullscreen),
        ),
    ]);

    let on_canvas = HashMap::from([
        (
            MouseBinding {
                modifiers: Modifiers::EMPTY,
                trigger: MouseTrigger::Button(BTN_LEFT),
            },
            MouseAction::PanViewport,
        ),
        (
            MouseBinding {
                modifiers: Modifiers::EMPTY,
                trigger: MouseTrigger::TrackpadScroll,
            },
            MouseAction::PanViewport,
        ),
        (
            MouseBinding {
                modifiers: Modifiers::EMPTY,
                trigger: MouseTrigger::WheelScroll,
            },
            MouseAction::Zoom,
        ),
    ]);

    let anywhere = HashMap::from([
        (
            MouseBinding {
                modifiers: m.clone(),
                trigger: MouseTrigger::Button(BTN_LEFT),
            },
            MouseAction::PanViewport,
        ),
        (
            MouseBinding {
                modifiers: m_ctrl,
                trigger: MouseTrigger::Button(BTN_LEFT),
            },
            MouseAction::CenterNearest,
        ),
        (
            MouseBinding {
                modifiers: m.clone(),
                trigger: MouseTrigger::TrackpadScroll,
            },
            MouseAction::PanViewport,
        ),
        (
            MouseBinding {
                modifiers: m,
                trigger: MouseTrigger::WheelScroll,
            },
            MouseAction::Zoom,
        ),
    ]);

    ContextBindings {
        on_window,
        on_canvas,
        anywhere,
    }
}

pub(super) fn default_gesture_bindings(
    mod_key: ModKey,
) -> ContextBindings<GestureBinding, GestureConfigEntry> {
    let m = mod_key.base();
    let alt_only = Modifiers {
        alt: true,
        ..Modifiers::EMPTY
    };

    let alt_shift = Modifiers {
        alt: true,
        shift: true,
        ..Modifiers::EMPTY
    };

    let on_window = HashMap::from([
        (
            GestureBinding {
                modifiers: alt_only.clone(),
                trigger: GestureTrigger::Swipe { fingers: 3 },
            },
            GestureConfigEntry::Continuous(ContinuousAction::ResizeWindow),
        ),
        (
            GestureBinding {
                modifiers: alt_shift,
                trigger: GestureTrigger::Swipe { fingers: 3 },
            },
            GestureConfigEntry::Continuous(ContinuousAction::ResizeWindowSnapped),
        ),
        (
            GestureBinding {
                modifiers: Modifiers::EMPTY,
                trigger: GestureTrigger::DoubletapSwipe { fingers: 3 },
            },
            GestureConfigEntry::Continuous(ContinuousAction::MoveWindow),
        ),
        (
            GestureBinding {
                modifiers: alt_only.clone(),
                trigger: GestureTrigger::PinchIn { fingers: 2 },
            },
            GestureConfigEntry::Threshold(ThresholdAction::Fixed(Action::FitWindow)),
        ),
        (
            GestureBinding {
                modifiers: alt_only.clone(),
                trigger: GestureTrigger::PinchOut { fingers: 2 },
            },
            GestureConfigEntry::Threshold(ThresholdAction::Fixed(Action::FitWindow)),
        ),
        (
            GestureBinding {
                modifiers: alt_only.clone(),
                trigger: GestureTrigger::PinchIn { fingers: 3 },
            },
            GestureConfigEntry::Threshold(ThresholdAction::Fixed(Action::ToggleFullscreen)),
        ),
        (
            GestureBinding {
                modifiers: alt_only,
                trigger: GestureTrigger::PinchOut { fingers: 3 },
            },
            GestureConfigEntry::Threshold(ThresholdAction::Fixed(Action::ToggleFullscreen)),
        ),
    ]);

    let on_canvas = HashMap::from([(
        GestureBinding {
            modifiers: Modifiers::EMPTY,
            trigger: GestureTrigger::Pinch { fingers: 2 },
        },
        GestureConfigEntry::Continuous(ContinuousAction::Zoom),
    )]);

    let anywhere = HashMap::from([
        // mod+2-finger-pinch = zoom (even over windows)
        (
            GestureBinding {
                modifiers: mod_key.base(),
                trigger: GestureTrigger::Pinch { fingers: 2 },
            },
            GestureConfigEntry::Continuous(ContinuousAction::Zoom),
        ),
        // Swipe
        (
            GestureBinding {
                modifiers: Modifiers::EMPTY,
                trigger: GestureTrigger::Swipe { fingers: 3 },
            },
            GestureConfigEntry::Continuous(ContinuousAction::PanViewport),
        ),
        (
            GestureBinding {
                modifiers: Modifiers::EMPTY,
                trigger: GestureTrigger::Swipe { fingers: 4 },
            },
            GestureConfigEntry::Threshold(ThresholdAction::CenterNearest),
        ),
        // Pinch
        (
            GestureBinding {
                modifiers: Modifiers::EMPTY,
                trigger: GestureTrigger::Pinch { fingers: 3 },
            },
            GestureConfigEntry::Continuous(ContinuousAction::Zoom),
        ),
        (
            GestureBinding {
                modifiers: Modifiers::EMPTY,
                trigger: GestureTrigger::PinchIn { fingers: 4 },
            },
            GestureConfigEntry::Threshold(ThresholdAction::Fixed(Action::ZoomToFit)),
        ),
        (
            GestureBinding {
                modifiers: Modifiers::EMPTY,
                trigger: GestureTrigger::PinchOut { fingers: 4 },
            },
            GestureConfigEntry::Threshold(ThresholdAction::Fixed(Action::HomeToggle)),
        ),
        (
            GestureBinding {
                modifiers: m.clone(),
                trigger: GestureTrigger::PinchIn { fingers: 3 },
            },
            GestureConfigEntry::Threshold(ThresholdAction::Fixed(Action::ZoomToFit)),
        ),
        (
            GestureBinding {
                modifiers: m.clone(),
                trigger: GestureTrigger::PinchOut { fingers: 3 },
            },
            GestureConfigEntry::Threshold(ThresholdAction::Fixed(Action::HomeToggle)),
        ),
        // Hold
        (
            GestureBinding {
                modifiers: Modifiers::EMPTY,
                trigger: GestureTrigger::Hold { fingers: 4 },
            },
            GestureConfigEntry::Threshold(ThresholdAction::Fixed(Action::CenterWindow)),
        ),
        (
            GestureBinding {
                modifiers: m.clone(),
                trigger: GestureTrigger::Hold { fingers: 3 },
            },
            GestureConfigEntry::Threshold(ThresholdAction::Fixed(Action::CenterWindow)),
        ),
        // Mod+swipe = navigate (same as 4-finger swipe)
        (
            GestureBinding {
                modifiers: m,
                trigger: GestureTrigger::Swipe { fingers: 3 },
            },
            GestureConfigEntry::Threshold(ThresholdAction::CenterNearest),
        ),
    ]);

    ContextBindings {
        on_window,
        on_canvas,
        anywhere,
    }
}

fn detect_terminal() -> String {
    if let Ok(term) = std::env::var("TERMINAL")
        && !term.is_empty()
    {
        return term;
    }
    for cmd in ["foot", "alacritty", "ptyxis", "kitty", "wezterm"] {
        if std::process::Command::new("which")
            .arg(cmd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return cmd.to_string();
        }
    }
    "foot".to_string()
}

fn detect_launcher() -> String {
    if let Ok(launcher) = std::env::var("LAUNCHER")
        && !launcher.is_empty()
    {
        return launcher;
    }
    for cmd in ["fuzzel", "wofi", "bemenu-run", "tofi"] {
        if std::process::Command::new("which")
            .arg(cmd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return cmd.to_string();
        }
    }
    "fuzzel".to_string()
}
