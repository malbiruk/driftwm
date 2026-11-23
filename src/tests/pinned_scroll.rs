//! A pinned window drawn over a stand-in owns the scroll. The swallow for
//! unbound scroll over a stand-in is a canvas-space test, so without a
//! screen-space exception it sees only the stand-in beneath the pin. Below
//! `interact_min` the retry that pans a bare scroll over a canvas window is
//! canvas-space too, and takes the same exception.

use smithay::utils::{Logical, Point, Size};

use super::client::ClientId;
use super::input_backend::{FakeDevice, pointer_to_screen, trackpad_scroll};
use super::{Fixture, config, map_window, tick_until_settled, to_canvas, window_by_app_id};
use crate::state::StageWindow;

const PICK_MODE_RULES: &str = r#"
[zoom]
interact_min = 0.5

[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [320, 240]
"#;

const UNDER_SIZE: (u16, u16) = (400, 300);

/// A pin at its default site and a second client's "under" window, in pick
/// mode (zoom 0.3 is below `interact_min`). Returns the pin's client, the
/// other client, and the screen point at the pin's centre.
fn pick_mode_scene() -> (Fixture, ClientId, ClientId, Point<f64, Logical>) {
    let mut f = Fixture::with_config(config(PICK_MODE_RULES));
    f.add_output(1, (1920, 1080));
    let pin_client = f.add_client();
    let other_client = f.add_client();
    map_window(&mut f, pin_client, "pin", (320, 240));
    tick_until_settled(&mut f);
    map_window(&mut f, other_client, "under", UNDER_SIZE);
    tick_until_settled(&mut f);
    f.state().set_zoom(0.3);
    f.state().with_output_state(|os| {
        os.camera_target = None;
        os.zoom_target = None;
    });
    let pin = window_by_app_id(&mut f, "pin").unwrap();
    let site = f.state().stage.pin_of(&pin).cloned().unwrap();
    let inside = (site.screen_pos + Point::from((160, 120))).to_f64();
    (f, pin_client, other_client, inside)
}

/// Move the "under" window so its centre is at `screen`.
fn put_window_under(f: &mut Fixture, screen: Point<f64, Logical>) {
    let under = window_by_app_id(f, "under").unwrap();
    let centre = to_canvas(f, screen).to_i32_round();
    let half = Point::from((i32::from(UNDER_SIZE.0) / 2, i32::from(UNDER_SIZE.1) / 2));
    f.state()
        .map_window(StageWindow::Client(under), centre - half, false);
}

#[test]
fn scroll_over_a_pinned_window_reaches_it_through_a_stand_in_beneath() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [320, 240]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let client = f.add_client();
    map_window(&mut f, client, "pin", (320, 240));
    let pin = window_by_app_id(&mut f, "pin").unwrap();
    tick_until_settled(&mut f);
    let site = f.state().stage.pin_of(&pin).cloned().unwrap();
    let inside = (site.screen_pos + Point::from((160, 120))).to_f64();

    // A stand-in on the canvas under the pin's on-screen rect.
    let under = f.state().camera() + inside - Point::from((300.0, 200.0));
    let sid = f.state().insert_suspended_for_test(
        1,
        under.to_i32_round(),
        Size::from((600, 400)),
        "s",
        "S",
    );
    pointer_to_screen(&mut f, &FakeDevice::touchpad(), inside);
    f.double_roundtrip(client);
    let before = f.client(client).state.pointer_axes.len();

    trackpad_scroll(&mut f, &FakeDevice::touchpad());
    f.double_roundtrip(client);

    assert_eq!(
        f.client(client).state.pointer_axes[before..],
        [15.0],
        "the pinned window received the scroll"
    );

    f.state().dismiss_suspended(sid);
}

#[test]
fn below_interact_min_a_scroll_over_a_pin_above_a_canvas_window_reaches_the_pin() {
    let (mut f, pin_client, other_client, inside) = pick_mode_scene();
    put_window_under(&mut f, inside);
    pointer_to_screen(&mut f, &FakeDevice::touchpad(), inside);
    f.double_roundtrip(pin_client);
    f.double_roundtrip(other_client);
    let camera = f.state().camera();
    let before = f.client(pin_client).state.pointer_axes.len();

    trackpad_scroll(&mut f, &FakeDevice::touchpad());
    f.double_roundtrip(pin_client);

    assert_eq!(
        f.client(pin_client).state.pointer_axes[before..],
        [15.0],
        "the pinned window received the scroll"
    );
    f.double_roundtrip(other_client);
    assert!(
        f.client(other_client).state.pointer_axes.is_empty(),
        "the window beneath the pin received none of it"
    );
    assert_eq!(f.state().camera(), camera, "the canvas did not pan");
}

#[test]
fn below_interact_min_a_scroll_over_a_pin_above_a_stand_in_reaches_the_pin() {
    let (mut f, pin_client, _other_client, inside) = pick_mode_scene();
    // Out from under the pin, so the stand-in is all that lies beneath it.
    put_window_under(&mut f, Point::from((200.0, 800.0)));
    let centre = to_canvas(&mut f, inside);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from(((centre.x - 300.0) as i32, (centre.y - 200.0) as i32)),
        Size::from((600, 400)),
        "s",
        "S",
    );
    pointer_to_screen(&mut f, &FakeDevice::touchpad(), inside);
    f.double_roundtrip(pin_client);
    let camera = f.state().camera();
    let before = f.client(pin_client).state.pointer_axes.len();

    trackpad_scroll(&mut f, &FakeDevice::touchpad());
    f.double_roundtrip(pin_client);

    assert_eq!(
        f.client(pin_client).state.pointer_axes[before..],
        [15.0],
        "the pinned window received the scroll"
    );
    assert_eq!(f.state().camera(), camera, "the canvas did not pan");

    f.state().dismiss_suspended(sid);
}

#[test]
fn below_interact_min_a_bare_scroll_over_a_canvas_window_away_from_the_pin_pans() {
    let (mut f, _pin_client, _other_client, _inside) = pick_mode_scene();
    let away = Point::from((200.0, 800.0));
    put_window_under(&mut f, away);
    pointer_to_screen(&mut f, &FakeDevice::touchpad(), away);
    // Bare canvas pans too, so the window has to be what the pointer is over.
    let at = to_canvas(&mut f, away);
    assert!(
        f.state().pick_target_under(at).is_some(),
        "test setup bug: the window must sit under the pointer"
    );
    let camera = f.state().camera();

    trackpad_scroll(&mut f, &FakeDevice::touchpad());

    assert_ne!(f.state().camera(), camera, "the canvas panned");
}
