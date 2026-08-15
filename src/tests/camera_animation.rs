//! Camera and zoom animations. `pan-viewport` extends `camera_target` and lets
//! `apply_camera_animation` lerp the camera there, warping the pointer by each
//! camera delta so the cursor keeps its screen position.
//! Combined zoom+camera animations pin the anchor's canvas point at a fixed
//! screen point while zoom lerps to target, and finish both coordinates in the
//! same tick — zoom snaps to target but keeps animating while the anchor is
//! still off its screen point, and there is never a camera-only handoff tail.
//!
//! The tests at the end cover the other side of that warp: a compositor grab
//! measures against a frozen canvas anchor, so camera motion it did not cause
//! reads to it as user input. Installing one takes the viewport out of flight —
//! except for edge-pan, the one camera motion a grab does cause.

use std::time::Duration;

use smithay::input::keyboard::ModifiersState;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};

use driftwm::config::{Action, BTN_LEFT, Config, Direction};

use crate::grabs::PanGrab;
use crate::state::{StageWindow, ZoomAnimationAnchor, output_state};

use super::client::ClientId;
use super::input_backend::{
    FakeDevice, pointer_relative_motion, pointer_relative_motion_at, pointer_to, press, release,
    touch_down, touch_motion, touch_up,
};
use super::{Fixture, end_grab, map_window, motion, window_by_app_id, window_position};

const TICK: Duration = Duration::from_millis(16);
const MAX_TICKS: usize = 600;

fn approx(a: Point<f64, Logical>, b: Point<f64, Logical>, tol: f64) -> bool {
    (a.x - b.x).abs() <= tol && (a.y - b.y).abs() <= tol
}

fn dist_sq(a: Point<f64, Logical>, b: Point<f64, Logical>) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    dx * dx + dy * dy
}

/// Canvas point currently shown at screen point `s`: `camera + s / zoom`.
fn point_at_screen(f: &mut Fixture, s: Point<f64, Logical>) -> Point<f64, Logical> {
    let camera = f.state().camera();
    let zoom = f.state().zoom();
    Point::from((camera.x + s.x / zoom, camera.y + s.y / zoom))
}

fn run_camera_animation(f: &mut Fixture) {
    for _ in 0..MAX_TICKS {
        if f.state().camera_target().is_none() {
            return;
        }
        f.state().apply_camera_animation(TICK);
    }
    panic!("camera animation did not converge within {MAX_TICKS} ticks");
}

fn run_zoom_animation(f: &mut Fixture) {
    for _ in 0..MAX_TICKS {
        if f.state().zoom_target().is_none() {
            return;
        }
        f.state().apply_zoom_animation(TICK);
    }
    panic!("zoom animation did not converge within {MAX_TICKS} ticks");
}

/// A pan action leaves the camera put and sets a target one step away; a second
/// pan extends the target from the target, not from the unmoved camera.
#[test]
fn pan_viewport_sets_target_instead_of_jumping() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let camera = f.state().camera();
    let step = f.state().config.pan_step / f.state().zoom();
    let (ux, uy) = Direction::Right.to_unit_vec();
    let delta = Point::from((ux * step, uy * step));

    f.state()
        .execute_action(&Action::PanViewport(Direction::Right));

    assert!(
        approx(f.state().camera(), camera, 1e-9),
        "a pan must not move the camera directly"
    );
    assert!(
        approx(f.state().camera_target().unwrap(), camera + delta, 1e-9),
        "a pan sets the target one step from the camera"
    );

    f.state()
        .execute_action(&Action::PanViewport(Direction::Right));

    assert!(approx(f.state().camera(), camera, 1e-9));
    assert!(
        approx(
            f.state().camera_target().unwrap(),
            camera + delta + delta,
            1e-9
        ),
        "a repeated pan extends the target from the target, not the camera"
    );
}

/// The camera lerps onto the target and clears it on arrival.
#[test]
fn pan_viewport_converges_and_clears_target() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    f.state()
        .execute_action(&Action::PanViewport(Direction::Right));
    let target = f
        .state()
        .camera_target()
        .expect("a pan sets a camera target");

    run_camera_animation(&mut f);

    assert!(
        f.state().camera_target().is_none(),
        "the target clears when the camera arrives"
    );
    assert!(
        approx(f.state().camera(), target, 1e-6),
        "the camera settles exactly on the target"
    );
}

/// Every camera tick warps the pointer by the camera delta, so the cursor's
/// screen position is unchanged across the whole pan.
#[test]
fn pan_keeps_pointer_screen_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let camera_before = f.state().camera();
    let pointer_before = f.state().seat.get_pointer().unwrap().current_location();

    f.state()
        .execute_action(&Action::PanViewport(Direction::Right));
    for _ in 0..MAX_TICKS {
        if f.state().camera_target().is_none() {
            break;
        }
        f.state().apply_camera_animation(TICK);
        let camera_delta = f.state().camera() - camera_before;
        let pointer_delta =
            f.state().seat.get_pointer().unwrap().current_location() - pointer_before;
        assert!(
            approx(pointer_delta, camera_delta, 1e-6),
            "the pointer shifts by the camera delta on every tick, not just overall"
        );
    }
    assert!(
        f.state().camera_target().is_none(),
        "camera animation did not converge within {MAX_TICKS} ticks"
    );
}

