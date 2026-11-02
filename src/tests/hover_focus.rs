//! Hover-driven `Activated` hint: under `focus_follows_mouse`, moving window
//! focus by hover must also flip the xdg-toplevel `Activated` state exclusively
//! — matching what a click/raise already does — without raising the window.
//! The scenarios at the end cover the hover pull inside `refresh_pointer_focus`:
//! a viewport move slides a new window under a cursor that never moves, and
//! sloppy focus owes a move to it even where the client-pointer delivery
//! dedup puts nothing on the wire. The ones after them are the gate's other
//! half: a navigation that focused the window it pans to keeps that focus
//! until the user moves the camera.

use driftwm::config::{Action, Direction};
use driftwm::stage::PinnedSite;
use smithay::desktop::Window;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size};
use smithay::wayland::compositor::get_parent;

use crate::decorations::DecorationHit;
use crate::input::PinnedChrome;
use crate::state::{FocusIntent, FocusTarget, StageWindow};

use super::client::ClientId;
use super::input_backend::{
    FakeDevice, key_press, key_release, pointer_to, touch_down, touch_down_screen_at, touch_motion,
    touch_motion_screen_at, touch_up, trackpad_scroll, wheel_notch_down,
};
use super::{
    Fixture, MAX_TICKS, TICK, adopt_last_configure, config, give_ssd, is_activated, keyboard_focus,
    map_window, server_surface, settle, to_canvas, window_by_app_id,
};

const KEY_LEFTMETA: u32 = 125;

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

/// Hovering a pin's title bar focuses the pin: the bar is chrome, so it yields
/// no client pointer focus, and the canvas walk (which skips pins) would
/// otherwise land on whatever is drawn behind the pin.
#[test]
fn hover_on_a_pinned_windows_title_bar_focuses_the_pin_not_the_window_beneath() {
    let mut f = Fixture::with_config(config(
        r#"
focus_follows_mouse = true
[decorations]
default_mode = "server"

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
    give_ssd(&mut f, &pin);
    let site = f.state().stage.pin_of(&pin).unwrap().screen_pos;

    let bar = f.state().config.decorations.title_bar_height as f64;
    let screen = Point::from((site.x as f64 + 200.0, site.y as f64 - bar / 2.0));
    let canvas = to_canvas(&mut f, screen);
    assert!(
        matches!(
            f.state().pinned_decoration_under(screen),
            PinnedChrome::Hit(_, DecorationHit::TitleBar)
        ),
        "precondition: the probe point must land on the pin's title bar"
    );
    assert!(
        f.state().pointer_focus_under(screen, canvas).is_none(),
        "precondition: chrome yields no client pointer focus"
    );

    map_window(&mut f, id, "beneath", (400, 300));
    let beneath = window_by_app_id(&mut f, "beneath").unwrap();
    let origin = Point::from((canvas.x as i32 - 80, canvas.y as i32 - 70));
    f.state()
        .map_window(StageWindow::Client(beneath.clone()), origin, false);
    assert!(
        canvas.x >= origin.x as f64
            && canvas.x < origin.x as f64 + 400.0
            && canvas.y >= origin.y as f64
            && canvas.y < origin.y as f64 + 300.0,
        "precondition: the canvas window must cover the hover point"
    );
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&beneath, serial);
    assert!(
        !is_activated(&pin),
        "precondition: the pin starts unfocused"
    );

    f.state().warp_pointer(canvas);
    f.state().maybe_hover_focus(canvas);

    assert!(
        is_activated(&pin),
        "hovering the pin's title bar must focus the pin drawn under the pointer"
    );
    assert!(!is_activated(&beneath));
}

/// A pin's resize band is invisible, so hovering it over a lower pin is
/// hovering the lower pin — the same rule that lets a canvas window under an
/// overhanging band take focus. Under the default `resize_on_border` the band
/// is a live chrome hit, and rejecting it must not hand the point to the canvas
/// window drawn behind both pins.
#[test]
fn hover_on_a_pins_resize_band_over_a_lower_pin_focuses_the_lower_pin() {
    let mut f = Fixture::with_config(config(
        r#"
focus_follows_mouse = true

[[window_rules]]
app_id = "lower"
pinned_to_screen = true
size = [400, 300]

[[window_rules]]
app_id = "upper"
pinned_to_screen = true
size = [400, 300]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "lower", (400, 300));
    let lower = window_by_app_id(&mut f, "lower").unwrap();
    map_window(&mut f, id, "upper", (400, 300));
    let upper = window_by_app_id(&mut f, "upper").unwrap();

    // The upper pin's left edge halfway across the lower one, so its left band
    // lies over the lower pin's content.
    let lower_site = f.state().stage.pin_of(&lower).unwrap().clone();
    let upper_site = PinnedSite {
        output: lower_site.output.clone(),
        screen_pos: lower_site.screen_pos + Point::from((200, 0)),
    };
    f.state().stage.set_pin(&upper, upper_site.clone());

    let screen = Point::from((
        upper_site.screen_pos.x as f64 - 4.0,
        upper_site.screen_pos.y as f64 + 150.0,
    ));
    let canvas = to_canvas(&mut f, screen);
    assert!(
        matches!(
            f.state().pinned_decoration_under(screen),
            PinnedChrome::Hit(ref w, DecorationHit::ResizeBorder(_)) if *w == upper
        ),
        "precondition: the probe point must land on the upper pin's resize band"
    );

    map_window(&mut f, id, "beneath", (400, 300));
    let beneath = window_by_app_id(&mut f, "beneath").unwrap();
    place(
        &mut f,
        &beneath,
        Point::from((canvas.x as i32 - 80, canvas.y as i32 - 70)),
    );
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&beneath, serial);
    assert!(
        !is_activated(&lower),
        "precondition: the lower pin starts unfocused"
    );

    f.state().warp_pointer(canvas);
    f.state().maybe_hover_focus(canvas);

    assert!(
        is_activated(&lower),
        "the lower pin is what is drawn under the upper pin's invisible band"
    );
    assert!(!is_activated(&beneath));
    assert!(!is_activated(&upper));
}

/// A subsurface is part of the pin as drawn, but it is not the toplevel's own
/// surface — a lookup that matches only that one finds no window to focus.
#[test]
fn hover_on_a_pins_subsurface_focuses_the_pin() {
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
    let pin_surface = map_window(&mut f, id, "pin", (400, 300));
    let pin = window_by_app_id(&mut f, "pin").unwrap();
    let site = f.state().stage.pin_of(&pin).unwrap().screen_pos;
    f.client(id)
        .create_subsurface(&pin_surface, (100, 80), (120, 90));
    f.double_roundtrip(id);

    let screen = Point::from((site.x as f64 + 150.0, site.y as f64 + 120.0));
    let canvas = to_canvas(&mut f, screen);
    let focus = f
        .state()
        .pointer_focus_under(screen, canvas)
        .map(|(t, _)| t.0);
    assert!(
        focus
            .as_ref()
            .is_some_and(|s| *s != server_surface(&pin) && get_parent(s).is_some()),
        "precondition: the probe point must land on the pin's subsurface, got {focus:?}"
    );

    map_window(&mut f, id, "beneath", (400, 300));
    let beneath = window_by_app_id(&mut f, "beneath").unwrap();
    place(
        &mut f,
        &beneath,
        Point::from((canvas.x as i32 - 80, canvas.y as i32 - 70)),
    );
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&beneath, serial);
    assert!(
        !is_activated(&pin),
        "precondition: the pin starts unfocused"
    );

    f.state().warp_pointer(canvas);
    f.state().maybe_hover_focus(canvas);

    assert!(
        is_activated(&pin),
        "hovering the pin's subsurface must focus the pin"
    );
    assert!(!is_activated(&beneath));
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

/// 1:1 canvas↔screen (camera origin, zoom 1), so a canvas point in a scenario
/// names the screen pixel the cursor rests on and a pan reads as a pure canvas
/// shift. Clears the placement camera target too, or a settling animation
/// would move the camera underneath the scenario.
fn origin_view(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.camera_target = None;
        os.zoom = 1.0;
        os.zoom_target = None;
    });
}

