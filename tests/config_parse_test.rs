use driftwm::config::{
    Action, BindingContext, Config, ContinuousAction, Direction, GestureConfigEntry, ModKey,
    MouseAction, MouseTrigger, ThresholdAction, BTN_LEFT, BTN_RIGHT,
    parse_action, parse_direction, parse_gesture_binding, parse_gesture_config_entry,
    parse_gesture_trigger, parse_key_combo, parse_mouse_action, parse_mouse_binding,
};
use smithay::backend::input::AxisSource;
use smithay::input::keyboard::{keysyms, Keysym, ModifiersState};

// ── Modifier helpers ─────────────────────────────────────────────────────

fn mods(alt: bool, ctrl: bool, shift: bool, logo: bool) -> ModifiersState {
    ModifiersState {
        alt,
        ctrl,
        shift,
        logo,
        ..ModifiersState::default()
    }
}

fn logo() -> ModifiersState {
    mods(false, false, false, true)
}

// ── parse_key_combo ───────────────────────────────────────────────────────

#[test]
fn parse_key_combo_mod_expands_to_logo_with_super() {
    let combo = parse_key_combo("Mod+Return", ModKey::Super).unwrap();
    assert!(combo.modifiers.logo, "Mod should expand to logo with ModKey::Super");
    assert!(!combo.modifiers.alt);
    assert_eq!(combo.sym, Keysym::from(keysyms::KEY_Return));
}

#[test]
fn parse_key_combo_mod_expands_to_alt_with_alt_modkey() {
    let combo = parse_key_combo("Mod+Return", ModKey::Alt).unwrap();
    assert!(combo.modifiers.alt, "Mod should expand to alt with ModKey::Alt");
    assert!(!combo.modifiers.logo);
    assert_eq!(combo.sym, Keysym::from(keysyms::KEY_Return));
}

#[test]
fn parse_key_combo_literal_alt_is_always_alt() {
    let combo = parse_key_combo("Alt+Tab", ModKey::Super).unwrap();
    assert!(combo.modifiers.alt, "literal Alt should set alt regardless of mod_key");
    assert!(!combo.modifiers.logo);
    assert_eq!(combo.sym, Keysym::from(keysyms::KEY_Tab));
}

#[test]
fn parse_key_combo_ctrl_shift_combination() {
    let combo = parse_key_combo("Ctrl+Shift+a", ModKey::Super).unwrap();
    assert!(combo.modifiers.ctrl);
    assert!(combo.modifiers.shift);
    assert!(!combo.modifiers.logo);
    assert!(!combo.modifiers.alt);
    assert_eq!(combo.sym, Keysym::from(keysyms::KEY_a));
}

#[test]
fn parse_key_combo_keysym_is_case_insensitive() {
    let lower = parse_key_combo("Mod+Return", ModKey::Super).unwrap();
    let upper = parse_key_combo("Mod+RETURN", ModKey::Super).unwrap();
    assert_eq!(lower.sym, upper.sym, "keysym lookup should be case insensitive");
}

#[test]
fn parse_key_combo_unknown_keysym_is_error() {
    let result = parse_key_combo("Mod+nonexistent_key", ModKey::Super);
    assert!(result.is_err(), "unknown keysym should return Err");
}

#[test]
fn parse_key_combo_unknown_modifier_is_error() {
    let result = parse_key_combo("Badmod+a", ModKey::Super);
    assert!(result.is_err(), "unknown modifier should return Err");
}

// ── parse_action ──────────────────────────────────────────────────────────

#[test]
fn parse_action_exec_single_word() {
    let result = parse_action("exec foot").unwrap();
    assert!(
        matches!(result, Action::Exec(ref s) if s == "foot"),
        "exec foot should yield Exec(\"foot\")"
    );
}

#[test]
fn parse_action_exec_with_arguments() {
    let result = parse_action("exec sh -c 'echo hello'").unwrap();
    assert!(
        matches!(result, Action::Exec(ref s) if s == "sh -c 'echo hello'"),
        "exec with args should preserve entire argument string"
    );
}

