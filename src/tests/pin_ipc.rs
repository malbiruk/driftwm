//! Scenarios that check the screen<->canvas math run at zoom 0.5, where a
//! wrong space is a wrong number rather than a coincidental identity.

use smithay::desktop::Window;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size};

use driftwm::canvas::{CanvasPos, ScreenPos, canvas_to_screen, screen_to_canvas};
use driftwm::stage::ElementId;

use crate::ipc::dispatch;
use crate::ipc::protocol::{Reply, Request, Response, WindowSelector};
use crate::state::window_animation::AnimSpace;
use crate::state::{AdoptOrigin, DeferredAdopt, StageWindow, SuspendedId};

use super::client::ClientId;
use super::{
    Fixture, is_activated, map_window, server_surface, tick_until_settled, window_by_app_id,
};

fn pin(f: &mut Fixture, window: Option<WindowSelector>, value: Option<bool>) -> Reply {
    dispatch(Request::Pin { window, value }, f.state())
}

fn visual_rect(f: &mut Fixture, eid: ElementId) -> Rectangle<f64, Logical> {
    f.state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap()
}

/// The screen site `canvas_to_screen`'s own math predicts for a window at
/// `loc`, rounded the way the pin verb rounds it.
fn expected_screen_site(
    loc: Point<f64, Logical>,
    camera: Point<f64, Logical>,
    zoom: f64,
) -> Point<i32, Logical> {
    let site = canvas_to_screen(CanvasPos(loc), camera, zoom).0;
    Point::from((site.x.round() as i32, site.y.round() as i32))
}

/// One output, one client with a focused 400x300 window (`"a"`), settled at
/// `zoom` with the camera at the origin and at rest.
fn setup(zoom: f64) -> (Fixture, Output, ClientId, Window, ElementId) {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let client = f.add_client();
    map_window(&mut f, client, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.zoom = zoom;
        os.camera_target = None;
        os.zoom_target = None;
    });
    f.state().update_output_from_camera();
    tick_until_settled(&mut f);
    let eid = f.state().stage.id_of(&window).unwrap();
    (f, output, client, window, eid)
}

/// A stand-in with no client behind it, plus its stage id.
fn insert_suspended(f: &mut Fixture) -> (SuspendedId, u64) {
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let element = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let window_id = f.state().stage.id_of(&element).unwrap().0;
    (sid, window_id)
}

#[test]
fn set_on_pins_at_the_screen_rect_the_window_is_drawn_at() {
    let (mut f, output, _client, window, eid) = setup(0.5);
    let (camera, zoom) = (f.state().camera(), f.state().zoom());
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let on_screen = Rectangle::new(
        Point::from(((loc.x - camera.x) * zoom, (loc.y - camera.y) * zoom)),
        Size::from((400.0 * zoom, 300.0 * zoom)),
    );

    assert_eq!(pin(&mut f, None, Some(true)), Ok(Response::Pin(true)));

    assert_eq!(
        f.state().stage.pin_of(&window).unwrap().screen_pos,
        expected_screen_site(loc, camera, zoom),
        "pinned at the site canvas_to_screen(position) predicts"
    );
    assert!(
        !f.state().stage.focus_history().iter().any(|w| w == &window),
        "pinning drops the window from the focus cycle"
    );
    assert_eq!(
        f.state().window_animations.geometry_space(eid),
        Some(AnimSpace::Screen(output.name())),
        "the pinned chase runs in its output's screen space"
    );
    let first_frame = visual_rect(&mut f, eid);
    assert_eq!(
        (first_frame.loc, first_frame.size),
        (on_screen.loc, on_screen.size),
        "the first frame draws exactly the pre-pin on-screen rect"
    );
}

