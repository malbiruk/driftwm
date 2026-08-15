//! A geometry action pressed while a window is fullscreen must behave exactly as
//! if the user had exited fullscreen first and then applied it to the restored
//! window. `execute_action` does exit first, but the exit only *sends* the
//! smaller configure: the client keeps reporting the fullscreen-sized buffer
//! until it acks, and the exit leaves an owed recenter behind. So any action that
//! sizes or centers from live geometry, or that a stale recenter can still move,
//! has to take its numbers from the restore authority instead.

use smithay::utils::{Logical, Point, Size};

use super::{Fixture, map_window, settle, window_by_app_id};

/// The focused window's stage rect once the client has caught up with every
/// configure in flight and any owed settle has fired.
fn settled_rect(
    f: &mut Fixture,
    id: super::client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
    app_id: &str,
) -> (Point<i32, Logical>, Size<i32, Logical>) {
    settle(f);
    f.double_roundtrip(id);
    super::adopt_last_configure(f, id, surface);
    f.double_roundtrip(id);
    settle(f);
    let window = window_by_app_id(f, app_id).expect("window still mapped");
    let pos = f.state().stage.position_of(&window).expect("staged");
    (pos, window.geometry().size)
}

/// A window mapped at a known canvas rect on a single output, camera quiet.
fn one_window(
    f: &mut Fixture,
) -> (
    super::client::ClientId,
    wayland_client::protocol::wl_surface::WlSurface,
) {
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(f, id, "w", (600, 400));
    let window = window_by_app_id(f, "w").unwrap();
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.zoom = 1.0;
        os.camera_target = None;
        os.zoom_target = None;
        os.zoom_animation_anchor = None;
        os.overview_return = None;
        os.momentum.stop();
    });
    f.state()
        .map_window(window.clone(), Point::from((200, 150)), false);
    f.state().update_output_from_camera();
    (id, surface)
}

/// Filling straight out of fullscreen must land the window exactly where filling
/// the restored window does. Live geometry still reports the viewport size at
/// that moment, so a fill seeded from it sees a rect that already covers the
/// whole usable area and finds nothing to grow into.
#[test]
fn filling_straight_out_of_fullscreen_matches_a_plain_fill() {
    // Reference: plain fill, no fullscreen involved.
    let want = {
        let mut f = Fixture::new();
        let (id, surface) = one_window(&mut f);
        let window = window_by_app_id(&mut f, "w").unwrap();
        f.state().fill_window(&window);
        settled_rect(&mut f, id, &surface, "w")
    };

    // Same window, but the fill is dispatched out of fullscreen.
    let mut f = Fixture::new();
    let (id, surface) = one_window(&mut f);
    let output = f.state().active_output().unwrap();
    let window = window_by_app_id(&mut f, "w").unwrap();

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);

    // No ack between the exit and the fill — that is the real dispatch order.
    f.state().exit_fullscreen_on(&output);
    f.state().fill_window(&window);
    let got = settled_rect(&mut f, id, &surface, "w");

    assert_eq!(
        got, want,
        "fill out of fullscreen must match a plain fill of the restored window"
    );
}

/// Unfitting straight out of fullscreen re-centers on the restored size, not on
/// the fullscreen buffer the client is still reporting.
#[test]
fn unfitting_straight_out_of_fullscreen_matches_a_plain_unfit() {
    let want = {
        let mut f = Fixture::new();
        let (id, surface) = one_window(&mut f);
        let window = window_by_app_id(&mut f, "w").unwrap();
        f.state().fit_window(&window);
        let _ = settled_rect(&mut f, id, &surface, "w");
        let window = window_by_app_id(&mut f, "w").unwrap();
        f.state().unfit_window(&window);
        settled_rect(&mut f, id, &surface, "w")
    };

    let mut f = Fixture::new();
    let (id, surface) = one_window(&mut f);
    let window = window_by_app_id(&mut f, "w").unwrap();
    f.state().fit_window(&window);
    let _ = settled_rect(&mut f, id, &surface, "w");
    let output = f.state().active_output().unwrap();
    let window = window_by_app_id(&mut f, "w").unwrap();

    // Fullscreen a fit window, then unfit straight out of the exit.
    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.state().exit_fullscreen_on(&output);
    f.state().unfit_window(&window);
    let got = settled_rect(&mut f, id, &surface, "w");

    assert_eq!(
        got, want,
        "unfit out of fullscreen must match a plain unfit of the restored window"
    );
}
