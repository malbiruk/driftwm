//! A pinned window drawn over a stand-in owns the scroll. The swallow for
//! unbound scroll over a stand-in is a canvas-space test, so without a
//! screen-space exception it sees only the stand-in beneath the pin.

use smithay::utils::{Point, Size};

use super::input_backend::{FakeDevice, pointer_to_screen, trackpad_scroll};
use super::{Fixture, config, map_window, tick_until_settled, window_by_app_id};

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

    assert!(
        f.client(client).state.pointer_axes.len() > before,
        "the pinned window received the scroll"
    );

    f.state().dismiss_suspended(sid);
}
