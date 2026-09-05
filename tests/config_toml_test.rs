use driftwm::config::{
    Action, BTN_RIGHT, BackgroundKind, BindingContext, Config, ContinuousAction,
    GestureConfigEntry, GestureTrigger, Modifiers, MouseAction, ThresholdAction,
};
use smithay::backend::input::AxisSource;
use smithay::input::keyboard::{Keysym, ModifiersState, keysyms};

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

fn alt() -> ModifiersState {
    mods(true, false, false, false)
}

// ── TOML round-trip integration tests ─────────────────────────────────────

#[test]
fn empty_toml_produces_defaults() {
    let config = Config::from_toml("").unwrap();
    // mod_key defaults to Super
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_q));
    assert!(
        matches!(result, Some(Action::CloseWindow)),
        "empty config should use Super as mod_key"
    );
}

#[test]
fn toml_mod_key_alt_switches_all_bindings() {
    let config = Config::from_toml("mod_key = \"alt\"").unwrap();
    // Alt+q should now work (not Super+q)
    let result = config.lookup(&alt(), Keysym::from(keysyms::KEY_q));
    assert!(
        matches!(result, Some(Action::CloseWindow)),
        "mod_key=alt should bind Alt+q to CloseWindow"
    );
    // Super+q should NOT be bound
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_q));
    assert!(
        result.is_none(),
        "Super+q should not be bound when mod_key=alt"
    );
}

#[test]
fn toml_keybinding_override() {
    let toml = r#"
        [keybindings]
        "Mod+x" = "exec alacritty"
    "#;
    let config = Config::from_toml(toml).unwrap();
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_x));
    assert!(
        matches!(result, Some(Action::Exec(s)) if s == "alacritty"),
        "user binding Mod+x should resolve to exec alacritty"
    );
    // Default bindings should still be present
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_q));
    assert!(
        matches!(result, Some(Action::CloseWindow)),
        "default Mod+q should still work after adding Mod+x"
    );
}

#[test]
fn toml_keybinding_unbind_with_none() {
    let toml = r#"
        [keybindings]
        "Mod+q" = "none"
    "#;
    let config = Config::from_toml(toml).unwrap();
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_q));
    assert!(
        result.is_none(),
        "Mod+q should be unbound after setting to none"
    );
    // Other bindings should still work
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_c));
    assert!(
        matches!(result, Some(Action::CenterWindow)),
        "Mod+c should still work after unbinding Mod+q"
    );
}

#[test]
fn toml_mouse_binding_override_anywhere() {
    let toml = r#"
        [mouse.anywhere]
        "Mod+Right" = "pan-viewport"
    "#;
    let config = Config::from_toml(toml).unwrap();
    let result = config.mouse_button_lookup_ctx(&logo(), BTN_RIGHT, BindingContext::Anywhere);
    assert!(
        matches!(result, Some(MouseAction::PanViewport)),
        "Mod+Right in anywhere should resolve to PanViewport"
    );
}

#[test]
fn toml_mouse_binding_unbind_with_none() {
    let toml = r#"
        [mouse.anywhere]
        "Mod+wheel-scroll" = "none"
    "#;
    let config = Config::from_toml(toml).unwrap();
    let result =
        config.mouse_scroll_lookup_ctx(&logo(), AxisSource::Wheel, BindingContext::Anywhere);
    assert!(
        result.is_none(),
        "Mod+wheel-scroll should be unbound after setting to none"
    );
}

#[test]
fn toml_gesture_section_parses() {
    let toml = r#"
        [gestures.anywhere]
        "4-finger-swipe" = "center-nearest"
    "#;
    let config = Config::from_toml(toml).unwrap();
    let entry = config.gesture_lookup(
        &ModifiersState::default(),
        &GestureTrigger::Swipe { fingers: 4 },
        BindingContext::Anywhere,
    );
    assert!(
        entry.is_some(),
        "4-finger-swipe should be bound in gestures.anywhere"
    );
}