/// Pan the viewport by `delta` and carry the cursor along, exactly as
/// `apply_scroll_momentum` and the axis/gesture pan arms do — the two halves
/// are inseparable here, since the pull hit-tests at the cursor's canvas
/// position and a camera move alone would leave it pointing at the old canvas.
fn pan_viewport(f: &mut Fixture, delta: Point<f64, Logical>) {
    let applied = f.state().drift_pan(delta, 0);
    let pos = f.state().seat.get_pointer().unwrap().current_location();
    f.state().warp_pointer(pos + applied);
}

/// The report this covers, end to end: two fingers pan the viewport, the cursor
/// holds its screen pixel, and the window that was under it slides away while
/// another takes its place — so `focus_follows_mouse` has to follow it there.
/// Client pointer focus already lands there (the axis arm dispatches it
/// synchronously), which is why the keyboard used to sit on the window the pan
/// carried away until the cursor was jiggled. Driven by real scroll events, so
/// the whole chain runs: the axis pan, its cursor carry, and the pull.
#[test]
fn a_trackpad_pan_under_a_resting_cursor_moves_sloppy_focus() {
    let mut f = Fixture::with_config(config(
        r#"
focus_follows_mouse = true

[navigation]
trackpad_speed = 100.0
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // Both start below the viewport, so only a pan can put them under the
    // cursor: at 15 px of finger scroll per notch and this speed, one notch is
    // 1500 canvas px — the first walks the canvas up onto `a`, the next onto
    // `b`.
    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    place(&mut f, &a, Point::from((0, 2300)));
    let b_surface = map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    place(&mut f, &b, Point::from((0, 3800)));
    origin_view(&mut f);

    let trackpad = FakeDevice::touchpad();
    // The cursor rests on bare canvas. That is also the only place an
    // unmodified two-finger scroll pans from, so the first notch has to come
    // from here; `recent_pan` stickiness is what keeps the second one panning
    // once `a` is under the cursor. Whatever mapping left focused, the pan has
    // to move it — which is the whole assertion.
    pointer_to(&mut f, &FakeDevice::mouse(), Point::from((200.0, 950.0)));
    f.double_roundtrip(id);
    assert!(
        !is_activated(&a),
        "test setup bug: neither window is under the resting cursor yet"
    );

    trackpad_scroll(&mut f, &trackpad);
    f.double_roundtrip(id);
    assert!(
        is_activated(&a),
        "test setup bug: the first notch puts a under the resting cursor, so a takes focus"
    );
    f.client(id).window(&b_surface).format_recent_configures();

    // The cursor never moves. This notch slides `a` off it and `b` onto it.
    trackpad_scroll(&mut f, &trackpad);
    f.double_roundtrip(id);

    assert!(
        is_activated(&b) && !is_activated(&a),
        "a viewport move must carry sloppy focus onto the window now under the \
         cursor, got a={} b={}",
        is_activated(&a),
        is_activated(&b)
    );
    let b_configures = f.client(id).window(&b_surface).format_recent_configures();
    assert!(
        b_configures.contains("Activated"),
        "the pan must flush an Activated configure to the window it focused, got:\n{b_configures}"
    );
}

/// The same pan with `focus_follows_mouse` off (the default): client pointer
/// focus follows the viewport, the keyboard stays where the last click put it.
#[test]
fn a_viewport_move_alone_leaves_focus_alone_with_sloppy_focus_off() {
    let mut f = Fixture::with_config(config(""));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    place(&mut f, &a, Point::from((0, 0)));
    map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    place(&mut f, &b, Point::from((0, 1500)));
    origin_view(&mut f);

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.double_roundtrip(id);
    assert!(is_activated(&a));

    pointer_to(&mut f, &FakeDevice::mouse(), Point::from((200.0, 150.0)));
    f.double_roundtrip(id);
    pan_viewport(&mut f, Point::from((0.0, 1500.0)));
    f.double_roundtrip(id);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&a)),
        "focus_follows_mouse=false must leave the keyboard on the clicked window"
    );
    assert!(!is_activated(&b));
}

