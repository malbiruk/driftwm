//! Back clicks stay in the client; Back drags stay in the compositor.
use super::input_backend::{FakeDevice, pointer_relative_motion, pointer_to, press, release};
use super::{Fixture, map_window, window_by_app_id};
use crate::state::StageWindow;
use driftwm::config::Config;
use smithay::utils::Point;

fn fixture(enabled: bool, zoom: f64) -> (Fixture, super::client::ClientId) {
    let mut f =
        Fixture::with_config(Config::from_toml(&format!("back_button_pan = {enabled}\n")).unwrap());
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();
    map_window(&mut f, id, "browser", (400, 300));
    let w = window_by_app_id(&mut f, "browser").unwrap();
    f.state()
        .map_window(StageWindow::Client(w), Point::from((0, 0)), true);
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.zoom = zoom;
    });
    pointer_to(&mut f, &FakeDevice::mouse(), Point::from((100.0, 70.0)));
    f.double_roundtrip(id);
    f.client(id).state.pointer_buttons.clear();
    (f, id)
}

#[test]
fn back_click_is_buffered_and_small_wobble_still_clicks() {
    for button in [275, 278] {
        let (mut f, id) = fixture(true, 1.0);
        let mouse = FakeDevice::mouse();
        let camera = f.state().camera();
        press(&mut f, &mouse, button);
        f.double_roundtrip(id);
        assert!(f.client(id).state.pointer_buttons.is_empty());
        pointer_relative_motion(&mut f, &mouse, Point::from((3.0, 0.0)));
        assert_eq!(f.state().camera(), camera);
        release(&mut f, &mouse, button);
        f.double_roundtrip(id);
        assert_eq!(
            f.client(id).state.pointer_buttons,
            vec![(button, 1), (button, 0)]
        );
        assert!(!f.state().panning());
    }
}

#[test]
fn back_drag_pans_at_any_zoom_and_never_sends_back() {
    for zoom in [1.0, 0.5] {
        let (mut f, id) = fixture(true, zoom);
        let mouse = FakeDevice::mouse();
        press(&mut f, &mouse, 275);
        pointer_relative_motion(&mut f, &mouse, Point::from((20.0, 0.0)));
        assert!(f.state().panning());
        assert!((f.state().camera().x + 20.0 / zoom).abs() < 1e-6);
        release(&mut f, &mouse, 275);
        f.double_roundtrip(id);
        assert!(f.client(id).state.pointer_buttons.is_empty());
        assert!(!f.state().panning());
    }
}

#[test]
fn disabled_back_pan_forwards_press_immediately() {
    let (mut f, id) = fixture(false, 1.0);
    let mouse = FakeDevice::mouse();
    press(&mut f, &mouse, 275);
    f.double_roundtrip(id);
    assert_eq!(f.client(id).state.pointer_buttons, vec![(275, 1)]);
    release(&mut f, &mouse, 275);
    f.double_roundtrip(id);
    assert_eq!(f.client(id).state.pointer_buttons, vec![(275, 1), (275, 0)]);
}

#[test]
fn another_button_preserves_back_click_pair() {
    let (mut f, id) = fixture(true, 1.0);
    let mouse = FakeDevice::mouse();
    press(&mut f, &mouse, 275);
    press(&mut f, &mouse, 272);
    release(&mut f, &mouse, 272);
    release(&mut f, &mouse, 275);
    f.double_roundtrip(id);
    assert_eq!(
        f.client(id).state.pointer_buttons,
        vec![(275, 1), (272, 1), (272, 0), (275, 0)]
    );
}

#[test]
fn releasing_back_before_another_button_does_not_send_orphan_releases() {
    let (mut f, id) = fixture(true, 1.0);
    let mouse = FakeDevice::mouse();
    press(&mut f, &mouse, 275);
    pointer_relative_motion(&mut f, &mouse, Point::from((20.0, 0.0)));
    press(&mut f, &mouse, 272);
    release(&mut f, &mouse, 275);
    assert!(!f.state().panning());
    release(&mut f, &mouse, 272);
    f.double_roundtrip(id);
    assert!(f.client(id).state.pointer_buttons.is_empty());
    assert!(!f.state().seat.get_pointer().unwrap().is_grabbed());
}

#[test]
fn modified_back_click_keeps_normal_client_handling() {
    use super::input_backend::{key_press, key_release};
    let (mut f, id) = fixture(true, 1.0);
    let mouse = FakeDevice::mouse();
    pointer_to(&mut f, &mouse, Point::from((200., 150.)));
    key_press(&mut f, 56); // Alt
    press(&mut f, &mouse, 275);
    f.double_roundtrip(id);
    assert_eq!(f.client(id).state.pointer_buttons, vec![(275, 1)]);
    pointer_relative_motion(&mut f, &mouse, Point::from((20., 0.)));
    assert_eq!(f.state().camera(), Point::from((0., 0.)));
    release(&mut f, &mouse, 275);
    key_release(&mut f, 56);
    f.double_roundtrip(id);
    assert_eq!(f.client(id).state.pointer_buttons, vec![(275, 1), (275, 0)]);
}
