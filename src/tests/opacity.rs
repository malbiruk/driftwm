//! Runtime per-window opacity over the IPC `opacity` verb. A bare window reads
//! the `[decorations]` focus-dependent default; a rule or the setter writes a
//! pin into the stored `AppliedWindowRule`, and a pinned window reads that one
//! value in both focus states.

use super::{
    Fixture, config, first_popup_surface, keyboard_focus, map_popup, map_window, server_surface,
    window_by_app_id,
};
use crate::ipc::dispatch;
use crate::ipc::protocol::{Request, Response, WindowSelector};
use crate::state::FocusTarget;
use driftwm::window_ext::WindowExt;
use smithay::utils::SERIAL_COUNTER;

fn read_opacity(f: &mut Fixture, window: Option<WindowSelector>) -> f64 {
    match dispatch(
        Request::Opacity {
            window,
            value: None,
        },
        f.state(),
    ) {
        Ok(Response::Opacity(v)) => v,
        other => panic!("expected an Opacity reply, got {other:?}"),
    }
}

#[test]
fn bare_window_reads_full_opacity() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "term", (400, 300));

    assert_eq!(read_opacity(&mut f, None), 1.0);
}

#[test]
fn set_then_get_roundtrips() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "term", (400, 300));

    let set = dispatch(
        Request::Opacity {
            window: None,
            value: Some(0.4),
        },
        f.state(),
    );
    assert_eq!(set, Ok(Response::Opacity(0.4)));
    assert_eq!(read_opacity(&mut f, None), 0.4);
}

#[test]
fn out_of_range_value_errors() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "term", (400, 300));

    for bad in [-0.1, 1.5, f64::NAN, f64::INFINITY] {
        assert!(
            dispatch(
                Request::Opacity {
                    window: None,
                    value: Some(bad),
                },
                f.state(),
            )
            .is_err(),
            "opacity {bad} must be rejected"
        );
    }
    // A rejected set leaves the stored value untouched.
    assert_eq!(read_opacity(&mut f, None), 1.0);
}

#[test]
fn id_selector_targets_unfocused_window() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "first", (400, 300));
    map_window(&mut f, id, "second", (400, 300));

    // `second` mapped last, so it holds focus; target `first` by id.
    let first = window_by_app_id(&mut f, "first").unwrap();
    let first_id = f.state().stage.id_of(&first).unwrap().0;

    let set = dispatch(
        Request::Opacity {
            window: Some(WindowSelector::Id(first_id)),
            value: Some(0.6),
        },
        f.state(),
    );
    assert_eq!(set, Ok(Response::Opacity(0.6)));

    assert_eq!(
        read_opacity(&mut f, Some(WindowSelector::Id(first_id))),
        0.6
    );
    // The focused window (`second`) is untouched.
    assert_eq!(read_opacity(&mut f, None), 1.0);
}

#[test]
fn rule_seeded_window_reads_rule_value() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "dim"
opacity = 0.3
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "dim", (400, 300));

    assert_eq!(read_opacity(&mut f, None), 0.3);
}

#[test]
fn set_preserves_other_rule_derived_fields() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "dim"
opacity = 0.3
widget = true
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "dim", (400, 300));

    // Widgets never take focus, so reach it by id rather than the default
    // (focused) selector.
    let window = window_by_app_id(&mut f, "dim").unwrap();
    let window_id = f.state().stage.id_of(&window).unwrap().0;

    let set = dispatch(
        Request::Opacity {
            window: Some(WindowSelector::Id(window_id)),
            value: Some(0.7),
        },
        f.state(),
    );
    assert_eq!(set, Ok(Response::Opacity(0.7)));

    assert_eq!(
        read_opacity(&mut f, Some(WindowSelector::Id(window_id))),
        0.7
    );
    // The rule's other field must survive the opacity-only field update.
    assert!(window.is_widget());
}

#[test]
fn app_id_selector_matches_case_insensitive_substring() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "alpha", (400, 300));
    map_window(&mut f, id, "beta", (400, 300));

    // `beta` mapped last, so it holds focus; target `alpha` by an
    // uppercase substring of its lowercase app_id.
    let set = dispatch(
        Request::Opacity {
            window: Some(WindowSelector::AppId("ALPH".into())),
            value: Some(0.5),
        },
        f.state(),
    );
    assert_eq!(set, Ok(Response::Opacity(0.5)));

    assert_eq!(
        read_opacity(&mut f, Some(WindowSelector::AppId("ALPH".into()))),
        0.5
    );
    // The focused window (`beta`) is untouched.
    assert_eq!(read_opacity(&mut f, None), 1.0);
}

#[test]
fn unpinned_windows_follow_focus() {
    let mut f = Fixture::with_config(config(
        r#"
[decorations]
opacity = 0.5
opacity_focused = 0.9
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "first", (400, 300));
    map_window(&mut f, id, "second", (400, 300));

    let first = window_by_app_id(&mut f, "first").unwrap();
    let first_id = f.state().stage.id_of(&first).unwrap().0;
    let second = window_by_app_id(&mut f, "second").unwrap();
    let second_id = f.state().stage.id_of(&second).unwrap().0;

    // `second` mapped last, so it holds focus.
    assert_eq!(read_opacity(&mut f, None), 0.9);
    assert_eq!(
        read_opacity(&mut f, Some(WindowSelector::Id(first_id))),
        0.5
    );

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&first, serial);
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&first)));

    assert_eq!(
        read_opacity(&mut f, Some(WindowSelector::Id(first_id))),
        0.9
    );
    assert_eq!(
        read_opacity(&mut f, Some(WindowSelector::Id(second_id))),
        0.5
    );
}