/// A pan that leaves the same window under the cursor must not churn focus: no
/// re-activation, no configure on the wire. The hover pull is gated on the
/// viewport having moved, so this is the half of that gate the pan scenario
/// above can't see — the cursor still rests on `a`, and `a` keeps the hint it
/// already had.
#[test]
fn a_pan_that_keeps_the_same_window_under_the_cursor_sends_no_focus_change() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    place(&mut f, &a, Point::from((0, 0)));
    map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    place(&mut f, &b, Point::from((2000, 0)));
    origin_view(&mut f);

    pointer_to(&mut f, &FakeDevice::mouse(), Point::from((200.0, 150.0)));
    f.double_roundtrip(id);
    assert!(is_activated(&a));
    // Drain the hover that just landed, so only a focus change can show up.
    f.client(id).window(&a_surface).format_recent_configures();

    pan_viewport(&mut f, Point::from((0.0, 40.0)));
    f.double_roundtrip(id);

    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&a)));
    assert!(!is_activated(&b));
    let configures = f.client(id).window(&a_surface).format_recent_configures();
    assert!(
        configures.is_empty(),
        "a pan that leaves focus where it was must not re-activate anything, got:\n{configures}"
    );
}

/// A suspended stand-in yields no client pointer focus at all, so the pan that
/// brings one under the cursor changes nothing the pull would put on the wire —
/// its delivery dedup skips the dispatch — and sloppy focus still has to move
/// onto it. Pins the hover pull as its own step rather than a tail of the
/// pointer dispatch.
#[test]
fn a_pan_onto_a_suspended_window_moves_sloppy_focus_onto_it() {
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
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((0, 1500)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    // Inserting the stand-in activates it too (a no-op on the wire, no
    // toplevel), so re-focus a to make the assertions isolate the pan.
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.double_roundtrip(id);
    assert!(is_activated(&a));

    pointer_to(&mut f, &FakeDevice::mouse(), Point::from((200.0, 150.0)));
    f.double_roundtrip(id);
    pan_viewport(&mut f, Point::from((0.0, 1500.0)));
    f.double_roundtrip(id);

    let intent = f.state().window_focus.clone();
    assert!(
        matches!(intent, Some(FocusIntent::Suspended(s)) if s == sid),
        "a pan onto a stand-in must set the Suspended focus intent, got: {intent:?}"
    );
    assert!(
        !is_activated(&a),
        "the window the pan carried away must lose the Activated hint"
    );

    f.state().dismiss_suspended(sid);
}

/// The shared setup of the navigation scenarios below: `a` holds the
/// keyboard at the left edge of the canvas, `b` sits far off to the right as a
/// navigation target, and the cursor rests on bare canvas at (100, 150).
fn park_for_navigation(f: &mut Fixture, id: ClientId) -> (Window, Window) {
    map_window(f, id, "a", (400, 300));
    let a = window_by_app_id(f, "a").unwrap();
    place(f, &a, Point::from((0, 600)));
    map_window(f, id, "b", (400, 300));
    let b = window_by_app_id(f, "b").unwrap();
    place(f, &b, Point::from((4000, 600)));
    origin_view(f);

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.double_roundtrip(id);
    assert_eq!(keyboard_focus(f), Some(server_surface(&a)));

    pointer_to(f, &FakeDevice::mouse(), Point::from((100.0, 150.0)));
    f.double_roundtrip(id);
    (a, b)
}

/// The third window, placed exactly where centering `b` leaves the resting
/// cursor: `navigate_to_window_on` aims `b`'s center at the usable-area center,
/// so the viewport lands at (3240, 210) and the cursor — pinned to its screen
/// pixel by the animation's own warp — reads canvas (3340, 360) from there on.
fn place_decoy_under_the_parked_cursor(f: &mut Fixture, id: ClientId) -> Window {
    map_window(f, id, "decoy", (400, 300));
    let decoy = window_by_app_id(f, "decoy").unwrap();
    place(f, &decoy, Point::from((3200, 250)));
    origin_view(f);
    decoy
}

/// An activation pans the camera to the window it just focused, which is one
/// of the moves the maintainer's report names: the animation carries the
/// resting cursor onto a third window, and the first pull after it used to hand
/// that window the keyboard the activation had just given to `b`.
#[test]
fn an_activation_that_panned_keeps_the_keyboard_under_a_resting_cursor() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (_a, b) = park_for_navigation(&mut f, id);
    let decoy = place_decoy_under_the_parked_cursor(&mut f, id);

    f.state().activate_window_output_local(&b);
    settle(&mut f);
    f.double_roundtrip(id);
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&b)));

    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&b)),
        "a navigation's own camera move must not hand its keyboard to the window \
         it slid under the resting cursor"
    );
    assert!(is_activated(&b) && !is_activated(&decoy));
}

