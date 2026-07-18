use driftwm::config::{
    AppliedWindowRule, Config, DecorationMode, PassKeys, Pattern, WindowRule, glob_matches,
};

fn bare_rule(app_id: Option<&str>, title: Option<&str>) -> WindowRule {
    WindowRule {
        app_id: app_id.map(|s| Pattern::Glob(s.to_string())),
        title: title.map(|s| Pattern::Glob(s.to_string())),
        position: None,
        size: None,
        fullscreen: None,
        widget: false,
        pinned_to_screen: false,
        decoration: None,
        blur: false,
        opacity: None,
        pass_keys: PassKeys::None,
        border_width: None,
        border_color: None,
        border_color_focused: None,
        corner_radius: None,
        shadow: None,
        output: None,
        layer_order: None,
    }
}

// ── glob_matches ─────────────────────────────────────────────────────────────

#[test]
fn glob_exact_match_succeeds() {
    assert!(glob_matches("firefox", "firefox"));
}

#[test]
fn glob_exact_match_does_not_match_longer_string() {
    assert!(!glob_matches("firefox", "firefox-nightly"));
}

#[test]
fn glob_star_at_end_matches_prefix() {
    assert!(glob_matches("fire*", "firefox"));
    assert!(glob_matches("fire*", "fire"));
}

#[test]
fn glob_star_at_start_matches_suffix() {
    assert!(glob_matches("*fox", "firefox"));
    assert!(glob_matches("*fox", "fox"));
}

#[test]
fn glob_star_only_matches_anything_including_empty() {
    assert!(glob_matches("*", "firefox"));
    assert!(glob_matches("*", ""));
}

#[test]
fn glob_multiple_wildcards_match_correctly() {
    assert!(glob_matches("*-*", "alacritty-debug"));
    assert!(!glob_matches("*-*", "alacritty"));
}

#[test]
fn glob_empty_val_only_matches_star() {
    assert!(glob_matches("*", ""));
    assert!(!glob_matches("x", ""));
}

#[test]
fn glob_is_case_sensitive() {
    assert!(!glob_matches("Firefox", "firefox"));
    assert!(!glob_matches("firefox", "Firefox"));
}

// ── Pattern::matches ─────────────────────────────────────────────────────────

#[test]
fn pattern_glob_matches_via_glob_rules() {
    let p = Pattern::Glob("foot*".to_string());
    assert!(p.matches("footclient"));
    assert!(!p.matches("alacritty"));
}

#[test]
fn pattern_regex_matches_via_regex_rules() {
    let p = Pattern::Regex(regex::Regex::new(r"^foo\d+$").unwrap());
    assert!(p.matches("foo42"));
    assert!(!p.matches("foo"));
    assert!(!p.matches("foobar"));
}

// ── WindowRule::matches / has_criteria ───────────────────────────────────────

#[test]
fn window_rule_matches_both_app_id_and_title_must_both_match() {
    let rule = WindowRule {
        app_id: Some(Pattern::Glob("firefox".to_string())),
        title: Some(Pattern::Glob("GitHub*".to_string())),
        ..bare_rule(None, None)
    };
    assert!(rule.matches("firefox", "GitHub — Mozilla Firefox"));
    assert!(!rule.matches("chromium", "GitHub — Mozilla Firefox"));
    assert!(!rule.matches("firefox", "Google"));
}

#[test]
fn window_rule_matches_only_app_id_title_is_wildcard() {
    let rule = bare_rule(Some("foot"), None);
    assert!(rule.matches("foot", "any title"));
    assert!(rule.matches("foot", ""));
    assert!(!rule.matches("alacritty", "any title"));
}

#[test]
fn window_rule_matches_only_title_app_id_is_wildcard() {
    let rule = bare_rule(None, Some("*term*"));
    assert!(rule.matches("any-app", "terminal"));
    assert!(!rule.matches("any-app", "browser"));
}

#[test]
fn window_rule_has_criteria_false_when_both_none() {
    let rule = bare_rule(None, None);
    assert!(!rule.has_criteria());
}

#[test]
fn window_rule_has_criteria_true_when_app_id_set() {
    let rule = bare_rule(Some("foot"), None);
    assert!(rule.has_criteria());
}

#[test]
fn window_rule_has_criteria_true_when_title_set() {
    let rule = bare_rule(None, Some("*"));
    assert!(rule.has_criteria());
}

// ── PassKeys::merge_from ─────────────────────────────────────────────────────

#[test]
fn pass_keys_none_plus_none_stays_none() {
    let mut base = PassKeys::None;
    base.merge_from(&PassKeys::None);
    assert!(matches!(base, PassKeys::None));
}

