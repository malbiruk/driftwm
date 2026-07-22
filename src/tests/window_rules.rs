//! Window-rule *application* wiring on first commit. The matching/merge
//! engine has its own unit suite; these drive a real client through the
//! commit path and assert the applied effect server-side or configure-side.

use super::{
    Fixture, config, is_activated, keyboard_focus, map_window, server_surface, window_by_app_id,
};

#[test]
fn widget_rule_does_not_take_focus() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "widget"
widget = true
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "normal", (400, 300));
    let normal = window_by_app_id(&mut f, "normal").unwrap();
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&normal)));

    map_window(&mut f, id, "widget", (200, 100));
    let widget = window_by_app_id(&mut f, "widget").unwrap();

    // Mapping a widget must neither steal keyboard focus nor enter the MRU.
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&normal)));
    assert!(!f.state().stage.focus_history().iter().any(|w| w == &widget));
}

#[test]
fn pinned_rule_pins_to_output_screen_space() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [320, 240]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "normal", (400, 300));
    map_window(&mut f, id, "pin", (320, 240));
    let window = window_by_app_id(&mut f, "pin").unwrap();

    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&window)));
    let site = f.state().stage.pin_of(&window).cloned().unwrap();
    assert_eq!(site.output, "HEADLESS-1");
    // No rule `position` means output center: screen top-left =
    // (1920/2 - 320/2, 1080/2 - 240/2).
    assert_eq!(site.screen_pos, smithay::utils::Point::from((800, 420)));
}

/// A screen-pinned window reports its `position`/`size` in the `state` reply in
/// rule coordinates — the same numbers the rule set, so users copy them straight
/// back into a `pinned_to_screen` rule.
#[test]
fn pinned_inventory_reports_rule_coords() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
position = [200, -150]
size = [320, 240]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "pin", (320, 240));

    let (_fullscreen, pinned) = f.state().screen_space_inventory();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].output, "HEADLESS-1");
    assert_eq!(pinned[0].position, [200, -150]);
    assert_eq!(pinned[0].size, [320, 240]);
}

/// A `pinned_to_screen` rule with an `output` pins to that display initially,
/// not the active output; the rule `position` resolves against the chosen one.
#[test]
fn pinned_rule_output_chooses_display() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
output = "HEADLESS-2"
size = [320, 240]
"#,
    ));
    // HEADLESS-1 (first-added) is the active output; the rule targets HEADLESS-2.
    let _out1 = f.add_output(1, (1920, 1080));
    let _out2 = f.add_output(2, (1280, 720));
    let id = f.add_client();

    map_window(&mut f, id, "pin", (320, 240));
    let window = window_by_app_id(&mut f, "pin").unwrap();

    let (_fullscreen, pinned) = f.state().screen_space_inventory();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].output, "HEADLESS-2");
    // Center resolves against the chosen 1280×720 output, not the 1920×1080
    // active one: (1280/2 - 320/2, 720/2 - 240/2).
    let site = f.state().stage.pin_of(&window).cloned().unwrap();
    assert_eq!(site.screen_pos, smithay::utils::Point::from((480, 240)));
}

/// A `pinned_to_screen` rule naming a disconnected output falls back to the
/// active output.
#[test]
fn pinned_rule_unknown_output_falls_back_to_active() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
output = "DOES-NOT-EXIST"
size = [320, 240]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let _out2 = f.add_output(2, (1280, 720));
    let id = f.add_client();

    map_window(&mut f, id, "pin", (320, 240));

    let (_fullscreen, pinned) = f.state().screen_space_inventory();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].output, "HEADLESS-1");
}

#[test]
fn multiple_matching_rules_merge() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "merge*"
size = [500, 400]

[[window_rules]]
title = "target"
position = [0, 0]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id("merge-1");
    window.set_title("target");
    window.commit();
    f.roundtrip(id);

    // The size rule shows up in the initial configure burst...
    let window = f.client(id).window(&surface);
    let configures = window.format_recent_configures();
    assert!(
        configures.contains("size: 500 × 400"),
        "size rule missing from initial configures:\n{configures}"
    );

    window.set_size(500, 400);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    // ...and the position rule from the second matching rule lands on map:
    // rule (0, 0) is the window center, so top-left = (-250, -200).
    let mapped = window_by_app_id(&mut f, "merge-1").unwrap();
    let pos = f.state().stage.position_of(&mapped).unwrap();
    assert_eq!(pos, smithay::utils::Point::from((-250, -200)));
}

