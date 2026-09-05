//! The IPC-facing `WindowInfo.mode` mirrors the stage's fit/fill membership:
//! `Normal` for a plain window, `Fit` for `fit-window`/`fit-window-snapped`,
//! `Fill` for `fill-window`, and back to `Normal` once unfit or unfilled.
//! A fill whose resulting rect equals the window's current one is a silent
//! no-op (`compute_fill_geometry`'s early return), and on an empty canvas a
//! fit window's rect already *is* that fill rect, since fit and fill both
//! inset the usable area by `snap_gap`. So a fit-to-fill transition needs a
//! neighbour to give the fill a different rect to land on.

use smithay::utils::Point;

use super::{Fixture, map_window, settle, window_by_app_id};
use crate::ipc::protocol::WindowMode;

/// The inventory's reported mode for the window named `app_id`.
fn mode_of(f: &mut Fixture, app_id: &str) -> WindowMode {
    f.state()
        .window_inventory()
        .into_iter()
        .find(|w| w.app_id == app_id)
        .expect("window in inventory")
        .mode
}

#[test]
fn a_freshly_mapped_window_reports_normal() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "w", (400, 300));

    assert_eq!(mode_of(&mut f, "w"), WindowMode::Normal);
}

#[test]
fn fit_window_reports_fit() {
    let mut f = Fixture::new();
    // The fit's camera move seeds a per-output blur generation that only
    // clears on output disconnect, so it can never return to the pre-output
    // baseline (see zoom_to_fit.rs).
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();

    f.state().fit_window(&window);
    settle(&mut f);

    assert_eq!(mode_of(&mut f, "w"), WindowMode::Fit);
}

#[test]
fn fit_window_snapped_also_reports_fit() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();

    f.state().fit_window_snapped(&window);
    settle(&mut f);

    assert_eq!(mode_of(&mut f, "w"), WindowMode::Fit);
}

#[test]
fn fill_window_reports_fill() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    // Fill right after mapping, before any camera move — fill_window silently
    // no-ops once the window has fallen off the active viewport.
    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();

    f.state().fill_window(&window);

    assert_eq!(mode_of(&mut f, "w"), WindowMode::Fill);
}

#[test]
fn unfit_window_returns_a_fit_window_to_normal() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();
    f.state().fit_window(&window);
    settle(&mut f);
    assert_eq!(
        mode_of(&mut f, "w"),
        WindowMode::Fit,
        "precondition: the fit took"
    );

    f.state().unfit_window(&window);
    settle(&mut f);

    assert_eq!(mode_of(&mut f, "w"), WindowMode::Normal);
}

#[test]
fn unfill_window_returns_a_filled_window_to_normal() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();
    f.state().fill_window(&window);
    assert_eq!(
        mode_of(&mut f, "w"),
        WindowMode::Fill,
        "precondition: the fill took"
    );

    f.state().unfill_window(&window);

    assert_eq!(mode_of(&mut f, "w"), WindowMode::Normal);
}

#[test]
fn fitting_then_filling_reports_fill_not_fit() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();
    // Pin the pre-fit position so the fit rect this grows to lands at a known
    // spot: fit keeps the window's own visual center fixed and pans the camera
    // to match, so a fit growing from (0, 0) ends up centered there too.
    f.state()
        .map_window(window.clone(), Point::from((0, 0)), false);

    // A neighbour straddling the fit rect's right edge, so the fill that
    // follows has somewhere to shrink from (see the module doc comment on why
    // an empty canvas can't exercise this transition).
    map_window(&mut f, id, "blocker", (300, 300));
    let blocker = window_by_app_id(&mut f, "blocker").unwrap();
    f.state().map_window(blocker, Point::from((900, 0)), false);

    f.state().fit_window(&window);
    settle(&mut f);
    f.state().fill_window(&window);

    assert_eq!(mode_of(&mut f, "w"), WindowMode::Fill);
}

#[test]
fn filling_then_fitting_reports_fit_not_fill() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();
    f.state().fill_window(&window);

    f.state().fit_window(&window);
    settle(&mut f);

    assert_eq!(mode_of(&mut f, "w"), WindowMode::Fit);
}
