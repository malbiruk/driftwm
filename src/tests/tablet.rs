//! Graphics-tablet (`wp_tablet_manager_v2`) input. A pen is not only a
//! tablet-protocol source: the compositor drives the seat pointer from it so
//! legacy apps and server-side decorations keep working, which is why most of
//! what follows asserts on pointer-side bookkeeping rather than tablet-v2 wire
//! traffic.
//!
//! The per-device output resolver is shared with touch
//! (`DriftWm::touch_output_for_device`); the fake reports no libinput device,
//! so resolution falls through to the config override or the first output.

use smithay::utils::{Logical, Point};
use smithay::wayland::tablet_manager::TabletSeatTrait;

use driftwm::config::BTN_LEFT;

use super::input_backend::{
    FakeDevice, FakeTabletAxes, pen_proximity_in, pen_proximity_in_screen, pen_tip_down,
    pen_tip_up, pen_to, pen_to_screen_with, tablet_added, tablet_removed, touch_down,
};
use super::{Fixture, config, keyboard_focus, map_window, server_surface, window_by_app_id};

/// A registered pen in proximity — the state every scenario below starts from.
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
    // Pinned so the expected canvas point is hand-computed rather than run back
    // through the inverse of the mapping under test: at zoom 1 a screen point
    // lands on `screen + camera`.
    f.state().set_camera(Point::from((-100.0, -50.0)));
    let device = FakeDevice::tablet();
    tablet_added(&mut f, &device);

    let screen = Point::from((500.0, 400.0));
    pen_proximity_in_screen(&mut f, &device, screen);
    pen_to_screen_with(&mut f, &device, screen, FakeTabletAxes::default());

    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        Point::from((400.0, 350.0)),
        "a pen in proximity drives the same seat pointer legacy apps read"
    );
}

#[test]
fn pen_motion_still_drives_the_pointer_before_the_tablet_registers() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let device = FakeDevice::tablet();
    let at = Point::from((40.0, -20.0));

    // No `tablet_added`, so the tablet-v2 half of the handler finds no tablet
    // to forward to.
    pen_to(&mut f, &device, at);

    // The pointer half runs first and unconditionally, so a missed registration
    // degrades to pointer-only input rather than to a dead pen.
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
    pen_tip_down(&mut f, &device);

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
    // matches this record, so a pen that moves the pointer without writing it
    // leaves a later resync comparing against a delivery that never happened.
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

    // A device pinned to one output makes that output active, or everything
    // reading `active_output()` afterwards acts on the wrong monitor.
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

    // Raw screen space: a corner is a screen-space zone, and which canvas point
    // lands on it depends on the camera.
    let corner = Point::from((2.0, 2.0));
    pen_proximity_in_screen(&mut f, &device, corner);
    pen_to_screen_with(&mut f, &device, corner, FakeTabletAxes::default());

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
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state().set_zoom(0.3);

    let at = Point::from((700.0, 550.0));
    let device = pen_in_proximity(&mut f, at);
    pen_to(&mut f, &device, at);
    f.roundtrip(id);

    // Below `interact_min` a canvas window takes no pointer input — clicks pick
    // or move it instead — and a pen drives the seat pointer like any mouse.
    assert!(
        super::pointer_focus(&mut f).is_none(),
        "in pick mode a pen must not hand the window pointer focus, or its \
         clicks reach the client while a mouse's would pick the window"
    );

    // Positive control: the same pen over the same window above the threshold
    // must reach it, or the assertion above would also pass on a pen that
    // simply never found the window.
    f.state().set_zoom(0.8);
    pen_to(&mut f, &device, at);
    f.roundtrip(id);
    assert!(
        super::pointer_focus(&mut f).is_some(),
        "above the threshold the pen reaches the window normally"
    );
}

#[test]
fn pen_proximity_takes_over_the_output_it_maps_to() {
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

    let device = FakeDevice::tablet();
    tablet_added(&mut f, &device);
    pen_proximity_in(&mut f, &device, Point::from((0.0, 0.0)));

    // Proximity resolves focus through the same cascade as motion, and that
    // cascade reads `active_output()` — so entering proximity has to claim the
    // output too, or the pen hit-tests its coordinates against another
    // monitor's layers and pins before it has moved once.
    assert_eq!(
        f.state().focused_output.as_ref(),
        Some(&out2),
        "entering proximity claims the pen's output, like motion does"
    );
}

#[test]
fn a_tip_without_a_registered_tool_still_clicks() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let device = FakeDevice::tablet();

    // No `tablet_added`, so the seat has no tool to send `tip_down` to.
    pen_tip_down(&mut f, &device);

    assert!(
        f.state().held_buttons.contains(&BTN_LEFT),
        "an absent tool must not swallow the emulated button, or a pen whose \
         registration was missed moves the cursor but can never click"
    );
}

#[test]
fn pen_motion_restores_a_cursor_touch_hid() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let at = Point::from((40.0, -20.0));

    touch_down(&mut f, at, 0);
    assert!(
        f.state().cursor.hidden_by_touch,
        "touch hides the cursor, or this scenario tests nothing"
    );

    let device = pen_in_proximity(&mut f, at);
    pen_to(&mut f, &device, at);

    assert!(
        !f.state().cursor.hidden_by_touch,
        "a pen is real pointer input, so it brings the cursor back"
    );
}

#[test]
fn a_tip_cycle_presses_and_releases_the_left_button() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let at = Point::from((40.0, -20.0));
    let device = pen_in_proximity(&mut f, at);
    pen_to(&mut f, &device, at);

    pen_tip_down(&mut f, &device);
    assert!(
        f.state().held_buttons.contains(&BTN_LEFT),
        "a tip-down is emulated as a left press for clients that speak no tablet protocol"
    );

    pen_tip_up(&mut f, &device);
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

#[test]
fn a_tablet_plugged_in_while_locked_still_registers() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    f.client(id).lock_session();
    f.roundtrip(id);
    assert!(
        f.state().session_lock.is_locked(),
        "the lock handler ran, or this scenario tests nothing"
    );

    let device = FakeDevice::tablet();
    tablet_added(&mut f, &device);

    // Registration is bookkeeping, not input delivery. Dropping it with the
    // rest of the locked event stream would leave the tablet dead after unlock
    // until it was physically replugged.
    assert_eq!(
        f.state().seat.tablet_seat().count_tablets(),
        1,
        "a hotplug behind the lock screen still reaches the seat"
    );
}
