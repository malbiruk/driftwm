//! `toggle-scratchpad` stows a window without unmapping it and brings it back
//! at the pin site it left from.

use smithay::desktop::Window;
use smithay::wayland::seat::WaylandFocus;

use driftwm::stage::ElementId;

use smithay::utils::Point;

use super::{Fixture, config, is_activated, map_window, tick_until_settled, window_by_app_id};

/// A pinned, focused 400x300 window (`"dd"`) beside an unpinned one (`"other"`),
/// both settled.
fn setup() -> (Fixture, Window, Window, ElementId) {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();
    map_window(&mut f, client, "other", (300, 200));
    map_window(&mut f, client, "dd", (400, 300));
    let dd = window_by_app_id(&mut f, "dd").unwrap();
    let other = window_by_app_id(&mut f, "other").unwrap();
    f.state().pin_window(&dd).unwrap();
    tick_until_settled(&mut f);
    let eid = f.state().stage.id_of(&dd).unwrap();
    (f, dd, other, eid)
}

#[test]
fn hide_keeps_the_window_mapped_and_hands_focus_back() {
    let (mut f, dd, other, _) = setup();
    let site = f.state().stage.pin_of(&dd).cloned().unwrap();
    f.state().raise_and_focus(&other, 0.into());
    f.state().raise_and_focus(&dd, 0.into());
    assert!(is_activated(&dd), "starts focused");

    f.state().toggle_scratchpad("dd", "true");

    assert!(f.state().hidden_by_deferred_adopt(&dd), "stowed");
    assert!(f.state().stage.contains(&dd), "still on the stage");
    assert_eq!(f.state().stage.pin_of(&dd), Some(&site), "pin site kept");
    assert!(!is_activated(&dd), "focus left the stowed window");
    assert!(is_activated(&other), "focus went to the other window");
}

#[test]
fn hiding_the_focused_window_focuses_the_most_recent_one() {
    let (mut f, dd, other, _) = setup();
    f.state().raise_and_focus(&other, 0.into());
    f.state().raise_and_focus(&dd, 0.into());

    f.state().toggle_scratchpad("dd", "true");
    tick_until_settled(&mut f);

    assert!(!is_activated(&dd));
    assert!(is_activated(&other));
}

#[test]
fn show_restores_the_same_site_focused_and_unanimated() {
    let (mut f, dd, _other, eid) = setup();
    let site = f.state().stage.pin_of(&dd).cloned().unwrap();
    let pos = f.state().stage.position_of(&dd);

    f.state().toggle_scratchpad("dd", "true");
    tick_until_settled(&mut f);
    f.state().toggle_scratchpad("dd", "true");

    assert!(!f.state().hidden_by_deferred_adopt(&dd), "shown again");
    assert_eq!(f.state().stage.pin_of(&dd), Some(&site), "same pin site");
    assert_eq!(f.state().stage.position_of(&dd), pos, "same position");
    assert!(is_activated(&dd), "focused on show");
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_none(),
        "no animation to ease it in"
    );
    assert_eq!(
        f.state().stage.windows().last(),
        Some(&dd.clone().into()),
        "raised"
    );
}

const PIN_RULE: &str = r#"
[[window_rules]]
app_id = "dd"
pinned_to_screen = true
size = [320, 240]
"#;

#[test]
fn hidden_scratchpad_follows_its_pin_when_its_output_is_unplugged() {
    let mut f = Fixture::with_config(config(PIN_RULE));
    let out2 = f.add_output(2, (1920, 1080));
    let _out1 = f.add_output(1, (800, 600));
    let client = f.add_client();
    map_window(&mut f, client, "dd", (320, 240));
    let dd = window_by_app_id(&mut f, "dd").unwrap();
    assert_eq!(f.state().stage.pin_of(&dd).unwrap().output, "HEADLESS-2");

    f.state().toggle_scratchpad("dd", "true");
    f.remove_output(&out2);

    let site = f.state().stage.pin_of(&dd).cloned().unwrap();
    assert_eq!(site.output, "HEADLESS-1", "rehomed onto the survivor");
    assert!(f.state().hidden_by_deferred_adopt(&dd), "still stowed");

    f.state().toggle_scratchpad("dd", "true");

    assert!(!f.state().hidden_by_deferred_adopt(&dd), "shown");
    assert!(is_activated(&dd), "focused");
    assert_eq!(
        f.state().stage.pin_of(&dd),
        Some(&site),
        "at the rehomed site"
    );
}