/// The other half of that gate: a camera move the user drives brings sloppy
/// focus back, and here the cursor rests on the very decoy the activation left
/// there — so the keyboard must go to it once the user starts panning.
#[test]
fn a_pan_after_a_navigation_hands_the_keyboard_to_the_cursor() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (_a, b) = park_for_navigation(&mut f, id);
    let decoy = place_decoy_under_the_parked_cursor(&mut f, id);

    f.state().activate_window_output_local(&b);
    settle(&mut f);
    f.double_roundtrip(id);
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&b)));

    pan_viewport(&mut f, Point::from((50.0, 0.0)));
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&decoy)),
        "once the user pans, the viewport is theirs again and sloppy focus owes \
         the keyboard to whatever rests under the cursor"
    );
    assert!(is_activated(&decoy) && !is_activated(&b));
}

/// A suspended stand-in is centered by the same pair of calls from the other
/// side: `center_on_suspended` sets the focus intent and pans. The gate has to
/// hold for an intent too — a stand-in holds no seat focus, so a pull that ran
/// after that pan would put the keyboard on a client window instead.
#[test]
fn a_centering_navigation_to_a_stand_in_keeps_the_focus_intent() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    place(&mut f, &a, Point::from((0, 600)));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((4000, 600)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let decoy = place_decoy_under_the_parked_cursor(&mut f, id);

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.double_roundtrip(id);
    pointer_to(&mut f, &FakeDevice::mouse(), Point::from((100.0, 150.0)));
    f.double_roundtrip(id);

    f.state().center_on_suspended(sid, true);
    settle(&mut f);
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    let intent = f.state().window_focus.clone();
    assert!(
        matches!(intent, Some(FocusIntent::Suspended(s)) if s == sid),
        "a pan to a stand-in must keep its focus intent out of the window it slid \
         under the cursor, got: {intent:?}"
    );
    assert!(!is_activated(&decoy));

    f.state().dismiss_suspended(sid);
}

/// A focus-nearest step whose target is clipped: `a` (300×200, focused) sits
/// one snap gap left of `b`, which hangs past the right edge, and the cursor
/// rests on bare canvas at screen (100, 20). The step walks within the cluster,
/// panning right just far enough to show `b`, which carries the cursor's canvas
/// point onto `decoy`. The decoy sits left of `a`, outside the search cone, so
/// the step can't pick it itself.
fn step_onto_a_clipped_window_past_a_decoy(f: &mut Fixture, id: ClientId) -> (Window, Window) {
    let gap = f.state().config.snap_gap as i32;
    map_window(f, id, "a", (300, 200));
    let a = window_by_app_id(f, "a").unwrap();
    place(f, &a, Point::from((1700 - gap - 300, 800)));
    map_window(f, id, "b", (400, 200));
    let b = window_by_app_id(f, "b").unwrap();
    place(f, &b, Point::from((1700, 800)));
    map_window(f, id, "decoy", (300, 240));
    let decoy = window_by_app_id(f, "decoy").unwrap();
    place(f, &decoy, Point::from((200, -200)));
    origin_view(f);
    // The search runs from `landing_view()`, which an anchor left armed by the
    // last map would move off the origin view.
    f.state()
        .with_output_state(|os| os.zoom_animation_anchor = None);

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.double_roundtrip(id);
    pointer_to(f, &FakeDevice::mouse(), Point::from((100.0, 20.0)));
    f.double_roundtrip(id);
    // Each map's auto-navigate parked the hover pull; a user pan re-arms it so
    // the step below has to park it again.
    pan_viewport(f, Point::from((0.0, -40.0)));
    f.double_roundtrip(id);
    assert_eq!(keyboard_focus(f), Some(server_surface(&a)));

    f.state()
        .execute_action(&Action::FocusNearest(Direction::Right));
    assert_eq!(
        f.state().focused_window(),
        Some(b.clone()),
        "test setup bug: the step must pick b, not the decoy"
    );
    settle(f);
    f.double_roundtrip(id);
    let cursor = f.state().seat.get_pointer().unwrap().current_location();
    assert!(
        Rectangle::new(Point::from((200.0, -200.0)), Size::from((300.0, 240.0))).contains(cursor),
        "test setup bug: the step's pan must leave the cursor on the decoy, got {cursor:?}"
    );
    (b, decoy)
}

