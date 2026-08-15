//! `focus_placement`: a centering navigation parks the focused window's visual
//! frame against a viewport edge (inset by `snap.gap`) instead of the usable
//! area's center. Every assertion here checks the settled **edge**, not the
//! center — a center-only check can't distinguish a correct placement from a
//! plain, wrong-by-half-a-window centering (see `align_point_on`'s doc comment
//! on `DriftWm` for the math).

use smithay::desktop::Window;
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};

use driftwm::config::{Action, Direction, FocusPlacement};

use crate::state::{NavZoom, StageWindow};

use super::{Fixture, adopt_last_configure, config, map_window, settle, window_by_app_id};

/// Park the viewport at `camera`/`zoom` with nothing in flight, so a later
/// settle is a genuine journey rather than a no-op that happens to already
/// sit at the destination.
fn park_view(f: &mut Fixture, camera: Point<f64, Logical>, zoom: f64) {
    f.state().with_output_state(|os| {
        os.camera = camera;
        os.camera_target = None;
        os.zoom = zoom;
        os.zoom_target = None;
        os.zoom_animation_anchor = None;
    });
}

/// Focus `window`, run `center-window`, and settle both viewport animations to
/// completion — the shape every real frame loop drives them in.
fn center_window_and_settle(f: &mut Fixture, window: &Window) {
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(window, serial);
    f.state().execute_action(&Action::CenterWindow);
    settle(f);
}

/// `elem`'s visual frame edges (border + SSD title bar + content) in screen
/// space, at the active output's current camera and zoom.
fn screen_edges(f: &mut Fixture, elem: &StageWindow) -> (f64, f64, f64, f64) {
    let rect = f.state().visual_frame_rect(elem).unwrap();
    let camera = f.state().camera();
    let zoom = f.state().zoom();
    (
        (rect.x_low - camera.x) * zoom,
        (rect.y_low - camera.y) * zoom,
        (rect.x_high - camera.x) * zoom,
        (rect.y_high - camera.y) * zoom,
    )
}

