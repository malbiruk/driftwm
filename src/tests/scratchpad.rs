//! `toggle-scratchpad` stows a window without unmapping it and brings it back
//! at the pin site it left from.

use smithay::desktop::Window;

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

    f.state().toggle_scratchpad("dd", "true");

    assert!(f.state().hidden_by_deferred_adopt(&dd), "stowed");
    assert!(f.state().stage.contains(&dd), "still on the stage");
    assert_eq!(f.state().stage.pin_of(&dd), Some(&site), "pin site kept");
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
