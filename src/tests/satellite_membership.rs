//! Wire-level output membership for xwayland-satellite windows. The pure policy
//! lives in `state::membership`; here a real satellite-flagged client connects
//! over a socket and we assert the `wl_surface.enter`/`leave` it actually
//! receives: a single sticky output, migrating only when the window's center
//! genuinely changes output — never when a second viewport merely pans over it.
//! A normal client with the same geometry is the control that enters both.
//!
//! Two 400×400 outputs sit on the canvas. Each output's camera is set
//! explicitly so its viewport is a known canvas rect (zoom stays 1, so
//! `output_geometry.loc == camera`); windows are placed at known canvas
//! positions. HEADLESS-1 is always the center's home; HEADLESS-2 is the
//! second viewport that must never steal a satellite window.

use smithay::desktop::Window;
use smithay::output::Output;
use smithay::utils::Point;

use super::client::{ClientId, OutputEvent};
use super::{Fixture, map_window, window_by_app_id};

/// Point HEADLESS-`n`'s viewport at canvas rect `[x, x+400) × [y, y+400)`.
/// Sets the per-output camera (what auto-placement reads) and syncs the Space
/// output geometry (what membership reads) directly, skipping the render-cache
/// bookkeeping `update_output_from_camera` would leave off-baseline.
fn set_camera(f: &mut Fixture, output: &Output, x: f64, y: f64) {
    f.state().set_camera_on(output, Point::from((x, y)));
    let loc = Point::<f64, _>::from((x, y)).to_i32_round();
    f.state().space.map_output(output, loc);
}

/// Move a mapped window's top-left to canvas `(x, y)`.
fn place(f: &mut Fixture, window: &Window, x: i32, y: i32) {
    f.state()
        .map_window(window.clone(), Point::from((x, y)), false);
}

/// Two overlapping 400×400 outputs and a mapped window straddling both
/// viewports with its center in HEADLESS-1: A covers `[0,400)`, B covers
/// `[240,640)`, and the 200×200 window at (100,100) spans `[100,300]` (center
/// (200,200), in A only) while overlapping B in `[240,300]`.
fn straddling_window(
    f: &mut Fixture,
    id: ClientId,
) -> wayland_client::protocol::wl_surface::WlSurface {
    let a = f.add_output(1, (400, 400));
    let b = f.add_output(2, (400, 400));
    set_camera(f, &a, 0.0, 0.0);
    set_camera(f, &b, 240.0, 0.0);

    let surface = map_window(f, id, "app", (200, 200));
    let window = window_by_app_id(f, "app").unwrap();
    place(f, &window, 100, 100);
    f.roundtrip(id);
    surface
}

/// A satellite window visible on both viewports enters only the output that
/// holds its center, and never the second.
#[test]
fn satellite_enters_only_the_output_holding_its_center() {
    let mut f = Fixture::new();
    let id = f.add_satellite_client();
    let surface = straddling_window(&mut f, id);

    assert_eq!(
        f.client(id).surface_outputs(&surface),
        vec!["HEADLESS-1".to_string()],
        "satellite window is entered on exactly the output holding its center"
    );
    let events = f.client(id).surface_output_events(&surface);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutputEvent::Enter(n) if n == "HEADLESS-2")),
        "satellite window must never enter the second overlapping output, got: {events:?}"
    );
}

/// A normal client with the identical geometry enters both outputs — this is
/// the control that proves the single-output behavior is satellite-scoped.
#[test]
fn normal_client_with_same_geometry_enters_both_outputs() {
    let mut f = Fixture::new();
    let id = f.add_client();
    let surface = straddling_window(&mut f, id);

    let mut outputs = f.client(id).surface_outputs(&surface);
    outputs.sort();
    assert_eq!(
        outputs,
        vec!["HEADLESS-1".to_string(), "HEADLESS-2".to_string()],
        "a non-satellite window enters every overlapping output"
    );
}

/// Poison sequence: a second viewport panning over a satellite window (and off
/// again) while the window's center stays put fires no enter/leave at all — the
/// association is pinned to the original output throughout.
#[test]
fn second_viewport_panning_over_satellite_fires_no_events() {
    let mut f = Fixture::new();
    let a = f.add_output(1, (400, 400));
    let b = f.add_output(2, (400, 400));
    // A covers [0,400); B parks far right at [600,1000), clear of the window.
    set_camera(&mut f, &a, 0.0, 0.0);
    set_camera(&mut f, &b, 600.0, 0.0);

    let id = f.add_satellite_client();
    let surface = map_window(&mut f, id, "app", (200, 200));
    let window = window_by_app_id(&mut f, "app").unwrap();
    // Window [100,300], center (200,200): wholly inside A, clear of B.
    place(&mut f, &window, 100, 100);
    f.roundtrip(id);

    let baseline = f.client(id).surface_output_events(&surface);
    assert_eq!(
        baseline,
        vec![OutputEvent::Enter("HEADLESS-1".to_string())],
        "the window enters HEADLESS-1 once and nothing else"
    );

    // B pans onto the window (overlaps [200,300]) — center still in A.
    set_camera(&mut f, &b, 200.0, 0.0);
    f.roundtrip(id);
    assert_eq!(
        f.client(id).surface_output_events(&surface),
        baseline,
        "a second viewport overlapping the window must not touch a satellite's membership"
    );

    // B pans back off the window.
    set_camera(&mut f, &b, 600.0, 0.0);
    f.roundtrip(id);
    assert_eq!(
        f.client(id).surface_output_events(&surface),
        baseline,
        "panning the second viewport away again must also fire nothing"
    );
}

/// When the window's own center crosses onto the other output, the satellite
/// association migrates — entering the new output before leaving the old.
#[test]
fn center_crossing_enters_new_output_before_leaving_old() {
    let mut f = Fixture::new();
    let a = f.add_output(1, (400, 400));
    let b = f.add_output(2, (400, 400));
    // Adjacent, non-overlapping viewports: A [0,400), B [400,800).
    set_camera(&mut f, &a, 0.0, 0.0);
    set_camera(&mut f, &b, 400.0, 0.0);

    let id = f.add_satellite_client();
    let surface = map_window(&mut f, id, "app", (200, 200));
    let window = window_by_app_id(&mut f, "app").unwrap();
    // Start on A: window [100,300], center (200,200).
    place(&mut f, &window, 100, 100);
    f.roundtrip(id);
    assert_eq!(
        f.client(id).surface_outputs(&surface),
        vec!["HEADLESS-1".to_string()]
    );

    // Move so the center (600,200) lands on B.
    place(&mut f, &window, 500, 100);
    f.roundtrip(id);

    assert_eq!(
        f.client(id).surface_outputs(&surface),
        vec!["HEADLESS-2".to_string()],
        "the association migrates to the output now holding the center"
    );
    let events = f.client(id).surface_output_events(&surface);
    let enter_b = events
        .iter()
        .position(|e| e == &OutputEvent::Enter("HEADLESS-2".to_string()))
        .expect("enter(HEADLESS-2) must be recorded");
    let leave_a = events
        .iter()
        .position(|e| e == &OutputEvent::Leave("HEADLESS-1".to_string()))
        .expect("leave(HEADLESS-1) must be recorded");
    assert!(
        enter_b < leave_a,
        "enter(HEADLESS-2) must precede leave(HEADLESS-1), got: {events:?}"
    );
}
