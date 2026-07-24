//! Stand-in drag/action parity: a suspended stand-in and a client window flow
//! through the one unified [`MoveGrab`], so a stand-in drag gains edge-pan,
//! cross-output teleport, and fullscreen-output park, and focus/center actions
//! resolve a focused stand-in the same way they resolve a focused client.
//!
//! Grabs are installed directly via the public `MoveGrab` constructors and
//! driven with smithay's public `PointerHandle` (motion loop + a synthesized
//! release), matching the single-motion precedent in `suspended.rs`.

use smithay::backend::input::ButtonState;
use smithay::input::pointer::{ButtonEvent, Focus, GrabStartData, MotionEvent};
use smithay::output::Output;
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};

use driftwm::config::{Action, BTN_LEFT};

use crate::grabs::MoveGrab;
use crate::state::{ClusterMember, StageWindow, output_state};

use super::{Fixture, config, is_activated, map_window, window_by_app_id};

fn pt(x: f64, y: f64) -> Point<f64, Logical> {
    Point::from((x, y))
}

/// Camera at the canvas origin, zoom 1, on the active output: canvas == screen.
fn origin_view(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });
}

fn set_view(output: &Output, camera: (f64, f64), zoom: f64) {
    let mut os = output_state(output);
    os.camera = Point::from(camera);
    os.zoom = zoom;
}

/// Install a live [`MoveGrab`] over `target` as if a body/title-bar drag had
/// just started at canvas-space `start`, with the element currently at
/// `initial`. Single-window (no cluster), matching a plain move.
fn install_move_grab(
    f: &mut Fixture,
    target: impl Into<ClusterMember>,
    start: Point<f64, Logical>,
    initial: Point<i32, Logical>,
    output: Output,
) {
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    let start_data = GrabStartData {
        focus: None,
        button: BTN_LEFT,
        location: start,
    };
    let grab = MoveGrab::new(start_data, target, initial, output, Vec::new());
    pointer.set_grab(f.state(), grab, serial, Focus::Clear);
}

/// Deliver one pointer motion at canvas-space `loc` to the active grab.
fn motion(f: &mut Fixture, loc: Point<f64, Logical>) {
    let pointer = f.state().seat.get_pointer().unwrap();
    let event = MotionEvent {
        location: loc,
        serial: SERIAL_COUNTER.next_serial(),
        time: 0,
    };
    pointer.motion(f.state(), None, &event);
}

/// Release the left button, ending the drag through the real grab teardown.
fn release(f: &mut Fixture) {
    let pointer = f.state().seat.get_pointer().unwrap();
    let event = ButtonEvent {
        button: BTN_LEFT,
        state: ButtonState::Released,
        serial: SERIAL_COUNTER.next_serial(),
        time: 0,
    };
    pointer.button(f.state(), &event);
}

/// A multi-motion drag lands a client and a stand-in at the same canvas
/// position: the two targets share one grab's snap/map math.
#[test]
fn plain_drag_moves_client_and_stand_in_alike() {
    let start = pt(600.0, 450.0);
    let initial = Point::from((400, 300));
    let motions = [pt(650.0, 470.0), pt(680.0, 500.0), pt(700.0, 480.0)];
    let expected = Point::from((500, 330));

    {
        let mut f = Fixture::new();
        let out = f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let id = f.add_client();
        map_window(&mut f, id, "c", (400, 300));
        let window = window_by_app_id(&mut f, "c").unwrap();
        f.state()
            .map_window(StageWindow::Client(window.clone()), initial, true);

        install_move_grab(&mut f, window.clone(), start, initial, out);
        for m in motions {
            motion(&mut f, m);
        }

        assert_eq!(
            f.state().stage.position_of(&StageWindow::Client(window)),
            Some(expected),
            "the client lands at the natural drag destination"
        );
        release(&mut f);
    }

    {
        let mut f = Fixture::new();
        let out = f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f
            .state()
            .insert_suspended_for_test(1, initial, Size::from((400, 300)), "s", "S");

        install_move_grab(&mut f, sid, start, initial, out);
        for m in motions {
            motion(&mut f, m);
        }

        let s = f.state().find_suspended(sid).unwrap();
        assert_eq!(
            f.state().stage.position_of(&StageWindow::Suspended(s)),
            Some(expected),
            "the stand-in lands at the same destination as the client"
        );
        release(&mut f);
        f.state().dismiss_suspended(sid);
    }
}