#[test]
fn parse_action_close_window() {
    let result = parse_action("close-window").unwrap();
    assert!(matches!(result, Action::CloseWindow));
}

#[test]
fn parse_action_nudge_window_up() {
    let result = parse_action("nudge-window up").unwrap();
    assert!(matches!(result, Action::NudgeWindow(Direction::Up)));
}

#[test]
fn parse_action_center_nearest_down_left() {
    let result = parse_action("center-nearest down-left").unwrap();
    assert!(matches!(result, Action::CenterNearest(Direction::DownLeft)));
}

#[test]
fn parse_action_cycle_windows_forward() {
    let result = parse_action("cycle-windows forward").unwrap();
    assert!(matches!(result, Action::CycleWindows { backward: false }));
}

#[test]
fn parse_action_cycle_windows_backward() {
    let result = parse_action("cycle-windows backward").unwrap();
    assert!(matches!(result, Action::CycleWindows { backward: true }));
}

#[test]
fn parse_action_zoom_in() {
    let result = parse_action("zoom-in").unwrap();
    assert!(matches!(result, Action::ZoomIn));
}

#[test]
fn parse_action_unknown_is_error() {
    let result = parse_action("unknown-action");
    assert!(result.is_err(), "unknown action name should return Err");
}

// ── parse_mouse_binding ───────────────────────────────────────────────────

#[test]
fn parse_mouse_binding_mod_left_with_super() {
    let binding = parse_mouse_binding("Mod+Left", ModKey::Super).unwrap();
    assert!(binding.modifiers.logo);
    assert!(!binding.modifiers.shift);
    assert_eq!(binding.trigger, MouseTrigger::Button(BTN_LEFT));
}

#[test]
fn parse_mouse_binding_mod_shift_right_with_super() {
    let binding = parse_mouse_binding("Mod+Shift+Right", ModKey::Super).unwrap();
    assert!(binding.modifiers.logo);
    assert!(binding.modifiers.shift);
    assert_eq!(binding.trigger, MouseTrigger::Button(BTN_RIGHT));
}

#[test]
fn parse_mouse_binding_mod_trackpad_scroll_with_super() {
    let binding = parse_mouse_binding("Mod+trackpad-scroll", ModKey::Super).unwrap();
    assert!(binding.modifiers.logo);
    assert_eq!(binding.trigger, MouseTrigger::TrackpadScroll);
}

#[test]
fn parse_mouse_binding_mod_wheel_scroll_with_super() {
    let binding = parse_mouse_binding("Mod+wheel-scroll", ModKey::Super).unwrap();
    assert!(binding.modifiers.logo);
    assert_eq!(binding.trigger, MouseTrigger::WheelScroll);
}

#[test]
fn parse_mouse_binding_unknown_trigger_is_error() {
    let result = parse_mouse_binding("Mod+BadTrigger", ModKey::Super);
    assert!(result.is_err(), "unknown mouse trigger should return Err");
}

// ── parse_mouse_action ────────────────────────────────────────────────────

#[test]
fn parse_mouse_action_move_window() {
    let result = parse_mouse_action("move-window").unwrap();
    assert!(matches!(result, MouseAction::MoveWindow));
}

#[test]
fn parse_mouse_action_move_snapped_windows() {
    let result = parse_mouse_action("move-snapped-windows").unwrap();
    assert!(matches!(result, MouseAction::MoveSnappedWindows));
}

#[test]
fn parse_mouse_action_zoom() {
    let result = parse_mouse_action("zoom").unwrap();
    assert!(matches!(result, MouseAction::Zoom));
}

#[test]
fn parse_mouse_action_unknown_is_error() {
    let result = parse_mouse_action("bad-action");
    assert!(result.is_err(), "unknown mouse action should return Err");
}

// ── parse_direction ───────────────────────────────────────────────────────

#[test]
fn parse_direction_up() {
    assert_eq!(parse_direction("up").unwrap(), Direction::Up);
}