#[test]
fn toml_gesture_context_priority() {
    let toml = r#"
        [gestures.on-window]
        "3-finger-swipe" = "move-window"
        [gestures.anywhere]
        "3-finger-swipe" = "pan-viewport"
    "#;
    let config = Config::from_toml(toml).unwrap();
    // on-window should override anywhere
    let entry = config.gesture_lookup(
        &ModifiersState::default(),
        &GestureTrigger::Swipe { fingers: 3 },
        BindingContext::OnWindow,
    );
    assert!(
        matches!(
            entry,
            Some(GestureConfigEntry::Continuous(ContinuousAction::MoveWindow))
        ),
        "on-window should take priority over anywhere"
    );
    // on-canvas should fall back to anywhere
    let entry = config.gesture_lookup(
        &ModifiersState::default(),
        &GestureTrigger::Swipe { fingers: 3 },
        BindingContext::OnCanvas,
    );
    assert!(
        matches!(
            entry,
            Some(GestureConfigEntry::Continuous(
                ContinuousAction::PanViewport
            ))
        ),
        "on-canvas should fall back to anywhere"
    );
}

#[test]
fn toml_touch_thresholds_default_to_the_recognizer_constants() {
    let (config, warnings) = Config::from_toml_collect("").unwrap();
    let th = &config.touch_thresholds;
    assert_eq!(th.swipe_distance_mm, 15.0);
    assert_eq!(th.pinch_in_scale, 0.85);
    assert_eq!(th.pinch_out_scale, 1.15);
    assert_eq!(th.tap_max_ms, 250);
    assert_eq!(th.double_tap_ms, 300);
    assert_eq!(th.hold_ms, 350);
    assert_eq!(th.dead_zone_mm, 2.0);
    assert!(
        warnings.is_empty(),
        "defaults must not warn, got {warnings:?}"
    );
}

#[test]
fn toml_touch_thresholds_can_be_overridden() {
    let toml = r#"
        [touch]
        swipe_threshold = 25.0
        pinch_in_threshold = 0.7
        pinch_out_threshold = 1.4
        tap_time = 180
        double_tap_time = 220
        hold_time = 500
        tap_travel = 3.5
    "#;
    let (config, warnings) = Config::from_toml_collect(toml).unwrap();
    let th = &config.touch_thresholds;
    assert_eq!(th.swipe_distance_mm, 25.0);
    assert_eq!(th.pinch_in_scale, 0.7);
    assert_eq!(th.pinch_out_scale, 1.4);
    assert_eq!(th.tap_max_ms, 180);
    assert_eq!(th.double_tap_ms, 220);
    assert_eq!(th.hold_ms, 500);
    assert_eq!(th.dead_zone_mm, 3.5);
    assert!(
        warnings.is_empty(),
        "valid values must not warn, got {warnings:?}"
    );
}

/// The whole point of the separate `[touch]` knobs: a trackpad tune must not
/// silently retune the touchscreen (their swipe units differ), and vice versa.
#[test]
fn toml_touch_thresholds_are_independent_of_gesture_thresholds() {
    let toml = r#"
        [gestures]
        swipe_threshold = 40.0
        pinch_in_threshold = 0.5
        pinch_out_threshold = 2.0
    "#;
    let config = Config::from_toml(toml).unwrap();
    let th = &config.touch_thresholds;
    assert_eq!(th.swipe_distance_mm, 15.0);
    assert_eq!(th.pinch_in_scale, 0.85);
    assert_eq!(th.pinch_out_scale, 1.15);

    let toml = r#"
        [touch]
        swipe_threshold = 40.0
        pinch_in_threshold = 0.5
        pinch_out_threshold = 2.0
    "#;
    let config = Config::from_toml(toml).unwrap();
    let gt = &config.gesture_thresholds;
    assert_eq!(gt.swipe_distance, 12.0);
    assert_eq!(gt.pinch_in_scale, 0.85);
    assert_eq!(gt.pinch_out_scale, 1.15);
}

#[test]
fn toml_touch_thresholds_reject_negatives_with_a_warning() {
    let toml = r#"
        [touch]
        pinch_in_threshold = -0.5
        tap_travel = -1.0
    "#;
    let (config, warnings) = Config::from_toml_collect(toml).unwrap();
    assert_eq!(config.touch_thresholds.pinch_in_scale, 0.0);
    assert_eq!(config.touch_thresholds.dead_zone_mm, 0.0);
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("touch.pinch_in_threshold"))
            && warnings.iter().any(|w| w.contains("touch.tap_travel")),
        "a negative value should floor at 0 with a warning, got {warnings:?}"
    );
}