#[test]
fn pass_keys_none_plus_all_becomes_all() {
    let mut base = PassKeys::None;
    base.merge_from(&PassKeys::All);
    assert!(matches!(base, PassKeys::All));
}

#[test]
fn pass_keys_all_is_sticky_against_none() {
    let mut base = PassKeys::All;
    base.merge_from(&PassKeys::None);
    assert!(matches!(base, PassKeys::All));
}

#[test]
fn pass_keys_all_is_sticky_against_only() {
    let combo = smithay::input::keyboard::Keysym::from(smithay::input::keyboard::keysyms::KEY_q);
    let mut base = PassKeys::All;
    base.merge_from(&PassKeys::Only(vec![driftwm::config::KeyCombo {
        modifiers: driftwm::config::Modifiers::EMPTY,
        sym: combo,
    }]));
    assert!(matches!(base, PassKeys::All));
}

#[test]
fn pass_keys_none_plus_only_becomes_only() {
    let combo = smithay::input::keyboard::Keysym::from(smithay::input::keyboard::keysyms::KEY_q);
    let c = driftwm::config::KeyCombo {
        modifiers: driftwm::config::Modifiers::EMPTY,
        sym: combo,
    };
    let mut base = PassKeys::None;
    base.merge_from(&PassKeys::Only(vec![c]));
    assert!(matches!(base, PassKeys::Only(ref v) if v.len() == 1));
}

#[test]
fn pass_keys_only_union_deduplicates() {
    use smithay::input::keyboard::keysyms;
    let mk = |raw| driftwm::config::KeyCombo {
        modifiers: driftwm::config::Modifiers::EMPTY,
        sym: smithay::input::keyboard::Keysym::from(raw),
    };
    let a = mk(keysyms::KEY_a);
    let b = mk(keysyms::KEY_b);
    let c = mk(keysyms::KEY_c);

    let mut base = PassKeys::Only(vec![a.clone(), b.clone()]);
    base.merge_from(&PassKeys::Only(vec![b.clone(), c.clone()]));

    match base {
        PassKeys::Only(v) => assert_eq!(v.len(), 3, "expected [a,b,c], got len {}", v.len()),
        other => panic!("expected Only, got {other:?}"),
    }
}

#[test]
fn pass_keys_only_plus_none_unchanged() {
    let combo = smithay::input::keyboard::Keysym::from(smithay::input::keyboard::keysyms::KEY_q);
    let c = driftwm::config::KeyCombo {
        modifiers: driftwm::config::Modifiers::EMPTY,
        sym: combo,
    };
    let mut base = PassKeys::Only(vec![c]);
    base.merge_from(&PassKeys::None);
    assert!(matches!(base, PassKeys::Only(ref v) if v.len() == 1));
}

#[test]
fn pass_keys_only_plus_all_upgrades_to_all() {
    let combo = smithay::input::keyboard::Keysym::from(smithay::input::keyboard::keysyms::KEY_q);
    let c = driftwm::config::KeyCombo {
        modifiers: driftwm::config::Modifiers::EMPTY,
        sym: combo,
    };
    let mut base = PassKeys::Only(vec![c]);
    base.merge_from(&PassKeys::All);
    assert!(matches!(base, PassKeys::All));
}

// ── AppliedWindowRule::merge_from ─────────────────────────────────────────────

fn applied_from_toml_rule(r: &WindowRule) -> AppliedWindowRule {
    AppliedWindowRule::from(r)
}

#[test]
fn applied_rule_bool_flags_are_sticky_on() {
    let rule_blur = WindowRule {
        blur: true,
        ..bare_rule(Some("x"), None)
    };
    let rule_no_blur = WindowRule {
        blur: false,
        ..bare_rule(Some("x"), None)
    };

    let mut applied = applied_from_toml_rule(&rule_blur);
    assert!(applied.blur);
    applied.merge_from(&rule_no_blur);
    // sticky-on: blur stays true even when second rule sets it false
    assert!(applied.blur);
}

#[test]
fn applied_rule_scalar_decoration_last_wins() {
    let rule1 = WindowRule {
        decoration: Some(DecorationMode::Server),
        ..bare_rule(Some("x"), None)
    };
    let rule2 = WindowRule {
        decoration: Some(DecorationMode::None),
        ..bare_rule(Some("x"), None)
    };

    let mut applied = applied_from_toml_rule(&rule1);
    assert_eq!(applied.decoration, Some(DecorationMode::Server));
    applied.merge_from(&rule2);
    assert_eq!(applied.decoration, Some(DecorationMode::None));
}