/// Dragging a stand-in into an output's edge zone arms that output's edge-pan
/// velocity in the direction of the nearest edge.
#[test]
fn suspended_drag_arms_edge_pan_near_the_edge() {
    let mut f = Fixture::new();
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    install_move_grab(
        &mut f,
        sid,
        pt(600.0, 450.0),
        Point::from((400, 300)),
        out.clone(),
    );

    // Within 100 screen px of the left edge (x = 0).
    motion(&mut f, pt(50.0, 500.0));

    let v = { output_state(&out).edge_pan_velocity };
    assert!(
        v.is_some_and(|v| v.x < 0.0),
        "a stand-in dragged into the left edge zone arms leftward edge-pan"
    );

    release(&mut f);
    f.state().dismiss_suspended(sid);
}

/// The armed edge-pan actually scrolls the camera while the stand-in drag is
/// live — the same animation-tick path a client drag drives.
#[test]
fn suspended_drag_edge_pan_drives_the_camera() {
    // Panning the camera leaves per-output blur-generation state, exactly as the
    // camera-animation tests do — that's the behavior under test, not a leak.
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    install_move_grab(
        &mut f,
        sid,
        pt(600.0, 450.0),
        Point::from((400, 300)),
        out.clone(),
    );

    motion(&mut f, pt(50.0, 500.0));
    assert!(
        { output_state(&out).edge_pan_velocity }.is_some(),
        "precondition: edge-pan armed"
    );

    f.state().apply_edge_pan();

    assert!(
        f.state().camera().x < 0.0,
        "the armed edge-pan scrolled the camera left"
    );

    release(&mut f);
    f.state().dismiss_suspended(sid);
}

/// Leaving the edge zone disarms edge-pan on the same drag.
#[test]
fn suspended_drag_clears_edge_pan_leaving_the_zone() {
    let mut f = Fixture::new();
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    install_move_grab(
        &mut f,
        sid,
        pt(600.0, 450.0),
        Point::from((400, 300)),
        out.clone(),
    );

    motion(&mut f, pt(50.0, 500.0));
    assert!(
        { output_state(&out).edge_pan_velocity }.is_some(),
        "precondition: edge-pan armed in the zone"
    );

    motion(&mut f, pt(960.0, 540.0));

    assert!(
        { output_state(&out).edge_pan_velocity }.is_none(),
        "leaving the edge zone disarms edge-pan"
    );

    release(&mut f);
    f.state().dismiss_suspended(sid);
}

/// Regression: a stand-in adopted/dismissed mid-pan clears its output's
/// edge-pan on the very next motion tick, not just at release — otherwise the
/// armed velocity would self-sustain and run the camera away.
#[test]
fn dismissing_a_stand_in_mid_pan_clears_edge_pan() {
    let mut f = Fixture::new();
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    install_move_grab(
        &mut f,
        sid,
        pt(600.0, 450.0),
        Point::from((400, 300)),
        out.clone(),
    );

    motion(&mut f, pt(50.0, 500.0));
    assert!(
        { output_state(&out).edge_pan_velocity }.is_some(),
        "precondition: edge-pan armed"
    );

    f.state().dismiss_suspended(sid);
    motion(&mut f, pt(45.0, 500.0));

    assert!(
        { output_state(&out).edge_pan_velocity }.is_none(),
        "a vanished drag target clears edge-pan on the next tick, not just at release"
    );

    release(&mut f);
}

/// A stand-in drag that crosses onto another output re-anchors under the cursor
/// there, and a subsequent motion drives edge-pan against the new output's
/// zoom/camera (the stale-zoom fix): the chosen point is inside the new
/// output's top edge zone only under its 2x zoom.
#[test]
fn suspended_drag_teleports_and_reanchors_on_the_new_output() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    set_view(&out1, (0.0, 0.0), 1.0);
    set_view(&out2, (5000.0, 5000.0), 2.0);

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    install_move_grab(
        &mut f,
        sid,
        pt(600.0, 450.0),
        Point::from((400, 300)),
        out1.clone(),
    );

    // Phase-3 routing would have moved focus to out2 and converted the motion
    // into out2's canvas space; fabricate that here.
    f.state().focused_output = Some(out2.clone());
    motion(&mut f, pt(5300.0, 5300.0));

    let s = f.state().find_suspended(sid).unwrap();
    assert_eq!(
        f.state().stage.position_of(&StageWindow::Suspended(s)),
        Some(Point::from((5100, 5150))),
        "the stand-in re-anchors under the cursor on the new output"
    );

    // The position assertion alone would pass without the teleport branch
    // (apply_move's delta formula matches the re-anchor); this zoom-sensitive
    // edge-pan check is what actually pins the output/zoom reassignment.
    motion(&mut f, pt(5300.0, 5030.0));
    let v = { output_state(&out2).edge_pan_velocity };
    assert!(
        v.is_some_and(|v| v.y < 0.0),
        "after teleport the grab uses the new output's zoom for edge detection"
    );

    release(&mut f);
    f.state().dismiss_suspended(sid);
}

