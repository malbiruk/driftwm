//! `[snap] outer_gap`: fit and fill measure the inset from the usable area to a
//! window's title bar / content edge, not to its visual frame — the border
//! lies outside that inset, off screen at `outer_gap = 0`.

use smithay::desktop::Window;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::state::StageWindow;

use super::{
    Fixture, TICK, adopt_last_configure, config, map_top_panel, map_window, settle,
    window_by_app_id,
};

/// Fit `window`, deliver the configure, ack it at the size the compositor
/// asked for, and settle the resulting camera/zoom animation — the shape
/// `configure_sequences.rs::fit_and_settle` drives, needed here because
/// `window.geometry().size` only reflects the fit's target size once the
/// client has acked it.
fn fit_and_adopt(
    f: &mut Fixture,
    window: &Window,
    id: super::client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) {
    f.state().fit_window(window);
    f.double_roundtrip(id);
    adopt_last_configure(f, id, surface);
    f.double_roundtrip(id);
    f.state().tick_window_animations(TICK);
    settle(f);
}

/// `window`'s content rect (no chrome) in screen space, at the active
/// output's current camera and zoom.
fn content_screen_rect(f: &mut Fixture, window: &Window) -> Rectangle<f64, Logical> {
    let loc = f.state().stage.position_of(window).unwrap();
    let size = window.geometry().size;
    let camera = f.state().camera();
    let zoom = f.state().zoom();
    Rectangle::new(
        Point::from((
            (loc.x as f64 - camera.x) * zoom,
            (loc.y as f64 - camera.y) * zoom,
        )),
        Size::from((size.w as f64 * zoom, size.h as f64 * zoom)),
    )
}

/// `window`'s visual frame (border + SSD title bar + content) in screen
/// space, as `focus_placement.rs::screen_edges`.
fn frame_screen_rect(f: &mut Fixture, window: &Window) -> Rectangle<f64, Logical> {
    let elem = StageWindow::Client(window.clone());
    let rect = f.state().visual_frame_rect(&elem).unwrap();
    let camera = f.state().camera();
    let zoom = f.state().zoom();
    Rectangle::new(
        Point::from((
            (rect.x_low - camera.x) * zoom,
            (rect.y_low - camera.y) * zoom,
        )),
        Size::from((
            (rect.x_high - rect.x_low) * zoom,
            (rect.y_high - rect.y_low) * zoom,
        )),
    )
}

fn assert_rect_eq(actual: Rectangle<f64, Logical>, expected: Rectangle<i32, Logical>, msg: &str) {
    let expected = expected.to_f64();
    assert!(
        (actual.loc.x - expected.loc.x).abs() < 1e-6
            && (actual.loc.y - expected.loc.y).abs() < 1e-6
            && (actual.size.w - expected.size.w).abs() < 1e-6
            && (actual.size.h - expected.size.h).abs() < 1e-6,
        "{msg}: got {actual:?}, expected {expected:?}"
    );
}

/// At `outer_gap = 0` a fit lands the content edge exactly on the usable area
/// and hangs the border off the far side of it, rather than inside.
#[test]
fn fit_with_zero_outer_gap_and_a_border_lands_content_on_the_usable_area() {
    let mut f = Fixture::with_config(config(
        r#"
        [snap]
        outer_gap = 0.0

        [decorations]
        border_width = 4
        "#,
    ));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();

    fit_and_adopt(&mut f, &window, id, &surface);

    let usable = f.state().get_usable_area();
    let content = content_screen_rect(&mut f, &window);
    assert_rect_eq(content, usable, "content must span the usable area exactly");

    let frame = frame_screen_rect(&mut f, &window);
    let expected_frame = Rectangle::new(
        Point::from((usable.loc.x - 4, usable.loc.y - 4)),
        Size::from((usable.size.w + 8, usable.size.h + 8)),
    );
    assert_rect_eq(
        frame,
        expected_frame,
        "the border hangs 4px past the usable area on every side",
    );
}

/// With no border, `outer_gap = 0` collapses frame and content onto the
/// usable area alike.
#[test]
fn fit_with_zero_outer_gap_and_no_border_lands_frame_and_content_flush() {
    let mut f = Fixture::with_config(config("[snap]\nouter_gap = 0.0\n"));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();

    fit_and_adopt(&mut f, &window, id, &surface);

    let usable = f.state().get_usable_area();
    let content = content_screen_rect(&mut f, &window);
    assert_rect_eq(content, usable, "content must span the usable area exactly");

    let frame = frame_screen_rect(&mut f, &window);
    assert_rect_eq(
        frame,
        usable,
        "with no border the frame is the content rect",
    );
}