/// A zoom animation with the anchor's canvas point already at its screen point
/// keeps that point pinned every tick while zoom lerps to target, then clears
/// cleanly with no camera-only tail.
#[test]
fn zoom_anchor_holds_screen_point() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let s = Point::from((960.0, 540.0));
    let camera = Point::from((100.0, 50.0));
    // The canvas point shown at S right now, so only zoom animates.
    let c = Point::from((camera.x + s.x, camera.y + s.y));
    f.state().with_output_state(|os| {
        os.camera = camera;
        os.zoom = 1.0;
        os.zoom_target = Some(0.5);
        os.zoom_animation_anchor = Some(ZoomAnimationAnchor {
            canvas: c,
            screen: s,
        });
        os.camera_target = None;
        os.overview_return = None;
    });

    let mut prev = dist_sq(point_at_screen(&mut f, s), c);
    let mut converged = false;
    for _ in 0..MAX_TICKS {
        f.state().apply_zoom_animation(TICK);
        let d = dist_sq(point_at_screen(&mut f, s), c);
        assert!(
            d <= prev + 1e-6,
            "the screen anchor drifted off its canvas point"
        );
        prev = d;
        if f.state().zoom_target().is_none() {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "zoom animation did not converge within {MAX_TICKS} ticks"
    );

    assert_eq!(f.state().zoom(), 0.5, "zoom lands exactly on target");
    assert!(
        approx(point_at_screen(&mut f, s), c, 1e-9),
        "the anchor's canvas point ends at its screen point"
    );
    assert!(f.state().zoom_animation_anchor().is_none());
    assert!(
        f.state().camera_target().is_none(),
        "there is no camera-only handoff tail"
    );
}

/// The coupled-finish invariant: when zoom reaches its close band it snaps to
/// target, but the animation stays alive while the anchor is still off its
/// screen point — and it drives the camera directly, never handing off through
/// `camera_target`. Both coordinates then clear in the same tick.
#[test]
fn zoom_finish_is_coupled() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let s = Point::from((960.0, 540.0));
    let camera = Point::from((100.0, 50.0));
    let zoom = 0.4995;
    // Displace the anchor's canvas point ~100px from the point now shown at S.
    let at_screen: Point<f64, Logical> =
        Point::from((camera.x + s.x / zoom, camera.y + s.y / zoom));
    let c = Point::from((at_screen.x + 100.0, at_screen.y));
    f.state().with_output_state(|os| {
        os.camera = camera;
        os.zoom = zoom;
        os.zoom_target = Some(0.5);
        os.zoom_animation_anchor = Some(ZoomAnimationAnchor {
            canvas: c,
            screen: s,
        });
        os.camera_target = None;
        os.overview_return = None;
    });

    f.state().apply_zoom_animation(TICK);

    assert_eq!(
        f.state().zoom(),
        0.5,
        "zoom snaps to target inside the close band"
    );
    assert!(
        f.state().zoom_target().is_some(),
        "the animation keeps running while the anchor converges"
    );
    assert!(
        f.state().camera_target().is_none(),
        "the anchor drives the camera directly, no handoff"
    );

    run_zoom_animation(&mut f);

    assert!(f.state().zoom_animation_anchor().is_none());
    assert!(f.state().camera_target().is_none());
    let expected_camera = Point::from((c.x - s.x / 0.5, c.y - s.y / 0.5));
    assert!(
        approx(f.state().camera(), expected_camera, 1e-9),
        "the camera lands exactly where the finish places it, not one lerp short"
    );
}

/// A keyboard zoom action anchors on the viewport center: the anchor's screen
/// point is the usable center and its canvas point is what that center shows,
/// which ends back under the center at the new zoom.
#[test]
fn zoom_action_anchors_at_viewport_center() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let camera = f.state().camera();
    let zoom = f.state().zoom();
    let center = f.state().usable_center_screen();

    f.state().execute_action(&Action::ZoomOut);

    let anchor = f
        .state()
        .zoom_animation_anchor()
        .expect("a zoom action arms the anchor");
    assert!(
        approx(anchor.screen, center, 1e-9),
        "the anchor screen point is the viewport center"
    );
    let expected_canvas = Point::from((camera.x + center.x / zoom, camera.y + center.y / zoom));
    assert!(
        approx(anchor.canvas, expected_canvas, 1e-9),
        "the anchor canvas point is what the viewport center shows"
    );

    run_zoom_animation(&mut f);

    assert!(
        approx(point_at_screen(&mut f, center), anchor.canvas, 1e-9),
        "the anchor's canvas point ends back under the viewport center"
    );
}

/// Camera at the canvas origin, zoom 1, so canvas and screen coincide.
fn origin_view(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.zoom = 1.0;
    });
}

/// Put a camera and a zoom flight in progress, aimed far enough away that a
/// handful of ticks move the camera by hundreds of canvas pixels.
fn arm_distant_flight(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.camera_target = Some(Point::from((2000.0, 0.0)));
        os.zoom_target = Some(2.0);
    });
}

/// How many configures the client has seen and the size the last one carried. A
/// resize nobody asked for shows up as another configure with a bigger size, so
/// pinning both catches it whichever way the fixture's baseline sits.
fn configure_trace(
    f: &mut Fixture,
    id: ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) -> (usize, (i32, i32)) {
    let configures = &f.client(id).window(surface).configures_received;
    (
        configures.len(),
        configures
            .last()
            .expect("the client has been configured at least once")
            .1
            .size,
    )
}

/// Map one 400x300 client at canvas (400, 300) on a single output, viewport at
/// the origin — the shared fixture for the grab-versus-camera scenarios.
fn one_window(f: &mut Fixture) -> (ClientId, wayland_client::protocol::wl_surface::WlSurface) {
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(f, id, "a", (400, 300));
    let window = window_by_app_id(f, "a").unwrap();
    origin_view(f);
    f.state()
        .map_window(StageWindow::Client(window), Point::from((400, 300)), true);
    (id, surface)
}

