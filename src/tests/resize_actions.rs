//! Keyboard resize steps: `grow-window` / `shrink-window` walk the edge their
//! direction names and hold the opposite one still, on clients and stand-ins
//! alike — and keep holding it across a held key, whatever the client does with
//! the sizes it is handed.

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Point, SERIAL_COUNTER, Size};
use wayland_client::protocol::wl_surface::WlSurface as ClientSurface;

use super::client::ClientId;
use super::real::TempDir;
use super::{
    Fixture, adopt_last_configure, config, configure_count, end_grab, install_client_resize_grab,
    last_configured, map_window, map_window_with_limits, window_by_app_id, window_position,
};
use crate::state::{ClusterResizeSnapshot, StageWindow};
use driftwm::config::{Action, Direction};

fn step(f: &mut Fixture, action: Action) {
    f.state().execute_action(&action);
}

/// Ack the configure and commit without taking the size that came with it — a
/// fixed-size dialog's answer. smithay drops the pending configure at *ack*, so
/// afterwards the compositor sees no outstanding configure and a committed
/// geometry that never moved.
fn ack_but_keep_size(f: &mut Fixture, id: ClientId, surface: &ClientSurface) {
    f.double_roundtrip(id);
    let window = f.client(id).window(surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
}

/// Ack the configure and commit `size` instead of the one asked for — a
/// cell-snapping terminal rounding down to a whole character cell.
fn ack_with_size(f: &mut Fixture, id: ClientId, surface: &ClientSurface, size: (u16, u16)) {
    f.double_roundtrip(id);
    let window = f.client(id).window(surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
}

#[test]
fn growing_right_holds_the_left_edge() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    step(&mut f, Action::GrowWindow(Direction::Right));

    assert_eq!(last_configured(&mut f, id, &surface), (420, 300));
    assert_eq!(
        window_position(&mut f, &window),
        before,
        "the right edge moved, so the left one stayed where it was"
    );
}

/// Stage positions and `to_unit_vec` are both Y-down, so `up` must lift the top
/// edge — a sign slip here would grow the bottom instead and look identical in
/// size assertions alone.
#[test]
fn growing_up_holds_the_bottom_edge() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    step(&mut f, Action::GrowWindow(Direction::Up));

    assert_eq!(last_configured(&mut f, id, &surface), (400, 320));
    assert_eq!(
        window_position(&mut f, &window),
        before + Point::from((0, -20))
    );
}

#[test]
fn growing_down_holds_the_top_edge() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    step(&mut f, Action::GrowWindow(Direction::Down));

    assert_eq!(last_configured(&mut f, id, &surface), (400, 320));
    assert_eq!(window_position(&mut f, &window), before);
}

#[test]
fn shrinking_left_walks_the_left_edge_inward() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    step(&mut f, Action::ShrinkWindow(Direction::Left));

    assert_eq!(last_configured(&mut f, id, &surface), (380, 300));
    assert_eq!(
        window_position(&mut f, &window),
        before + Point::from((20, 0)),
        "the named edge walks inward while the right one holds"
    );
}

#[test]
fn a_diagonal_grow_steps_both_axes_by_the_projected_step() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    step(&mut f, Action::GrowWindow(Direction::UpLeft));

    assert_eq!(last_configured(&mut f, id, &surface), (414, 314));
    assert_eq!(
        window_position(&mut f, &window),
        before + Point::from((-14, -14))
    );
}

#[test]
fn a_diagonal_shrink_pulls_both_named_edges_in() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    step(&mut f, Action::ShrinkWindow(Direction::UpLeft));

    assert_eq!(last_configured(&mut f, id, &surface), (386, 286));
    assert_eq!(
        window_position(&mut f, &window),
        before + Point::from((14, 14)),
        "the down-right corner anchors, so the position follows the shrink"
    );
}

