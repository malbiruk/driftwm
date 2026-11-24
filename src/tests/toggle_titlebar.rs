//! `toggle-titlebar`: hides/shows a server-decorated window's title bar at
//! runtime by flipping its decoration between `server` and `minimal`.

use driftwm::config::{Action, DecorationMode, applied_rule};
use smithay::reexports::wayland_server::Resource as _;

use super::{Fixture, config, map_window, server_surface, window_by_app_id};
use crate::state::StageWindow;

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

/// The snap rect is cached at settle events. A frame that changes by the bar
/// height without refreshing it would read as a grow on the next commit and make
/// `reflow_grown_snapped_window` relocate the window.
#[test]
fn the_cached_snap_rect_follows_the_frame() {
    let mut f = Fixture::with_config(config(
        "[[window_rules]]\napp_id = \"bar\"\ndecoration = \"server\"\n",
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "bar", (400, 300));
    let window = window_by_app_id(&mut f, "bar").unwrap();
    let surface_id = server_surface(&window).id();
    let height = |f: &mut Fixture| {
        let cached = *f.state().stable_snap_rects.get(&surface_id).unwrap();
        let live = f
            .state()
            .snap_rect_for(&StageWindow::Client(window.clone()))
            .unwrap();
        assert_eq!(cached.y_high - cached.y_low, live.y_high - live.y_low);
        cached.y_high - cached.y_low
    };

    let with_bar = height(&mut f);
    f.state().execute_action(&Action::ToggleTitlebar);
    let without_bar = height(&mut f);
    assert_eq!(
        with_bar - without_bar,
        f64::from(f.state().config.decorations.title_bar_height)
    );

    f.state().execute_action(&Action::ToggleTitlebar);
    assert_eq!(height(&mut f), with_bar);
}

/// Fit and fill size the window against the bar it has now; the toggle would
/// leave it a bar-height off the usable area, so it leaves them alone.
#[test]
fn leaves_a_fitted_window_alone() {
    let mut f = Fixture::with_config(config(
        "[[window_rules]]\napp_id = \"fit\"\ndecoration = \"server\"\n",
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "fit", (400, 300));

    f.state().execute_action(&Action::FitWindow);
    f.double_roundtrip(id);
    let window = window_by_app_id(&mut f, "fit").unwrap();
    assert!(f.state().stage.is_fit(&window), "the window is fitted");

    f.state().execute_action(&Action::ToggleTitlebar);
    assert!(has_bar(&mut f, "fit"));
    assert_eq!(decoration_of(&mut f, "fit"), Some(DecorationMode::Server));
}

/// The compositor clips a bar-less window's corners itself, so its opaque region
/// has to give them up as the bar goes, without waiting for the client to commit.
#[test]
fn carves_the_opaque_corners_when_the_bar_goes() {
    use smithay::wayland::compositor::{RectangleKind, SurfaceAttributes, with_states};

    let mut f = Fixture::with_config(config(
        "[[window_rules]]\napp_id = \"bar\"\ndecoration = \"server\"\n",
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "bar", (400, 300));
    f.client(id)
        .set_opaque_region(&surface, &[(0, 0, 400, 300)]);
    f.client(id).window(&surface).commit();
    f.roundtrip(id);

    let carved = |f: &mut Fixture| {
        let window = window_by_app_id(f, "bar").unwrap();
        with_states(&server_surface(&window), |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            guard.current().opaque_region.as_ref().map_or(0, |region| {
                region
                    .rects
                    .iter()
                    .filter(|(kind, _)| matches!(kind, RectangleKind::Subtract))
                    .count()
            })
        })
    };

    assert_eq!(carved(&mut f), 0, "a bar draws its own corners");
    f.state().execute_action(&Action::ToggleTitlebar);
    assert_eq!(carved(&mut f), 4, "one carved rect per corner");

    f.state().execute_action(&Action::ToggleTitlebar);
    f.state().execute_action(&Action::ToggleTitlebar);
    assert_eq!(carved(&mut f), 4, "toggling again does not stack carves");
}