/// focus-nearest's walking step pans through its own minimal reveal, not
/// `navigate_to_window_on`, so it parks the hover pull itself: the decoy it
/// slid under the resting cursor must not take the keyboard from `b`.
#[test]
fn a_focus_nearest_step_that_panned_keeps_the_keyboard_under_a_resting_cursor() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (b, decoy) = step_onto_a_clipped_window_past_a_decoy(&mut f, id);

    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&b)),
        "a focus-nearest step's own pan must not hand its keyboard to the window \
         it slid under the resting cursor"
    );
    assert!(is_activated(&b) && !is_activated(&decoy));
}

#[test]
fn a_pan_after_a_focus_nearest_step_hands_the_keyboard_to_the_cursor() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (b, decoy) = step_onto_a_clipped_window_past_a_decoy(&mut f, id);

    pan_viewport(&mut f, Point::from((50.0, 0.0)));
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&decoy)));
    assert!(is_activated(&decoy) && !is_activated(&b));
}

/// A mouse-wheel zoom is a user-driven camera move: after an activation parked
/// the hover pull, it brings it back and the decoy under the cursor takes the
/// keyboard. The zoom anchors on the cursor, so the decoy stays under it.
#[test]
fn a_wheel_zoom_after_a_navigation_hands_the_keyboard_to_the_cursor() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (_a, b) = park_for_navigation(&mut f, id);
    let decoy = place_decoy_under_the_parked_cursor(&mut f, id);

    f.state().activate_window_output_local(&b);
    settle(&mut f);
    f.double_roundtrip(id);
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&b)));

    key_press(&mut f, KEY_LEFTMETA);
    wheel_notch_down(&mut f, &FakeDevice::mouse());
    key_release(&mut f, KEY_LEFTMETA);
    assert!(
        f.state().zoom_target().is_some_and(|z| z < 1.0),
        "test setup bug: mod+wheel must arm a zoom-out"
    );
    settle(&mut f);
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&decoy)));
    assert!(is_activated(&decoy) && !is_activated(&b));
}

/// The touchscreen pinch used to hand the keyboard to the decoy exactly like
/// the wheel zoom above — until the parked-cursor report did: touch hides the
/// pointer and leaves it parked, so the pull behind both scenarios declines.
/// Sloppy focus follows the pointer, and the pointer is not the device in
/// play; the keyboard stays where the navigation put it. The fingers land on
/// bare canvas below the windows.
#[test]
fn a_touch_pinch_after_a_navigation_keeps_the_keyboard_on_its_window() {
    let mut f = Fixture::with_config(config(
        r#"
focus_follows_mouse = true

[touch.on-canvas]
"2-finger-swipe" = "none"
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (_a, b) = park_for_navigation(&mut f, id);
    let decoy = place_decoy_under_the_parked_cursor(&mut f, id);

    f.state().activate_window_output_local(&b);
    settle(&mut f);
    f.double_roundtrip(id);
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&b)));

    touch_down(&mut f, Point::from((3540.0, 1110.0)), 0);
    touch_down(&mut f, Point::from((4140.0, 1110.0)), 1);
    for step in 1..=3 {
        let inset = 60.0 * f64::from(step);
        touch_motion(&mut f, Point::from((3540.0 + inset, 1110.0)), 0);
        touch_motion(&mut f, Point::from((4140.0 - inset, 1110.0)), 1);
    }
    touch_up(&mut f, 0);
    touch_up(&mut f, 1);
    assert!(
        f.state().zoom() < 1.0,
        "test setup bug: the pinch must zoom out"
    );
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&b)),
        "a touch pinch must leave the keyboard where the navigation put it, \
         not on the window under the parked cursor"
    );
    assert!(
        is_activated(&b) && !is_activated(&decoy),
        "the decoy under the parked cursor must stay unactivated, got \
         b={} decoy={}",
        is_activated(&b),
        is_activated(&decoy)
    );
}