/// `swipe_threshold` is a divisor, not a floored knob: the recognizer rates a
/// swipe's travel as a fraction of it and weighs that against the pinch scales,
/// so 0 would read as infinite swipe progress and starve the pinch rather than
/// hair-trigger the swipe. It falls back to the default instead.
#[test]
fn toml_non_positive_touch_swipe_threshold_falls_back_to_the_default() {
    for value in ["0.0", "-5.0"] {
        let toml = format!("[touch]\nswipe_threshold = {value}\n");
        let (config, warnings) = Config::from_toml_collect(&toml).unwrap();
        assert_eq!(config.touch_thresholds.swipe_distance_mm, 15.0);
        assert!(
            warnings.iter().any(|w| w.contains("touch.swipe_threshold")),
            "{value} should warn, got {warnings:?}"
        );
    }
}

/// The timings are integers, so a negative one must still correct-and-warn like
/// every other numeric key rather than failing the whole file to parse.
#[test]
fn toml_touch_timings_reject_negatives_with_a_warning() {
    let toml = r#"
        [touch]
        tap_time = -1
        hold_time = -200
    "#;
    let (config, warnings) = Config::from_toml_collect(toml).unwrap();
    assert_eq!(config.touch_thresholds.tap_max_ms, 0);
    assert_eq!(config.touch_thresholds.hold_ms, 0);
    assert!(
        warnings.iter().any(|w| w.contains("touch.tap_time"))
            && warnings.iter().any(|w| w.contains("touch.hold_time")),
        "negative timings should floor at 0 with a warning, got {warnings:?}"
    );
}

// ── [bindings] disable_defaults ──────────────────────────────────────────

#[test]
fn toml_disable_defaults_keys_clears_default_keybindings_only() {
    let toml = r#"
        [bindings]
        disable_defaults = ["keys"]
        [keybindings]
        "Mod+x" = "close-window"
    "#;
    let config = Config::from_toml(toml).unwrap();

    assert!(
        config
            .lookup(&logo(), Keysym::from(keysyms::KEY_q))
            .is_none(),
        "default Mod+q should be gone when keys defaults are disabled"
    );
    assert!(
        matches!(
            config.lookup(&logo(), Keysym::from(keysyms::KEY_x)),
            Some(Action::CloseWindow)
        ),
        "user-defined Mod+x should still resolve"
    );
    assert!(
        matches!(
            config.mouse_button_lookup_ctx(&alt(), BTN_RIGHT, BindingContext::OnWindow),
            Some(MouseAction::ResizeWindow)
        ),
        "mouse defaults should survive disabling keys defaults"
    );
    assert!(
        config
            .gesture_lookup(
                &ModifiersState::default(),
                &GestureTrigger::Swipe { fingers: 3 },
                BindingContext::Anywhere,
            )
            .is_some(),
        "gesture defaults should survive disabling keys defaults"
    );
}

#[test]
fn toml_disable_defaults_mouse_clears_default_mouse_bindings_only() {
    let toml = r#"
        [bindings]
        disable_defaults = ["mouse"]
    "#;
    let config = Config::from_toml(toml).unwrap();

    assert!(
        config
            .mouse_button_lookup_ctx(&alt(), BTN_RIGHT, BindingContext::OnWindow)
            .is_none(),
        "default Alt+RightClick resize should be gone when mouse defaults are disabled"
    );
    assert!(
        matches!(
            config.lookup(&logo(), Keysym::from(keysyms::KEY_q)),
            Some(Action::CloseWindow)
        ),
        "key defaults should survive disabling mouse defaults"
    );
}

#[test]
fn toml_disable_defaults_gestures_clears_default_gestures_only() {
    let toml = r#"
        [bindings]
        disable_defaults = ["gestures"]
    "#;
    let config = Config::from_toml(toml).unwrap();

    assert!(
        config
            .gesture_lookup(
                &ModifiersState::default(),
                &GestureTrigger::Swipe { fingers: 3 },
                BindingContext::Anywhere,
            )
            .is_none(),
        "default 3-finger swipe should be gone when gesture defaults are disabled"
    );
    assert!(
        matches!(
            config.lookup(&logo(), Keysym::from(keysyms::KEY_q)),
            Some(Action::CloseWindow)
        ),
        "key defaults should survive disabling gesture defaults"
    );
}

