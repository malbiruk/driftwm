//! Hover-driven `Activated` hint: under `focus_follows_mouse`, moving window
//! focus by hover must also flip the xdg-toplevel `Activated` state exclusively
//! — matching what a click/raise already does — without raising the window.

use smithay::desktop::Window;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use crate::state::StageWindow;

use super::{
    Fixture, config, is_activated, keyboard_focus, map_window, server_surface, window_by_app_id,
};

/// Force `window` to a known canvas position without touching activation —
/// auto-placement alone doesn't guarantee two same-size windows land apart,
/// and these tests need an unambiguous point to hover. Note: re-mapping also
/// raises the window, so establish z-order after the last `place` call.
fn place(f: &mut Fixture, window: &Window, pos: Point<i32, Logical>) {
    f.state()
        .map_window(StageWindow::Client(window.clone()), pos, false);
}

/// Canvas-space center of `window`'s current geometry.
fn window_center(f: &mut Fixture, window: &Window) -> Point<f64, Logical> {
    let pos = f.state().stage.position_of(window).unwrap();
    let size = window.geometry().size;
    Point::from((
        pos.x as f64 + size.w as f64 / 2.0,
        pos.y as f64 + size.h as f64 / 2.0,
    ))
}

/// Hovering a different window flips the `Activated` hint exclusively to it,
/// and the configure actually reaches the client — not just the server-side
/// pending state.
#[test]
fn hover_flips_activated_hint_on_the_wire() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    place(&mut f, &a, Point::from((0, 0)));
    let b_surface = map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    place(&mut f, &b, Point::from((2000, 0)));

    // Click-focus A explicitly, same as a real raise-to-focus click.
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.double_roundtrip(id);
    assert!(is_activated(&a));
    // Drain the settle so only the hover-triggered configures show up below.
    f.client(id).window(&a_surface).format_recent_configures();
    f.client(id).window(&b_surface).format_recent_configures();

    let b_center = window_center(&mut f, &b);
    f.state().warp_pointer(b_center);
    f.state().maybe_hover_focus(b_center);
    f.double_roundtrip(id);

    assert!(is_activated(&b));
    assert!(!is_activated(&a));

    let b_configures = f.client(id).window(&b_surface).format_recent_configures();
    assert!(
        b_configures.contains("Activated"),
        "hover must flush an Activated configure to the newly-focused window, got:\n{b_configures}"
    );
    let a_configures = f.client(id).window(&a_surface).format_recent_configures();
    assert!(
        !a_configures.is_empty() && !a_configures.contains("Activated"),
        "hover must flush a deactivate configure to the window it took focus from, got:\n{a_configures}"
    );
}

/// Hover focus never raises: the window gaining `Activated` stays wherever it
/// was in the z-order.
#[test]
fn hover_focus_does_not_raise_the_window() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    place(&mut f, &a, Point::from((0, 0)));
    map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    place(&mut f, &b, Point::from((2000, 0)));

    // Click-focus A, raising it above B.
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    let before: Vec<StageWindow> = f.state().stage.windows().cloned().collect();
    assert_eq!(
        before.last(),
        Some(&StageWindow::Client(a.clone())),
        "the click raises a to the top"
    );

    let b_center = window_center(&mut f, &b);
    f.state().warp_pointer(b_center);
    f.state().maybe_hover_focus(b_center);

    assert!(
        is_activated(&b),
        "hover reaches b despite it sitting below a"
    );
    let after: Vec<StageWindow> = f.state().stage.windows().cloned().collect();
    assert_eq!(before, after, "hover-focus must not reorder the z-order");
}

/// With `focus_follows_mouse` off (the default), pointer motion never touches
/// the `Activated` hint.
#[test]
fn hover_is_a_no_op_with_focus_follows_mouse_off() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    place(&mut f, &a, Point::from((0, 0)));
    let b_surface = map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    place(&mut f, &b, Point::from((2000, 0)));

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.double_roundtrip(id);
    f.client(id).window(&b_surface).format_recent_configures();

    let b_center = window_center(&mut f, &b);
    f.state().warp_pointer(b_center);
    f.state().maybe_hover_focus(b_center);
    f.double_roundtrip(id);

    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&a)));
    assert!(!is_activated(&b));
    let b_configures = f.client(id).window(&b_surface).format_recent_configures();
    assert!(
        b_configures.is_empty(),
        "focus_follows_mouse=false must leave the hovered window untouched, got:\n{b_configures}"
    );
}

/// Hovering a widget-rule window under `focus_follows_mouse` does not steal
/// the `Activated` hint from the currently-focused normal window.
#[test]
fn hover_over_widget_does_not_steal_activation() {
    let mut f = Fixture::with_config(config(
        r#"
focus_follows_mouse = true

[[window_rules]]
app_id = "widget"
widget = true
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "normal", (400, 300));
    let normal = window_by_app_id(&mut f, "normal").unwrap();
    place(&mut f, &normal, Point::from((0, 0)));
    map_window(&mut f, id, "widget", (200, 100));
    let widget = window_by_app_id(&mut f, "widget").unwrap();
    place(&mut f, &widget, Point::from((2000, 0)));
    assert!(is_activated(&normal));

    let widget_center = window_center(&mut f, &widget);
    f.state().warp_pointer(widget_center);
    f.state().maybe_hover_focus(widget_center);

    assert!(is_activated(&normal));
    assert!(!is_activated(&widget));
}