#[test]
fn parse_direction_down_right() {
    assert_eq!(parse_direction("down-right").unwrap(), Direction::DownRight);
}

#[test]
fn parse_direction_is_case_insensitive() {
    assert_eq!(parse_direction("UP").unwrap(), Direction::Up);
}

#[test]
fn parse_direction_unknown_is_error() {
    let result = parse_direction("diagonal");
    assert!(result.is_err(), "unknown direction should return Err");
}

// ── Default mouse bindings (context-aware) ───────────────────────────────

#[test]
fn default_mouse_bindings_move_window_on_alt_left() {
    let config = Config::default();
    let alt = mods(true, false, false, false);
    let result = config.mouse_button_lookup_ctx(&alt, BTN_LEFT, BindingContext::OnWindow);
    assert!(result.is_some(), "Alt+Left on window should be bound");
    assert!(
        matches!(result.unwrap(), MouseAction::MoveWindow),
        "Alt+Left on window should resolve to MoveWindow"
    );
}

#[test]
fn default_mouse_bindings_move_snapped_windows_on_alt_shift_left() {
    let config = Config::default();
    let alt_shift = mods(true, false, true, false);
    let result =
        config.mouse_button_lookup_ctx(&alt_shift, BTN_LEFT, BindingContext::OnWindow);
    assert!(result.is_some(), "Alt+Shift+Left on window should be bound");
    assert!(
        matches!(result.unwrap(), MouseAction::MoveSnappedWindows),
        "Alt+Shift+Left on window should resolve to MoveSnappedWindows"
    );
}

#[test]
fn default_mouse_bindings_resize_window_on_alt_right() {
    let config = Config::default();
    let alt = mods(true, false, false, false);
    let result = config.mouse_button_lookup_ctx(&alt, BTN_RIGHT, BindingContext::OnWindow);
    assert!(result.is_some(), "Alt+Right on window should be bound");
    assert!(
        matches!(result.unwrap(), MouseAction::ResizeWindow),
        "Alt+Right on window should resolve to ResizeWindow"
    );
}

#[test]
fn default_mouse_bindings_pan_viewport_on_super_left_anywhere() {
    let config = Config::default();
    let result = config.mouse_button_lookup_ctx(&logo(), BTN_LEFT, BindingContext::Anywhere);
    assert!(result.is_some(), "Super+Left anywhere should be bound");
    assert!(
        matches!(result.unwrap(), MouseAction::PanViewport),
        "Super+Left anywhere should resolve to PanViewport"
    );
}

#[test]
fn default_mouse_bindings_zoom_on_super_wheel_scroll() {
    let config = Config::default();
    let result = config.mouse_scroll_lookup_ctx(&logo(), AxisSource::Wheel, BindingContext::Anywhere);
    assert!(result.is_some(), "Super+WheelScroll should be bound");
    assert!(
        matches!(result.unwrap(), MouseAction::Zoom),
        "Super+WheelScroll should resolve to Zoom"
    );
}

#[test]
fn default_mouse_bindings_pan_on_super_trackpad_scroll() {
    let config = Config::default();
    let result = config.mouse_scroll_lookup_ctx(&logo(), AxisSource::Finger, BindingContext::Anywhere);
    assert!(result.is_some(), "Super+TrackpadScroll should be bound");
    assert!(
        matches!(result.unwrap(), MouseAction::PanViewport),
        "Super+TrackpadScroll should resolve to PanViewport"
    );
}

#[test]
fn default_mouse_bindings_empty_canvas_left_click_pans() {
    let config = Config::default();
    let result = config.mouse_button_lookup_ctx(
        &mods(false, false, false, false), BTN_LEFT, BindingContext::OnCanvas,
    );
    assert!(
        matches!(result, Some(MouseAction::PanViewport)),
        "Unmodified left click on canvas should resolve to PanViewport"
    );
}