#[test]
fn toml_disable_defaults_touch_clears_default_touch_bindings_only() {
    let toml = r#"
        [bindings]
        disable_defaults = ["touch"]
        [touch.on-canvas]
        "1-finger-swipe" = "center-nearest"
    "#;
    let config = Config::from_toml(toml).unwrap();

    assert!(
        config
            .touch_lookup(
                &GestureTrigger::Pinch { fingers: 2 },
                BindingContext::OnCanvas,
            )
            .is_none(),
        "default 2-finger canvas pinch should be gone when touch defaults are disabled"
    );
    assert!(
        config
            .touch_lookup(
                &GestureTrigger::Swipe { fingers: 3 },
                BindingContext::Anywhere,
            )
            .is_none(),
        "default 3-finger touch swipe should be gone when touch defaults are disabled"
    );
    assert_eq!(
        config.touch_lookup(
            &GestureTrigger::Swipe { fingers: 1 },
            BindingContext::OnCanvas,
        ),
        Some(&GestureConfigEntry::Threshold(
            ThresholdAction::CenterNearest
        )),
        "user-defined touch binding should still resolve"
    );
    assert!(
        config
            .gesture_lookup(
                &ModifiersState::default(),
                &GestureTrigger::Swipe { fingers: 3 },
                BindingContext::Anywhere,
            )
            .is_some(),
        "trackpad gesture defaults should survive disabling touch defaults"
    );
    assert!(
        matches!(
            config.lookup(&logo(), Keysym::from(keysyms::KEY_q)),
            Some(Action::CloseWindow)
        ),
        "key defaults should survive disabling touch defaults"
    );
    assert!(
        matches!(
            config.mouse_button_lookup_ctx(&alt(), BTN_RIGHT, BindingContext::OnWindow),
            Some(MouseAction::ResizeWindow)
        ),
        "mouse defaults should survive disabling touch defaults"
    );
}

#[test]
fn toml_disable_defaults_unknown_category_warns_and_keeps_defaults() {
    let toml = r#"
        [bindings]
        disable_defaults = ["typo"]
    "#;
    let (config, warnings) = Config::from_toml_collect(toml).unwrap();

    assert!(
        warnings.iter().any(|w| w.contains("typo")),
        "an unknown disable_defaults category should produce a warning, got: {warnings:?}"
    );
    assert!(
        matches!(
            config.lookup(&logo(), Keysym::from(keysyms::KEY_q)),
            Some(Action::CloseWindow)
        ),
        "defaults should be untouched for an unknown category"
    );
}

#[test]
fn toml_old_flat_mouse_section_is_rejected() {
    let toml = r#"
        [mouse]
        "alt+left" = "move-window"
    "#;
    let result = Config::from_toml(toml);
    assert!(
        result.is_err(),
        "old flat [mouse] format should be rejected by deny_unknown_fields"
    );
}

#[test]
fn toml_scalar_overrides() {
    let toml = r#"
        [navigation]
        trackpad_speed = 2.5
        nudge_step = 50
        drift = 0.92

        [zoom]
        step = 1.2
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert!((config.trackpad_speed - 2.5).abs() < f64::EPSILON);
    assert!((config.drift - 0.92).abs() < f64::EPSILON);
    assert_eq!(config.nudge_step, 50);
    assert!((config.zoom_step - 1.2).abs() < f64::EPSILON);
}

#[test]
fn toml_navigation_friction_is_migration_error_not_fatal() {
    // `friction` was renamed to `drift`, but deny_unknown_fields would otherwise
    // make a stale value fail the whole parse — it must degrade to a migration
    // message instead.
    let toml = r#"
        [navigation]
        friction = 0.94
        nudge_step = 42
    "#;
    let (config, warnings) =
        Config::from_toml_collect(toml).expect("friction should not fail the parse");
    assert_eq!(
        config.nudge_step, 42,
        "rest of the config should still apply"
    );
    assert!(
        (config.drift - 0.5).abs() < f64::EPSILON,
        "drift falls back to default"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("friction") && w.contains("drift")),
        "expected a friction→drift migration message, got {warnings:?}"
    );
}

