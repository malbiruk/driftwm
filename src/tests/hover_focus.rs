//! Hover-driven `Activated` hint: under `focus_follows_mouse`, moving window
//! focus by hover must also flip the xdg-toplevel `Activated` state exclusively
//! — matching what a click/raise already does — without raising the window.

use smithay::desktop::Window;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use crate::state::{FocusTarget, StageWindow};

use super::{
    Fixture, config, give_ssd, is_activated, keyboard_focus, map_window, server_surface,
    window_by_app_id,
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

/// SSD chrome lives outside the surface bbox that `element_under` scans, so
/// hover-focus must resolve a title-bar hit through the decoration channel
/// rather than missing it entirely.
#[test]
fn hover_on_an_ssd_title_bar_focuses_its_window() {
    let mut f = Fixture::with_config(config(
        r#"
focus_follows_mouse = true
[decorations]
default_mode = "server"
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    place(&mut f, &a, Point::from((0, 0)));
    map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    place(&mut f, &b, Point::from((2000, 0)));
    give_ssd(&mut f, &b);

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    assert!(is_activated(&a));

    // B's title bar band, above its own content: y in [-25, 0).
    let bar = Point::from((2010.0, -10.0));
    f.state().warp_pointer(bar);
    f.state().maybe_hover_focus(bar);

    assert!(
        is_activated(&b),
        "hovering the SSD title bar must focus its window"
    );
}

/// The close button is a separate chrome band from the title bar; hover must
/// resolve it to the window too, not just the bar body.
#[test]
fn hover_on_an_ssd_close_button_focuses_its_window() {
    let mut f = Fixture::with_config(config(
        r#"
focus_follows_mouse = true
[decorations]
default_mode = "server"
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    place(&mut f, &a, Point::from((0, 0)));
    map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    place(&mut f, &b, Point::from((2000, 0)));
    give_ssd(&mut f, &b);

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    assert!(is_activated(&a));

    // B's close button, bar 25px tall: x in [2000+400-25-8, 2000+400-8), y in [-25, 0).
    let close = Point::from((2000.0 + 400.0 - 20.0, -12.0));
    f.state().warp_pointer(close);
    f.state().maybe_hover_focus(close);

    assert!(
        is_activated(&b),
        "hovering the close button must still focus the window, not swallow the hover"
    );
}

/// The invisible CSD resize margin is chrome too, but hover must not treat it
/// as a target on its own — otherwise every window would grab focus slightly
/// outside its own edges. `b` holds focus so a wrongly-accepted margin hit has
/// something to visibly steal: with only `a` on the canvas (already the sole
/// focus target), this test couldn't fail no matter what the margin does.
#[test]
fn hover_on_the_csd_resize_margin_over_empty_canvas_does_not_focus() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    place(&mut f, &a, Point::from((0, 0)));
    let b_surface = map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    place(&mut f, &b, Point::from((2000, 0)));

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&b, serial);
    f.double_roundtrip(id);
    assert!(is_activated(&b));
    f.client(id).window(&b_surface).format_recent_configures();

    // A's outer 8px CSD resize margin, left edge — clear of any window content,
    // and clear of b too.
    let margin = Point::from((-4.0, 150.0));
    f.state().warp_pointer(margin);
    f.state().maybe_hover_focus(margin);
    f.double_roundtrip(id);

    assert!(
        is_activated(&b),
        "hovering a's resize margin over empty canvas must not steal focus"
    );
    let configures = f.client(id).window(&b_surface).format_recent_configures();
    assert!(
        configures.is_empty(),
        "hovering the resize margin over empty canvas must not touch focus, got:\n{configures}"
    );
}

/// Window A's resize margin can overhang window B's content — a click there
/// still resizes A, but hover must reject-and-continue past A's margin to
/// focus B underneath.
#[test]
fn hover_on_a_resize_margin_overhanging_another_window_focuses_that_window() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    place(&mut f, &b, Point::from((0, 0)));
    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    place(&mut f, &a, Point::from((400, 0)));

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    assert!(is_activated(&a));

    // A's left resize margin overhangs B's right edge: x in [392, 400).
    let overhang = Point::from((396.0, 150.0));
    f.state().warp_pointer(overhang);
    f.state().maybe_hover_focus(overhang);

    assert!(
        is_activated(&b),
        "the overhanging margin must reject-and-continue to the window beneath it"
    );
    assert!(!is_activated(&a));
}

/// A sits above B, and a point on A's SSD title bar also lies directly over
/// B's content underneath. The two title-bar tests above place windows
/// 2000px apart and don't exercise this overlap, where the walk must prefer
/// the topmost window's chrome over a lower window's content.
#[test]
fn hover_on_a_higher_windows_title_bar_over_a_lower_windows_content_focuses_the_higher_window() {
    let mut f = Fixture::with_config(config(
        r#"
focus_follows_mouse = true
[decorations]
default_mode = "server"
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    place(&mut f, &b, Point::from((0, 0)));
    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    // A's title bar (y in [0, 25)) sits directly over B's content; A's own
    // content starts at y = 25.
    place(&mut f, &a, Point::from((0, 25)));
    give_ssd(&mut f, &a);

    // Focus b directly, without the raise that `place`/`raise_and_focus`
    // would apply — z-order must stay with a on top.
    let serial = SERIAL_COUNTER.next_serial();
    f.state()
        .set_window_focus(Some(FocusTarget(server_surface(&b))), serial);
    f.state()
        .set_activated_exclusive(&StageWindow::Client(b.clone()));
    assert!(is_activated(&b));

    let bar = Point::from((100.0, 10.0));
    f.state().warp_pointer(bar);
    f.state().maybe_hover_focus(bar);

    assert!(
        is_activated(&a),
        "a point on the topmost window's title bar must resolve to it, not the content beneath"
    );
    assert!(!is_activated(&b));
}

/// Hover's counterpart to the pinned-window phantom-rect case covered for
/// gestures in `gesture_resize.rs`: a pinned window's stage entry is a
/// cached canvas position, re-anchored to its live `screen_pos` only on a
/// camera move, so a zoom change alone leaves it stale. `pin` starts focused
/// so a wrong hit on the stale phantom entry is a visible no-op; hover must
/// instead reach `under`, which is what's genuinely drawn there.
#[test]
fn hover_on_a_pinned_windows_phantom_rect_focuses_the_window_really_there() {
    let mut f = Fixture::with_config(config(
        r#"
focus_follows_mouse = true

[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [400, 300]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "pin", (400, 300));
    let pin = window_by_app_id(&mut f, "pin").unwrap();
    let site = f.state().stage.pin_of(&pin).unwrap().screen_pos;

    map_window(&mut f, id, "under", (400, 300));
    let under = window_by_app_id(&mut f, "under").unwrap();
    // Overlap the pin's (zoom-1) phantom canvas rect exactly, then re-raise
    // the pin so it stays topmost.
    f.state()
        .map_window(StageWindow::Client(under.clone()), site, false);
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&pin, serial);
    assert!(is_activated(&pin));

    // Zoom in without panning: the pin keeps rendering at `site` (pinned
    // windows always draw at scale 1), but its cached canvas position is only
    // re-derived on the next camera move — so a point deep in the phantom
    // rect now maps to a screen position well outside where the pin actually
    // is.
    f.state().with_output_state(|os| os.zoom = 2.0);

    let point = Point::from((site.x as f64 + 350.0, site.y as f64 + 250.0));
    f.state().warp_pointer(point);
    f.state().maybe_hover_focus(point);

    assert!(
        is_activated(&under),
        "hovering the pin's phantom rect must reach the window really drawn there"
    );
    assert!(!is_activated(&pin));
}

/// Hovering outside any window edge (in the 8px external resize margin) yields
/// no pointer motion or focus to the client on any side. Out-of-bounds coordinates
/// (such as x < 0 or y < 0) must never be sent.
#[test]
fn hover_outside_window_edges_yields_no_pointer_motion() {
    let mut f = Fixture::with_config(config(""));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _surface = map_window(&mut f, id, "w", (400, 300));
    let w = window_by_app_id(&mut f, "w").unwrap();
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });
    place(&mut f, &w, Point::from((500, 400)));

    let mouse = super::input_backend::FakeDevice::mouse();
    f.client(id).state.pointer_positions.clear();

    // 4 px outside each of the four edges (within the 8px CSD resize margin):
    let top_outside = Point::from((700.0, 396.0));
    let left_outside = Point::from((496.0, 550.0));
    let right_outside = Point::from((904.0, 550.0));
    let bottom_outside = Point::from((700.0, 704.0));

    for (name, pt) in [
        ("top", top_outside),
        ("left", left_outside),
        ("right", right_outside),
        ("bottom", bottom_outside),
    ] {
        assert!(
            f.state().pointer_focus_under(pt, pt).is_none(),
            "{name} margin must yield no pointer focus"
        );

        super::input_backend::pointer_to(&mut f, &mouse, pt);
        f.double_roundtrip(id);

        assert!(
            f.client(id).state.pointer_positions.is_empty(),
            "{name} margin must deliver no pointer motion to the client, but got: {:?}",
            f.client(id).state.pointer_positions
        );
        assert!(
            f.state()
                .seat
                .get_pointer()
                .unwrap()
                .current_focus()
                .is_none(),
            "{name} margin must leave current pointer focus None"
        );
    }

    // Moving strictly inside the window yields pointer motion with valid coordinates:
    let inside = Point::from((700.0, 550.0));
    super::input_backend::pointer_to(&mut f, &mouse, inside);
    f.double_roundtrip(id);

    assert_eq!(
        f.client(id).state.pointer_positions.last(),
        Some(&(200.0, 150.0)),
        "pointer inside the window must deliver local coordinates to the client"
    );
    assert_eq!(
        f.state()
            .seat
            .get_pointer()
            .unwrap()
            .current_focus()
            .as_ref()
            .map(|t| &t.0),
        Some(&server_surface(&w)),
    );

    // Moving back outside past the left edge clears pointer focus (sends leave):
    super::input_backend::pointer_to(&mut f, &mouse, left_outside);
    f.double_roundtrip(id);

    assert!(
        f.state()
            .seat
            .get_pointer()
            .unwrap()
            .current_focus()
            .is_none(),
        "moving outside must drop pointer focus"
    );
}

/// On an SSD window, hovering the title bar (and the margin above it) also delivers
/// no pointer motion or focus to the client.
#[test]
fn hover_on_ssd_chrome_yields_no_pointer_motion_to_client() {
    let mut f = Fixture::with_config(config(
        r#"
[decorations]
default_mode = "server"
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _surface = map_window(&mut f, id, "w", (400, 300));
    let w = window_by_app_id(&mut f, "w").unwrap();
    give_ssd(&mut f, &w);
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });
    place(&mut f, &w, Point::from((500, 400)));

    let mouse = super::input_backend::FakeDevice::mouse();
    f.client(id).state.pointer_positions.clear();

    // Bar is 25px tall above 400: y in [375, 400).
    // Margin above bar is 8px: y in [367, 375).
    let bar_pt = Point::from((700.0, 385.0));
    let margin_above_bar = Point::from((700.0, 370.0));

    for (name, pt) in [
        ("title bar", bar_pt),
        ("margin above bar", margin_above_bar),
    ] {
        assert!(
            f.state().pointer_focus_under(pt, pt).is_none(),
            "{name} must yield no pointer focus"
        );

        super::input_backend::pointer_to(&mut f, &mouse, pt);
        f.double_roundtrip(id);

        assert!(
            f.client(id).state.pointer_positions.is_empty(),
            "{name} must deliver no pointer motion to the client, but got: {:?}",
            f.client(id).state.pointer_positions
        );
    }
}
