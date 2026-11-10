//! `toggle-titlebar`: hides/shows a server-decorated window's title bar at
//! runtime by flipping its decoration between `server` and `minimal`.

use driftwm::config::{Action, DecorationMode, applied_rule};
use smithay::reexports::wayland_server::Resource as _;

use super::{Fixture, config, map_window, server_surface, window_by_app_id};

fn has_bar(f: &mut Fixture, app_id: &str) -> bool {
    let window = window_by_app_id(f, app_id).unwrap();
    let key = crate::decorations::DecorationKey::Surface(server_surface(&window).id());
    f.state().decorations.contains_key(&key)
}

fn decoration_of(f: &mut Fixture, app_id: &str) -> Option<DecorationMode> {
    let window = window_by_app_id(f, app_id).unwrap();
    applied_rule(&server_surface(&window)).and_then(|r| r.decoration)
}

#[test]
fn toggles_a_server_decorated_window_between_bar_and_minimal() {
    let mut f = Fixture::with_config(config(
        "[[window_rules]]\napp_id = \"bar\"\ndecoration = \"server\"\n",
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "bar", (400, 300));
    assert!(has_bar(&mut f, "bar"), "the rule gave it a title bar");

    f.state().execute_action(&Action::ToggleTitlebar);
    assert!(!has_bar(&mut f, "bar"), "first toggle hides the bar");
    assert_eq!(decoration_of(&mut f, "bar"), Some(DecorationMode::Minimal));

    f.state().execute_action(&Action::ToggleTitlebar);
    assert!(has_bar(&mut f, "bar"), "second toggle brings it back");
    assert_eq!(decoration_of(&mut f, "bar"), Some(DecorationMode::Server));
}

#[test]
fn a_minimal_rule_window_gets_a_bar_on_first_toggle() {
    let mut f = Fixture::with_config(config(
        "[[window_rules]]\napp_id = \"min\"\ndecoration = \"minimal\"\n",
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "min", (400, 300));
    assert!(!has_bar(&mut f, "min"));

    f.state().execute_action(&Action::ToggleTitlebar);
    assert!(has_bar(&mut f, "min"));
    assert_eq!(decoration_of(&mut f, "min"), Some(DecorationMode::Server));
}

/// A client that draws its own chrome must not suddenly get a second bar.
#[test]
fn leaves_a_client_decorated_window_alone() {
    let mut f = Fixture::with_config(config(
        "[[window_rules]]\napp_id = \"csd\"\ndecoration = \"client\"\n",
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "csd", (400, 300));

    f.state().execute_action(&Action::ToggleTitlebar);
    assert!(!has_bar(&mut f, "csd"));
    assert_eq!(decoration_of(&mut f, "csd"), Some(DecorationMode::Client));
}