#[test]
fn toml_navigation_animation_speed_is_migration_error_not_fatal() {
    // `[navigation] animation_speed` was renamed to `camera_speed`, but
    // deny_unknown_fields would otherwise make a stale value fail the whole
    // parse — it must degrade to a migration message instead. The value is
    // discarded (not carried over): window effects are tuned separately.
    let toml = r#"
        [navigation]
        animation_speed = 0.8
        nudge_step = 42
    "#;
    let (config, warnings) =
        Config::from_toml_collect(toml).expect("animation_speed should not fail the parse");
    assert_eq!(
        config.nudge_step, 42,
        "rest of the config should still apply"
    );
    assert!(
        (config.camera_speed - 0.3).abs() < f64::EPSILON,
        "camera_speed falls back to default (value not carried over)"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("animation_speed") && w.contains("camera_speed")),
        "expected an animation_speed→camera_speed migration message, got {warnings:?}"
    );
}

#[test]
fn toml_snap_renamed_keys_are_migration_errors_not_fatal() {
    // `same_edge`/`edge_center` were renamed to `corners`/`centers`, but
    // deny_unknown_fields would otherwise make a stale value fail the whole
    // parse — each must degrade to a migration message instead.
    let toml = r#"
        [snap]
        same_edge = true
        edge_center = true
        gap = 20.0
    "#;
    let (config, warnings) =
        Config::from_toml_collect(toml).expect("renamed snap keys should not fail the parse");
    assert_eq!(
        config.snap_gap, 20.0,
        "rest of the config should still apply"
    );
    assert!(
        !config.snap_corners && !config.snap_centers,
        "corners/centers fall back to default (off)"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("same_edge") && w.contains("corners")),
        "expected a same_edge→corners migration message, got {warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("edge_center") && w.contains("centers")),
        "expected an edge_center→centers migration message, got {warnings:?}"
    );
}

#[test]
fn toml_zoom_reset_policies_default_true() {
    let config = Config::from_toml("").unwrap();
    assert!(config.zoom_reset_on_new_window);
    assert!(config.zoom_reset_on_activation);
}

#[test]
fn toml_zoom_reset_policies_can_be_disabled_independently() {
    let toml = r#"
        [zoom]
        reset_on_new_window = false
        reset_on_activation = true
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert!(!config.zoom_reset_on_new_window);
    assert!(config.zoom_reset_on_activation);
}

#[test]
fn toml_zoom_interact_min_defaults_off_without_warning() {
    let (config, warnings) = Config::from_toml_collect("").unwrap();
    assert_eq!(config.zoom_interact_min, 0.0);
    assert!(
        !warnings.iter().any(|w| w.contains("interact_min")),
        "the default (feature off) must parse warning-free, got {warnings:?}"
    );
}

#[test]
fn toml_zoom_interact_min_valid_value_parses_without_warning() {
    let toml = r#"
        [zoom]
        interact_min = 0.3
    "#;
    let (config, warnings) = Config::from_toml_collect(toml).unwrap();
    assert!((config.zoom_interact_min - 0.3).abs() < f64::EPSILON);
    assert!(
        !warnings.iter().any(|w| w.contains("interact_min")),
        "a valid in-range value must not warn, got {warnings:?}"
    );
}

#[test]
fn toml_zoom_interact_min_above_max_clamps_to_one_with_warning() {
    let toml = r#"
        [zoom]
        interact_min = 2.0
    "#;
    let (config, warnings) = Config::from_toml_collect(toml).unwrap();
    assert!((config.zoom_interact_min - 1.0).abs() < f64::EPSILON);
    assert!(
        warnings.iter().any(|w| w.contains("interact_min")),
        "an above-max value should clamp with a warning, got {warnings:?}"
    );
}

#[test]
fn toml_zoom_interact_min_negative_clamps_to_zero_with_warning() {
    let toml = r#"
        [zoom]
        interact_min = -1.0
    "#;
    let (config, warnings) = Config::from_toml_collect(toml).unwrap();
    assert_eq!(config.zoom_interact_min, 0.0);
    assert!(
        warnings.iter().any(|w| w.contains("interact_min")),
        "a negative value should clamp with a warning, got {warnings:?}"
    );
}