/// The report this covers: a finger flicks the viewport while the cursor sits
/// parked where touch left it — hidden, unsteered. The coast carries the
/// cursor along with the camera, so windows slide under that spot while the
/// finger is elsewhere, and the per-frame pull used to hand each of them the
/// keyboard: sloppy focus follows the pointer, and the pointer is not the
/// device the user is holding. Touch's own focus comes from taps, at the
/// finger. (The trackpad pan above keeps the pull, deliberately — there the
/// cursor is live, merely resting.) Hover focus at the cursor resumes the
/// moment real pointer motion clears the hidden flag.
#[test]
fn a_touch_pan_under_a_parked_cursor_leaves_sloppy_focus_alone() {
    let mut f = Fixture::with_config(config(
        r#"
focus_follows_mouse = true

[navigation]
touch_speed = 3.0
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    place(&mut f, &a, Point::from((0, 600)));
    map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    // Below the viewport: only the pan can carry it under the cursor.
    place(&mut f, &b, Point::from((0, 1300)));
    // On-screen at origin and clear of the finger's lane, for the resume half.
    map_window(&mut f, id, "c", (300, 200));
    let c = window_by_app_id(&mut f, "c").unwrap();
    place(&mut f, &c, Point::from((1000, 400)));
    origin_view(&mut f);

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.double_roundtrip(id);
    pointer_to(&mut f, &FakeDevice::mouse(), Point::from((100.0, 150.0)));
    f.double_roundtrip(id);
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&a)));
    assert!(
        !is_activated(&b),
        "test setup bug: b starts below the viewport"
    );

    // One finger flicks up through bare canvas: 360 screen px over 80 ms at
    // speed 3.0 — 1080 canvas px of pan, then a coast. The coast is what the
    // report bites on: it moves the camera with no grab live, so every
    // frame's pointer-focus refresh re-runs the pull, and the warp the coast
    // carries the cursor through slides new windows under that spot.
    // Timestamps are pinned to a real flick's spacing — the fake clock's
    // 1 ms step would read this drag at ~16× real speed and bank a coast of
    // thousands of pixels.
    let lane = 700.0;
    touch_down_screen_at(&mut f, Point::from((lane, 950.0)), 0, 0);
    for (y, t) in [
        (940.0, 16),
        (820.0, 32),
        (720.0, 48),
        (650.0, 64),
        (610.0, 80),
        (590.0, 96),
    ] {
        touch_motion_screen_at(&mut f, Point::from((lane, y)), 0, t);
    }
    touch_up(&mut f, 0);
    f.double_roundtrip(id);

    // Momentum is driven the way the render loop drives it: one tick of
    // coast, one pointer-focus refresh, per frame. The coast warps the parked
    // cursor along with it, so windows slide under that spot as it travels —
    // stop the drive at the tick the cursor's canvas point lands on `b`: the
    // report is about exactly that moment, and stopping there keeps the
    // landing deterministic.
    let mut on_b = false;
    for _ in 0..MAX_TICKS {
        f.state().apply_scroll_momentum(TICK);
        f.state().refresh_pointer_focus();
        let y = f.state().seat.get_pointer().unwrap().current_location().y;
        if (1300.0..=1600.0).contains(&y) {
            on_b = true;
            break;
        }
    }
    assert!(
        on_b,
        "test setup bug: the coast must carry the parked cursor onto b, got \
         cursor at canvas {:?}",
        f.state().seat.get_pointer().unwrap().current_location()
    );
    f.double_roundtrip(id);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&a)),
        "a touch pan must not hand the keyboard to the window under the \
         parked cursor"
    );
    assert!(
        is_activated(&a) && !is_activated(&b),
        "the window slid under the parked cursor must stay unactivated, got \
         a={} b={}",
        is_activated(&a),
        is_activated(&b)
    );

    // The pointer coming back resumes sloppy focus at its live position. The
    // coast left the view far from home, so it comes back first; the hover
    // then lands on `c` the way any hover would — proof the touch-hidden flag
    // no longer holds the pull back.
    origin_view(&mut f);
    pointer_to(&mut f, &FakeDevice::mouse(), Point::from((1100.0, 500.0)));
    f.double_roundtrip(id);
    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&c)),
        "real pointer motion must clear the touch-hidden flag and hover \
         focus at the cursor again"
    );
    assert!(is_activated(&c));
}

/// fit-window focuses the window it fits and flies the camera to it, so it
/// parks the hover pull as a navigation does. `a` sits off to the right; the
/// flight sweeps the resting cursor across `b` on the way, and a pull taken
/// mid-flight must leave the keyboard on `a`.
#[test]
fn a_fit_window_flight_keeps_the_keyboard_under_a_resting_cursor() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    place(&mut f, &a, Point::from((2000, 400)));
    map_window(&mut f, id, "b", (300, 250));
    let b = window_by_app_id(&mut f, "b").unwrap();
    place(&mut f, &b, Point::from((700, 0)));
    origin_view(&mut f);
    f.state()
        .with_output_state(|os| os.zoom_animation_anchor = None);

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.double_roundtrip(id);
    pointer_to(&mut f, &FakeDevice::mouse(), Point::from((100.0, 140.0)));
    f.double_roundtrip(id);
    // A user pan re-arms the hover pull the maps parked.
    pan_viewport(&mut f, Point::from((0.0, -40.0)));
    f.double_roundtrip(id);
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&a)));

    f.state().execute_action(&Action::FitWindow);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);
    let b_rect = Rectangle::new(Point::from((700.0, 0.0)), Size::from((300.0, 250.0)));
    let mut over_b = false;
    for _ in 0..MAX_TICKS {
        // The fit holds its pan until the client commits the fitted size;
        // the window-animation tick is what then releases it.
        f.state().tick_window_animations(TICK);
        f.state().apply_zoom_animation(TICK);
        f.state().apply_camera_animation(TICK);
        let cursor = f.state().seat.get_pointer().unwrap().current_location();
        if b_rect.contains(cursor) {
            over_b = true;
            break;
        }
    }
    assert!(
        over_b,
        "test setup bug: the flight must carry the cursor across b"
    );
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&a)),
        "a fit's own camera flight must not hand the keyboard to a window it \
         sweeps under the resting cursor"
    );
    assert!(is_activated(&a) && !is_activated(&b));
}

/// A finger lift after a navigation launches nothing, since the navigation
/// cleared the momentum tracker. That empty launch must not count as a user
/// camera move: the activation's flight keeps the keyboard on `b`.
#[test]
fn a_finger_lift_after_a_navigation_leaves_its_flight_alone() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (_a, b) = park_for_navigation(&mut f, id);
    let decoy = place_decoy_under_the_parked_cursor(&mut f, id);

    f.state().activate_window_output_local(&b);
    f.state().launch_momentum();
    settle(&mut f);
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&b)));
    assert!(is_activated(&b) && !is_activated(&decoy));
}

