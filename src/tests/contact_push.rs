//! Contact packing through real pointer dispatch, not direct geometry mutation.
use super::input_backend::{FakeDevice, key_press, key_release, pointer_to, press, release};
use super::{Fixture, map_window, settle, window_by_app_id};
use crate::state::StageWindow;
use driftwm::config::{BTN_LEFT, BTN_RIGHT, Config};
use smithay::utils::Point;

fn fixture(enabled: bool) -> Fixture {
    let config = Config::from_toml(&format!("[snap]\ncontact_push = {enabled}\n[decorations]\ndefault_mode = \"none\"\nborder_width = 0\n")).unwrap();
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();
    for (name, x) in [("driven", 0), ("second", 500), ("third", 1000)] {
        map_window(&mut f, id, name, (400, 300));
        let w = window_by_app_id(&mut f, name).unwrap();
        f.state()
            .map_window(StageWindow::Client(w), Point::from((x, 0)), false);
    }
    settle(&mut f);
    f.state().set_camera(Point::from((0., 0.)));
    f
}
fn x(f: &mut Fixture, name: &str) -> i32 {
    let w = window_by_app_id(f, name).unwrap();
    f.state().stage.position_of(&w).unwrap().x
}

#[test]
fn contact_push_move_discovers_unconnected_windows_and_reverses() {
    let mut f = fixture(true);
    let mouse = FakeDevice::mouse();
    pointer_to(&mut f, &mouse, Point::from((200., 150.)));
    key_press(&mut f, 56);
    press(&mut f, &mouse, BTN_LEFT);
    pointer_to(&mut f, &mouse, Point::from((850., 150.)));
    assert_eq!(x(&mut f, "driven"), 650);
    assert_eq!(x(&mut f, "second"), 1062);
    assert_eq!(x(&mut f, "third"), 1474);
    let w = window_by_app_id(&mut f, "second").unwrap();
    assert_eq!(w.geometry().size.w, 400);
    pointer_to(&mut f, &mouse, Point::from((200., 150.)));
    assert_eq!(x(&mut f, "second"), 500);
    assert_eq!(x(&mut f, "third"), 1000);
    release(&mut f, &mouse, BTN_LEFT);
    key_release(&mut f, 56);
}

#[test]
fn contact_push_disabled_keeps_original_overlap_behaviour() {
    let mut f = fixture(false);
    let mouse = FakeDevice::mouse();
    pointer_to(&mut f, &mouse, Point::from((200., 150.)));
    key_press(&mut f, 56);
    press(&mut f, &mouse, BTN_LEFT);
    pointer_to(&mut f, &mouse, Point::from((850., 150.)));
    assert_eq!(x(&mut f, "second"), 500);
    assert_eq!(x(&mut f, "third"), 1000);
    release(&mut f, &mouse, BTN_LEFT);
    key_release(&mut f, 56);
}

#[test]
fn contact_push_resize_discovers_neighbour_and_reverses() {
    let mut f = fixture(true);
    let mouse = FakeDevice::mouse();
    pointer_to(&mut f, &mouse, Point::from((390., 150.)));
    key_press(&mut f, 56);
    press(&mut f, &mouse, BTN_RIGHT);
    pointer_to(&mut f, &mouse, Point::from((590., 150.)));
    assert_eq!(x(&mut f, "second"), 612);
    assert_eq!(x(&mut f, "third"), 1024);
    pointer_to(&mut f, &mouse, Point::from((390., 150.)));
    assert_eq!(x(&mut f, "second"), 500);
    assert_eq!(x(&mut f, "third"), 1000);
    release(&mut f, &mouse, BTN_RIGHT);
    key_release(&mut f, 56);
}

#[test]
fn neighbour_pinned_mid_drag_is_no_longer_pushed() {
    let mut f = fixture(true);
    let mouse = FakeDevice::mouse();
    pointer_to(&mut f, &mouse, Point::from((200., 150.)));
    key_press(&mut f, 56);
    press(&mut f, &mouse, BTN_LEFT);
    pointer_to(&mut f, &mouse, Point::from((300., 150.)));
    let neighbour = window_by_app_id(&mut f, "second").unwrap();
    f.state().pin_window(&neighbour).unwrap();
    let before = f.state().stage.position_of(&neighbour).unwrap();
    pointer_to(&mut f, &mouse, Point::from((850., 150.)));
    assert_eq!(f.state().stage.position_of(&neighbour), Some(before));
    assert!(f.state().is_pinned(&neighbour));
    release(&mut f, &mouse, BTN_LEFT);
    key_release(&mut f, 56);
}