#[test]
fn applied_rule_scalar_not_cleared_by_none() {
    let rule1 = WindowRule {
        opacity: Some(0.7),
        ..bare_rule(Some("x"), None)
    };
    let rule2 = WindowRule {
        opacity: None,
        ..bare_rule(Some("x"), None)
    };

    let mut applied = applied_from_toml_rule(&rule1);
    applied.merge_from(&rule2);
    assert_eq!(applied.opacity, Some(0.7));
}

#[test]
fn applied_rule_position_last_wins() {
    let rule1 = WindowRule {
        position: Some((10, 20)),
        ..bare_rule(Some("x"), None)
    };
    let rule2 = WindowRule {
        position: Some((30, 40)),
        ..bare_rule(Some("x"), None)
    };

    let mut applied = applied_from_toml_rule(&rule1);
    applied.merge_from(&rule2);
    assert_eq!(applied.position, Some((30, 40)));
}

// ── From<&WindowRule> for AppliedWindowRule ──────────────────────────────────

#[test]
fn from_window_rule_copies_all_scalar_fields() {
    let rule = WindowRule {
        widget: true,
        blur: true,
        opacity: Some(0.5),
        position: Some((1, 2)),
        size: Some((800, 600)),
        decoration: Some(DecorationMode::Minimal),
        pass_keys: PassKeys::All,
        ..bare_rule(Some("x"), None)
    };
    let applied = AppliedWindowRule::from(&rule);
    assert!(applied.widget);
    assert!(applied.blur);
    assert_eq!(applied.opacity, Some(0.5));
    assert_eq!(applied.position, Some((1, 2)));
    assert_eq!(applied.size, Some((800, 600)));
    assert_eq!(applied.decoration, Some(DecorationMode::Minimal));
    assert!(matches!(applied.pass_keys, PassKeys::All));
}

// ── Config::resolve_window_rules ─────────────────────────────────────────────

#[test]
fn resolve_window_rules_no_rules_returns_none() {
    let config = Config::from_toml("").unwrap();
    assert!(config.resolve_window_rules("firefox", "title").is_none());
}

#[test]
fn resolve_window_rules_single_matching_rule_returns_applied() {
    let toml = r#"
        [[window_rules]]
        app_id = "foot"
        blur = true
    "#;
    let config = Config::from_toml(toml).unwrap();
    let applied = config.resolve_window_rules("foot", "shell").unwrap();
    assert!(applied.blur);
}

#[test]
fn resolve_window_rules_no_matching_rule_returns_none() {
    let toml = r#"
        [[window_rules]]
        app_id = "foot"
        blur = true
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert!(config.resolve_window_rules("firefox", "title").is_none());
}

#[test]
fn resolve_window_rules_layer_order_parses_and_merges_last_wins() {
    let toml = r#"
        [[window_rules]]
        app_id = "*"
        layer_order = 5

        [[window_rules]]
        app_id = "wvkbd"
        layer_order = 10
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert_eq!(
        config
            .resolve_window_rules("wvkbd", "")
            .unwrap()
            .layer_order,
        Some(10)
    );
    assert_eq!(
        config
            .resolve_window_rules("touchview", "")
            .unwrap()
            .layer_order,
        Some(5)
    );
}

#[test]
fn resolve_window_rules_layer_order_defaults_to_none() {
    let toml = r#"
        [[window_rules]]
        app_id = "wvkbd"
        blur = true
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert_eq!(
        config
            .resolve_window_rules("wvkbd", "")
            .unwrap()
            .layer_order,
        None
    );
}

#[test]
fn resolve_window_rules_two_matching_rules_merge_in_order() {
    // Rule 1 (wildcard): sets blur=true, opacity=0.5
    // Rule 2 (specific):  sets opacity=0.8 (last-wins), widget stays false
    let toml = r#"
        [[window_rules]]
        app_id = "*"
        blur = true
        opacity = 0.5

        [[window_rules]]
        app_id = "foot"
        opacity = 0.8
    "#;
    let config = Config::from_toml(toml).unwrap();
    let applied = config.resolve_window_rules("foot", "shell").unwrap();
    // blur from first rule is sticky-on
    assert!(applied.blur);
    // opacity from second rule wins
    assert!((applied.opacity.unwrap() - 0.8).abs() < f64::EPSILON);
}

#[test]
fn resolve_window_rules_wildcard_rule_matches_all_apps() {
    let toml = r#"
        [[window_rules]]
        app_id = "*"
        decoration = "server"
    "#;
    let config = Config::from_toml(toml).unwrap();
    let a = config.resolve_window_rules("firefox", "title").unwrap();
    let b = config.resolve_window_rules("alacritty", "term").unwrap();
    assert_eq!(a.decoration, Some(DecorationMode::Server));
    assert_eq!(b.decoration, Some(DecorationMode::Server));
}

