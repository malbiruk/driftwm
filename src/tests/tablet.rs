//! Graphics-tablet (`wp_tablet_manager_v2`) input, driven through the real
//! `process_input_event` entry point with the synthetic backend
//! (`input_backend`). A pen is not only a tablet-protocol source: the
//! compositor also drives the seat pointer from it so legacy apps and
//! server-side decorations keep working, which is why most of what follows
//! asserts on pointer-side bookkeeping rather than on tablet-v2 wire traffic.
//!
//! The per-device output resolver is shared with touch
//! (`DriftWm::touch_output_for_device`); the fake reports no libinput device,
//! so resolution falls through to the config override or the first output.

use smithay::utils::{Logical, Point};
use smithay::wayland::tablet_manager::TabletSeatTrait;

use driftwm::config::BTN_LEFT;

use super::input_backend::{
    FakeDevice, pen_proximity_in, pen_tip_down, pen_tip_up, pen_to, tablet_added, tablet_removed,
};
use super::{Fixture, config, keyboard_focus, map_window, server_surface, window_by_app_id};

/// A pen that has been registered and brought into proximity — the state every
/// scenario below starts from, since an unregistered tablet drops axis events.
fn pen_in_proximity(f: &mut Fixture, at: Point<f64, Logical>) -> FakeDevice {
    let device = FakeDevice::tablet();
    tablet_added(f, &device);
    pen_proximity_in(f, &device, at);
    device
}

#[test]
fn pen_motion_moves_the_pointer() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let at = Point::from((40.0, -20.0));
    let device = pen_in_proximity(&mut f, at);

    pen_to(&mut f, &device, at);

    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        at,
        "a pen in proximity drives the same seat pointer legacy apps read"
    );
}

#[test]
fn pen_motion_still_drives_the_pointer_before_the_tablet_registers() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let device = FakeDevice::tablet();
    let at = Point::from((40.0, -20.0));

    // No `tablet_added`, so the seat holds no tablet under this descriptor and
    // the tablet-v2 half of the handler finds nothing to forward to.
    pen_to(&mut f, &device, at);

    // The pointer half runs first and unconditionally, which is what keeps a
    // missed registration (a hotplug the compositor never saw) degrading to
    // pointer-only input rather than to a dead pen.
    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        at,
        "the pointer half of a pen motion must not depend on tablet registration"
    );
}

#[test]
fn pen_tip_down_focuses_the_window_under_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "pen-target", (400, 300));
    let window = window_by_app_id(&mut f, "pen-target").expect("mapped");
    let pos = f.state().stage.position_of(&window).expect("staged");
    let at = Point::from((f64::from(pos.x) + 200.0, f64::from(pos.y) + 150.0));

    let device = pen_in_proximity(&mut f, at);
    pen_to(&mut f, &device, at);
    pen_tip_down(&mut f, &device, at);

    assert_eq!(
        keyboard_focus(&mut f).as_ref(),
        Some(&server_surface(&window)),
        "a tip-down is routed through the pointer button path, so it focuses \
         the window under the pen like a click would"
    );
}

#[test]
fn pen_motion_records_what_it_delivered() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "pen-target", (400, 300));
    let window = window_by_app_id(&mut f, "pen-target").expect("mapped");
    let pos = f.state().stage.position_of(&window).expect("staged");
    let at = Point::from((f64::from(pos.x) + 200.0, f64::from(pos.y) + 150.0));

    let device = pen_in_proximity(&mut f, at);
    pen_to(&mut f, &device, at);
    f.roundtrip(id);

    let screen = {
        let camera = f.state().camera();
        let zoom = f.state().zoom();
        driftwm::canvas::canvas_to_screen(driftwm::canvas::CanvasPos(at), camera, zoom).0
    };
    let (focus, origin) = f
        .state()
        .pointer_focus_under_pick(screen, at)
        .expect("the window must be under the pen, or this scenario tests nothing");

    // `refresh_pointer_focus` skips a resync when the delivery it would make
    // matches this record. A pen that moves the pointer without writing it
    // leaves whatever the last pointer event left, so a later resync compares
    // against a delivery that never happened.
    assert_eq!(
        f.state().last_pointer_delivery,
        Some((focus, origin, at - origin)),
        "pen motion must record what it delivered, like every other path that \
         moves the pointer"
    );
}