/// A resize grab measures every delta from the canvas point it was pressed at,
/// and a camera tick warps the pointer synchronously into whatever grab is live.
/// A flight still running when the grab installs would therefore resize the
/// window from a mouse that never moved.
#[test]
fn a_camera_flight_does_not_resize_the_window_a_resize_grab_just_took() {
    let mut f = Fixture::new();
    let (id, surface) = one_window(&mut f);
    let window = window_by_app_id(&mut f, "a").unwrap();

    arm_distant_flight(&mut f);
    assert!(
        f.state().camera_target().is_some(),
        "precondition: a camera flight is in progress when the grab installs"
    );

    let grab_at = Point::from((790.0, 450.0));
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    assert!(
        f.state().start_compositor_resize_with_edge(
            &pointer,
            &window,
            grab_at,
            BTN_LEFT,
            serial,
            Some(xdg_toplevel::ResizeEdge::Right),
            false,
        ),
        "precondition: the resize grab installed"
    );
    // Park the cursor on the grab origin, so anything the size does from here is
    // the camera's doing and not the pointer's.
    motion(&mut f, grab_at);
    f.double_roundtrip(id);
    let before = configure_trace(&mut f, id, &surface);
    assert!(
        f.state().seat.get_pointer().unwrap().is_grabbed(),
        "precondition: the pointer is grabbed, so a warp reaches the grab \
         synchronously instead of taking the deferred branch"
    );

    for _ in 0..5 {
        f.state().apply_camera_animation(TICK);
    }
    f.double_roundtrip(id);

    assert_eq!(
        configure_trace(&mut f, id, &surface),
        before,
        "a motionless mouse resized nothing"
    );
    end_grab(&mut f);
}

/// The move half of the same rule. Driven through `try_start_gesture_move`
/// rather than the pinned path, whose screen-space math shifts cursor and
/// camera by the same delta and so cannot show the defect either way.
#[test]
fn a_camera_flight_does_not_move_the_window_a_move_grab_just_took() {
    let mut f = Fixture::new();
    let (_id, _surface) = one_window(&mut f);
    let element = StageWindow::Client(window_by_app_id(&mut f, "a").unwrap());

    arm_distant_flight(&mut f);
    assert!(
        f.state().camera_target().is_some(),
        "precondition: a camera flight is in progress when the grab installs"
    );

    let grab_at = Point::from((600.0, 450.0));
    assert!(
        f.state().try_start_gesture_move(grab_at, false),
        "precondition: the move grab installed"
    );
    motion(&mut f, grab_at);
    assert!(
        f.state().seat.get_pointer().unwrap().is_grabbed(),
        "precondition: the pointer is grabbed, so a warp reaches the grab \
         synchronously instead of taking the deferred branch"
    );

    for _ in 0..5 {
        f.state().apply_camera_animation(TICK);
    }

    assert_eq!(
        f.state().stage.position_of(&element),
        Some(Point::from((400, 300))),
        "a motionless mouse dragged nothing"
    );
    end_grab(&mut f);
}

/// `begin_client_resize` is the chokepoint every client-resize entry point runs
/// through, so stopping the flight there covers all of them at once.
#[test]
fn starting_a_client_resize_ends_the_camera_flight() {
    let mut f = Fixture::new();
    let (_id, _surface) = one_window(&mut f);
    let window = window_by_app_id(&mut f, "a").unwrap();

    arm_distant_flight(&mut f);
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    assert!(f.state().start_compositor_resize_with_edge(
        &pointer,
        &window,
        Point::from((790.0, 450.0)),
        BTN_LEFT,
        serial,
        Some(xdg_toplevel::ResizeEdge::Right),
        false,
    ));

    assert!(f.state().camera_target().is_none(), "the pan is called off");
    assert!(f.state().zoom_target().is_none(), "and so is the zoom");
    end_grab(&mut f);
}

/// `arm_interactive_move` is the other chokepoint: every move-grab install and
/// the stand-in resize arms run through it.
#[test]
fn starting_a_move_grab_ends_the_camera_flight() {
    let mut f = Fixture::new();
    let (_id, _surface) = one_window(&mut f);

    arm_distant_flight(&mut f);
    assert!(
        f.state()
            .try_start_gesture_move(Point::from((600.0, 450.0)), false)
    );

    assert!(f.state().camera_target().is_none(), "the pan is called off");
    assert!(f.state().zoom_target().is_none(), "and so is the zoom");
    end_grab(&mut f);
}

/// A stand-in resize reaches neither `begin_client_resize` (there is no client
/// to configure) nor any move grab, and still has to stop the flight — it runs
/// the same `ResizeGrab` against the same frozen anchor.
#[test]
fn starting_a_stand_in_resize_ends_the_camera_flight() {
    let config = Config::from_toml(
        r#"
        [decorations]
        default_mode = "server"
        [mouse.anywhere]
        "super+left" = "resize-window"
    "#,
    )
    .unwrap();
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    arm_distant_flight(&mut f);
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    let held = ModifiersState {
        logo: true,
        ..Default::default()
    };
    assert!(
        f.state().try_suspended_button(
            &pointer,
            Point::from((790.0, 450.0)),
            BTN_LEFT,
            serial,
            held
        ),
        "precondition: the stand-in resize grab installed"
    );

    assert!(f.state().camera_target().is_none(), "the pan is called off");
    assert!(f.state().zoom_target().is_none(), "and so is the zoom");
    end_grab(&mut f);
    f.state().dismiss_suspended(sid);
}