/// A restore that moves nothing re-checks nothing: `zoom-reset` at zoom 1.0
/// leaves the view where the activation put it, so the keyboard stays on `b`
/// even though the decoy rests under the cursor.
#[test]
fn a_restore_that_moves_nothing_keeps_a_navigations_focus() {
    let mut f = Fixture::with_config(config("focus_follows_mouse = true\n"));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (_a, b) = park_for_navigation(&mut f, id);
    let decoy = place_decoy_under_the_parked_cursor(&mut f, id);

    f.state().activate_window_output_local(&b);
    settle(&mut f);
    f.double_roundtrip(id);
    let view = (f.state().camera(), f.state().zoom());

    f.state().execute_action(&Action::ZoomReset);
    settle(&mut f);
    assert_eq!(
        (f.state().camera(), f.state().zoom()),
        view,
        "test setup bug: zoom-reset at 1.0 must leave the view put"
    );
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&b)));
    assert!(is_activated(&b) && !is_activated(&decoy));
}

const ZOOM_OUT_CONFIG: &str = r#"
focus_follows_mouse = true

[zoom]
step = 1.5
"#;

/// `a` at the left, `b` level with it to the right, `decoy` up-left of both and
/// outside the search cone from `a`. `b` is fully visible both at the resting
/// view and where [`rest_and_zoom_out`] lands, so nothing that focuses it
/// pans to it.
fn map_past_a_decoy(f: &mut Fixture, id: ClientId) -> (Window, Window, Window) {
    map_window(f, id, "a", (300, 200));
    let a = window_by_app_id(f, "a").unwrap();
    place(f, &a, Point::from((0, 600)));
    map_window(f, id, "b", (300, 200));
    let b = window_by_app_id(f, "b").unwrap();
    place(f, &b, Point::from((1500, 600)));
    map_window(f, id, "decoy", (300, 200));
    let decoy = window_by_app_id(f, "decoy").unwrap();
    place(f, &decoy, Point::from((-450, -350)));
    (a, b, decoy)
}

/// Focus `focused`, rest the cursor on bare canvas at screen (100, 20) and set
/// a keyboard zoom-out flying. The step of 1.5 lands at 2/3, above
/// `interact_min`, and carries the cursor's canvas point a few hundred px
/// up-left onto the decoy.
fn rest_and_zoom_out(f: &mut Fixture, id: ClientId, focused: &Window) {
    origin_view(f);
    f.state()
        .with_output_state(|os| os.zoom_animation_anchor = None);

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(focused, serial);
    f.double_roundtrip(id);
    pointer_to(f, &FakeDevice::mouse(), Point::from((100.0, 20.0)));
    f.double_roundtrip(id);
    // Each map's auto-navigate parked the hover pull; a user pan re-arms it.
    pan_viewport(f, Point::from((0.0, -40.0)));
    f.double_roundtrip(id);
    assert_eq!(keyboard_focus(f), Some(server_surface(focused)));

    let resting = f.state().zoom_target();
    f.state().execute_action(&Action::ZoomOut);
    assert_ne!(
        f.state().zoom_target(),
        resting,
        "test setup bug: zoom-out must arm a flight"
    );
}

/// A keyboard zoom-out under way from `a`'s focus.
fn zoom_out_past_a_decoy(f: &mut Fixture, id: ClientId) -> (Window, Window) {
    let (a, b, decoy) = map_past_a_decoy(f, id);
    rest_and_zoom_out(f, id, &a);
    (b, decoy)
}

/// Run the zoom to its landing, which leaves the resting cursor on `decoy`.
fn land_the_zoom_on(f: &mut Fixture, id: ClientId, decoy: &Window) {
    settle(f);
    f.double_roundtrip(id);
    let cursor = f.state().seat.get_pointer().unwrap().current_location();
    let decoy_rect = Rectangle::new(
        f.state().stage.position_of(decoy).unwrap().to_f64(),
        decoy.geometry().size.to_f64(),
    );
    assert!(
        decoy_rect.contains(cursor),
        "test setup bug: the zoom must land the cursor on the decoy, got {cursor:?}"
    );
}

/// A focus-nearest step onto a window the zoom shows whole sets focus with no
/// pan of its own. The zoom keeps flying, and its remaining frames must not
/// hand the keyboard to the decoy they land under the resting cursor.
#[test]
fn a_focus_nearest_step_with_no_pan_keeps_the_keyboard_through_a_zoom() {
    let mut f = Fixture::with_config(config(ZOOM_OUT_CONFIG));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (b, decoy) = zoom_out_past_a_decoy(&mut f, id);

    f.state()
        .execute_action(&Action::FocusNearest(Direction::Right));
    assert_eq!(
        f.state().focused_window(),
        Some(b.clone()),
        "test setup bug: the step must pick b, not the decoy"
    );
    land_the_zoom_on(&mut f, id, &decoy);
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&b)),
        "a zoom still under way must not hand a focus-nearest step's keyboard \
         to the window it lands under the resting cursor"
    );
    assert!(is_activated(&b) && !is_activated(&decoy));
}

