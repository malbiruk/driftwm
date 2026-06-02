use driftwm::config::{
    Action, BTN_RIGHT, BackgroundKind, BindingContext, Config, ContinuousAction,
    GestureConfigEntry, GestureTrigger, MouseAction,
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

fn ctrl() -> ModifiersState {
    mods(false, true, false, false)
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
        [input.scroll]
        speed = 2.5
        friction = 0.92

        [navigation]
        nudge_step = 50

        [zoom]
        step = 1.2
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert!((config.trackpad_speed - 2.5).abs() < f64::EPSILON);
    assert!((config.friction - 0.92).abs() < f64::EPSILON);
    assert_eq!(config.nudge_step, 50);
    assert!((config.zoom_step - 1.2).abs() < f64::EPSILON);
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
fn toml_new_navigation_fields_override_deprecated_scroll() {
    let toml = r#"
        [input.scroll]
        speed = 2.5
        friction = 0.92

        [navigation]
        trackpad_speed = 3.0
        friction = 0.96
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert!((config.trackpad_speed - 3.0).abs() < f64::EPSILON);
    assert!((config.friction - 0.96).abs() < f64::EPSILON);
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
fn toml_cycle_modifier_ctrl() {
    let config = Config::from_toml("cycle_modifier = \"ctrl\"").unwrap();
    // Cycle bindings should now use Ctrl
    let result = config.lookup(&ctrl(), Keysym::from(keysyms::KEY_Tab));
    assert!(
        matches!(result, Some(Action::CycleWindows { backward: false })),
        "cycle_modifier=ctrl should bind Ctrl+Tab"
    );
    // Alt+Tab should no longer be bound for cycling
    let result = config.lookup(&alt(), Keysym::from(keysyms::KEY_Tab));
    assert!(
        result.is_none(),
        "Alt+Tab should not be bound when cycle_modifier=ctrl"
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