#[test]
fn set_off_returns_to_canvas_with_no_visual_jump() {
    let (mut f, output, _client, window, eid) = setup(0.5);
    assert_eq!(pin(&mut f, None, Some(true)), Ok(Response::Pin(true)));
    tick_until_settled(&mut f);
    let before = f.state().window_screen_rect_on(&window, &output).unwrap();
    let site = f.state().stage.pin_of(&window).cloned().unwrap();
    let (camera, zoom) = (f.state().camera(), f.state().zoom());

    assert_eq!(pin(&mut f, None, Some(false)), Ok(Response::Pin(false)));

    assert!(!f.state().is_pinned(&window), "the window unpinned");
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(
            screen_to_canvas(ScreenPos(site.screen_pos.to_f64()), camera, zoom)
                .0
                .to_i32_round()
        ),
        "landed on the canvas point its screen site maps to under the live camera"
    );
    assert_eq!(
        f.state().window_animations.geometry_space(eid),
        Some(AnimSpace::Canvas),
        "the unpinned chase runs through the camera again"
    );
    let after = f.state().window_screen_rect_on(&window, &output).unwrap();
    assert_eq!(
        (after.loc, after.size),
        (before.loc, before.size),
        "unpinning drew no visual jump"
    );
}

#[test]
fn get_reports_pin_membership_and_re_setting_the_held_state_is_a_no_op() {
    let (mut f, _output, _client, window, eid) = setup(0.5);

    assert_eq!(pin(&mut f, None, None), Ok(Response::Pin(false)));
    assert_eq!(pin(&mut f, None, Some(true)), Ok(Response::Pin(true)));
    tick_until_settled(&mut f);
    assert_eq!(
        pin(&mut f, Some(WindowSelector::Id(eid.0)), None),
        Ok(Response::Pin(true)),
        "a selector reports the same membership a bare get does"
    );

    let site = f.state().stage.pin_of(&window).cloned();
    let chases = f.state().window_animations.len();
    assert_eq!(
        pin(&mut f, None, Some(true)),
        Ok(Response::Pin(true)),
        "re-setting the held state echoes it back"
    );
    assert_eq!(
        f.state().stage.pin_of(&window).cloned(),
        site,
        "the site did not move"
    );
    assert_eq!(
        f.state().window_animations.len(),
        chases,
        "no new chase started"
    );

    assert_eq!(pin(&mut f, None, Some(false)), Ok(Response::Pin(false)));
    tick_until_settled(&mut f);
    let position = f.state().stage.position_of(&window);
    let chases = f.state().window_animations.len();
    assert_eq!(
        pin(&mut f, None, Some(false)),
        Ok(Response::Pin(false)),
        "and so does re-setting off"
    );
    assert_eq!(
        f.state().stage.position_of(&window),
        position,
        "position untouched"
    );
    assert_eq!(
        f.state().window_animations.len(),
        chases,
        "no new chase started"
    );
}

#[test]
fn the_pin_seed_is_the_in_flight_visual_not_the_settled_rect() {
    let (mut f, _output, _client, window, eid) = setup(0.5);
    f.state()
        .map_window(window.clone(), Point::from((900, 300)), false);
    f.state()
        .animate_window_move_from(&window, Point::from((0, 0)), None);
    f.state()
        .tick_window_animations(std::time::Duration::from_millis(16));
    let in_flight = visual_rect(&mut f, eid);

    assert_eq!(pin(&mut f, None, Some(true)), Ok(Response::Pin(true)));

    let (camera, zoom) = (f.state().camera(), f.state().zoom());
    let on_screen = Rectangle::new(
        Point::from((
            (in_flight.loc.x - camera.x) * zoom,
            (in_flight.loc.y - camera.y) * zoom,
        )),
        Size::from((in_flight.size.w * zoom, in_flight.size.h * zoom)),
    );
    let seed = visual_rect(&mut f, eid);
    assert_eq!(
        (seed.loc, seed.size),
        (on_screen.loc, on_screen.size),
        "the pin's first frame is the in-flight visual on screen, not the settled target rect"
    );
}

