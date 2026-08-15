//! `zoom-to-fit` is a camera toggle, not a mode: it saves the pre-fit camera
//! and zoom so a second press returns there, but any deliberate move — a pan
//! or a navigation — disarms that return and keeps the zoomed-out zoom
//! instead of restoring the saved one.

use driftwm::config::{Action, Direction};
use smithay::utils::{Logical, Point};

use crate::state::StageWindow;

use super::{Fixture, TICK, map_window, settle, window_by_app_id};

/// Two windows far enough apart that fitting them needs a zoom well under 1.0.
/// Focus ends on the right-hand one, so `center-nearest` left has a target.
fn two_spread_windows(f: &mut Fixture) {
    f.add_output(1, (1920, 1080));
    // Moving the camera seeds a per-output blur generation that only clears on
    // output disconnect, so it can never return to the pre-output baseline.
    f.skip_baseline_check();
    let id = f.add_client();
    map_window(f, id, "left", (400, 300));
    map_window(f, id, "right", (400, 300));

    let left = window_by_app_id(f, "left").expect("left window");
    let right = window_by_app_id(f, "right").expect("right window");
    f.state()
        .map_window(StageWindow::Client(left), Point::from((0, 0)), false);
    f.state()
        .map_window(StageWindow::Client(right), Point::from((4000, 0)), true);
    settle(f);
}

/// Fit the spread layout and settle there, returning the framing it lands on.
fn enter_fit_view(f: &mut Fixture) -> (f64, Point<f64, Logical>) {
    f.state().execute_action(&Action::ZoomToFit);
    settle(f);
    let fit_zoom = f.state().zoom();
    assert!(
        fit_zoom < 1.0,
        "the layout should need zooming out to fit, got {fit_zoom}"
    );
    (fit_zoom, f.state().camera())
}

#[test]
fn navigating_from_a_fit_view_keeps_the_zoomed_out_zoom() {
    let mut f = Fixture::new();
    two_spread_windows(&mut f);
    let (fit_zoom, _) = enter_fit_view(&mut f);

    f.state()
        .execute_action(&Action::CenterNearest(Direction::Left));
    settle(&mut f);

    // Without this the zoom assert below passes vacuously: a search that finds
    // no target is a no-op, which also leaves the zoom untouched.
    let left = window_by_app_id(&mut f, "left").expect("left window");
    assert!(
        f.state().focused_window() == Some(left),
        "the search reached the left window"
    );
    assert!(
        (f.state().zoom() - fit_zoom).abs() < 1e-6,
        "navigating stays at the fit zoom instead of restoring the pre-fit one, \
         got {} want {fit_zoom}",
        f.state().zoom()
    );
}

#[test]
fn navigating_disarms_the_fit_toggle() {
    let mut f = Fixture::new();
    two_spread_windows(&mut f);
    let (fit_zoom, fit_camera) = enter_fit_view(&mut f);

    f.state()
        .execute_action(&Action::CenterNearest(Direction::Left));
    settle(&mut f);

    // The return is spent, so this press fits afresh — same bbox, so the same
    // framing as the first fit — instead of jumping to the pre-fit viewport.
    f.state().execute_action(&Action::ZoomToFit);
    settle(&mut f);

    assert!(
        (f.state().zoom() - fit_zoom).abs() < 1e-6,
        "a fresh fit lands on the fit zoom, got {}",
        f.state().zoom()
    );
    let camera = f.state().camera();
    assert!(
        (camera.x - fit_camera.x).abs() < 1e-6 && (camera.y - fit_camera.y).abs() < 1e-6,
        "a fresh fit reproduces the first fit's framing, got {camera:?} want {fit_camera:?}"
    );
}

#[test]
fn a_second_press_returns_to_the_pre_fit_viewport() {
    let mut f = Fixture::new();
    two_spread_windows(&mut f);
    let camera_before = f.state().camera();
    let zoom_before = f.state().zoom();

    enter_fit_view(&mut f);
    f.state().execute_action(&Action::ZoomToFit);
    settle(&mut f);

    assert!(
        (f.state().zoom() - zoom_before).abs() < 1e-6,
        "the toggle restores the pre-fit zoom, got {} want {zoom_before}",
        f.state().zoom()
    );
    let camera = f.state().camera();
    assert!(
        (camera.x - camera_before.x).abs() < 1e-6 && (camera.y - camera_before.y).abs() < 1e-6,
        "the toggle restores the pre-fit camera, got {camera:?} want {camera_before:?}"
    );
}