#[test]
fn toml_auto_navigate_on_close_defaults_true() {
    let config = Config::from_toml("").unwrap();
    assert!(config.auto_navigate_on_close);
}

#[test]
fn toml_auto_navigate_on_close_can_be_disabled() {
    let toml = r#"
        [navigation]
        auto_navigate_on_close = false
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert!(!config.auto_navigate_on_close);
}

#[test]
fn toml_auto_navigate_on_click_defaults_false() {
    let config = Config::from_toml("").unwrap();
    assert!(!config.auto_navigate_on_click);
}

#[test]
fn toml_auto_navigate_on_click_can_be_enabled() {
    let toml = r#"
        [navigation]
        auto_navigate_on_click = true
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert!(config.auto_navigate_on_click);
}

#[test]
fn toml_resize_on_border_defaults_true() {
    let config = Config::from_toml("").unwrap();
    assert!(config.resize_on_border);
}

#[test]
fn toml_resize_on_border_can_be_disabled() {
    let toml = r#"
        [mouse]
        resize_on_border = false
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert!(!config.resize_on_border);
}

#[test]
fn toml_invalid_keybinding_is_skipped() {
    let toml = r#"
        [keybindings]
        "Mod+nonexistent_key_xyz" = "close-window"
        "Mod+c" = "center-window"
    "#;
    let config = Config::from_toml(toml).unwrap();
    // Valid binding should still work
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_c));
    assert!(matches!(result, Some(Action::CenterWindow)));
}

#[test]
fn toml_invalid_action_is_skipped() {
    let toml = r#"
        [keybindings]
        "Mod+y" = "not-a-real-action"
        "Mod+c" = "center-window"
    "#;
    let config = Config::from_toml(toml).unwrap();
    // The invalid action binding should be skipped
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_y));
    assert!(result.is_none());
    // Valid binding should still work
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_c));
    assert!(matches!(result, Some(Action::CenterWindow)));
}

#[test]
fn toml_deny_unknown_fields() {
    let toml = "typo_field = \"oops\"";
    let result = Config::from_toml(toml);
    assert!(
        result.is_err(),
        "unknown top-level field should be rejected"
    );
}

#[test]
fn cycle_hold_modifier_follows_forward_binding() {
    // Default Alt+Tab cycling → the hold modifier (released to commit) is Alt.
    let config = Config::from_toml("").unwrap();
    assert_eq!(
        config.cycle_hold,
        Modifiers {
            alt: true,
            ..Modifiers::EMPTY
        }
    );

    // Rebinding cycle-windows forward moves the hold modifier with it — any
    // modifier works now, not just alt/ctrl. (Unbind the default so there's a
    // single forward binding.)
    let toml = r#"
        [keybindings]
        "alt+tab" = "none"
        "super+grave" = "cycle-windows forward"
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert_eq!(
        config.cycle_hold,
        Modifiers {
            logo: true,
            ..Modifiers::EMPTY
        }
    );
}

#[test]
fn toml_background_new_form_wallpaper() {
    let toml = r#"
        [background]
        type = "wallpaper"
        path = "~/Pictures/wp.png"
    "#;
    let config = Config::from_toml(toml).unwrap();
    match config.background.kind {
        BackgroundKind::Wallpaper(path) => {
            assert!(!path.starts_with("~"), "tilde should be expanded");
            assert!(path.ends_with("/Pictures/wp.png"));
        }
        other => panic!("expected BackgroundKind::Wallpaper, got {other:?}"),
    }
}

#[test]
fn toml_background_unknown_type_falls_back_to_default() {
    let toml = r#"
        [background]
        type = "video"
        path = "~/v.mp4"
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert_eq!(config.background.kind, BackgroundKind::Default);
}

#[test]
fn toml_background_new_form_shader() {
    let toml = r#"
        [background]
        type = "shader"
        path = "~/shaders/my.glsl"
    "#;
    let config = Config::from_toml(toml).unwrap();
    match config.background.kind {
        BackgroundKind::Shader { path, texture } => {
            assert!(!path.starts_with("~"), "tilde should be expanded");
            assert!(path.ends_with("/shaders/my.glsl"));
            assert_eq!(texture, None);
        }
        other => panic!("expected BackgroundKind::Shader, got {other:?}"),
    }
}