#[test]
fn fullscreen_windows_refuse_pin_and_unpin_but_answer_get_and_keep_their_pin_on_exit() {
    let (mut f, output, _client, window, _eid) = setup(0.5);
    assert_eq!(pin(&mut f, None, Some(true)), Ok(Response::Pin(true)));
    f.state().enter_fullscreen(&window, Some(output.clone()));

    assert_eq!(
        pin(&mut f, None, Some(true)),
        Err("fullscreen windows can't be pinned".to_string())
    );
    assert_eq!(
        pin(&mut f, None, Some(false)),
        Err("fullscreen windows can't be pinned".to_string())
    );
    assert_eq!(
        pin(&mut f, None, None),
        Ok(Response::Pin(false)),
        "get reads false while fullscreen holds the pin"
    );

    f.state().exit_fullscreen_on(&output);
    assert!(
        f.state().is_pinned(&window),
        "the pin came back on exit; neither refusal consumed it"
    );
}

#[test]
fn stand_ins_and_deferred_adopt_targets_refuse_to_pin() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (sid, stand_in_id) = insert_suspended(&mut f);
    assert!(
        pin(&mut f, Some(WindowSelector::Id(stand_in_id)), None)
            .is_err_and(|e| e.contains("stand-in")),
        "a get on a stand-in refuses"
    );
    assert!(
        pin(&mut f, Some(WindowSelector::Id(stand_in_id)), Some(true))
            .is_err_and(|e| e.contains("stand-in")),
        "a set on a stand-in refuses too"
    );
    f.state().dismiss_suspended(sid);

    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    let window_id = f.state().stage.id_of(&window).unwrap().0;
    let (hidden_sid, _) = insert_suspended(&mut f);
    // The stash entry is itself what hides the window; nothing else has to be
    // staged for the refusal to see it.
    f.state().deferred_adoptions.push(DeferredAdopt {
        root: server_surface(&window),
        sid: hidden_sid,
        origin: AdoptOrigin::FirstCommit,
    });

    assert!(
        pin(&mut f, Some(WindowSelector::Id(window_id)), Some(true))
            .is_err_and(|e| e.contains("not on screen")),
        "a window nobody can see has no on-screen rect to pin"
    );

    // Drained by hand so the teardown sweep never meets an adopt it can't land.
    f.state().deferred_adoptions.pop();
    f.state().dismiss_suspended(hidden_sid);
}

#[test]
fn unpinning_by_selector_does_not_steal_activation_from_the_focused_window() {
    let (mut f, _output, client, a, _eid) = setup(0.5);
    map_window(&mut f, client, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    let b_id = f.state().stage.id_of(&b).unwrap().0;
    assert_eq!(
        pin(&mut f, Some(WindowSelector::Id(b_id)), Some(true)),
        Ok(Response::Pin(true))
    );
    f.state().raise_and_focus(&a, SERIAL_COUNTER.next_serial());

    assert_eq!(
        pin(&mut f, Some(WindowSelector::Id(b_id)), Some(false)),
        Ok(Response::Pin(false))
    );

    assert!(is_activated(&a), "a keeps its Activated hint");
    assert!(!is_activated(&b), "b is not activated by the selector path");
}

#[test]
fn the_action_verb_still_toggles_pin_after_the_split() {
    let (mut f, _output, _client, window, _eid) = setup(0.5);
    let (camera, zoom) = (f.state().camera(), f.state().zoom());
    let canvas_loc = f.state().stage.position_of(&window).unwrap().to_f64();

    assert_eq!(
        dispatch(Request::Action("toggle-pin-to-screen".into()), f.state()),
        Ok(Response::Ok)
    );
    assert_eq!(
        f.state().stage.pin_of(&window).unwrap().screen_pos,
        expected_screen_site(canvas_loc, camera, zoom),
        "the action verb pins at the site canvas_to_screen(position) predicts"
    );

    tick_until_settled(&mut f);
    dispatch(Request::Action("toggle-pin-to-screen".into()), f.state()).unwrap();

    assert!(!f.state().is_pinned(&window), "the second toggle unpinned");
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(canvas_loc.to_i32_round()),
        "and landed back on the canvas point it started from"
    );
}
