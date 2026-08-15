//! Input driven through the real `process_input_event` entry point with the
//! synthetic backend (`input_backend`). The sub-handlers are directly callable
//! but generic over the backend, so a scenario needs a backend impl regardless;
//! going in at the top then keeps the event whole, which is what the
//! device-capability gate on a buffered middle click reads. Covered here: that
//! gate, the hardcoded click-to-focus fallback that runs when no mouse binding
//! matched, the screen→canvas mapping applied to what a device reports, and the
//! key-repeat latch a held binding leaves behind.

use std::time::Duration;

use driftwm::config::{BTN_LEFT, BTN_MIDDLE, Config};
use smithay::backend::input::ButtonState;
use smithay::desktop::Window;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use crate::input::is_interaction_tail;
use crate::state::StageWindow;

use super::client::ClientId;
use super::input_backend::{
    FakeDevice, button_event, click, key_press, key_release, pointer_to, pointer_to_screen, press,
    touch_down, touch_up_event,
};
use super::{
    Fixture, config, keyboard_focus, last_configured, map_window, server_surface, window_by_app_id,
};

/// Canvas-space center of `window`'s current geometry.
fn center_of(f: &mut Fixture, window: &Window) -> Point<f64, Logical> {
    let pos = f.state().stage.position_of(window).unwrap();
    let size = window.geometry().size;
    Point::from((
        pos.x as f64 + size.w as f64 / 2.0,
        pos.y as f64 + size.h as f64 / 2.0,
    ))
}

/// Two windows far enough apart to aim at unambiguously, with focus on the
/// second and the camera at the canvas origin (so canvas == screen). Auto
/// placement alone doesn't guarantee where two same-size windows land, and a
/// freshly mapped window is already focused — so a scenario asking "did this
/// input move focus?" needs both pinned down. Returns `(first, second)`.
fn two_windows(f: &mut Fixture, id: ClientId) -> (Window, Window) {
    map_window(f, id, "first", (400, 300));
    let first = window_by_app_id(f, "first").unwrap();
    f.state().map_window(
        StageWindow::Client(first.clone()),
        Point::from((0, 0)),
        false,
    );

    map_window(f, id, "second", (400, 300));
    let second = window_by_app_id(f, "second").unwrap();
    f.state().map_window(
        StageWindow::Client(second.clone()),
        Point::from((1000, 0)),
        false,
    );

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&second, serial);
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.camera_target = None;
        os.zoom = 1.0;
        os.zoom_target = None;
    });
    (first, second)
}

/// An unmodified left click on a window matches no mouse binding, so it lands in
/// the hardcoded fallback: raise + focus.
#[test]
fn click_focuses_the_window_under_the_pointer() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (first, second) = two_windows(&mut f, id);
    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&second)),
        "the second window holds focus before the click"
    );

    let target = center_of(&mut f, &first);
    click(&mut f, &FakeDevice::mouse(), target, BTN_LEFT);
    f.double_roundtrip(id);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&first)),
        "a click on a window focuses it"
    );
}

/// A touchpad's middle click may be the tap half of a 3-finger swipe, so it is
/// held back rather than dispatched. The press-time bookkeeping ahead of the
/// buffer — held buttons, tap taint, the cancelled navigate/pick/momentum — has
/// already run; what waits is everything downstream of it. The 300 ms timer
/// that would release the buffer outlives the scenario, which never pumps it.
#[test]
fn touchpad_middle_press_is_buffered() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (first, second) = two_windows(&mut f, id);

    let target = center_of(&mut f, &first);
    let touchpad = FakeDevice::touchpad();
    pointer_to(&mut f, &touchpad, target);
    press(&mut f, &touchpad, BTN_MIDDLE);

    assert!(
        f.state().pending_middle_click.is_some(),
        "a gesture-capable device's middle click waits for a possible swipe"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&second)),
        "the buffered press must not have been dispatched yet"
    );
}

/// A mouse has no 3-finger swipe to wait for, so its middle click dispatches
/// immediately — down the same fallback an unmodified click takes.
#[test]
fn mouse_middle_press_is_never_buffered() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (first, second) = two_windows(&mut f, id);
    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&second)),
        "the second window holds focus before the click"
    );

    let target = center_of(&mut f, &first);
    click(&mut f, &FakeDevice::mouse(), target, BTN_MIDDLE);
    f.double_roundtrip(id);

    assert!(
        f.state().pending_middle_click.is_none(),
        "a device without gesture capability must never have its click delayed"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&first)),
        "the press went straight through to the normal dispatch"
    );
}