/// The anchor shift comes off the delta the clamp granted, not the one asked
/// for: measured against the request, the left edge would run 10px past a size
/// the client refused to take.
#[test]
fn a_partly_granted_grow_shifts_by_the_granted_delta() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window_with_limits(&mut f, id, "term", (400, 300), (0, 0), (410, 900));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    step(&mut f, Action::GrowWindow(Direction::Left));

    assert_eq!(last_configured(&mut f, id, &surface), (410, 300));
    assert_eq!(
        window_position(&mut f, &window),
        before + Point::from((-10, 0))
    );
}

#[test]
fn a_fully_clamped_grow_is_a_clean_no_op() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window_with_limits(&mut f, id, "term", (400, 300), (0, 0), (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);
    let configures = configure_count(&mut f, id, &surface);

    step(&mut f, Action::GrowWindow(Direction::Left));

    assert_eq!(
        window_position(&mut f, &window),
        before,
        "no size was granted, so the anchor edge must not drift either"
    );
    assert_eq!(
        configure_count(&mut f, id, &surface),
        configures,
        "a zero-delta step sends no configure at all"
    );
}

/// A client is free to declare a `max_size` its current size already violates —
/// `fit_window` does not clamp, and clients tighten their limits after mapping.
/// A step must not answer by resizing the axis it never named.
#[test]
fn a_step_leaves_the_axis_it_does_not_name_alone() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window_with_limits(&mut f, id, "term", (1000, 300), (0, 0), (800, 900));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    step(&mut f, Action::GrowWindow(Direction::Up));

    assert_eq!(
        last_configured(&mut f, id, &surface),
        (1000, 320),
        "grow-window up must not narrow the window to its declared maximum"
    );
    assert_eq!(
        window_position(&mut f, &window),
        before + Point::from((0, -20)),
        "and the right edge, which no clamp may move, stays put"
    );
}

#[test]
fn a_zero_resize_step_does_nothing() {
    let mut f = Fixture::with_config(config(
        r#"
[navigation]
resize_step = 0
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);
    let configures = configure_count(&mut f, id, &surface);

    step(&mut f, Action::GrowWindow(Direction::Left));

    assert_eq!(window_position(&mut f, &window), before);
    assert_eq!(configure_count(&mut f, id, &surface), configures);
}

/// `non_negative` accepts any non-negative `i32`, so the target size has to
/// saturate rather than wrap into a negative width.
#[test]
fn a_pathological_resize_step_does_not_overflow() {
    let mut f = Fixture::with_config(config(
        r#"
[navigation]
resize_step = 2147483647
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));

    step(&mut f, Action::GrowWindow(Direction::Right));

    assert!(last_configured(&mut f, id, &surface).0 > 400);
}

#[test]
fn a_pinned_window_is_left_alone() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [320, 240]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "pin", (320, 240));
    let window = window_by_app_id(&mut f, "pin").unwrap();
    assert!(
        !f.state().is_canvas_window(&window),
        "precondition: a pinned window holds no canvas rect"
    );
    let before = f.state().stage.position_of(&window);
    let configures = configure_count(&mut f, id, &surface);

    step(&mut f, Action::GrowWindow(Direction::Right));

    assert_eq!(f.state().stage.position_of(&window), before);
    assert_eq!(configure_count(&mut f, id, &surface), configures);
}

#[test]
fn a_step_is_ignored_under_an_interactive_move_grab() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);
    let configures = configure_count(&mut f, id, &surface);

    f.state().arm_interactive_move(&window);
    step(&mut f, Action::GrowWindow(Direction::Left));
    assert_eq!(window_position(&mut f, &window), before);
    assert_eq!(configure_count(&mut f, id, &surface), configures);

    f.state().disarm_interactive_move(&window);
    step(&mut f, Action::GrowWindow(Direction::Left));
    assert_eq!(last_configured(&mut f, id, &surface), (420, 300));
}