/// Scoping the cancel to the active output leaves a real hole: the cancel runs
/// once at install, but `focused_output` keeps moving — a `ResizeGrab` forces it
/// onto its own output on the first motion that crosses — so a flight left
/// running elsewhere becomes the active one mid-grab and warps the pointer then.
#[test]
fn a_grab_install_ends_the_camera_flight_on_every_output() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    origin_view(&mut f);
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((400, 300)),
        true,
    );
    assert_eq!(
        f.state().active_output(),
        Some(out1),
        "precondition: the grab installs while the other output is inactive"
    );

    {
        let mut os = output_state(&out2);
        os.camera_target = Some(Point::from((2000.0, 0.0)));
        os.zoom_target = Some(2.0);
    }

    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    assert!(f.state().start_compositor_resize_with_edge(
        &pointer,
        &window,
        Point::from((790.0, 450.0)),
        BTN_LEFT,
        serial,
        Some(xdg_toplevel::ResizeEdge::Right),
        false,
    ));

    let os = output_state(&out2);
    assert!(
        os.camera_target.is_none() && os.zoom_target.is_none(),
        "the inactive output's flight is called off too"
    );
    drop(os);
    end_grab(&mut f);
}

/// Edge-pan is the one camera motion a grab does cause, and it drives the camera
/// directly rather than through `camera_target` — so the install cancel must
/// leave it alone.
#[test]
fn edge_pan_still_drives_the_camera_under_a_live_move_grab() {
    let mut f = Fixture::new();
    let out = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    origin_view(&mut f);
    f.state()
        .map_window(StageWindow::Client(window), Point::from((400, 300)), true);

    assert!(
        f.state()
            .try_start_gesture_move(Point::from((600.0, 450.0)), false)
    );
    // Drag into the left edge zone; the grab arms edge-pan itself.
    motion(&mut f, Point::from((50.0, 500.0)));
    assert!(
        { output_state(&out).edge_pan_velocity }.is_some(),
        "precondition: the drag armed edge-pan"
    );

    // Every tick re-drives the grab through `warp_pointer`, which re-arms the
    // request — so a suppression anywhere in that loop shows up as a camera that
    // stalls, not only as a missing first step.
    let mut previous = f.state().camera().x;
    for _ in 0..3 {
        f.state().apply_edge_pan();
        let now = f.state().camera().x;
        assert!(
            now < previous,
            "the grab's own camera motion still runs, tick after tick"
        );
        previous = now;
    }
    end_grab(&mut f);
}

/// Drag the right edge out by a fractional amount, so the grab is carrying a
/// real displacement rather than sitting on its origin where every delta reads
/// as zero regardless. Fractional because the delta feeds an `as i32`
/// truncation and the screen round-trip is only exact to within an ulp.
const DRAG_OUT: Point<f64, Logical> = Point::new(60.5, 0.0);

/// Cancelling the flight at grab install only covers the flights already
/// running. Sixteen producers can arm one mid-drag — a keyboard pan, a bookmark
/// jump, a window mapping — and the warp that follows is not the user's hand.
#[test]
fn a_camera_flight_armed_after_a_resize_grab_does_not_resize_the_window() {
    let mut f = Fixture::new();
    let (id, surface) = one_window(&mut f);
    let window = window_by_app_id(&mut f, "a").unwrap();

    let grab_at = Point::from((790.0, 450.0));
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    assert!(
        f.state().start_compositor_resize_with_edge(
            &pointer,
            &window,
            grab_at,
            BTN_LEFT,
            serial,
            Some(xdg_toplevel::ResizeEdge::Right),
            false,
        ),
        "precondition: the resize grab installed"
    );
    motion(&mut f, grab_at + DRAG_OUT);
    f.double_roundtrip(id);
    let before = configure_trace(&mut f, id, &surface);

    arm_distant_flight(&mut f);
    assert!(
        f.state().seat.get_pointer().unwrap().is_grabbed(),
        "precondition: the pointer is grabbed, so a warp reaches the grab \
         synchronously instead of taking the deferred branch"
    );
    for _ in 0..5 {
        f.state().apply_camera_animation(TICK);
    }
    f.double_roundtrip(id);

    assert!(
        f.state().camera().x > 100.0,
        "precondition: the flight moved the camera a long way — an unchanged \
         trace means nothing if nothing happened"
    );
    assert_eq!(
        configure_trace(&mut f, id, &surface),
        before,
        "the canvas slid under a held edge without resizing it"
    );
    end_grab(&mut f);
}

/// The zoom half. The cursor has to be off the grab origin first: parked on it
/// the screen delta is zero at any zoom, and an anchor that divided by the
/// *live* zoom would sail through.
#[test]
fn a_zoom_flight_armed_after_a_resize_grab_does_not_rescale_the_window() {
    let mut f = Fixture::new();
    let (id, surface) = one_window(&mut f);
    let window = window_by_app_id(&mut f, "a").unwrap();

    let grab_at = Point::from((790.0, 450.0));
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    assert!(
        f.state().start_compositor_resize_with_edge(
            &pointer,
            &window,
            grab_at,
            BTN_LEFT,
            serial,
            Some(xdg_toplevel::ResizeEdge::Right),
            false,
        ),
        "precondition: the resize grab installed"
    );
    motion(&mut f, grab_at + DRAG_OUT);
    f.double_roundtrip(id);
    let before = configure_trace(&mut f, id, &surface);
    assert_eq!(
        before.1,
        (460, 300),
        "precondition: the drag is displaced from the grab origin, so a zoom \
         change has something to rescale"
    );

    f.state().with_output_state(|os| os.zoom_target = Some(0.5));
    run_zoom_animation(&mut f);
    f.double_roundtrip(id);

    assert_eq!(
        f.state().zoom(),
        0.5,
        "precondition: the zoom flight actually ran"
    );
    assert_eq!(
        configure_trace(&mut f, id, &surface),
        before,
        "the drag stayed the size the hand made it, at the new zoom"
    );
    end_grab(&mut f);
}