// ── pinned_to_screen ─────────────────────────────────────────────────────────

#[test]
fn pinned_to_screen_defaults_to_false() {
    let rule = bare_rule(Some("foot"), None);
    assert!(!rule.pinned_to_screen);
}

#[test]
fn applied_rule_pinned_to_screen_defaults_to_false() {
    let rule = bare_rule(Some("foot"), None);
    let applied = AppliedWindowRule::from(&rule);
    assert!(!applied.pinned_to_screen);
}

#[test]
fn from_window_rule_copies_pinned_to_screen_true() {
    let rule = WindowRule {
        pinned_to_screen: true,
        ..bare_rule(Some("foot"), None)
    };
    let applied = AppliedWindowRule::from(&rule);
    assert!(applied.pinned_to_screen);
}

#[test]
fn pinned_to_screen_is_sticky_on_in_merge_from() {
    let rule_pinned = WindowRule {
        pinned_to_screen: true,
        ..bare_rule(Some("x"), None)
    };
    let rule_not_pinned = WindowRule {
        pinned_to_screen: false,
        ..bare_rule(Some("x"), None)
    };

    let mut applied = AppliedWindowRule::from(&rule_pinned);
    assert!(applied.pinned_to_screen);
    applied.merge_from(&rule_not_pinned);
    assert!(
        applied.pinned_to_screen,
        "sticky-on: false rule must not clear pinned_to_screen"
    );
}

#[test]
fn pinned_to_screen_false_then_true_flips_on() {
    let rule_not_pinned = WindowRule {
        pinned_to_screen: false,
        ..bare_rule(Some("x"), None)
    };
    let rule_pinned = WindowRule {
        pinned_to_screen: true,
        ..bare_rule(Some("x"), None)
    };

    let mut applied = AppliedWindowRule::from(&rule_not_pinned);
    assert!(!applied.pinned_to_screen);
    applied.merge_from(&rule_pinned);
    assert!(applied.pinned_to_screen);
}

#[test]
fn pinned_to_screen_parses_from_toml() {
    let toml = r#"
        [[window_rules]]
        app_id = "myapp"
        pinned_to_screen = true
    "#;
    let config = Config::from_toml(toml).unwrap();
    let applied = config.resolve_window_rules("myapp", "title").unwrap();
    assert!(applied.pinned_to_screen);
}

#[test]
fn pinned_to_screen_omitted_in_toml_defaults_false() {
    let toml = r#"
        [[window_rules]]
        app_id = "myapp"
        blur = true
    "#;
    let config = Config::from_toml(toml).unwrap();
    let applied = config.resolve_window_rules("myapp", "title").unwrap();
    assert!(!applied.pinned_to_screen);
}

#[test]
fn pinned_to_screen_sticky_across_two_toml_rules() {
    // First rule (wildcard) pins; second rule (specific) does not — should stay pinned.
    let toml = r#"
        [[window_rules]]
        app_id = "*"
        pinned_to_screen = true

        [[window_rules]]
        app_id = "myapp"
        opacity = 0.9
    "#;
    let config = Config::from_toml(toml).unwrap();
    let applied = config.resolve_window_rules("myapp", "title").unwrap();
    assert!(applied.pinned_to_screen);
}

#[test]
fn output_parses_from_toml() {
    let toml = r#"
        [[window_rules]]
        app_id = "myapp"
        output = "DP-1"
    "#;
    let config = Config::from_toml(toml).unwrap();
    let applied = config.resolve_window_rules("myapp", "title").unwrap();
    assert_eq!(applied.output.as_deref(), Some("DP-1"));
}

#[test]
fn output_omitted_in_toml_defaults_none() {
    let toml = r#"
        [[window_rules]]
        app_id = "myapp"
        blur = true
    "#;
    let config = Config::from_toml(toml).unwrap();
    let applied = config.resolve_window_rules("myapp", "title").unwrap();
    assert_eq!(applied.output, None);
}

#[test]
fn output_last_wins_across_two_toml_rules() {
    let toml = r#"
        [[window_rules]]
        app_id = "*"
        output = "DP-1"

        [[window_rules]]
        app_id = "myapp"
        output = "HDMI-A-1"
    "#;
    let config = Config::from_toml(toml).unwrap();
    let applied = config.resolve_window_rules("myapp", "title").unwrap();
    assert_eq!(applied.output.as_deref(), Some("HDMI-A-1"));
}
