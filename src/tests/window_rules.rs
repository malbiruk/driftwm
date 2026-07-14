//! Window-rule *application* wiring on first commit. The matching/merge
//! engine has its own unit suite; these drive a real client through the
//! commit path and assert the applied effect server-side or configure-side.

use super::{Fixture, config, keyboard_focus, map_window, server_surface, window_by_app_id};

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
    assert!(!f.state().stage.focus_history().contains(&widget));
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