/// The client-resize half is a separate witness: a `ResizeGrab` over a client
/// arms no `interactive_move` entry, only the surface's `ResizeState`. Guarding
/// on grab membership alone would let this one through.
#[test]
fn a_step_is_ignored_under_a_client_resize_grab() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);
    let configures = configure_count(&mut f, id, &surface);

    install_client_resize_grab(
        &mut f,
        &window,
        xdg_toplevel::ResizeEdge::Right,
        Point::from((before.x as f64 + 390.0, before.y as f64 + 150.0)),
        output,
        ClusterResizeSnapshot::empty(),
    );

    step(&mut f, Action::GrowWindow(Direction::Left));

    assert_eq!(window_position(&mut f, &window), before);
    assert_eq!(configure_count(&mut f, id, &surface), configures);

    end_grab(&mut f);
}

/// A client that acks and then commits the size it already had — a fixed-size
/// dialog — is the case committed geometry cannot report, since smithay drops
/// the pending configure at ack rather than at commit. Without reconciliation
/// every repeat re-derives from the same stale size, shifts the anchor edge
/// another step, and the window slides away at the key-repeat rate while never
/// changing size at all.
#[test]
fn repeated_grows_do_not_walk_a_client_that_keeps_its_size() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    for _ in 0..5 {
        step(&mut f, Action::GrowWindow(Direction::Left));
        ack_but_keep_size(&mut f, id, &surface);
    }

    assert_eq!(
        window.geometry().size,
        Size::from((400, 300)),
        "precondition: the client never took any of the sizes it was handed"
    );
    assert_eq!(
        window_position(&mut f, &window),
        before + Point::from((-20, 0)),
        "five refused steps leave one step's outstanding offer, not five steps of drift"
    );
}

/// The counterpart, pinning both axes at once: a step on the far edges never
/// writes a position, so nothing the client does with the size may move one.
/// Taking the correction back on an axis the step never anchored would push the
/// window the opposite way instead.
#[test]
fn repeated_grows_of_the_far_edges_never_move_the_window() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    for _ in 0..5 {
        step(&mut f, Action::GrowWindow(Direction::DownRight));
        ack_but_keep_size(&mut f, id, &surface);
        assert_eq!(window_position(&mut f, &window), before);
    }
}

/// A cell-snapping terminal grants less than it was handed on every step, so the
/// residual repeats. The placement is optimistic by design, so the edge does sit
/// one step's ungranted remainder off until the next step takes it back — what
/// must never happen is the remainders summing, which is what an uncorrected
/// step does: 4px, 8px, 12px, walking away for as long as the key is held.
#[test]
fn repeated_grows_do_not_creep_on_a_client_that_snaps_its_size() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let right_edge = window_position(&mut f, &window).x + 400;

    for granted in [416, 432, 448, 464] {
        step(&mut f, Action::GrowWindow(Direction::Left));
        ack_with_size(&mut f, id, &surface, (granted, 300));
        assert_eq!(
            right_edge - (window_position(&mut f, &window).x + window.geometry().size.w),
            4,
            "after the step granted as {granted} the anchored right edge is off by one \
             step's remainder, the same as after the first"
        );
    }
}

/// The promise only covers the rect the step itself placed. A nudge in between
/// moves the window, and the next step has to measure from where it now is
/// rather than dragging it back onto a stale placement.
#[test]
fn a_move_between_steps_voids_the_previous_promise() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();

    step(&mut f, Action::GrowWindow(Direction::Left));
    ack_but_keep_size(&mut f, id, &surface);
    step(&mut f, Action::NudgeWindow(Direction::Right));
    let after_nudge = window_position(&mut f, &window);

    step(&mut f, Action::GrowWindow(Direction::Left));

    assert_eq!(
        window_position(&mut f, &window),
        after_nudge + Point::from((-20, 0)),
        "the step measured from the nudged position, not the pre-nudge promise"
    );
}

