//! `driftwm msg zoom` / `driftwm msg camera` setters on a fullscreen output
//! must exit fullscreen before applying, not arm a target the per-tick
//! fullscreen disarm silently drops. The disarm and the camera/zoom locks it
//! backs up live in `src/state/viewport_animation.rs` and
//! `src/state/viewport.rs`.

use smithay::output::Output;

use super::{Fixture, map_window, settle, window_by_app_id};
use crate::ipc::dispatch;
use crate::ipc::protocol::{Request, Response};

fn read_camera(f: &mut Fixture) -> (f64, f64) {
    match dispatch(Request::Camera(None), f.state()) {
        Ok(Response::Camera { x, y }) => (x, y),
        other => panic!("expected a Camera reply, got {other:?}"),
    }
}

fn read_zoom(f: &mut Fixture) -> f64 {
    match dispatch(Request::Zoom(None), f.state()) {
        Ok(Response::Zoom(z)) => z,
        other => panic!("expected a Zoom reply, got {other:?}"),
    }
}

fn approx(got: (f64, f64), want: (f64, f64), tol: f64) -> bool {
    (got.0 - want.0).abs() < tol && (got.1 - want.1).abs() < tol
}

/// A single window fullscreened on a freshly added output, camera untouched
/// beforehand — so entering (and later exiting) fullscreen alone never moves
/// the output away from its baseline position.
fn fullscreen_one_window(f: &mut Fixture) -> Output {
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(f, id, "term", (400, 300));
    let window = window_by_app_id(f, "term").unwrap();
    f.state().enter_fullscreen(&window, Some(output.clone()));
    assert!(f.state().is_fullscreen(), "precondition: fullscreen");
    output
}

#[test]
fn zoom_set_on_fullscreen_output_exits_fullscreen_and_applies() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    fullscreen_one_window(&mut f);

    let reply = dispatch(Request::Zoom(Some(0.8)), f.state());
    assert_eq!(reply, Ok(Response::Zoom(0.8)));

    settle(&mut f);

    assert!(
        !f.state().is_fullscreen(),
        "a zoom set must exit fullscreen"
    );
    assert_eq!(
        read_zoom(&mut f),
        0.8,
        "the zoom must actually apply, not get silently dropped"
    );
}

#[test]
fn camera_set_on_fullscreen_output_exits_fullscreen_and_applies() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    fullscreen_one_window(&mut f);

    let want = (300.0, -150.0);
    let reply = dispatch(Request::Camera(Some(want)), f.state());
    assert_eq!(
        reply,
        Ok(Response::Camera {
            x: want.0,
            y: want.1
        })
    );

    settle(&mut f);

    assert!(
        !f.state().is_fullscreen(),
        "a camera set must exit fullscreen"
    );
    assert!(
        approx(read_camera(&mut f), want, 1e-6),
        "the camera must actually land at the requested point, not get silently dropped"
    );
}

/// The ordering trap: `cmd_camera` derives its target from `state.zoom()`,
/// which a fullscreen park pins at 1.0 — the exit has to run *before* that
/// read, or the target is derived from the wrong (parked) zoom while the
/// camera actually converges under the restored one. A pre-fullscreen zoom of
/// 1.0 can't tell the two orderings apart (both read the same value), so this
/// deliberately zooms out first.
#[test]
fn camera_set_on_fullscreen_output_uses_the_restored_zoom_not_the_parked_one() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();

    dispatch(Request::Zoom(Some(0.6)), f.state()).unwrap();
    settle(&mut f);
    assert_eq!(
        read_zoom(&mut f),
        0.6,
        "precondition: a non-1.0 pre-fullscreen zoom"
    );

    f.state().enter_fullscreen(&window, Some(output));
    assert_eq!(
        f.state().zoom(),
        1.0,
        "precondition: fullscreen parks zoom at 1.0"
    );

    let want = (300.0, -150.0);
    dispatch(Request::Camera(Some(want)), f.state()).unwrap();
    settle(&mut f);

    assert!(!f.state().is_fullscreen());
    assert!(
        approx(read_camera(&mut f), want, 1e-6),
        "the camera must land where asked, computed from the restored zoom \
         (0.6) rather than the parked one (1.0)"
    );
}

#[test]
fn zoom_query_on_fullscreen_output_does_not_exit_fullscreen() {
    let mut f = Fixture::new();
    let output = fullscreen_one_window(&mut f);

    let reply = dispatch(Request::Zoom(None), f.state());
    assert_eq!(reply, Ok(Response::Zoom(1.0)));
    assert!(
        f.state().is_fullscreen(),
        "a bare query must not exit fullscreen"
    );

    f.state().exit_fullscreen_on(&output);
}

#[test]
fn camera_query_on_fullscreen_output_does_not_exit_fullscreen() {
    let mut f = Fixture::new();
    let output = fullscreen_one_window(&mut f);

    assert!(dispatch(Request::Camera(None), f.state()).is_ok());
    assert!(
        f.state().is_fullscreen(),
        "a bare query must not exit fullscreen"
    );

    f.state().exit_fullscreen_on(&output);
}

#[test]
fn zoom_set_without_fullscreen_still_applies() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));

    let reply = dispatch(Request::Zoom(Some(0.8)), f.state());
    assert_eq!(reply, Ok(Response::Zoom(0.8)));

    settle(&mut f);
    assert_eq!(read_zoom(&mut f), 0.8);
}

#[test]
fn camera_set_without_fullscreen_still_applies() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));

    let want = (300.0, -150.0);
    let reply = dispatch(Request::Camera(Some(want)), f.state());
    assert_eq!(
        reply,
        Ok(Response::Camera {
            x: want.0,
            y: want.1
        })
    );

    settle(&mut f);
    assert!(approx(read_camera(&mut f), want, 1e-6));
}

/// A non-finite camera target is rejected outright — `serde_json` parses an
/// overflowing literal like `1e999` to `inf` rather than erroring, so this is
/// a realistic wire request, not a synthetic one.
#[test]
fn camera_set_rejects_a_non_finite_target() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let reply = dispatch(Request::Camera(Some((f64::INFINITY, 0.0))), f.state());
    assert!(
        reply.is_err(),
        "a non-finite camera target must be rejected"
    );
}

/// The finiteness check has to run *above* the fullscreen exit, or a bad
/// request tears down fullscreen on its way to being rejected. This pins the
/// ordering: a rejected non-finite request leaves an existing fullscreen
/// completely intact.
#[test]
fn camera_set_rejecting_a_non_finite_target_leaves_fullscreen_intact() {
    let mut f = Fixture::new();
    let output = fullscreen_one_window(&mut f);

    let reply = dispatch(Request::Camera(Some((f64::NAN, 0.0))), f.state());
    assert!(
        reply.is_err(),
        "a non-finite camera target must be rejected"
    );
    assert!(
        f.state().is_fullscreen(),
        "the rejected request must not have torn down fullscreen"
    );

    f.state().exit_fullscreen_on(&output);
}
