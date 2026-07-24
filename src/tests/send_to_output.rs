//! `send-to-output` relocates the focused window to the adjacent output with
//! output-native semantics: a fullscreen window re-fullscreens on the target, a
//! pinned window keeps its screen position (clamped) and rebinds its pin, and a
//! normal window keeps the canvas-center placement. Outputs tile left-to-right
//! by add order, so the second-added output sits to the right of the first.

use smithay::utils::{Point, Size};

use driftwm::config::{Action, Direction};

use crate::state::StageWindow;

use super::{Fixture, config, map_window, window_by_app_id};

/// A fullscreen window re-fullscreens on the target output; the source output
/// loses its fullscreen entry and the client sees a Fullscreen configure at the
/// target's size.
#[test]
fn fullscreen_moves_to_target_output() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let _out2 = f.add_output(2, (1280, 720));
    let id = f.add_client();

    let surface = map_window(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();

    // Fullscreen with no requested output lands on the active output (out1).
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert_eq!(
        f.state().stage.fullscreen_output_of(&window),
        Some("HEADLESS-1")
    );

    // Discard configures seen so far so the post-move ones stand alone.
    let _ = f.client(id).window(&surface).format_recent_configures();

    f.state()
        .execute_action(&Action::SendToOutput(Direction::Right));
    f.double_roundtrip(id);

    assert_eq!(
        f.state().stage.fullscreen_output_of(&window),
        Some("HEADLESS-2")
    );
    assert!(f.state().fullscreen_window_on(&out1).is_none());
    assert_eq!(f.state().focused_window().as_ref(), Some(&window));

    // The re-fullscreen configure carries Fullscreen at out2's 1280×720 size,
    // even if a transient windowed exit configure was emitted first.
    let post = f.client(id).window(&surface).format_recent_configures();
    assert!(
        post.lines()
            .any(|l| l.contains("Fullscreen") && l.contains("1280") && l.contains("720")),
        "expected a Fullscreen configure at the target size, got:\n{post}"
    );
}

/// A fullscreen window sent to another output stays there when the client
/// re-asserts fullscreen with no requested output (toolkits do this on focus
/// changes) — it must not yank back to the still-active source output.
#[test]
fn fullscreen_reassert_stays_on_moved_output() {
    let mut f = Fixture::new();
    let _out1 = f.add_output(1, (1920, 1080));
    let _out2 = f.add_output(2, (1280, 720));
    let id = f.add_client();

    let surface = map_window(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();

    // Fullscreen with no requested output lands on the active output (out1).
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert_eq!(
        f.state().stage.fullscreen_output_of(&window),
        Some("HEADLESS-1")
    );

    f.state()
        .execute_action(&Action::SendToOutput(Direction::Right));
    f.double_roundtrip(id);
    assert_eq!(
        f.state().stage.fullscreen_output_of(&window),
        Some("HEADLESS-2")
    );
    // Active output is still HEADLESS-1 — without the guard, resolution would
    // fall through to it instead of staying on HEADLESS-2.
    assert_eq!(
        f.state().active_output().map(|o| o.name()).as_deref(),
        Some("HEADLESS-1")
    );

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert_eq!(
        f.state().stage.fullscreen_output_of(&window),
        Some("HEADLESS-2")
    );
}

/// A pinned window rebinds to the target output, keeping its screen position
/// clamped into the smaller target's bounds; it stays pinned (not converted to
/// a canvas window).
#[test]
fn pinned_rebinds_with_clamp() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [320, 240]
"#,
    ));
    // out1 (first-added) is the active output, so the pin binds to it; out2 is
    // the smaller output to its right, forcing a clamp on the move.
    let _out1 = f.add_output(1, (1920, 1080));
    let _out2 = f.add_output(2, (800, 600));
    let id = f.add_client();

    map_window(&mut f, id, "pin", (320, 240));
    let window = window_by_app_id(&mut f, "pin").unwrap();
    let site = f.state().stage.pin_of(&window).cloned().unwrap();
    assert_eq!(site.output, "HEADLESS-1");
    // Output center: (1920/2 - 320/2, 1080/2 - 240/2).
    assert_eq!(site.screen_pos, Point::from((800, 420)));

    f.state()
        .execute_action(&Action::SendToOutput(Direction::Right));

    let site = f.state().stage.pin_of(&window).cloned().unwrap();
    assert_eq!(site.output, "HEADLESS-2");
    // Clamped into the 800×600 target for the 320×240 window:
    // x -> min(800, 800-320)=480, y -> min(420, 600-240)=360.
    assert_eq!(site.screen_pos, Point::from((480, 360)));
    assert!(f.state().is_pinned(&window));
}

/// With a single output there's nowhere to send to, but the fullscreen guard
/// must not exit fullscreen: the window stays fullscreen (the observable effect
/// of `SendToOutput` running during fullscreen).
#[test]
fn single_output_fullscreen_stays() {
    let mut f = Fixture::new();
    let _out1 = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert!(f.state().is_window_fullscreen(&window));

    f.state()
        .execute_action(&Action::SendToOutput(Direction::Right));

    assert!(f.state().is_window_fullscreen(&window));
}