/// Fitting a window straight out of fullscreen must center it like any other
/// fit. The exit only *sends* the smaller configure, so the client is still
/// committing viewport-sized buffers when fit runs: deriving the fit camera from
/// live geometry then centers the viewport on the middle of a fullscreen-sized
/// rect, parking the real window up and left of centre (its bottom-right corner
/// near the middle of the screen).
#[test]
fn fitting_straight_out_of_fullscreen_centers_the_window() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (600, 400));
    let window = window_by_app_id(&mut f, "fs").unwrap();

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);

    // Exit + fit back-to-back, exactly as `execute_action` does for a fit
    // binding pressed while fullscreen.
    f.state().exit_fullscreen_on(&output);
    f.state().fit_window(&window);
    settle(&mut f);
    // Let the client catch up to the fit configure and drain any settle.
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);
    // The fit's pan rides the window's freeze, so it is only armed once a window
    // tick sees the ack release it.
    f.state().tick_window_animations(TICK);
    settle(&mut f);

    let usable = f.state().get_usable_area();
    let camera = f.state().camera();
    let center = f.state().window_visual_center(&window).unwrap();
    let want_x = camera.x + usable.loc.x as f64 + usable.size.w as f64 / 2.0;
    let want_y = camera.y + usable.loc.y as f64 + usable.size.h as f64 / 2.0;
    assert!(
        (center.x - want_x).abs() <= 2.0 && (center.y - want_y).abs() <= 2.0,
        "fit window centre {center:?} should sit at the usable centre ({want_x}, {want_y})"
    );
}

/// A snapped fit out of a fullscreen exit must still push both its neighbours
/// aside. The client is still committing viewport-sized buffers when the fit
/// runs, and cluster membership read off that live size sees a primary rect that
/// swallows the right-hand neighbour rather than sitting a gap away from it — so
/// nothing is adjacent, the cluster degrades to the primary alone, and both
/// passes no-op. The left-hand neighbour rides the `TopLeft` pass, and its
/// adjacency turns on its own far edge rather than the primary's: only the
/// primary is mid-configure, so everything else has to be measured from what it
/// has actually committed.
#[test]
fn fitting_snapped_out_of_fullscreen_still_pushes_its_neighbours() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let gap = f.state().config.snap_gap as i32;
    let id = f.add_client();

    let surface = map_window(&mut f, id, "primary", (400, 300));
    let primary = window_by_app_id(&mut f, "primary").unwrap();
    f.state()
        .map_window(primary.clone(), Point::from((900, 300)), false);

    let side_w = 300;
    map_window(&mut f, id, "right", (300, 300));
    let right = window_by_app_id(&mut f, "right").unwrap();
    f.state()
        .map_window(right.clone(), Point::from((900 + 400 + gap, 300)), false);

    map_window(&mut f, id, "left", (300, 300));
    let left = window_by_app_id(&mut f, "left").unwrap();
    f.state()
        .map_window(left.clone(), Point::from((900 - gap - side_w, 300)), false);
    settle(&mut f);

    f.state().enter_fullscreen(&primary, Some(output.clone()));
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);

    // Exit + fit back-to-back, exactly as `execute_action` does for a fit
    // binding pressed while fullscreen.
    f.state().exit_fullscreen_on(&output);
    f.state().fit_window_snapped(&primary);

    // Borders are off by default, so the frame edges the expectations walk are
    // the content edges.
    let loc = f.state().stage.position_of(&primary).unwrap();
    let size = crate::state::configured_window_size(&primary);
    assert_eq!(
        f.state().stage.position_of(&right).unwrap(),
        Point::from((loc.x + size.w + gap, 300)),
        "the right neighbour tracks the fitted primary's right edge"
    );
    assert_eq!(
        f.state().stage.position_of(&left).unwrap(),
        Point::from((loc.x - gap - side_w, 300)),
        "the left neighbour tracks the fitted primary's left edge"
    );
}

/// The mirror case: a snapped unfit out of a fullscreen exit must pull the
/// neighbour the fit pushed aside back in, or it is left stranded at the fit's
/// spacing.
#[test]
fn unfitting_snapped_out_of_fullscreen_still_pulls_the_neighbour_back() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let gap = f.state().config.snap_gap as i32;
    let id = f.add_client();

    let surface = map_window(&mut f, id, "primary", (400, 300));
    let primary = window_by_app_id(&mut f, "primary").unwrap();
    f.state().fit_window(&primary);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    settle(&mut f);

    let fit_loc = f.state().stage.position_of(&primary).unwrap();
    let fit_size = primary.geometry().size;
    map_window(&mut f, id, "neighbour", (300, 300));
    let neighbour = window_by_app_id(&mut f, "neighbour").unwrap();
    f.state().map_window(
        neighbour.clone(),
        Point::from((fit_loc.x + fit_size.w + gap, fit_loc.y)),
        false,
    );

    f.state().enter_fullscreen(&primary, Some(output.clone()));
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);

    f.state().exit_fullscreen_on(&output);
    f.state().unfit_window_snapped(&primary);

    let loc = f.state().stage.position_of(&primary).unwrap();
    let size = crate::state::configured_window_size(&primary);
    assert_eq!(
        f.state().stage.position_of(&neighbour).unwrap(),
        Point::from((loc.x + size.w + gap, fit_loc.y)),
        "the neighbour tracks the unfitted primary's right edge back in"
    );
}
