//! `toggle-borders`: hides/shows a window's border, shadow and corner clip as
//! one by flipping a runtime flag on its stored rule.

use driftwm::config::{Action, applied_rule};
use smithay::reexports::wayland_server::Resource as _;

use super::{Fixture, config, map_window, server_surface, window_by_app_id};
use crate::state::StageWindow;

const BORDERED: &str = "[decorations]\nborder_width = 4\ncorner_radius = 12\n";

fn fixture(rules: &str) -> (Fixture, super::client::ClientId) {
    let mut f = Fixture::with_config(config(&format!("{BORDERED}{rules}")));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    (f, id)
}

fn border_width(f: &mut Fixture, app_id: &str) -> i32 {
    let window = window_by_app_id(f, app_id).unwrap();
    f.state().window_border_width(&server_surface(&window))
}

fn hidden(f: &mut Fixture, app_id: &str) -> bool {
    let window = window_by_app_id(f, app_id).unwrap();
    applied_rule(&server_surface(&window)).is_some_and(|r| r.borders_hidden)
}

#[test]
fn toggles_the_border_off_and_back_to_what_the_config_says() {
    let (mut f, id) = fixture("[[window_rules]]\napp_id = \"w\"\ndecoration = \"server\"\n");
    map_window(&mut f, id, "w", (400, 300));
    assert_eq!(border_width(&mut f, "w"), 4);

    f.state().execute_action(&Action::ToggleBorders);
    assert_eq!(border_width(&mut f, "w"), 0);
    assert!(hidden(&mut f, "w"));

    f.state().execute_action(&Action::ToggleBorders);
    assert_eq!(border_width(&mut f, "w"), 4, "restored, nothing was saved");
    assert!(!hidden(&mut f, "w"));
}

#[test]
fn restores_a_rules_own_border_width_not_the_global_one() {
    let (mut f, id) =
        fixture("[[window_rules]]\napp_id = \"w\"\ndecoration = \"server\"\nborder_width = 9\n");
    map_window(&mut f, id, "w", (400, 300));
    assert_eq!(border_width(&mut f, "w"), 9);

    f.state().execute_action(&Action::ToggleBorders);
    f.state().execute_action(&Action::ToggleBorders);
    assert_eq!(border_width(&mut f, "w"), 9);
}

/// The compositor draws a border, shadow and corner clip around a client-decorated
/// window too, so unlike the title bar this applies to it.
#[test]
fn applies_to_a_client_decorated_window() {
    let (mut f, id) = fixture("[[window_rules]]\napp_id = \"csd\"\ndecoration = \"client\"\n");
    map_window(&mut f, id, "csd", (400, 300));
    assert_eq!(border_width(&mut f, "csd"), 4);

    f.state().execute_action(&Action::ToggleBorders);
    assert_eq!(border_width(&mut f, "csd"), 0);
}

#[test]
fn leaves_a_decoration_none_window_alone() {
    let (mut f, id) = fixture("[[window_rules]]\napp_id = \"bare\"\ndecoration = \"none\"\n");
    map_window(&mut f, id, "bare", (400, 300));

    f.state().execute_action(&Action::ToggleBorders);
    assert!(!hidden(&mut f, "bare"), "it already has no chrome to hide");
}

#[test]
fn leaves_a_fitted_window_alone() {
    let (mut f, id) = fixture("[[window_rules]]\napp_id = \"fit\"\ndecoration = \"server\"\n");
    map_window(&mut f, id, "fit", (400, 300));
    f.state().execute_action(&Action::FitWindow);
    f.double_roundtrip(id);
    let window = window_by_app_id(&mut f, "fit").unwrap();
    assert!(f.state().stage.is_fit(&window));

    f.state().execute_action(&Action::ToggleBorders);
    assert!(!hidden(&mut f, "fit"));
    assert_eq!(border_width(&mut f, "fit"), 4);
}

/// The frame is the content plus the border on every side, so hiding it shrinks
/// the frame by twice its width; the cached snap rect has to follow.
#[test]
fn the_cached_snap_rect_follows_the_frame() {
    let (mut f, id) = fixture("[[window_rules]]\napp_id = \"w\"\ndecoration = \"server\"\n");
    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();
    let surface_id = server_surface(&window).id();
    let width = |f: &mut Fixture| {
        let cached = *f.state().stable_snap_rects.get(&surface_id).unwrap();
        let live = f
            .state()
            .snap_rect_for(&StageWindow::Client(window.clone()))
            .unwrap();
        assert_eq!(cached.x_high - cached.x_low, live.x_high - live.x_low);
        cached.x_high - cached.x_low
    };

    let with_border = width(&mut f);
    f.state().execute_action(&Action::ToggleBorders);
    assert_eq!(with_border - width(&mut f), 8.0);

    f.state().execute_action(&Action::ToggleBorders);
    assert_eq!(width(&mut f), with_border);
}

/// Border and title bar are independent toggles, so a user can have either.
#[test]
fn is_independent_of_the_title_bar() {
    let (mut f, id) = fixture("[[window_rules]]\napp_id = \"w\"\ndecoration = \"server\"\n");
    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();
    let key = crate::decorations::DecorationKey::Surface(server_surface(&window).id());

    f.state().execute_action(&Action::ToggleBorders);
    assert!(f.state().decorations.contains_key(&key), "bar stays");

    f.state().execute_action(&Action::ToggleTitlebar);
    assert!(!f.state().decorations.contains_key(&key));
    assert!(hidden(&mut f, "w"), "border stays hidden");
    assert_eq!(border_width(&mut f, "w"), 0);
}