/// The pinned arm freezes its anchor too. It already measured in screen space,
/// but re-projecting a canvas anchor through the live camera every motion drifts
/// it by the camera delta scaled by zoom.
#[test]
fn a_camera_flight_armed_after_a_pinned_resize_grab_does_not_resize_the_window() {
    let config = Config::from_toml(
        r#"
        [[window_rules]]
        app_id = "a"
        pinned_to_screen = true
        size = [400, 300]
    "#,
    )
    .unwrap();
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    origin_view(&mut f);
    let window = window_by_app_id(&mut f, "a").unwrap();
    let site = f
        .state()
        .stage
        .pin_of(&window)
        .expect("precondition: the rule pinned the window to the screen")
        .screen_pos;

    // Camera and zoom are the identity here, so the pin's screen rect and its
    // canvas rect coincide and the grab point can be written in either.
    let grab_at = Point::from((site.x as f64 + 390.0, site.y as f64 + 150.0));
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    assert!(
        f.state().start_compositor_resize_with_edge(
            &pointer,
            &window,
            grab_at,
            BTN_LEFT,
            serial,
            Some(xdg_toplevel::ResizeEdge::Right),
            false,
        ),
        "precondition: the pinned resize grab installed"
    );
    motion(&mut f, grab_at + DRAG_OUT);
    f.double_roundtrip(id);
    let before = configure_trace(&mut f, id, &surface);

    arm_distant_flight(&mut f);
    for _ in 0..5 {
        f.state().apply_camera_animation(TICK);
    }
    f.double_roundtrip(id);

    assert!(
        f.state().camera().x > 100.0,
        "precondition: the flight moved the camera a long way — an unchanged \
         trace means nothing if nothing happened"
    );
    assert_eq!(
        configure_trace(&mut f, id, &surface),
        before,
        "a pinned window ignores the camera it is pinned away from"
    );
    end_grab(&mut f);
}

/// The move grab is the deliberate opposite, and the ordering that breaks the
/// resize grab is the one that makes this work: hold a window, jump somewhere
/// else, and the window comes along.
#[test]
fn a_camera_flight_armed_after_a_move_grab_still_carries_the_window() {
    let mut f = Fixture::new();
    let (_id, _surface) = one_window(&mut f);
    let element = StageWindow::Client(window_by_app_id(&mut f, "a").unwrap());

    let grab_at = Point::from((600.0, 450.0));
    assert!(
        f.state().try_start_gesture_move(grab_at, false),
        "precondition: the move grab installed"
    );
    motion(&mut f, grab_at);
    assert_eq!(
        f.state().stage.position_of(&element),
        Some(Point::from((400, 300))),
        "precondition: the grab itself moved nothing"
    );

    arm_distant_flight(&mut f);
    for _ in 0..5 {
        f.state().apply_camera_animation(TICK);
    }

    let travelled = f.state().camera().x;
    assert!(
        travelled > 100.0,
        "precondition: the flight moved the camera a long way, not a hair"
    );
    let position = f.state().stage.position_of(&element).unwrap();
    assert!(
        (position.x as f64 - (400.0 + travelled)).abs() <= 1.0,
        "the held window rode the camera to {travelled}, landing at {position:?}"
    );
    end_grab(&mut f);
}

/// Long enough for the 50 ms auto-launch deadline to be safely in the past.
const PAST_MOMENTUM_DEADLINE: Duration = Duration::from_millis(80);

/// Two pans a few ms apart on `output`, which is what the velocity tracker needs
/// to produce a non-zero launch velocity — one sample launches at zero.
fn pan_burst(f: &mut Fixture, output: &smithay::output::Output, first_time_ms: u32) {
    f.state()
        .drift_pan_on(Point::from((10.0, 0.0)), first_time_ms, output);
    f.state()
        .drift_pan_on(Point::from((10.0, 0.0)), first_time_ms + 10, output);
}

fn coasting(output: &smithay::output::Output) -> bool {
    output_state(output).momentum.coasting
}

/// A pan burst arrives at touchpad rates, so the auto-launch timer is inserted
/// once and left alone; only its deadline moves.
#[test]
fn a_pan_burst_arms_the_momentum_timer_once() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);

    f.state().drift_pan(Point::from((10.0, 0.0)), 0);
    let armed = f.state().momentum_timer;
    assert!(
        armed.is_some(),
        "the first pan of the burst armed the timer"
    );
    let first_deadline = f.state().momentum_deadline.clone().unwrap().0;

    for n in 1..8 {
        f.state().drift_pan(Point::from((10.0, 0.0)), n * 5);
        assert_eq!(
            f.state().momentum_timer,
            armed,
            "pan {n} rode the timer already armed instead of re-registering one"
        );
    }

    assert!(
        f.state().momentum_deadline.clone().unwrap().0 > first_deadline,
        "the deadline is what the burst moves"
    );
}

/// The timer fires once, launches, and drops itself — and clears its own token
/// on the way out, so the next burst arms a fresh one. Leaving the token set
/// would wedge the lazy re-arm and silently kill auto-launch for the session.
#[test]
fn the_momentum_timer_fires_once_and_a_later_pan_re_arms_it() {
    let mut f = Fixture::new();
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);

    pan_burst(&mut f, &out, 0);
    std::thread::sleep(PAST_MOMENTUM_DEADLINE);
    f.pump(1);

    assert!(
        f.state().momentum_timer.is_none(),
        "the fired timer dropped itself"
    );
    assert!(
        f.state().momentum_deadline.is_none(),
        "and took its deadline with it"
    );
    assert!(
        coasting(&out),
        "the finger lift the touchpad never reported auto-launched momentum"
    );

    pan_burst(&mut f, &out, 100);
    assert!(
        !coasting(&out),
        "precondition: the new burst is live input again"
    );
    assert!(
        f.state().momentum_timer.is_some(),
        "the next burst arms a fresh timer"
    );

    std::thread::sleep(PAST_MOMENTUM_DEADLINE);
    f.pump(1);
    assert!(
        coasting(&out),
        "and it fires, so auto-launch survives the first burst"
    );
}