/// An explicit `outer_gap` insets the content edge by exactly that much, on
/// every side.
#[test]
fn fit_with_an_explicit_outer_gap_insets_content_by_it() {
    let mut f = Fixture::with_config(config("[snap]\nouter_gap = 12.0\n"));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();

    fit_and_adopt(&mut f, &window, id, &surface);

    let usable = f.state().get_usable_area();
    let gap = f.state().config.snap_outer_gap as i32;
    let content = content_screen_rect(&mut f, &window);
    let expected = Rectangle::new(
        Point::from((usable.loc.x + gap, usable.loc.y + gap)),
        Size::from((usable.size.w - 2 * gap, usable.size.h - 2 * gap)),
    );
    assert_rect_eq(
        content,
        expected,
        "an explicit outer_gap insets the content edge by it",
    );
}

/// With no `[snap]` table at all, fit still insets by whatever `outer_gap`
/// defaults to — read from `config`, not hardcoded, so this holds regardless
/// of what that default is.
#[test]
fn fit_with_an_empty_config_insets_content_by_the_configured_default() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();

    fit_and_adopt(&mut f, &window, id, &surface);

    let usable = f.state().get_usable_area();
    let gap = f.state().config.snap_outer_gap;
    let content = content_screen_rect(&mut f, &window);
    assert!(
        (content.loc.x - (usable.loc.x as f64 + gap)).abs() < 1e-6
            && (content.loc.y - (usable.loc.y as f64 + gap)).abs() < 1e-6,
        "content edge must sit at usable.loc + the configured default outer_gap, got {content:?}"
    );
}

/// A panel's exclusive zone shrinks the usable area fit reads from, not the
/// bare output rect: at `outer_gap = 0` the content edge tracks that shrunk
/// area, landing at the panel's own bottom edge.
#[test]
fn fit_respects_a_panels_exclusive_zone_not_the_bare_output_rect() {
    let mut f = Fixture::with_config(config("[snap]\nouter_gap = 0.0\n"));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    map_top_panel(&mut f, id, 100);
    assert_eq!(
        f.state().get_usable_area().size,
        Size::from((1920, 980)),
        "precondition: the panel claimed a 100px exclusive zone"
    );

    let surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();

    fit_and_adopt(&mut f, &window, id, &surface);

    let usable = f.state().get_usable_area();
    let content = content_screen_rect(&mut f, &window);
    assert_rect_eq(
        content,
        usable,
        "content must span the panel-shrunk usable area, top edge at y=100",
    );
}

/// A lone window's fill lands its content on the `outer_gap`-inset usable area
/// — the same rect a fit grows to.
#[test]
fn fill_on_a_lone_window_lands_content_on_the_usable_area() {
    let mut f = Fixture::with_config(config(
        r#"
        [snap]
        outer_gap = 0.0

        [decorations]
        border_width = 4
        "#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // Fill silently no-ops once the window has fallen off the active
    // viewport, so map and fill immediately, before any camera move.
    let surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();

    f.state().fill_window(&window);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);

    let usable = f.state().get_usable_area();
    let content = content_screen_rect(&mut f, &window);
    assert_rect_eq(
        content,
        usable,
        "a lone window's fill must land on the content rect fit would produce",
    );
}

/// Regression test for the fit camera rounding: an odd content height (no SSD
/// bar to balance it) used to leave the fitted frame half a pixel inside the
/// usable edge. The camera must land on a whole pixel and the content edge
/// exactly on `usable.loc + outer_gap`.
#[test]
fn fit_rounds_the_camera_to_a_whole_pixel_even_with_an_odd_height_window() {
    let mut f = Fixture::with_config(config("[snap]\nouter_gap = 12.0\n"));
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let surface = map_window(&mut f, id, "a", (800, 601));
    let window = window_by_app_id(&mut f, "a").unwrap();

    fit_and_adopt(&mut f, &window, id, &surface);

    let camera = f.state().camera();
    assert!(
        camera.x.fract() == 0.0 && camera.y.fract() == 0.0,
        "the fit camera must land on a whole pixel, got {camera:?}"
    );

    let gap = f.state().config.snap_outer_gap;
    let usable = f.state().get_usable_area();
    let content = content_screen_rect(&mut f, &window);
    assert!(
        (content.loc.x - (usable.loc.x as f64 + gap)).abs() < 1e-6
            && (content.loc.y - (usable.loc.y as f64 + gap)).abs() < 1e-6,
        "content edge must land exactly on usable.loc + outer_gap, got {content:?}"
    );
}