#[test]
fn hidden_scratchpad_survives_losing_and_regaining_the_last_output() {
    let mut f = Fixture::with_config(config(PIN_RULE));
    let out1 = f.add_output(1, (1920, 1080));
    let client = f.add_client();
    map_window(&mut f, client, "dd", (320, 240));
    let dd = window_by_app_id(&mut f, "dd").unwrap();

    f.state().toggle_scratchpad("dd", "true");
    f.remove_output(&out1);
    let out2 = f.add_output(2, (1280, 720));
    f.roundtrip(client);

    assert!(f.state().hidden_by_deferred_adopt(&dd), "still stowed");
    assert_eq!(f.state().stage.pin_of(&dd).unwrap().output, out2.name());

    f.state().toggle_scratchpad("dd", "true");

    assert!(!f.state().hidden_by_deferred_adopt(&dd), "shown");
    assert!(is_activated(&dd), "focused");
}

#[test]
fn hidden_scratchpad_is_not_hit_by_the_pointer() {
    let mut f = Fixture::with_config(config(PIN_RULE));
    f.add_output(1, (1920, 1080));
    let client = f.add_client();
    map_window(&mut f, client, "dd", (320, 240));
    let dd = window_by_app_id(&mut f, "dd").unwrap();
    tick_until_settled(&mut f);
    let site = f.state().stage.pin_of(&dd).cloned().unwrap();
    let inside = (site.screen_pos + Point::from((160, 120))).to_f64();
    let on_edge = (site.screen_pos + Point::from((-2, 120))).to_f64();

    assert_eq!(f.state().pinned_element_under(inside), Some(dd.clone()));
    assert_eq!(f.state().pinned_owner_under(on_edge), Some(dd.clone()));
    assert_eq!(
        f.state().pinned_resize_margin_under(on_edge),
        Some(dd.clone())
    );
    assert!(f.state().pinned_window_under(inside, inside).is_some());

    f.state().toggle_scratchpad("dd", "true");

    assert_eq!(f.state().pinned_element_under(inside), None, "content");
    assert_eq!(f.state().pinned_owner_under(inside), None, "owner");
    assert_eq!(f.state().pinned_owner_under(on_edge), None, "chrome");
    assert_eq!(
        f.state().pinned_resize_margin_under(on_edge),
        None,
        "margin"
    );
    assert!(f.state().pinned_window_under(inside, inside).is_none());
    assert!(matches!(
        f.state().pinned_decoration_under(on_edge),
        crate::input::PinnedChrome::Miss
    ));
}

#[test]
fn a_window_awaiting_its_first_placement_is_left_alone() {
    let (mut f, dd, _other, _) = setup();
    let surface = dd.wl_surface().unwrap().into_owned();
    f.state().pending_center.insert(surface.clone());

    f.state().toggle_scratchpad("dd", "true");

    assert!(
        !f.state().hidden_by_deferred_adopt(&dd),
        "not stowed before placement"
    );
    f.state().pending_center.remove(&surface);
}

#[test]
fn a_fullscreen_asked_for_while_stowed_applies_on_show() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();
    let surface = map_window(&mut f, client, "dd", (400, 300));
    let dd = window_by_app_id(&mut f, "dd").unwrap();
    tick_until_settled(&mut f);

    f.state().toggle_scratchpad("dd", "true");
    f.client(client).window(&surface).set_fullscreen(None);
    f.double_roundtrip(client);
    assert!(!f.state().stage.is_fullscreen(&dd), "queued while stowed");

    f.state().toggle_scratchpad("dd", "true");
    f.double_roundtrip(client);

    assert!(f.state().stage.is_fullscreen(&dd), "applied on show");
}

#[test]
fn windows_sharing_the_app_id_and_their_children_toggle_together() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();
    let main = map_window(&mut f, client, "dd", (400, 300));
    // A second top-level of the app (a compose window) and a dialog on it.
    map_window(&mut f, client, "dd", (300, 200));
    let toplevel = f.client(client).window(&main).xdg_toplevel.clone();
    let dialog = f.client(client).create_window();
    let dsurface = dialog.surface.clone();
    dialog.set_app_id("other-app");
    dialog.set_parent(Some(&toplevel));
    dialog.commit();
    f.roundtrip(client);
    let dwin = f.client(client).window(&dsurface);
    dwin.set_size(200, 100);
    dwin.attach_new_buffer();
    dwin.ack_last_and_commit();
    f.double_roundtrip(client);
    tick_until_settled(&mut f);
    let all: Vec<Window> = f
        .state()
        .stage
        .windows()
        .filter_map(|w| w.client())
        .cloned()
        .collect();
    assert_eq!(all.len(), 3);

    f.state().toggle_scratchpad("dd", "true");
    assert!(
        all.iter().all(|w| f.state().hidden_by_deferred_adopt(w)),
        "both windows and the dialog are stowed"
    );

    f.state().toggle_scratchpad("dd", "true");
    assert!(
        all.iter().all(|w| !f.state().hidden_by_deferred_adopt(w)),
        "and shown together"
    );
}