#[test]
fn a_rule_opacity_pins_both_focus_states() {
    let mut f = Fixture::with_config(config(
        r#"
[decorations]
opacity = 0.5
opacity_focused = 0.9

[[window_rules]]
app_id = "pinned"
opacity = 0.7
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "pinned", (400, 300));
    map_window(&mut f, id, "other", (400, 300));

    let pinned = window_by_app_id(&mut f, "pinned").unwrap();
    let pinned_id = f.state().stage.id_of(&pinned).unwrap().0;

    // `other` mapped last, so `pinned` reads its pin while unfocused.
    assert_eq!(
        read_opacity(&mut f, Some(WindowSelector::Id(pinned_id))),
        0.7
    );

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&pinned, serial);
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&pinned)));

    assert_eq!(read_opacity(&mut f, None), 0.7);
}

#[test]
fn an_ipc_set_pins_the_window() {
    let mut f = Fixture::with_config(config(
        r#"
[decorations]
opacity = 0.5
opacity_focused = 0.9
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "first", (400, 300));
    map_window(&mut f, id, "second", (400, 300));

    let first = window_by_app_id(&mut f, "first").unwrap();
    let first_id = f.state().stage.id_of(&first).unwrap().0;
    let second = window_by_app_id(&mut f, "second").unwrap();

    let set = dispatch(
        Request::Opacity {
            window: Some(WindowSelector::Id(first_id)),
            value: Some(0.3),
        },
        f.state(),
    );
    assert_eq!(set, Ok(Response::Opacity(0.3)));

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&first, serial);
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&first)));
    assert_eq!(
        read_opacity(&mut f, Some(WindowSelector::Id(first_id))),
        0.3
    );

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&second, serial);
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&second)));
    assert_eq!(
        read_opacity(&mut f, Some(WindowSelector::Id(first_id))),
        0.3
    );
}

#[test]
fn existing_wildcard_configs_are_unchanged() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "*"
opacity = 0.7

[[window_rules]]
app_id = "firefox"
opacity = 1.0
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "firefox", (400, 300));
    map_window(&mut f, id, "term", (400, 300));

    let firefox = window_by_app_id(&mut f, "firefox").unwrap();
    let firefox_id = f.state().stage.id_of(&firefox).unwrap().0;
    let term = window_by_app_id(&mut f, "term").unwrap();
    let term_id = f.state().stage.id_of(&term).unwrap().0;

    // `term` mapped last, so it holds focus.
    assert_eq!(
        read_opacity(&mut f, Some(WindowSelector::Id(firefox_id))),
        1.0
    );
    assert_eq!(read_opacity(&mut f, Some(WindowSelector::Id(term_id))), 0.7);

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&firefox, serial);
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&firefox)));

    assert_eq!(
        read_opacity(&mut f, Some(WindowSelector::Id(firefox_id))),
        1.0
    );
    assert_eq!(read_opacity(&mut f, Some(WindowSelector::Id(term_id))), 0.7);
}

#[test]
fn a_fullscreen_window_never_dims() {
    let mut f = Fixture::with_config(config(
        r#"
[decorations]
opacity = 0.5
opacity_focused = 0.9
"#,
    ));
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "fs", (400, 300));
    map_window(&mut f, id, "other", (400, 300));

    let fs = window_by_app_id(&mut f, "fs").unwrap();
    let fs_id = f.state().stage.id_of(&fs).unwrap().0;
    let other = window_by_app_id(&mut f, "other").unwrap();

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&fs, serial);
    f.state().enter_fullscreen(&fs, Some(output.clone()));
    f.double_roundtrip(id);

    // Exempt even from `opacity_focused`.
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&fs)));
    assert_eq!(read_opacity(&mut f, Some(WindowSelector::Id(fs_id))), 1.0);

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&other, serial);
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&other)));
    assert_eq!(read_opacity(&mut f, Some(WindowSelector::Id(fs_id))), 1.0);

    f.state().exit_fullscreen_on(&output);
}

#[test]
fn a_widget_window_never_dims() {
    let mut f = Fixture::with_config(config(
        r#"
[decorations]
opacity = 0.5
opacity_focused = 0.9

[[window_rules]]
app_id = "clock"
widget = true
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "clock", (400, 300));

    // Widgets never take focus, so reach it by id rather than the default
    // (focused) selector.
    let clock = window_by_app_id(&mut f, "clock").unwrap();
    let clock_id = f.state().stage.id_of(&clock).unwrap().0;

    assert_eq!(
        read_opacity(&mut f, Some(WindowSelector::Id(clock_id))),
        1.0
    );
}

/// A popup grab moves the seat's keyboard focus onto the popup surface at the
/// first key event, so the parent's own surface stops holding focus while its
/// menu is keyed through. The parent is still the focused window, and must not
/// dim under its own menu.
#[test]
fn a_window_stays_focused_while_its_popup_holds_the_keyboard() {
    let mut f = Fixture::with_config(config(
        r#"
[decorations]
opacity = 0.5
opacity_focused = 0.9
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let parent = map_window(&mut f, id, "parent", (400, 300));
    map_popup(&mut f, id, &parent);

    let window = window_by_app_id(&mut f, "parent").unwrap();
    let window_id = f.state().stage.id_of(&window).unwrap().0;
    let popup = first_popup_surface(&server_surface(&window)).expect("popup tracked");

    let serial = SERIAL_COUNTER.next_serial();
    let keyboard = f.state().seat.get_keyboard().unwrap();
    keyboard.set_focus(f.state(), Some(FocusTarget(popup.clone())), serial);
    assert_eq!(keyboard_focus(&mut f), Some(popup));

    assert_eq!(
        read_opacity(&mut f, Some(WindowSelector::Id(window_id))),
        0.9
    );
}