/// A pan driven onto a non-active output launches momentum *there*. The deadline
/// carries the output it was armed for, rather than the callback asking which
/// output happens to be active when it fires.
#[test]
fn a_pan_on_an_inactive_output_auto_launches_momentum_there() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    assert_eq!(
        f.state().active_output(),
        Some(out1.clone()),
        "precondition: the panned output is not the active one"
    );

    pan_burst(&mut f, &out2, 0);
    std::thread::sleep(PAST_MOMENTUM_DEADLINE);
    f.pump(1);

    assert!(coasting(&out2), "the output the hand panned coasts");
    assert!(
        !coasting(&out1),
        "and the active output, which was never panned, does not"
    );
}

/// A real finger lift launches immediately and disarms the deadline; the timer
/// it leaves behind fires once, finds nothing pending, and collects itself
/// without launching a second time.
#[test]
fn an_explicit_launch_leaves_the_armed_timer_to_collect_itself() {
    let mut f = Fixture::new();
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);

    pan_burst(&mut f, &out, 0);
    f.state().launch_momentum();
    assert!(
        f.state().momentum_deadline.is_none(),
        "the lift took the pending auto-launch with it"
    );
    assert!(coasting(&out), "and launched momentum itself");

    output_state(&out).momentum.stop();
    std::thread::sleep(PAST_MOMENTUM_DEADLINE);
    f.pump(1);

    assert!(
        f.state().momentum_timer.is_none(),
        "the orphaned timer collected itself"
    );
    assert!(
        !coasting(&out),
        "and did not launch a second time behind the lift"
    );
}

/// `cancel_animations_on` is per-output — fit, navigation and every grab install
/// route through it — so it must disarm only a launch pending on its own output.
#[test]
fn cancelling_one_output_leaves_anothers_pending_launch_armed() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));

    pan_burst(&mut f, &out2, 0);
    assert!(
        f.state().momentum_deadline.is_some(),
        "precondition: a launch is pending on the second output"
    );

    f.state().cancel_animations_on(&out1);
    assert!(
        f.state().momentum_deadline.is_some(),
        "a cancel on the other output leaves it armed"
    );

    f.state().cancel_animations_on(&out2);
    assert!(
        f.state().momentum_deadline.is_none(),
        "its own output's cancel disarms it"
    );

    std::thread::sleep(PAST_MOMENTUM_DEADLINE);
    f.pump(1);
    assert!(
        !coasting(&out2),
        "so the cancelled burst never coasts after the fact"
    );
}

/// An explicit launch disarms per-output for the same reason: a finger lift
/// reported on one screen must not swallow the auto-launch another screen's
/// burst is still waiting on.
#[test]
fn launching_one_output_leaves_anothers_pending_launch_armed() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    assert_eq!(
        f.state().active_output(),
        Some(out1.clone()),
        "precondition: the lift below lands on the output that was never panned"
    );

    pan_burst(&mut f, &out2, 0);
    assert!(
        f.state().momentum_deadline.is_some(),
        "precondition: a launch is pending on the second output"
    );

    // The finger-lift path, which targets the active output.
    f.state().launch_momentum();
    assert!(
        f.state().momentum_deadline.is_some(),
        "a lift on the other output leaves it armed"
    );

    std::thread::sleep(PAST_MOMENTUM_DEADLINE);
    f.pump(1);
    assert!(
        coasting(&out2),
        "so the burst still gets the auto-launch it was waiting for"
    );
}

/// A fullscreen window is parked at its output's camera; a drag that outlives
/// the entry must not slide it off. Motion after the park leaves both the
/// camera and the window exactly where the park put them, a release banks no
/// fling from the frozen portion of the drag, and a later exit restores the
/// camera the drag interrupted rather than the rounded park value.
#[test]
fn fullscreen_mid_pan_grab_freezes_the_camera_and_release_banks_no_fling() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    // Non-integer, so the park (which rounds) is a different value than the
    // camera the exit must restore — an integer start would make the two
    // indistinguishable and the exit assertion below trivially true either way.
    f.state().with_output_state(|os| {
        os.camera = Point::from((120.3, 45.7));
        os.zoom = 1.0;
    });
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((3000, 3000)),
        true,
    );
    let pre_fullscreen_camera = f.state().camera();

    let device = FakeDevice::mouse();
    pointer_to(&mut f, &device, Point::from((200.0, 100.0)));
    press(&mut f, &device, BTN_LEFT);
    assert!(
        f.state()
            .seat
            .get_pointer()
            .unwrap()
            .with_grab(|_, g| g.is::<PanGrab>())
            .unwrap_or(false),
        "precondition: a press on empty canvas starts a PanGrab"
    );

    f.state().enter_fullscreen(&window, Some(output.clone()));
    let parked_camera = f.state().camera();
    assert_ne!(
        parked_camera, pre_fullscreen_camera,
        "precondition: the park rounded the camera to a different value"
    );
    assert_eq!(
        window_position(&mut f, &window),
        parked_camera.to_i32_round(),
        "precondition: the fullscreen park mapped the window at the camera origin"
    );

    // Pinned timestamps 10 ms apart: the fling assertion below only bites while
    // both motions stay inside the velocity window, and the shared clock cannot
    // promise that under parallel threads.
    pointer_relative_motion_at(&mut f, &device, Point::from((80.0, -40.0)), 1_000);
    pointer_relative_motion_at(&mut f, &device, Point::from((-30.0, 90.0)), 1_010);
    assert_eq!(
        f.state().camera(),
        parked_camera,
        "further drag motion after fullscreen lands must not move the locked camera"
    );
    assert_eq!(
        window_position(&mut f, &window),
        parked_camera.to_i32_round(),
        "and the window must stay exactly where the park put it"
    );

    release(&mut f, &device, BTN_LEFT);
    // `launch()` always flips `coasting` true and lets the next tick judge the
    // velocity — with the tracker never accumulated, that tick finds zero and
    // clears it right back.
    f.state().tick_all_animations();
    assert!(
        !coasting(&output),
        "the release must bank no momentum for a drag that only ran while frozen"
    );
    assert_eq!(
        f.state().camera(),
        parked_camera,
        "no fling appeared on release"
    );

    f.state().exit_fullscreen_on(&output);
    assert_eq!(
        f.state().camera(),
        pre_fullscreen_camera,
        "exiting restores the camera the drag was interrupted at, not the rounded park value"
    );
}