#[test]
fn pen_motion_takes_over_the_output_it_maps_to() {
    let mut f = Fixture::with_config(config(
        r#"
        [input.tablet]
        map_to_output = "HEADLESS-2"
        "#,
    ));
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1920, 1080));
    assert_eq!(
        f.state().focused_output.as_ref(),
        Some(&out1),
        "the first output must start active, or this scenario tests nothing"
    );

    let at = Point::from((0.0, 0.0));
    let device = pen_in_proximity(&mut f, at);
    pen_to(&mut f, &device, at);

    // Touch does exactly this after resolving its own per-device output
    // (`input/touch.rs`): a device pinned to one output makes that output the
    // active one, or everything reading `active_output()` afterwards — hot
    // corners, relative motion, window placement — acts on the wrong monitor.
    assert_eq!(
        f.state().focused_output.as_ref(),
        Some(&out2),
        "a pen pinned to an output must make that output active"
    );
}

#[test]
fn pen_motion_checks_hot_corners() {
    let mut f = Fixture::with_config(config(
        r#"
        [[outputs]]
        name = "*"
        [outputs.hot_corners]
        top_left = "zoom-out"
        "#,
    ));
    f.add_output(1, (1920, 1080));

    let device = FakeDevice::tablet();
    tablet_added(&mut f, &device);
    let output = f.state().active_output().expect("an output");

    // Raw screen space: the corner is a screen-space zone, and the canvas
    // point that maps onto it depends on the camera.
    let corner = Point::from((2.0, 2.0));
    super::input_backend::pen_proximity_in_screen(&mut f, &device, corner);
    super::input_backend::pen_to_screen_with(
        &mut f,
        &device,
        corner,
        super::input_backend::FakeTabletAxes::default(),
    );

    assert!(
        crate::state::output_state(&output).zoom_target.is_some(),
        "a pen entering a hot corner must arm it, like pointer motion does"
    );
}

#[test]
fn pen_motion_respects_pick_mode() {
    let mut f = Fixture::with_config(config(
        r#"
        [zoom]
        interact_min = 0.5
        "#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "pen-target", (400, 300));
    let window = window_by_app_id(&mut f, "pen-target").expect("mapped");
    f.state().map_window(
        crate::state::StageWindow::Client(window.clone()),
        Point::from((500, 400)),
        true,
    );
    f.state().set_zoom(0.3);

    let at = Point::from((700.0, 550.0));
    let device = pen_in_proximity(&mut f, at);
    pen_to(&mut f, &device, at);
    f.roundtrip(id);

    // Below `interact_min` a canvas window takes no pointer input — clicks pick
    // or move it instead. `pointer_focus_under_pick` is what enforces that, and
    // its own docs say every real-input pointer path must go through it (touch
    // is the one documented exception). A pen drives the seat pointer, so it is
    // such a path.
    assert!(
        super::pointer_focus(&mut f).is_none(),
        "in pick mode a pen must not hand the window pointer focus, or its \
         clicks reach the client while a mouse's would pick the window"
    );
}

#[test]
fn a_tip_cycle_presses_and_releases_the_left_button() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let at = Point::from((40.0, -20.0));
    let device = pen_in_proximity(&mut f, at);
    pen_to(&mut f, &device, at);

    pen_tip_down(&mut f, &device, at);
    assert!(
        f.state().held_buttons.contains(&BTN_LEFT),
        "a tip-down is emulated as a left press for clients that speak no tablet protocol"
    );

    pen_tip_up(&mut f, &device, at);
    assert!(
        !f.state().held_buttons.contains(&BTN_LEFT),
        "a tip-up must release it again, or the next click lands on a stuck button"
    );
}

#[test]
fn removing_a_tablet_takes_it_off_the_seat() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let device = FakeDevice::tablet();

    tablet_added(&mut f, &device);
    assert_eq!(
        f.state().seat.tablet_seat().count_tablets(),
        1,
        "a device advertising TabletTool registers on the seat"
    );

    tablet_removed(&mut f, &device);
    assert_eq!(
        f.state().seat.tablet_seat().count_tablets(),
        0,
        "unplugging it takes it back off"
    );
}