#[test]
fn default_mouse_bindings_context_fallback_to_anywhere() {
    let config = Config::default();
    // Super+Left is defined in anywhere → should be found from on-window context too
    let result = config.mouse_button_lookup_ctx(&logo(), BTN_LEFT, BindingContext::OnWindow);
    assert!(
        matches!(result, Some(MouseAction::PanViewport)),
        "Super+Left should fall back from on-window to anywhere"
    );
}

// ── Gesture trigger parsing ──────────────────────────────────────────────

#[test]
fn parse_gesture_trigger_3_finger_swipe() {
    use driftwm::config::GestureTrigger;
    let trigger = parse_gesture_trigger("3-finger-swipe").unwrap();
    assert_eq!(trigger, GestureTrigger::Swipe { fingers: 3 });
}

#[test]
fn parse_gesture_trigger_4_finger_pinch_in() {
    use driftwm::config::GestureTrigger;
    let trigger = parse_gesture_trigger("4-finger-pinch-in").unwrap();
    assert_eq!(trigger, GestureTrigger::PinchIn { fingers: 4 });
}

#[test]
fn parse_gesture_trigger_3_finger_doubletap_swipe() {
    use driftwm::config::GestureTrigger;
    let trigger = parse_gesture_trigger("3-finger-doubletap-swipe").unwrap();
    assert_eq!(trigger, GestureTrigger::DoubletapSwipe { fingers: 3 });
}

#[test]
fn parse_gesture_trigger_4_finger_hold() {
    use driftwm::config::GestureTrigger;
    let trigger = parse_gesture_trigger("4-finger-hold").unwrap();
    assert_eq!(trigger, GestureTrigger::Hold { fingers: 4 });
}

#[test]
fn parse_gesture_trigger_invalid_finger_count() {
    assert!(parse_gesture_trigger("1-finger-swipe").is_err());
    assert!(parse_gesture_trigger("6-finger-swipe").is_err());
}

// ── Gesture binding parsing ──────────────────────────────────────────────

#[test]
fn parse_gesture_binding_with_modifier() {
    use driftwm::config::GestureTrigger;
    let binding = parse_gesture_binding("mod+3-finger-swipe", ModKey::Super).unwrap();
    assert!(binding.modifiers.logo);
    assert_eq!(binding.trigger, GestureTrigger::Swipe { fingers: 3 });
}

#[test]
fn parse_gesture_binding_without_modifier() {
    use driftwm::config::GestureTrigger;
    let binding = parse_gesture_binding("4-finger-pinch-out", ModKey::Super).unwrap();
    assert_eq!(binding.modifiers, driftwm::config::Modifiers::EMPTY);
    assert_eq!(binding.trigger, GestureTrigger::PinchOut { fingers: 4 });
}

// ── Gesture config entry validation ──────────────────────────────────────

#[test]
fn gesture_swipe_continuous_action_is_valid() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::Swipe { fingers: 3 };
    let entry = parse_gesture_config_entry(&trigger, "pan-viewport").unwrap();
    assert!(matches!(entry, GestureConfigEntry::Continuous(ContinuousAction::PanViewport)));
}

#[test]
fn gesture_swipe_threshold_action_is_valid() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::Swipe { fingers: 4 };
    let entry = parse_gesture_config_entry(&trigger, "center-nearest").unwrap();
    assert!(matches!(entry, GestureConfigEntry::Threshold(ThresholdAction::CenterNearest)));
}

#[test]
fn gesture_pinch_continuous_is_valid() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::Pinch { fingers: 3 };
    let entry = parse_gesture_config_entry(&trigger, "zoom").unwrap();
    assert!(matches!(entry, GestureConfigEntry::Continuous(ContinuousAction::Zoom)));
}

#[test]
fn gesture_pinch_threshold_is_error() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::Pinch { fingers: 3 };
    let result = parse_gesture_config_entry(&trigger, "zoom-to-fit");
    assert!(result.is_err(), "pinch + threshold action should be rejected");
    assert!(
        result.unwrap_err().contains("pinch-in or pinch-out"),
        "error message should suggest pinch-in/pinch-out"
    );
}