/// The compensating pointer warp only makes sense because the pan it offsets
/// moves the camera. Once fullscreen locks the camera, the warp must go
/// inert too, or the cursor slides opposite the drag while the camera sits
/// still.
#[test]
fn a_live_pan_grabs_pointer_does_not_drift_once_fullscreen_locks_the_camera() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    origin_view(&mut f);
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((3000, 3000)),
        true,
    );

    let device = FakeDevice::mouse();
    pointer_to(&mut f, &device, Point::from((500.0, 500.0)));
    press(&mut f, &device, BTN_LEFT);
    f.state().enter_fullscreen(&window, Some(output.clone()));

    let delta = Point::from((30.0, -15.0));
    let before1 = f.state().seat.get_pointer().unwrap().current_location();
    pointer_relative_motion(&mut f, &device, delta);
    let after1 = f.state().seat.get_pointer().unwrap().current_location();
    assert!(
        approx(after1 - before1, delta, 1e-6),
        "the cursor must track the hand exactly while the camera is locked"
    );

    let before2 = after1;
    pointer_relative_motion(&mut f, &device, delta);
    let after2 = f.state().seat.get_pointer().unwrap().current_location();
    assert!(
        approx(after2 - before2, delta, 1e-6),
        "a second motion must not compound an extra offset carried over from the first"
    );

    release(&mut f, &device, BTN_LEFT);
    f.state().exit_fullscreen_on(&output);
}

/// The multi-output regression guard: the fullscreen lock is
/// `is_output_fullscreen(output)` on the grab's own output, never global. A
/// second monitor must keep panning normally while the first is fullscreen —
/// a lock that read the active output (or any output) instead of the grab's
/// own would freeze every viewport, not just the fullscreen one.
#[test]
fn panning_output_b_still_works_while_output_a_is_fullscreen() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));

    let id = f.add_client();
    map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    f.state().enter_fullscreen(&window, Some(out1.clone()));
    let out1_parked = { output_state(&out1).camera };

    // Route the next press to output B, far from anything on the shared canvas.
    {
        let mut os = output_state(&out2);
        os.camera = Point::from((50_000.0, 50_000.0));
        os.zoom = 1.0;
    }
    f.state().focused_output = Some(out2.clone());

    let device = FakeDevice::mouse();
    pointer_to(&mut f, &device, Point::from((50_100.0, 50_100.0)));
    press(&mut f, &device, BTN_LEFT);
    let grab_output = f
        .state()
        .seat
        .get_pointer()
        .unwrap()
        .with_grab(|_, g| g.downcast_ref::<PanGrab>().map(|p| p.output.clone()))
        .flatten();
    assert_eq!(
        grab_output,
        Some(out2.clone()),
        "precondition: the press pinned the grab to output B"
    );

    let out2_before = { output_state(&out2).camera };
    pointer_relative_motion(&mut f, &device, Point::from((60.0, 40.0)));

    assert_ne!(
        { output_state(&out2).camera },
        out2_before,
        "output B's camera must still move — the fullscreen lock is per-output"
    );
    assert_eq!(
        { output_state(&out1).camera },
        out1_parked,
        "output A's parked camera must stay put; a global guard would be the real regression"
    );

    release(&mut f, &device, BTN_LEFT);
    f.state().exit_fullscreen_on(&out1);
}

/// Touch reaches the same inertness as the mouse grab: `apply_pan` funnels
/// through `drift_pan_on` exactly like `PanGrab` does, so a one-finger pan
/// already running when fullscreen lands goes inert too.
#[test]
fn a_touch_pan_in_flight_goes_inert_once_fullscreen_locks_the_camera() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    origin_view(&mut f);
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((3000, 3000)),
        true,
    );

    touch_down(&mut f, Point::from((500.0, 500.0)), 0);
    // Clears the dead zone; the recognizer only baselines here, no pan yet.
    touch_motion(&mut f, Point::from((500.0, 480.0)), 0);
    // A real pan, still pre-fullscreen — proves the rig actually drives the camera.
    touch_motion(&mut f, Point::from((500.0, 440.0)), 0);
    assert_ne!(
        f.state().camera(),
        Point::from((0.0, 0.0)),
        "precondition: the touch drag is really panning before fullscreen lands"
    );

    f.state().enter_fullscreen(&window, Some(output.clone()));
    let parked_camera = f.state().camera();

    touch_motion(&mut f, Point::from((500.0, 400.0)), 0);
    touch_motion(&mut f, Point::from((500.0, 350.0)), 0);
    assert_eq!(
        f.state().camera(),
        parked_camera,
        "touch motion after fullscreen lands must not move the locked camera"
    );
    assert_eq!(
        window_position(&mut f, &window),
        parked_camera.to_i32_round(),
        "the window must stay exactly where the park put it"
    );

    touch_up(&mut f, 0);
    f.state().exit_fullscreen_on(&output);
}

