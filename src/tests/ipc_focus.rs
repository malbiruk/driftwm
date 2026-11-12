//! `driftwm msg focus <window>` focuses and raises the window, and pans to it
//! only when it is not already fully on screen. "On screen" is judged where
//! the view lands, not at the frame a flight still under way happens to be
//! in, for a live client and for a stand-in alike.

use smithay::utils::{Logical, Point, Size};

use super::{Fixture, map_window, settle, window_by_app_id};
use crate::ipc::dispatch;
use crate::ipc::protocol::{Request, Response, WindowSelector};
use crate::state::StageWindow;

/// Where an unrelated pan flight is heading: far enough right that a window
/// near the canvas origin is off screen once it lands.
const FAR_CAMERA: (f64, f64) = (5000.0, 0.0);

fn origin_view(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.camera_target = None;
        os.zoom = 1.0;
        os.zoom_target = None;
        os.zoom_animation_anchor = None;
    });
}

/// Start a pan flight toward [`FAR_CAMERA`], and confirm `element` is on screen
/// now but will not be once the flight lands.
fn pan_away_from(f: &mut Fixture, element: &StageWindow) {
    f.state().set_camera_target(Some(FAR_CAMERA.into()));
    assert!(
        f.state().window_fully_in_viewport(element),
        "precondition: the window is on screen in the current frame"
    );
    let (camera, zoom) = f.state().landing_view();
    assert!(
        !f.state().window_fully_in_view_at(element, camera, zoom),
        "precondition: the flight leaves the window behind"
    );
}

fn focus(f: &mut Fixture, element: &StageWindow) {
    let id = f.state().stage.id_of(element).expect("staged").0;
    let reply = dispatch(Request::Focus(Some(WindowSelector::Id(id))), f.state());
    assert!(
        matches!(reply, Ok(Response::Focused(Some(_)))),
        "expected a Focused reply, got {reply:?}"
    );
}

/// The `Focused` reply carries the client pid (same value as `state`
/// windows[]), so an agent can map focus to the AT-SPI tree without a second
/// `msg state` round-trip. Stand-ins have no client, so theirs is `None`.
#[test]
fn focus_reply_carries_the_client_pid() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "app", (400, 300));
    let window = window_by_app_id(&mut f, "app").unwrap();
    let element = StageWindow::Client(window);

    let wid = f.state().stage.id_of(&element).expect("staged").0;
    let reply = dispatch(Request::Focus(Some(WindowSelector::Id(wid))), f.state());
    let info = match reply {
        Ok(Response::Focused(Some(info))) => info,
        other => panic!("expected a Focused reply, got {other:?}"),
    };
    let state_pid = f
        .state()
        .window_inventory()
        .into_iter()
        .find(|w| w.id == wid)
        .and_then(|w| w.pid);
    assert_eq!(
        info.pid, state_pid,
        "Focused pid must match the state inventory pid"
    );
    assert!(
        info.pid.is_some(),
        "a live client focus reply must carry a pid"
    );
}

#[test]
fn focus_on_a_window_a_pan_flight_is_leaving_behind_pans_to_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "app", (400, 300));
    let window = window_by_app_id(&mut f, "app").unwrap();
    origin_view(&mut f);
    let element = StageWindow::Client(window.clone());
    f.state()
        .map_window(element.clone(), Point::from((200, 200)), false);
    pan_away_from(&mut f, &element);

    focus(&mut f, &element);
    settle(&mut f);

    assert!(
        f.state().window_fully_in_viewport(&element),
        "msg focus must bring the window on screen where the view lands"
    );
}

#[test]
fn focus_on_a_stand_in_a_pan_flight_is_leaving_behind_pans_to_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let origin: Point<i32, Logical> = Point::from((200, 200));
    let sid =
        f.state()
            .insert_suspended_for_test(7, origin, Size::from((320, 240)), "susapp", "Sus App");
    let element = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    pan_away_from(&mut f, &element);

    focus(&mut f, &element);
    settle(&mut f);

    assert!(
        f.state().window_fully_in_viewport(&element),
        "msg focus must bring the stand-in on screen where the view lands"
    );

    f.state().dismiss_suspended(sid);
}

/// `state` outputs[] publish each output's global-layout origin (the same
/// space `msg cursor`/`msg pointer` use), so an agent can map global screen
/// coords to outputs. Outputs tile left-to-right by add order.
#[test]
fn state_outputs_publish_global_layout_origins() {
    use crate::ipc::state_info;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));

    let state = state_info(f.state());
    assert_eq!(state.outputs.len(), 2);

    // First output anchors the layout; the second sits to its right.
    assert_eq!(state.outputs[0].layout_origin, [0, 0]);
    assert_eq!(
        state.outputs[1].layout_origin,
        [1920, 0],
        "second output must start where the 1920-wide first ends"
    );
}
