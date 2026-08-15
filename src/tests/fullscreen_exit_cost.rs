//! What a fullscreen exit must *not* leave behind. The exit restores the saved
//! camera and zoom in one step, so any viewport state still aimed at the parked
//! fullscreen viewport is carried onto a different one — and any work it defers
//! to the next frame is work it already did.

use smithay::utils::Point;

use super::input_backend::{FakeDevice, pointer_to};
use super::{Fixture, map_window, window_by_app_id};

/// A mapped window on a single output with a quiet camera, and the pointer over
/// the middle of the viewport.
fn fullscreen_ready(
    f: &mut Fixture,
) -> (
    super::client::ClientId,
    smithay::output::Output,
    smithay::desktop::Window,
) {
    let output = f.add_output(1, (1920, 1080));
    // Camera writes seed a per-output blur generation that only clears on output
    // disconnect, so these scenarios can't return to the pre-output baseline.
    f.skip_baseline_check();
    let id = f.add_client();
    map_window(f, id, "w", (600, 400));
    let window = window_by_app_id(f, "w").unwrap();
    f.state().map_window(
        crate::state::StageWindow::Client(window.clone()),
        Point::from((200, 150)),
        false,
    );
    // Zoomed out so the restore actually moves the viewport and has to warp the
    // cursor; at zoom 1 with the camera already parked, the exit warps nothing.
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.zoom = 0.5;
        os.camera_target = None;
        os.zoom_target = None;
        os.zoom_animation_anchor = None;
        os.momentum.stop();
    });
    f.state().update_output_from_camera();
    pointer_to(f, &FakeDevice::mouse(), Point::from((400.0, 300.0)));
    (id, output, window)
}

/// A pan that was still coasting when the exit landed must not carry its
/// velocity onto the restored viewport — the delta it holds was measured
/// against the parked one. `enter_fullscreen` stops it on the way in.
#[test]
fn exiting_fullscreen_stops_a_coasting_camera() {
    let mut f = Fixture::new();
    let (id, output, window) = fullscreen_ready(&mut f);

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);

    f.state().with_output_state(|os| {
        os.momentum.accumulate(Point::from((40.0, 25.0)), 0);
        os.momentum.accumulate(Point::from((40.0, 25.0)), 8);
        os.momentum.launch();
    });
    assert!(
        f.state().with_output_state(|os| os.momentum.coasting) == Some(true),
        "the scenario needs a live coast to exit out of"
    );

    f.state().exit_fullscreen_on(&output);
    let restored = f.state().camera();
    assert!(
        !f.state().panning(),
        "a live pan would make the momentum ticks below no-ops"
    );

    // The udev turn, which is the one that ships on hardware; it takes its dt
    // from the wall clock rather than a caller-supplied tick.
    for _ in 0..10 {
        f.state().tick_all_animations();
    }

    assert_eq!(
        f.state().camera(),
        restored,
        "the restored camera must stay put; a coast aimed at the parked \
         viewport has no business moving it"
    );
}

/// The exit re-seats pointer focus itself, so it must not also leave the
/// deferred resync armed: that recomputes the same answer next frame — a second
/// full hit-test walk and a second motion — and keeps the render loop out of
/// its idle path meanwhile.
#[test]
fn exiting_fullscreen_leaves_no_deferred_pointer_resync() {
    let mut f = Fixture::new();
    let (id, output, window) = fullscreen_ready(&mut f);

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    f.state().pending_pointer_resync = false;
    let before = f.state().seat.get_pointer().unwrap().current_location();
    assert!(
        !f.state().seat.get_pointer().unwrap().is_grabbed(),
        "a grab would send the warp down a branch that arms nothing"
    );

    f.state().exit_fullscreen_on(&output);

    assert_ne!(
        f.state().seat.get_pointer().unwrap().current_location(),
        before,
        "the scenario needs an exit that actually warps the cursor, or the \
         deferred resync it arms is never armed and the assertion below is free"
    );
    assert!(
        !f.state().pending_pointer_resync,
        "the exit's own re-seat is the resync; a second one is pure repeat"
    );
}