/// A pinned window sent to another output then fullscreened opens on its pin
/// output, not the still-active source output, and re-pins there on exit.
#[test]
fn fullscreen_opens_on_pin_output() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [400, 300]
"#,
    ));
    // out1 (first-added) is active and gets the pin; out2 sits to its right.
    let _out1 = f.add_output(1, (1920, 1080));
    let _out2 = f.add_output(2, (1280, 720));
    let id = f.add_client();

    let surface = map_window(&mut f, id, "pin", (400, 300));
    let window = window_by_app_id(&mut f, "pin").unwrap();
    assert_eq!(
        f.state().stage.pin_of(&window).unwrap().output,
        "HEADLESS-1"
    );

    // Rebind the pin to out2. Nothing moved the cursor, so out1 stays active.
    f.state()
        .execute_action(&Action::SendToOutput(Direction::Right));
    assert_eq!(
        f.state().stage.pin_of(&window).unwrap().output,
        "HEADLESS-2"
    );
    assert_eq!(
        f.state().active_output().map(|o| o.name()).as_deref(),
        Some("HEADLESS-1")
    );

    // A client fullscreen with no requested output falls through to the pin
    // output (HEADLESS-2), not the active output (HEADLESS-1).
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert_eq!(
        f.state().stage.fullscreen_output_of(&window),
        Some("HEADLESS-2")
    );

    // Exiting re-pins on the same output — enter/exit stay symmetric.
    f.client(id).window(&surface).unset_fullscreen();
    f.double_roundtrip(id);
    assert_eq!(
        f.state().stage.pin_of(&window).unwrap().output,
        "HEADLESS-2"
    );
    assert!(f.state().is_pinned(&window));
}

/// A normal (non-fullscreen, non-pinned) canvas window moves to the adjacent
/// output.
#[test]
fn canvas_window_moves_to_target_output() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    // Both outputs default to a camera centered on the canvas origin, so their
    // viewports overlap. Pan out2 to a distinct canvas region so the move lands
    // the window somewhere only out2 covers and output_for_window can tell.
    crate::state::output_state(&out2).camera = Point::from((5000.0, 5000.0));
    let id = f.add_client();

    map_window(&mut f, id, "app", (400, 300));
    let window = window_by_app_id(&mut f, "app").unwrap();
    assert_eq!(
        f.state().output_for_window(&window).map(|o| o.name()),
        Some(out1.name())
    );

    f.state()
        .execute_action(&Action::SendToOutput(Direction::Right));

    assert_eq!(
        f.state().output_for_window(&window).map(|o| o.name()),
        Some(out2.name())
    );
}

/// A focused stand-in obeys `send-to-output` through the shared `execute_action`
/// dispatch: it re-centers on the target output's usable area, keeps gated
/// suspended focus, and lands on the target output.
#[test]
fn stand_in_moves_to_target_output() {
    let mut f = Fixture::new();
    let _out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    // Pan out2 to a distinct canvas region so output_for_window can tell where
    // the stand-in landed.
    crate::state::output_state(&out2).camera = Point::from((5000.0, 5000.0));

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    f.state().focus_and_raise_suspended(sid);
    assert_eq!(
        f.state().gated_suspended_focus(),
        Some(sid),
        "precondition: the stand-in holds focus"
    );

    f.state()
        .execute_action(&Action::SendToOutput(Direction::Right));

    let s = f.state().find_suspended(sid).unwrap();
    // out2's usable center in canvas coords: camera (5000,5000) + (640,360).
    assert_eq!(
        f.state()
            .stage
            .position_of(&StageWindow::Suspended(s.clone())),
        Some(Point::from((5440, 5210))),
        "the stand-in centers on the target output's usable area"
    );
    assert_eq!(
        f.state()
            .output_for_window(&StageWindow::Suspended(s))
            .map(|o| o.name()),
        Some(out2.name())
    );
    assert_eq!(
        f.state().gated_suspended_focus(),
        Some(sid),
        "focus follows the stand-in to the target output"
    );

    f.state().dismiss_suspended(sid);
}

/// With a single output there is nowhere to send to: a focused stand-in stays
/// put.
#[test]
fn stand_in_send_to_output_single_output_is_noop() {
    let mut f = Fixture::new();
    let _out1 = f.add_output(1, (1920, 1080));

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    f.state().focus_and_raise_suspended(sid);

    f.state()
        .execute_action(&Action::SendToOutput(Direction::Right));

    let s = f.state().find_suspended(sid).unwrap();
    assert_eq!(
        f.state().stage.position_of(&StageWindow::Suspended(s)),
        Some(Point::from((400, 300))),
        "a single output leaves the stand-in in place"
    );

    f.state().dismiss_suspended(sid);
}