/// A stand-in drag over a fullscreen output freezes in place — the same
/// output-level park a client drag hits, so a window can't vanish into a
/// culled fullscreen viewport.
#[test]
fn suspended_drag_parks_over_a_fullscreen_output() {
    let mut f = Fixture::new();
    let out = f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let fs_surface = map_window(&mut f, id, "fs", (400, 300));
    f.client(id).window(&fs_surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert!(
        f.state().is_output_fullscreen(&out),
        "precondition: the output is fullscreen"
    );

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    install_move_grab(
        &mut f,
        sid,
        pt(600.0, 450.0),
        Point::from((400, 300)),
        out.clone(),
    );
    f.state().focused_output = Some(out.clone());

    motion(&mut f, pt(1000.0, 900.0));

    let s = f.state().find_suspended(sid).unwrap();
    assert_eq!(
        f.state().stage.position_of(&StageWindow::Suspended(s)),
        Some(Point::from((400, 300))),
        "a drag over a fullscreen output freezes the stand-in in place"
    );

    release(&mut f);
    f.state().dismiss_suspended(sid);
}

/// After the stand-in is dismissed mid-drag, further motions no-op the move but
/// still forward, so the pointer keeps tracking; releasing tears the
/// pass-through grab down.
#[test]
fn dismissing_a_stand_in_mid_drag_forwards_then_release_cleans_up() {
    let mut f = Fixture::new();
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    install_move_grab(&mut f, sid, pt(600.0, 450.0), Point::from((400, 300)), out);

    motion(&mut f, pt(700.0, 480.0));
    f.state().dismiss_suspended(sid);

    motion(&mut f, pt(900.0, 600.0));
    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        pt(900.0, 600.0),
        "a dismissed drag still forwards motion so the pointer keeps tracking"
    );

    release(&mut f);
    assert!(
        !f.state().seat.get_pointer().unwrap().is_grabbed(),
        "releasing the button tears the pass-through grab down"
    );
}

/// Focusing a stand-in clears the previously-active client's xdg `activated`
/// hint — focus moving to a stand-in must not leave a client looking focused.
#[test]
fn focusing_a_stand_in_clears_the_active_clients_hint() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((900, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    // Re-focus A so it holds the activated hint right before the stand-in takes
    // focus (inserting the stand-in already deactivated everyone).
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    assert!(is_activated(&a), "precondition: A holds the activated hint");

    f.state().focus_and_raise_suspended(sid);

    assert!(
        !is_activated(&a),
        "focusing the stand-in cleared A's activated hint"
    );

    f.state().dismiss_suspended(sid);
}

/// `CenterWindow` on a focused non-canvas client (pinned) still reaches the
/// nearest-canvas fallback — the D2 match must keep a catch-all arm, not
/// no-op on a failed `is_canvas_window` guard.
#[test]
fn center_window_falls_through_to_nearest_for_a_pinned_client() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [320, 240]
"#,
    ));
    f.add_output(1, (1920, 1080));

    let idc = f.add_client();
    map_window(&mut f, idc, "canvas", (400, 300));
    let canvas_win = window_by_app_id(&mut f, "canvas").unwrap();
    f.state().map_window(
        StageWindow::Client(canvas_win),
        Point::from((2000, 400)),
        true,
    );

    let idp = f.add_client();
    map_window(&mut f, idp, "pin", (320, 240));
    let pin = window_by_app_id(&mut f, "pin").unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&pin, serial);
    assert_eq!(f.state().focused_window().as_ref(), Some(&pin));
    assert!(
        !f.state().is_canvas_window(&pin),
        "precondition: the focused client is not a canvas window"
    );

    f.state().with_output_state(|os| {
        os.camera_target = None;
        os.zoom_target = None;
    });

    f.state().execute_action(&Action::CenterWindow);

    assert!(
        f.state().camera_target().is_some(),
        "a focused pinned client falls through to center the nearest canvas window"
    );
}