#[test]
fn a_pan_after_a_zoom_a_focus_nearest_step_rode_hands_the_keyboard_to_the_cursor() {
    let mut f = Fixture::with_config(config(ZOOM_OUT_CONFIG));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (b, decoy) = zoom_out_past_a_decoy(&mut f, id);

    f.state()
        .execute_action(&Action::FocusNearest(Direction::Right));
    assert_eq!(f.state().focused_window(), Some(b.clone()));
    land_the_zoom_on(&mut f, id, &decoy);
    pan_viewport(&mut f, Point::from((50.0, 0.0)));
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&decoy)));
    assert!(is_activated(&decoy) && !is_activated(&b));
}

/// An activation of a window already in full view focuses it without a pan,
/// and must hold that focus through the zoom.
#[test]
fn a_visible_activation_keeps_the_keyboard_through_a_zoom() {
    let mut f = Fixture::with_config(config(ZOOM_OUT_CONFIG));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (b, decoy) = zoom_out_past_a_decoy(&mut f, id);

    f.state().activate_window_output_local(&b);
    assert_eq!(f.state().focused_window(), Some(b.clone()));
    land_the_zoom_on(&mut f, id, &decoy);
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&b)));
    assert!(is_activated(&b) && !is_activated(&decoy));
}

/// Close a focused `c` placed at `c_pos` while the zoom is under way;
/// close-follow lands on `b` without a pan of its own.
fn close_during_a_zoom_out(f: &mut Fixture, id: ClientId, c_pos: (i32, i32)) -> (Window, Window) {
    let (_a, b, decoy) = map_past_a_decoy(f, id);
    let c_surface = map_window(f, id, "c", (100, 100));
    let c = window_by_app_id(f, "c").unwrap();
    place(f, &c, Point::from(c_pos));
    // Close-follow measures `c` by its cached rect, still where it mapped.
    f.state()
        .refresh_stable_snap_rect(&StageWindow::Client(c.clone()));
    rest_and_zoom_out(f, id, &c);

    f.client(id).window(&c_surface).destroy();
    f.double_roundtrip(id);
    assert_eq!(
        f.state().focused_window(),
        Some(b.clone()),
        "test setup bug: close-follow must land on b"
    );
    (b, decoy)
}

/// Close-follow onto the window `c` sat inside, which is in full view.
#[test]
fn a_close_following_onto_a_window_in_view_keeps_the_keyboard_through_a_zoom() {
    let mut f = Fixture::with_config(config(ZOOM_OUT_CONFIG));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (b, decoy) = close_during_a_zoom_out(&mut f, id, (1550, 650));
    land_the_zoom_on(&mut f, id, &decoy);
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&b)));
    assert!(is_activated(&b) && !is_activated(&decoy));
}

/// With nothing related to `c` to follow, the fallback focuses the nearest
/// visible window.
#[test]
fn a_close_falling_back_to_the_nearest_window_keeps_the_keyboard_through_a_zoom() {
    let mut f = Fixture::with_config(config(ZOOM_OUT_CONFIG));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (b, decoy) = close_during_a_zoom_out(&mut f, id, (1100, 300));
    land_the_zoom_on(&mut f, id, &decoy);
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&b)));
    assert!(is_activated(&b) && !is_activated(&decoy));
}

/// focus-center with the cursor on a pin's resize band focuses the pin and
/// moves no camera. Hover skips the band for the canvas window behind it, so
/// the zoom's landing must not hand that window the keyboard.
#[test]
fn focus_center_on_a_pins_resize_band_keeps_the_pin_through_a_zoom() {
    let mut f = Fixture::with_config(config(
        r#"
focus_follows_mouse = true

[zoom]
step = 1.5

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
    map_window(&mut f, id, "beneath", (600, 500));
    let beneath = window_by_app_id(&mut f, "beneath").unwrap();
    place(&mut f, &beneath, Point::from((500, 300)));
    origin_view(&mut f);
    f.state()
        .with_output_state(|os| os.zoom_animation_anchor = None);
    let output = f.state().stage.pin_of(&pin).unwrap().output.clone();
    f.state().stage.set_pin(
        &pin,
        PinnedSite {
            output,
            screen_pos: Point::from((800, 400)),
        },
    );

    let band = Point::from((796.0, 550.0));
    assert!(
        matches!(
            f.state().pinned_decoration_under(band),
            PinnedChrome::Hit(ref w, DecorationHit::ResizeBorder(_)) if *w == pin
        ),
        "test setup bug: the cursor must rest on the pin's resize band"
    );
    let canvas = to_canvas(&mut f, band);
    pointer_to(&mut f, &FakeDevice::mouse(), canvas);
    f.double_roundtrip(id);

    f.state().execute_action(&Action::ZoomOut);
    f.state().execute_action(&Action::FocusCenter);
    assert_eq!(
        f.state().focused_window(),
        Some(pin.clone()),
        "test setup bug: focus-center must pick the pin"
    );
    settle(&mut f);
    f.double_roundtrip(id);
    let cursor = f.state().seat.get_pointer().unwrap().current_location();
    assert!(
        Rectangle::new(Point::from((500.0, 300.0)), Size::from((600.0, 500.0))).contains(cursor),
        "test setup bug: the zoom must land the cursor over beneath, got {cursor:?}"
    );
    f.state().refresh_pointer_focus();
    f.double_roundtrip(id);

    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&pin)));
    assert!(is_activated(&pin) && !is_activated(&beneath));
}