/// `center-window` under `focus_placement = "left"` parks the frame's left
/// edge at `usable.loc.x + snap_gap * zoom`, not merely somewhere left of
/// center.
#[test]
fn center_window_left_placement_pins_the_left_edge_at_the_gap() {
    let mut f = Fixture::with_config(config(r#"focus_placement = "left""#));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let _surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();

    park_view(&mut f, Point::from((4000.0, -3000.0)), 1.0);
    center_window_and_settle(&mut f, &window);

    let gap = f.state().config.snap_gap;
    let zoom = f.state().zoom();
    let usable = f.state().get_usable_area();
    let elem = StageWindow::Client(window.clone());
    let (x_low, ..) = screen_edges(&mut f, &elem);

    let expected = usable.loc.x as f64 + gap * zoom;
    assert!(
        (x_low - expected).abs() < 1e-6,
        "left edge must sit exactly `gap` in from the usable area's left: \
         {x_low} vs {expected}"
    );
}

/// The vertical counterpart: `focus_placement = "top"` pins the top edge.
#[test]
fn center_window_top_placement_pins_the_top_edge_at_the_gap() {
    let mut f = Fixture::with_config(config(r#"focus_placement = "top""#));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let _surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();

    park_view(&mut f, Point::from((4000.0, -3000.0)), 1.0);
    center_window_and_settle(&mut f, &window);

    let gap = f.state().config.snap_gap;
    let zoom = f.state().zoom();
    let usable = f.state().get_usable_area();
    let elem = StageWindow::Client(window.clone());
    let (_, y_low, ..) = screen_edges(&mut f, &elem);

    let expected = usable.loc.y as f64 + gap * zoom;
    assert!(
        (y_low - expected).abs() < 1e-6,
        "top edge must sit exactly `gap` in from the usable area's top: \
         {y_low} vs {expected}"
    );
}

/// The corner case: `focus_placement = "top-left"` pins both edges at once.
#[test]
fn center_window_top_left_placement_pins_both_edges_at_the_gap() {
    let mut f = Fixture::with_config(config(r#"focus_placement = "top-left""#));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let _surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();

    park_view(&mut f, Point::from((4000.0, -3000.0)), 1.0);
    center_window_and_settle(&mut f, &window);

    let gap = f.state().config.snap_gap;
    let zoom = f.state().zoom();
    let usable = f.state().get_usable_area();
    let elem = StageWindow::Client(window.clone());
    let (x_low, y_low, ..) = screen_edges(&mut f, &elem);

    assert!(
        (x_low - (usable.loc.x as f64 + gap * zoom)).abs() < 1e-6,
        "left edge at the gap"
    );
    assert!(
        (y_low - (usable.loc.y as f64 + gap * zoom)).abs() < 1e-6,
        "top edge at the gap too"
    );
}

/// A frame whose width plus its two gutters doesn't fit the usable area falls
/// back to centering on that axis alone; the other axis still places, so a
/// wide window under "top-left" still goes to the top.
#[test]
fn a_too_wide_frame_centers_on_x_but_still_places_on_y() {
    let mut f = Fixture::with_config(config(r#"focus_placement = "top-left""#));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    // 1900 + 2*12 = 1924 > 1920: the x axis can't fit its two gutters.
    // 600 + 2*12 = 624 <= 1080: the y axis fits fine.
    let _surface = map_window(&mut f, id, "a", (1900, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();

    park_view(&mut f, Point::from((4000.0, -3000.0)), 1.0);
    center_window_and_settle(&mut f, &window);

    let gap = f.state().config.snap_gap;
    let zoom = f.state().zoom();
    let usable = f.state().get_usable_area();
    let elem = StageWindow::Client(window.clone());
    let (x_low, y_low, x_high, _) = screen_edges(&mut f, &elem);

    let expected_center_x = usable.loc.x as f64 + usable.size.w as f64 / 2.0;
    assert!(
        (((x_low + x_high) / 2.0) - expected_center_x).abs() < 1e-6,
        "too wide for its two gutters: x falls back to centering"
    );
    assert!(
        (y_low - (usable.loc.y as f64 + gap * zoom)).abs() < 1e-6,
        "y still places at the top, independent of the x fallback"
    );
}

/// `NavZoom::Keep` (directional/gesture navigation) settles on the placement
/// point once the camera animation completes, not just at a coincidental
/// first tick — `navigate_to_window_on` always arms a zoom target, so the
/// anchor branch runs for Keep exactly as it does for Reset.
#[test]
fn keep_zoom_navigation_settles_at_the_placed_spot() {
    let mut f = Fixture::with_config(config(r#"focus_placement = "left""#));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let _surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();

    park_view(&mut f, Point::from((4000.0, -3000.0)), 0.7);
    f.state().navigate_to_window(&window, NavZoom::Keep);
    settle(&mut f);

    assert!(
        (f.state().zoom() - 0.7).abs() < 1e-9,
        "Keep leaves the outgoing zoom alone"
    );
    let gap = f.state().config.snap_gap;
    let zoom = f.state().zoom();
    let usable = f.state().get_usable_area();
    let elem = StageWindow::Client(window.clone());
    let (x_low, ..) = screen_edges(&mut f, &elem);
    assert!(
        (x_low - (usable.loc.x as f64 + gap * zoom)).abs() < 1e-6,
        "and still lands on the placement point at that zoom"
    );
}

/// `NavZoom::Reset` (an intentional navigation) settles on the same placement
/// point once its zoom-to-1.0 animation completes.
#[test]
fn reset_zoom_navigation_settles_at_the_placed_spot() {
    let mut f = Fixture::with_config(config(r#"focus_placement = "left""#));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let _surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();

    park_view(&mut f, Point::from((4000.0, -3000.0)), 0.7);
    f.state().navigate_to_window(&window, NavZoom::Reset);
    settle(&mut f);

    assert!(
        (f.state().zoom() - 1.0).abs() < 1e-9,
        "Reset animates zoom to 1.0"
    );
    let gap = f.state().config.snap_gap;
    let zoom = f.state().zoom();
    let usable = f.state().get_usable_area();
    let elem = StageWindow::Client(window.clone());
    let (x_low, ..) = screen_edges(&mut f, &elem);
    assert!(
        (x_low - (usable.loc.x as f64 + gap * zoom)).abs() < 1e-6,
        "and lands on the placement point at the reset zoom"
    );
}

/// `center-window` on a filled window replays the camera + zoom the fill was
/// computed in, and `focus_placement` must not re-aim that: a window filled
/// beside a left neighbor already sits right of center, and a "left" pull
/// re-applied on restore would push it toward its own neighbor instead of
/// back to the fill's own view.
#[test]
fn center_window_on_a_filled_window_restores_its_view_ignoring_focus_placement() {
    let mut f = Fixture::with_config(config(r#"focus_placement = "left""#));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    // "b" pinned along the left edge so "a" fills only the space to its
    // right, stopping short of the usable area's own left edge.
    let _b_surface = map_window(&mut f, id, "b", (400, 1056));
    let b = window_by_app_id(&mut f, "b").unwrap();
    let gap = f.state().config.snap_gap as i32;
    park_view(&mut f, Point::from((0.0, 0.0)), 1.0);
    f.state().map_window(b, Point::from((gap, gap)), false);

    let a_surface = map_window(&mut f, id, "a", (800, 600));
    let a = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(a.clone(), Point::from((800, 300)), false);

    f.state().toggle_fill_window(&a);
    assert!(f.state().stage.is_fill(&a), "precondition: the fill ran");
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    let fill_camera = f.state().camera();
    let fill_zoom = f.state().zoom();

    park_view(&mut f, Point::from((4000.0, -3000.0)), 1.0);
    center_window_and_settle(&mut f, &a);

    let camera = f.state().camera();
    let zoom = f.state().zoom();
    assert!(
        (camera.x - fill_camera.x).abs() < 1e-6 && (camera.y - fill_camera.y).abs() < 1e-6,
        "the restore returns to the exact view the fill ran in, not the left \
         placement point: {camera:?} vs {fill_camera:?}"
    );
    assert!((zoom - fill_zoom).abs() < 1e-9, "and so does the zoom");
}

/// A suspended stand-in centered under a placement lands its visual frame on
/// the same edge a client window would — but not the same center, since a
/// stand-in always wears a title bar and a CSD client does not.
#[test]
fn a_suspended_stand_in_matches_a_clients_frame_edge_but_not_its_center() {
    let mut f = Fixture::with_config(config(r#"focus_placement = "top""#));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    park_view(&mut f, Point::from((4000.0, -3000.0)), 1.0);
    center_window_and_settle(&mut f, &window);

    let usable = f.state().get_usable_area();
    let gap = f.state().config.snap_gap;
    let zoom = f.state().zoom();
    let client_elem = StageWindow::Client(window.clone());
    let (_, client_y_low, _, client_y_high) = screen_edges(&mut f, &client_elem);
    assert!(
        (client_y_low - (usable.loc.y as f64 + gap * zoom)).abs() < 1e-6,
        "precondition: the client's top edge sits at the gap"
    );
    let client_center_y = (client_y_low + client_y_high) / 2.0;

    // A suspended stand-in of the same content size, wearing its title bar.
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((2000, 2000)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    park_view(&mut f, Point::from((4000.0, -3000.0)), 1.0);
    f.state().center_on_suspended(sid, true);
    settle(&mut f);

    let suspended = f.state().find_suspended(sid).unwrap();
    let suspended_elem = StageWindow::Suspended(suspended);
    let (_, s_y_low, _, s_y_high) = screen_edges(&mut f, &suspended_elem);
    assert!(
        (s_y_low - (usable.loc.y as f64 + gap * zoom)).abs() < 1e-6,
        "the stand-in's top edge lands at the exact same gap the client did"
    );
    let suspended_center_y = (s_y_low + s_y_high) / 2.0;

    assert!(
        (suspended_center_y - client_center_y).abs() > 1.0,
        "but its center sits lower, pushed down by its title bar: \
         {suspended_center_y} vs {client_center_y}"
    );

    f.state().dismiss_suspended(sid);
}

/// A `[[outputs]]` override applies only to its own output: a window centered
/// on the unconfigured output still centers, while one centered on the
/// overridden output lands at its edge instead.
#[test]
fn an_output_override_places_a_window_differently_than_the_global_default() {
    let mut f = Fixture::with_config(config(
        r#"
        [[outputs]]
        name = "HEADLESS-2"
        focus_placement = "right"
        "#,
    ));
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let _a_surface = map_window(&mut f, id, "a", (400, 300));
    let window_a = window_by_app_id(&mut f, "a").unwrap();
    let _b_surface = map_window(&mut f, id, "b", (400, 300));
    let window_b = window_by_app_id(&mut f, "b").unwrap();

    f.state().focused_output = Some(out1.clone());
    f.state()
        .navigate_to_window_on(&window_a, &out1, NavZoom::Reset);
    settle(&mut f);
    let elem_a = StageWindow::Client(window_a.clone());
    let (ax_low, _, ax_high, _) = screen_edges(&mut f, &elem_a);
    let vc1 = f.state().usable_center_screen_on(&out1);
    assert!(
        ((ax_low + ax_high) / 2.0 - vc1.x).abs() < 1e-6,
        "HEADLESS-1 has no override: it centers on the global default"
    );

    f.state().focused_output = Some(out2.clone());
    f.state()
        .navigate_to_window_on(&window_b, &out2, NavZoom::Reset);
    settle(&mut f);
    let elem_b = StageWindow::Client(window_b.clone());
    let (_, _, bx_high, _) = screen_edges(&mut f, &elem_b);
    let usable2 = f.state().usable_area_on(&out2);
    let gap = f.state().config.snap_gap;
    let zoom_b = f.state().zoom();
    let expected = usable2.loc.x as f64 + usable2.size.w as f64 - gap * zoom_b;
    assert!(
        (bx_high - expected).abs() < 1e-6,
        "HEADLESS-2's override pins the right edge instead: {bx_high} vs {expected}"
    );
}

/// `center-nearest` inherits `focus_placement` too: the directional step
/// lands its target at the placement point, not the plain viewport center.
#[test]
fn center_nearest_inherits_focus_placement() {
    let mut f = Fixture::with_config(config(r#"focus_placement = "left""#));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let _b_surface = map_window(&mut f, id, "b", (200, 200));
    let b = window_by_app_id(&mut f, "b").unwrap();
    f.state()
        .map_window(b.clone(), Point::from((0, 400)), false);

    let _a_surface = map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(a.clone(), Point::from((2000, 400)), false);

    park_view(&mut f, Point::from((0.0, 0.0)), 1.0);
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&b, serial);

    f.state()
        .execute_action(&Action::CenterNearest(Direction::Right));
    settle(&mut f);

    let gap = f.state().config.snap_gap;
    let zoom = f.state().zoom();
    let usable = f.state().get_usable_area();
    let elem_a = StageWindow::Client(a.clone());
    let (ax_low, ..) = screen_edges(&mut f, &elem_a);
    assert!(
        (ax_low - (usable.loc.x as f64 + gap * zoom)).abs() < 1e-6,
        "center-nearest lands its target on the focus_placement point too"
    );
}

/// `fit-window` still fills the whole viewport (gap-inset on every side)
/// under a non-center placement, and its camera lands on the plain viewport
/// center — fit is excluded from `focus_placement` entirely.
#[test]
fn fit_window_ignores_focus_placement_and_still_fills_the_viewport() {
    let mut f = Fixture::with_config(config(r#"focus_placement = "left""#));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();

    park_view(&mut f, Point::from((4000.0, -3000.0)), 1.0);

    f.state().fit_window(&window);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    settle(&mut f);

    let gap = f.state().config.snap_gap;
    let usable = f.state().get_usable_area();
    let elem = StageWindow::Client(window.clone());
    let (x_low, y_low, x_high, y_high) = screen_edges(&mut f, &elem);

    assert!(
        (x_low - (usable.loc.x as f64 + gap)).abs() < 1.0,
        "left edge at the gap, not shifted further left by the placement"
    );
    assert!(
        (x_high - (usable.loc.x as f64 + usable.size.w as f64 - gap)).abs() < 1.0,
        "right edge also at the gap — a full fill, not a left-pinned frame"
    );
    assert!(
        (y_low - (usable.loc.y as f64 + gap)).abs() < 1.0,
        "top edge at the gap"
    );
    assert!(
        (y_high - (usable.loc.y as f64 + usable.size.h as f64 - gap)).abs() < 1.0,
        "bottom edge at the gap"
    );

    let pos = f.state().stage.position_of(&window).unwrap();
    let size = window.geometry().size;
    let content_center: Point<f64, Logical> = Point::from((
        pos.x as f64 + size.w as f64 / 2.0,
        pos.y as f64 + size.h as f64 / 2.0,
    ));
    let camera = f.state().camera();
    let zoom = f.state().zoom();
    let screen_center: Point<f64, Logical> = Point::from((
        (content_center.x - camera.x) * zoom,
        (content_center.y - camera.y) * zoom,
    ));
    let vc = f.state().usable_center_screen();
    assert!(
        (screen_center.x - vc.x).abs() < 1e-6 && (screen_center.y - vc.y).abs() < 1e-6,
        "the fit's camera centers on the plain viewport center, ignoring \
         focus_placement: {screen_center:?} vs {vc:?}"
    );
}

/// A newly mapped window under `window_placement = "center"` seeds directly
/// under the placement point, leaving the map's own navigate at most the
/// cascade offset to travel. Pinned at zoom 1.0, where the seed's zoom
/// conversion agrees with the navigate's target zoom (see the
/// `compositor.rs` new-window seed comment for why that only holds there).
#[test]
fn a_newly_mapped_window_seeds_under_the_placement_point() {
    let mut f = Fixture::with_config(config(r#"focus_placement = "left""#));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    park_view(&mut f, Point::from((0.0, 0.0)), 1.0);
    let camera_before = f.state().camera();

    let _surface = map_window(&mut f, id, "a", (800, 600));
    settle(&mut f);

    let cascade = f.state().config.decorations.title_bar_height as f64;
    let camera = f.state().camera();
    assert!(
        (camera.x - camera_before.x).abs() <= cascade
            && (camera.y - camera_before.y).abs() <= cascade,
        "the seed already sits on the placement point, leaving the map's \
         navigate at most the cascade offset to pan: {camera:?} vs \
         {camera_before:?}"
    );

    let window = window_by_app_id(&mut f, "a").unwrap();
    let elem = StageWindow::Client(window);
    let rect = f.state().visual_frame_rect(&elem).unwrap();
    let usable = f.state().get_usable_area();
    let gap = f.state().config.snap_gap;
    // Canvas coords, at the zoom-1.0 camera the window spawned into.
    let expected = camera_before.x + usable.loc.x as f64 + gap;
    assert!(
        (rect.x_low - expected).abs() <= 1.0,
        "the seed puts the frame on the left gutter of the view it spawned \
         into, not under the usable center: {} vs {expected}",
        rect.x_low
    );
}

/// `[[outputs]] focus_placement = "center"` is not the same as no entry at
/// all: the explicit value has to beat a non-center global, while an output
/// with no entry of its own inherits that global. The whole reason the
/// per-output field is an `Option`.
#[test]
fn an_explicit_output_center_beats_a_non_center_global() {
    let mut f = Fixture::with_config(config(
        r#"
        focus_placement = "right"

        [[outputs]]
        name = "HEADLESS-1"
        focus_placement = "center"
        "#,
    ));
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1920, 1080));

    assert_eq!(
        f.state().focus_placement_on(&out1),
        FocusPlacement::Center,
        "an explicit per-output center wins over the global right"
    );
    assert_eq!(
        f.state().focus_placement_on(&out2),
        FocusPlacement::Right,
        "an output with no entry inherits the global instead of forcing center"
    );
}

/// `send-to-output` is followed by no navigation, so it has to land the window
/// on the target output's placement point itself — otherwise a window sent to
/// a monitor sits somewhere no spawn or navigation on that monitor would put
/// it.
#[test]
fn send_to_output_lands_on_the_targets_placement_point() {
    let mut f = Fixture::with_config(config(r#"focus_placement = "right""#));
    let _out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    f.skip_baseline_check();
    // Both outputs default to a camera on the canvas origin, so their viewports
    // overlap; pan out2 away so the move lands somewhere only out2 covers.
    crate::state::output_state(&out2).camera = Point::from((5000.0, 5000.0));
    let id = f.add_client();

    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    f.state()
        .execute_action(&Action::SendToOutput(Direction::Right));

    let elem = StageWindow::Client(window.clone());
    let rect = f.state().visual_frame_rect(&elem).unwrap();
    let usable2 = f.state().usable_area_on(&out2);
    let gap = f.state().config.snap_gap;
    let (cam2, zoom2) = {
        let os = crate::state::output_state(&out2);
        (os.camera, os.zoom)
    };
    let expected = cam2.x + (usable2.loc.x as f64 + usable2.size.w as f64) / zoom2 - gap;
    assert!(
        (rect.x_high - expected).abs() <= 1.5,
        "the moved window's right edge sits at the target output's gap, not \
         at its center: {} vs {expected}",
        rect.x_high
    );
}