#[test]
fn a_repeat_press_while_the_app_starts_does_not_spawn_it_again() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.state().toggle_scratchpad("slow", "true");
    let first = f.state().scratchpad_spawns["slow"];
    f.state().toggle_scratchpad("slow", "true");

    assert_eq!(f.state().scratchpad_spawns["slow"], first, "debounced");
}

const BOUND: &str = "[keybindings]\n\"mod+w\" = \"toggle-scratchpad dd true\"\n";

#[test]
fn reload_releases_a_stowed_window_nothing_can_reach() {
    let mut f = Fixture::with_config(config(BOUND));
    // A reload leaves its own mode-change intent behind.
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();
    map_window(&mut f, client, "dd", (400, 300));
    map_window(&mut f, client, "kept", (400, 300));
    let dd = window_by_app_id(&mut f, "dd").unwrap();
    let kept = window_by_app_id(&mut f, "kept").unwrap();
    f.state().toggle_scratchpad("dd", "true");
    f.state().toggle_scratchpad("kept", "true");
    assert!(f.state().hidden_by_deferred_adopt(&dd));
    assert!(f.state().hidden_by_deferred_adopt(&kept));

    f.state().reload_config_from_contents(
        "[keybindings]\n\"mod+w\" = \"toggle-scratchpad dd true\"\n\"mod+k\" = \"toggle-scratchpad kept true\"\n",
    );
    assert!(f.state().hidden_by_deferred_adopt(&dd), "still bound");
    assert!(f.state().hidden_by_deferred_adopt(&kept), "still bound");

    f.state().reload_config_from_contents(
        "[keybindings]\n\"mod+k\" = \"toggle-scratchpad kept true\"\n",
    );

    assert!(!f.state().hidden_by_deferred_adopt(&dd), "released");
    assert!(f.state().hidden_by_deferred_adopt(&kept), "bound ones stay");
    f.state().toggle_scratchpad("kept", "true");
}

#[test]
fn every_binding_kind_marks_a_scratchpad_app() {
    for toml in [
        "[keybindings]\n\"mod+w\" = \"toggle-scratchpad dd true\"\n",
        "[mouse.anywhere]\n\"mod+middle\" = \"toggle-scratchpad dd true\"\n",
        "[gestures.anywhere]\n\"mod+3-finger-pinch-out\" = \"toggle-scratchpad dd true\"\n",
        "[touch.anywhere]\n\"3-finger-doubletap\" = \"toggle-scratchpad dd true\"\n",
    ] {
        let cfg = driftwm::config::Config::from_toml(toml).unwrap();
        assert!(cfg.is_scratchpad_app("dd"), "{toml}");
        assert!(!cfg.is_scratchpad_app("other"), "{toml}");
    }
}

#[test]
fn a_fullscreen_window_comes_back_fullscreen_after_stow_and_show() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let client = f.add_client();
    let surface = map_window(&mut f, client, "dd", (400, 300));
    let dd = window_by_app_id(&mut f, "dd").unwrap();
    f.client(client).window(&surface).set_fullscreen(None);
    f.double_roundtrip(client);
    f.client(client).window(&surface).ack_last_and_commit();
    f.double_roundtrip(client);
    assert!(f.state().stage.is_fullscreen(&dd), "starts fullscreen");

    // What the action guard does before the toggle runs.
    let was = f.state().active_fullscreen_window();
    f.state().exit_fullscreen();
    f.state().toggle_scratchpad_from("dd", "true", was.as_ref());
    f.double_roundtrip(client);
    assert!(f.state().hidden_by_deferred_adopt(&dd), "stowed");
    assert!(!f.state().stage.is_fullscreen(&dd), "desktop is showing");

    f.state().toggle_scratchpad("dd", "true");
    f.double_roundtrip(client);

    assert!(f.state().stage.is_fullscreen(&dd), "fullscreen again");
}