#[test]
fn gesture_pinch_in_threshold_is_valid() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::PinchIn { fingers: 4 };
    let entry = parse_gesture_config_entry(&trigger, "zoom-to-fit").unwrap();
    assert!(matches!(entry, GestureConfigEntry::Threshold(ThresholdAction::Fixed(_))));
}

#[test]
fn gesture_pinch_in_continuous_is_error() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::PinchIn { fingers: 4 };
    let result = parse_gesture_config_entry(&trigger, "zoom");
    assert!(result.is_err(), "pinch-in + continuous action should be rejected");
}

#[test]
fn gesture_swipe_up_continuous_is_error() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::SwipeUp { fingers: 4 };
    let result = parse_gesture_config_entry(&trigger, "pan-viewport");
    assert!(result.is_err(), "swipe-up + continuous action should be rejected");
}

#[test]
fn gesture_swipe_up_threshold_is_valid() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::SwipeUp { fingers: 4 };
    let entry = parse_gesture_config_entry(&trigger, "exec notify-send hi").unwrap();
    assert!(matches!(entry, GestureConfigEntry::Threshold(ThresholdAction::Fixed(_))));
}

#[test]
fn gesture_hold_continuous_is_error() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::Hold { fingers: 4 };
    let result = parse_gesture_config_entry(&trigger, "zoom");
    assert!(result.is_err(), "hold + continuous action should be rejected");
}

#[test]
fn gesture_hold_threshold_is_valid() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::Hold { fingers: 4 };
    let entry = parse_gesture_config_entry(&trigger, "center-window").unwrap();
    assert!(matches!(entry, GestureConfigEntry::Threshold(ThresholdAction::Fixed(_))));
}

// ── Gesture validation edge cases ─────────────────────────────────────────

#[test]
fn gesture_binding_invalid_modifier_is_error() {
    let result = parse_gesture_binding("typo+3-finger-swipe", ModKey::Super);
    assert!(result.is_err(), "unknown modifier in gesture binding should be rejected");
}

#[test]
fn gesture_zoom_on_swipe_is_error() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::Swipe { fingers: 3 };
    let result = parse_gesture_config_entry(&trigger, "zoom");
    assert!(result.is_err(), "zoom on swipe trigger should be rejected");
    assert!(
        result.unwrap_err().contains("pinch trigger"),
        "error message should mention pinch trigger"
    );
}

#[test]
fn gesture_center_nearest_on_hold_is_error() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::Hold { fingers: 4 };
    let result = parse_gesture_config_entry(&trigger, "center-nearest");
    assert!(result.is_err(), "center-nearest on hold should be rejected");
}

#[test]
fn gesture_center_nearest_on_pinch_in_is_error() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::PinchIn { fingers: 4 };
    let result = parse_gesture_config_entry(&trigger, "center-nearest");
    assert!(result.is_err(), "center-nearest on pinch-in should be rejected");
}

#[test]
fn gesture_doubletap_swipe_move_is_valid() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::DoubletapSwipe { fingers: 3 };
    let entry = parse_gesture_config_entry(&trigger, "move-window").unwrap();
    assert!(matches!(entry, GestureConfigEntry::Continuous(ContinuousAction::MoveWindow)));
}

#[test]
fn gesture_doubletap_swipe_pan_is_error() {
    use driftwm::config::GestureTrigger;
    let trigger = GestureTrigger::DoubletapSwipe { fingers: 3 };
    let result = parse_gesture_config_entry(&trigger, "pan-viewport");
    assert!(result.is_err(), "doubletap-swipe + pan-viewport should be rejected");
    assert!(
        result.unwrap_err().contains("move-window and resize-window"),
        "error message should mention supported actions"
    );
}

// ── Background paths ─────────────────────────────────────────────────────

#[test]
fn default_config_background_paths_are_none() {
    let config = Config::default();
    assert!(
        config.background.shader_path.is_none(),
        "default config should have no shader_path"
    );
    assert!(
        config.background.tile_path.is_none(),
        "default config should have no tile_path"
    );
}