/// The pinch-zoom counterpart: `apply_zoom` has its own fullscreen guard (it
/// can't funnel through `drift_pan_on` like a pan does — a pinch also
/// re-anchors the camera), so a two-finger pinch already spreading when
/// fullscreen lands must leave both the camera and the zoom exactly where the
/// park put them.
#[test]
fn a_touch_pinch_in_flight_goes_inert_once_fullscreen_locks_the_zoom() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    origin_view(&mut f);
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((3000, 3000)),
        true,
    );

    // A pinch-IN (fingers converging): `MAX_ZOOM` is 1.0 (the canvas's "actual
    // pixels" ceiling), so a spreading pinch from zoom 1.0 clamps away to
    // nothing — only a contraction can show a real, observable zoom change here.
    touch_down(&mut f, Point::from((300.0, 500.0)), 0);
    touch_down(&mut f, Point::from((900.0, 500.0)), 1);
    // A real pinch, still pre-fullscreen — proves the rig actually drives the
    // zoom (same step shape as `two_finger_spread_on_canvas_zooms` in the
    // recognizer's own unit tests, mirrored to contract instead of spread).
    touch_motion(&mut f, Point::from((360.0, 500.0)), 0);
    touch_motion(&mut f, Point::from((840.0, 500.0)), 1);
    touch_motion(&mut f, Point::from((420.0, 500.0)), 0);
    touch_motion(&mut f, Point::from((780.0, 500.0)), 1);
    touch_motion(&mut f, Point::from((480.0, 500.0)), 0);
    touch_motion(&mut f, Point::from((720.0, 500.0)), 1);
    assert_ne!(
        f.state().zoom(),
        1.0,
        "precondition: the touch pinch is really zooming before fullscreen lands"
    );

    f.state().enter_fullscreen(&window, Some(output.clone()));
    let parked_camera = f.state().camera();
    let parked_zoom = f.state().zoom();

    touch_motion(&mut f, Point::from((540.0, 500.0)), 0);
    touch_motion(&mut f, Point::from((660.0, 500.0)), 1);
    touch_motion(&mut f, Point::from((570.0, 500.0)), 0);
    touch_motion(&mut f, Point::from((630.0, 500.0)), 1);
    assert_eq!(
        f.state().zoom(),
        parked_zoom,
        "pinch motion after fullscreen lands must not move the locked zoom"
    );
    assert_eq!(
        f.state().camera(),
        parked_camera,
        "nor the locked camera — apply_zoom re-anchors it on every engaged frame"
    );
    assert_eq!(
        window_position(&mut f, &window),
        parked_camera.to_i32_round(),
        "the window must stay exactly where the park put it"
    );

    touch_up(&mut f, 0);
    touch_up(&mut f, 1);
    f.state().exit_fullscreen_on(&output);
}

/// Edge-pan drives the camera directly, not through `camera_target`, so it
/// needs its own fullscreen check in `effective_edge_pan_velocity` — a
/// velocity the camera cannot act on must not walk the cursor either.
#[test]
fn edge_pan_goes_inert_on_a_fullscreen_output() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    f.state().enter_fullscreen(&window, Some(output.clone()));

    {
        let mut os = output_state(&output);
        os.edge_pan_velocity = Some(Point::from((-400.0, 0.0)));
        os.edge_pan_screen_pos = Some(Point::from((0.0, 500.0)));
    }

    let camera_before = f.state().camera();
    let pointer_before = f.state().seat.get_pointer().unwrap().current_location();

    f.state().apply_edge_pan();

    assert_eq!(
        f.state().camera(),
        camera_before,
        "edge-pan must not move a fullscreen output's locked camera"
    );
    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        pointer_before,
        "with nothing to compensate for, the warp must not run either"
    );

    f.state().exit_fullscreen_on(&output);
}

/// Arming a zoom/camera flight on a fullscreen output must clear it, not
/// merely leave it stuck — `set_zoom_target(None)` is only reached once zoom
/// *arrives*, so an ignored target would spin the lerp for the whole
/// fullscreen session. Both backends call `disarm_view_flight_on_fullscreen`
/// to answer it: winit inline, udev once per output per tick.
#[test]
fn arming_a_zoom_target_on_a_fullscreen_output_gets_cleared_not_ignored() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    f.state().enter_fullscreen(&window, Some(output.clone()));

    // `zoom_to_anchored` arms targets without consulting fullscreen; only the
    // eventual camera/zoom write is guarded. Zooming out since `MAX_ZOOM` is 1.0.
    f.state().zoom_to_anchored(0.5);
    assert!(
        f.state().zoom_target().is_some(),
        "precondition: arming a target is not itself guarded"
    );

    // Winit's frame loop calls this directly, once per frame.
    f.state().disarm_view_flight_on_fullscreen(&output);
    assert!(
        f.state().zoom_target().is_none(),
        "the per-tick disarm cleared the target rather than leaving it stuck"
    );
    assert!(f.state().camera_target().is_none());
    assert_eq!(f.state().zoom(), 1.0, "zoom itself never left the park");

    // udev's tick_all_animations calls the same function once per output.
    f.state().zoom_to_anchored(0.5);
    f.state().tick_all_animations();
    assert!(
        f.state().zoom_target().is_none(),
        "udev's per-output tick disarms it the same way"
    );
    assert_eq!(f.state().zoom(), 1.0);

    f.state().exit_fullscreen_on(&output);
}