#[test]
fn toml_background_shader_with_texture() {
    let toml = r#"
        [background]
        type = "shader"
        path = "~/shaders/my.glsl"
        texture = "~/Pictures/tex.png"
    "#;
    let config = Config::from_toml(toml).unwrap();
    match config.background.kind {
        BackgroundKind::Shader { path, texture } => {
            assert!(path.ends_with("/shaders/my.glsl"));
            let texture = texture.expect("texture should be set");
            assert!(!texture.starts_with("~"), "tilde should be expanded");
            assert!(texture.ends_with("/Pictures/tex.png"));
        }
        other => panic!("expected BackgroundKind::Shader, got {other:?}"),
    }
}

#[test]
fn toml_background_transparent_shader_parses() {
    let toml = r#"
        [background]
        type = "shader"
        path = "~/shaders/my.glsl"
        transparent_shader = true
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert!(config.background.transparent_shader);
}

#[test]
fn toml_background_transparent_shader_defaults_false() {
    let toml = r#"
        [background]
        type = "shader"
        path = "~/shaders/my.glsl"
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert!(!config.background.transparent_shader);
}

#[test]
fn toml_background_new_form_tile() {
    let toml = r#"
        [background]
        type = "tile"
        path = "~/Pictures/tile.png"
    "#;
    let config = Config::from_toml(toml).unwrap();
    match config.background.kind {
        BackgroundKind::Tile(path) => {
            assert!(!path.starts_with("~"), "tilde should be expanded");
            assert!(path.ends_with("/Pictures/tile.png"));
        }
        other => panic!("expected BackgroundKind::Tile, got {other:?}"),
    }
}

#[test]
fn toml_background_type_without_path_falls_back_to_default() {
    let toml = r#"
        [background]
        type = "wallpaper"
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert_eq!(config.background.kind, BackgroundKind::Default);
}

#[test]
fn toml_background_none() {
    let toml = r#"
        [background]
        type = "none"
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert_eq!(config.background.kind, BackgroundKind::None);
}

#[test]
fn toml_background_none_ignores_path() {
    let toml = r#"
        [background]
        type = "none"
        path = "~/Pictures/ignored.png"
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert_eq!(config.background.kind, BackgroundKind::None);
}

#[test]
fn toml_gesture_anywhere_only_not_on_window() {
    let toml = r#"
        [gestures.on-window]
        "3-finger-swipe" = "move-window"
        [gestures.anywhere]
        "3-finger-swipe" = "pan-viewport"
    "#;
    let config = Config::from_toml(toml).unwrap();
    // Query with Anywhere context — should return the anywhere binding, not on-window
    let entry = config.gesture_lookup(
        &ModifiersState::default(),
        &GestureTrigger::Swipe { fingers: 3 },
        BindingContext::Anywhere,
    );
    assert!(
        matches!(
            entry,
            Some(GestureConfigEntry::Continuous(
                ContinuousAction::PanViewport
            ))
        ),
        "Anywhere context should return the anywhere binding, not on-window"
    );
}

#[test]
fn toml_decoration_opacity_defaults_to_opaque_with_blur_off() {
    let config = Config::from_toml("").unwrap();
    assert_eq!(config.decorations.opacity, 1.0);
    assert_eq!(config.decorations.opacity_focused, 1.0);
    assert!(!config.decorations.blur);
}

#[test]
fn toml_decoration_opacity_and_blur_round_trip() {
    let toml = r#"
        [decorations]
        opacity = 0.6
        opacity_focused = 0.9
        blur = true
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert_eq!(config.decorations.opacity, 0.6);
    assert_eq!(config.decorations.opacity_focused, 0.9);
    assert!(config.decorations.blur);
}

#[test]
fn toml_decoration_opacity_above_one_clamps_with_a_warning() {
    let toml = r#"
        [decorations]
        opacity = 1.5
    "#;
    let (config, warnings) = Config::from_toml_collect(toml).unwrap();
    assert_eq!(config.decorations.opacity, 1.0);
    assert!(
        warnings.iter().any(|w| w.contains("decorations.opacity")),
        "an out-of-range opacity should clamp with a warning, got {warnings:?}"
    );
}