/// The promise belongs to one window. Focus moving to another that happens to
/// sit where the first was left — windows do stack — must not hand the second
/// one the first's outstanding correction.
#[test]
fn the_promise_does_not_transfer_to_another_window() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface_a = map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    step(&mut f, Action::GrowWindow(Direction::Left));
    ack_but_keep_size(&mut f, id, &surface_a);
    let placed_a = window_position(&mut f, &a);

    f.state()
        .map_window(StageWindow::Client(b.clone()), placed_a, true);
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&b, serial);

    step(&mut f, Action::GrowWindow(Direction::Left));

    assert_eq!(
        window_position(&mut f, &b),
        placed_a + Point::from((-20, 0)),
        "B stepped off its own rect, not off A's outstanding promise"
    );
}

/// A client that resizes itself between steps (a font change, a GTK `resize()`)
/// has not answered the configure, it has replaced it — the size lands outside
/// what was on offer, and correcting toward the promised rect would teleport the
/// window by the whole difference.
#[test]
fn a_self_resize_between_steps_voids_the_previous_promise() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();

    step(&mut f, Action::GrowWindow(Direction::Left));
    let placed = window_position(&mut f, &window);
    ack_with_size(&mut f, id, &surface, (500, 300));
    assert_eq!(
        window.geometry().size,
        Size::from((500, 300)),
        "precondition: the client took a size of its own, past the one offered"
    );

    step(&mut f, Action::GrowWindow(Direction::Left));

    assert_eq!(last_configured(&mut f, id, &surface), (520, 300));
    assert_eq!(
        window_position(&mut f, &window),
        placed + Point::from((-20, 0)),
        "the step measured from where the window is, not from a promise it never took"
    );
}

/// Neither action is in `runs_during_fullscreen`, so the guard exits first and
/// the step lands on the restored rect — visible, unlike the silent no-op a
/// fullscreen window's failed canvas guard would produce. The exit's owed
/// recenter has to go with it, or it fires on the very commit this configure
/// provokes and drags the window back to a center recorded before the exit.
#[test]
fn a_step_during_fullscreen_exits_first_and_resizes_the_restored_window() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let key = super::server_surface(&window).id();
    let before = window_position(&mut f, &window);

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    assert!(f.state().is_fullscreen(), "precondition: fullscreen");

    step(&mut f, Action::GrowWindow(Direction::Right));

    assert!(!f.state().is_fullscreen());
    assert!(!f.state().pending_recenter.contains_key(&key));
    assert_eq!(last_configured(&mut f, id, &surface), (420, 300));
    assert_eq!(
        window_position(&mut f, &window),
        before,
        "the left edge anchors the grow, and no owed recenter moved it"
    );

    adopt_last_configure(&mut f, id, &surface);
    assert_eq!(
        window_position(&mut f, &window),
        before,
        "nor did the commit that configure provoked"
    );
}

/// A stand-in has no client to declare a minimum, so the chrome floor is the
/// bound — and the anchor shift again follows what the floor granted.
#[test]
fn a_stand_in_shrink_stops_at_the_chrome_floor() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(tmp.path().join("session.json"));

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((130, 130)),
        "s",
        "S",
    );
    f.state().focus_and_raise_suspended(sid);
    assert_eq!(
        f.state().gated_suspended_focus(),
        Some(sid),
        "precondition: the stand-in holds focus"
    );
    let generation = f.state().render.blur_geometry_generation;

    step(&mut f, Action::ShrinkWindow(Direction::Left));

    let element = crate::state::StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    assert_eq!(
        f.state().find_suspended(sid).unwrap().size.get(),
        Size::from((120, 130))
    );
    assert_eq!(
        f.state().stage.position_of(&element),
        Some(Point::from((410, 300))),
        "the floor granted 10px of the 20px step, and the left edge walked only that far"
    );
    assert!(f.state().render.blur_geometry_generation > generation);
    assert!(
        f.state().session_store_dirty(),
        "a stand-in's canvas rect is durable session state"
    );

    // Cancels the debounce timer the mark armed; `debug_counters` has no entry
    // for event-loop timers, so the teardown baseline would not catch one.
    f.state().dismiss_suspended(sid);
}