#[test]
fn output_rule_directs_fullscreen() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "fs"
output = "HEADLESS-2"
"#,
    ));
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));
    let id = f.add_client();

    let client_surface = map_window(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();

    // Client requests fullscreen with no output; the rule must win.
    let cw = f.client(id).window(&client_surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);

    assert_eq!(
        f.state().stage.fullscreen_output_of(&window),
        Some("HEADLESS-2")
    );
    let cw = f.client(id).window(&client_surface);
    let configures = cw.format_recent_configures();
    assert!(
        configures.contains("size: 1280 × 720") && configures.contains("Fullscreen"),
        "expected a HEADLESS-2-sized fullscreen configure, got:\n{configures}"
    );
}

#[test]
fn focus_on_open_false_maps_without_focus_or_navigation() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "nofocus"
focus_on_open = false
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "normal", (400, 300));
    let normal = window_by_app_id(&mut f, "normal").unwrap();
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&normal)));
    assert!(
        f.state().camera_target().is_some(),
        "the default map-time behavior navigates the camera"
    );
    f.state().with_output_state(|os| os.camera_target = None);

    map_window(&mut f, id, "nofocus", (200, 100));
    let nofocus = window_by_app_id(&mut f, "nofocus").unwrap();

    // Mapping the suppressed window must neither steal keyboard focus nor
    // move the camera.
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&normal)));
    assert!(f.state().camera_target().is_none());

    // ...and it must not steal the xdg Activated chrome either: the window that
    // keeps keyboard focus stays activated, the suppressed one is not.
    assert!(is_activated(&normal));
    assert!(!is_activated(&nofocus));

    // A normal focus path (here: the same raise_and_focus click-to-focus and
    // hover-focus funnel into) still reaches it afterwards.
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&nofocus, serial);
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&nofocus)));
}

/// The very first window mapped is suppressed: there is no prior focus holder
/// for the activation correction to fall back to, so it must clear its own
/// Activated state rather than panic or leave the map-sequence configure
/// advertising a stale Activated.
#[test]
fn focus_on_open_false_as_first_window_takes_no_focus_or_activation() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "nofocus"
focus_on_open = false
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "nofocus", (200, 100));
    let nofocus = window_by_app_id(&mut f, "nofocus").unwrap();

    assert_eq!(keyboard_focus(&mut f), None);
    assert!(!is_activated(&nofocus));
}

#[test]
fn focus_on_open_true_focuses_and_navigates_like_default() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "explicit"
focus_on_open = true
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "explicit", (400, 300));
    let window = window_by_app_id(&mut f, "explicit").unwrap();

    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&window)));
    assert!(f.state().camera_target().is_some());
    assert!(is_activated(&window));
}

#[test]
fn focus_on_open_false_with_pinned_to_screen_does_not_steal_focus() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "hud"
pinned_to_screen = true
focus_on_open = false
size = [320, 240]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "normal", (400, 300));
    let normal = window_by_app_id(&mut f, "normal").unwrap();
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&normal)));

    map_window(&mut f, id, "hud", (320, 240));
    let hud = window_by_app_id(&mut f, "hud").unwrap();

    // The pinned overlay maps and pins successfully, but keeps the existing
    // window focused — and does not steal the xdg Activated chrome from it.
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&normal)));
    assert!(f.state().stage.pin_of(&hud).is_some());
    assert!(is_activated(&normal));
    assert!(!is_activated(&hud));
}

#[test]
fn non_matching_rule_leaves_window_alone() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "other"
size = [640, 480]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id("plain");
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    let configures = window.format_recent_configures();
    assert!(
        configures.starts_with("size: 0 × 0"),
        "unmatched window must not receive a rule size, got:\n{configures}"
    );
}