/// A finger landing on a window focuses it, down the same fallback a click
/// takes. The finger is never lifted, so the scenario ends inside smithay's
/// touch-down grab — the fake has no frame event to close the sequence with.
#[test]
fn touch_down_focuses_the_window_under_the_finger() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let (first, second) = two_windows(&mut f, id);
    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&second)),
        "the second window holds focus before the touch"
    );

    let target = center_of(&mut f, &first);
    touch_down(&mut f, target, 0);
    f.double_roundtrip(id);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&first)),
        "a touch on a window focuses it"
    );
}

/// The fake reports raw screen coordinates, so this is what pins down the
/// screen→canvas mapping the handler puts them through. The expected point is
/// worked out by hand: running it back through `canvas_to_screen` would let a
/// bug shared by both directions round-trip clean, and at the default camera
/// and zoom the mapping is the identity, which any sign flip survives.
#[test]
fn absolute_motion_maps_screen_through_camera_and_zoom() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.state().with_output_state(|os| {
        os.camera = Point::from((300.0, -200.0));
        os.camera_target = None;
        os.zoom = 2.0;
        os.zoom_target = None;
    });

    pointer_to_screen(&mut f, &FakeDevice::mouse(), Point::from((640.0, 360.0)));

    // canvas = screen / zoom + camera = (640/2 + 300, 360/2 - 200)
    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        Point::from((620.0, -20.0)),
        "the pointer sits where the camera and zoom put the reported position"
    );
}

/// A binding that turns the screen off runs on the press; the release that
/// follows must not light the panel straight back up. `set_dpms` is inert
/// without a seat session, so the fixture can only reach the classification the
/// wake is gated on — the DPMS transition itself is hardware-only.
#[test]
fn a_release_is_not_a_reason_to_wake_a_dark_panel() {
    let mouse = FakeDevice::mouse();

    assert!(
        !is_interaction_tail(&button_event(&mouse, BTN_LEFT, ButtonState::Pressed)),
        "a press is the input that should wake a dark panel"
    );
    assert!(
        is_interaction_tail(&button_event(&mouse, BTN_LEFT, ButtonState::Released)),
        "a button coming back up must not undo what its press did"
    );
    assert!(
        is_interaction_tail(&touch_up_event(0)),
        "a finger lifting must not undo what its landing did"
    );
}

/// evdev codes, the space `key_press` reports in.
const KEY_Z: u32 = 44;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTALT: u32 = 56;
const KEY_F1: u32 = 59;

/// `z` grows the focused window, and holding it repeats with no initial delay
/// and 10ms between repeats.
fn held_grow_config() -> Config {
    config(
        r#"
        [input.keyboard]
        repeat_delay = 0
        repeat_rate = 100
        [keybindings]
        "z" = "grow-window right"
    "#,
    )
}

/// One repeat, waited out rather than faked: `apply_key_repeat` reads the wall
/// clock, and the config above puts repeats 10ms apart.
fn repeat_tick(f: &mut Fixture) {
    std::thread::sleep(Duration::from_millis(15));
    f.state().apply_key_repeat();
}

#[test]
fn releasing_the_key_ends_a_repeated_action() {
    let mut f = Fixture::with_config(held_grow_config());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));

    key_press(&mut f, KEY_Z);
    repeat_tick(&mut f);
    assert_eq!(
        last_configured(&mut f, id, &surface),
        (440, 300),
        "the press grows once and the held key grows again"
    );

    key_release(&mut f, KEY_Z);
    repeat_tick(&mut f);

    assert_eq!(last_configured(&mut f, id, &surface), (440, 300));
}

/// A VT switch is the case where the release never arrives — it lands on the VT
/// we left. Left latched, the action goes on resizing the window at the repeat
/// rate, redrawing every output per frame, on a session nobody is looking at.
#[test]
fn a_vt_switch_ends_a_repeated_action() {
    let mut f = Fixture::with_config(held_grow_config());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));

    key_press(&mut f, KEY_Z);
    repeat_tick(&mut f);
    assert_eq!(last_configured(&mut f, id, &surface), (440, 300));

    // Ctrl+Alt+F1, which xkb turns into XF86Switch_VT_1. No release follows for
    // any of the three.
    key_press(&mut f, KEY_LEFTCTRL);
    key_press(&mut f, KEY_LEFTALT);
    key_press(&mut f, KEY_F1);
    repeat_tick(&mut f);
    repeat_tick(&mut f);

    assert_eq!(
        last_configured(&mut f, id, &surface),
        (440, 300),
        "the held action must not keep firing on the VT we left"
    );
}
