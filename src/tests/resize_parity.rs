//! Resize-grab parity: a suspended stand-in and a client window flow through
//! the one unified [`ResizeGrab`], so a stand-in resize gains the client's
//! constraint floor, output-edge clamp, and cursor reset, while the client's
//! configure/commit settle and the shared interactive-resize blur bump keep
//! their existing behavior.
//!
//! Client grabs are installed directly via the public `ResizeGrab` struct
//! literal, over the same `begin_client_resize` the real entry points run, so
//! `handle_resize_commit` reposition/settle runs instead of early-returning.
//! Suspended grabs run through the real `try_suspended_button`
//! button path so the cursor and cluster install exactly as production drives
//! them — the single-motion precedent in `suspended.rs`.
//!
//! The tests at the end instead drive the start paths themselves — the pointer
//! helpers directly, and the client's own `xdg_toplevel.resize` over the wire —
//! since what they pin down is what those paths do before any grab exists.

use std::cell::RefCell;

use smithay::backend::input::ButtonState;
use smithay::desktop::Window;
use smithay::input::keyboard::ModifiersState;
use smithay::input::pointer::{ButtonEvent, CursorIcon, CursorImageStatus, MotionEvent};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};
use smithay::wayland::compositor::with_states;

use driftwm::config::{BTN_LEFT, Config};
use wayland_client::protocol::wl_surface::WlSurface as WlClientSurface;

use crate::grabs::ResizeState;
use crate::state::{ClusterResizeSnapshot, FocusTarget, StageWindow};

use super::{
    Fixture, adopt_last_configure, assert_resize_entered, client_sees_maximized, fit_and_frame,
    install_client_resize_grab, map_window, seed_fit_and_fill, server_surface, window_by_app_id,
};

fn pt(x: f64, y: f64) -> Point<f64, Logical> {
    Point::from((x, y))
}

/// Camera at the canvas origin, zoom 1, on the active output: canvas == screen.
fn origin_view(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });
}

/// Server decorations on so suspended chrome resolves for `try_suspended_button`,
/// plus a held-modifier resize binding to start a stand-in resize from a click.
fn config_resize_binding() -> Config {
    Config::from_toml(
        r#"
        [decorations]
        default_mode = "server"
        [mouse.anywhere]
        "super+left" = "resize-window"
    "#,
    )
    .unwrap()
}

/// Deliver one pointer motion at canvas-space `loc` to the active grab.
fn motion(f: &mut Fixture, loc: Point<f64, Logical>) {
    let pointer = f.state().seat.get_pointer().unwrap();
    let event = MotionEvent {
        location: loc,
        serial: SERIAL_COUNTER.next_serial(),
        time: 0,
    };
    pointer.motion(f.state(), None, &event);
}

/// Release the left button, ending the resize through the real grab teardown.
fn release(f: &mut Fixture) {
    let pointer = f.state().seat.get_pointer().unwrap();
    let event = ButtonEvent {
        button: BTN_LEFT,
        state: ButtonState::Released,
        serial: SERIAL_COUNTER.next_serial(),
        time: 0,
    };
    pointer.button(f.state(), &event);
}

/// Start a stand-in border resize the way production does: a held `super+left`
/// over the stand-in. The resize edge is derived from where `click` lands
/// within the body, and the cursor + cluster install through the real path.
fn start_suspended_resize(f: &mut Fixture, click: Point<f64, Logical>) {
    let pointer = f.state().seat.get_pointer().unwrap();
    let held = ModifiersState {
        logo: true,
        ..Default::default()
    };
    let serial = SERIAL_COUNTER.next_serial();
    f.state()
        .try_suspended_button(&pointer, click, BTN_LEFT, serial, held);
}

/// Read the server-side `ResizeState` a grab/commit left on `surface`.
fn resize_state(surface: &WlSurface) -> ResizeState {
    with_states(surface, |states| {
        *states
            .data_map
            .get::<RefCell<ResizeState>>()
            .expect("resize state seeded")
            .borrow()
    })
}

/// A right-edge shrink past the usable-chrome floor stops at `MIN_SUSPENDED_SIZE`
/// on both axes — the stand-in arm folds its floor into the shared constraints.
#[test]
fn suspended_resize_floors_at_min_size() {
    let mut f = Fixture::with_config(config_resize_binding());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    // Right third of the body → a right-edge resize; drag the edge far left.
    start_suspended_resize(&mut f, pt(700.0, 450.0));
    motion(&mut f, pt(400.0, 450.0));

    let s = f.state().find_suspended(sid).unwrap();
    assert_eq!(
        s.size.get(),
        Size::from((120, 300)),
        "a shrink past the floor clamps to MIN_SUSPENDED_SIZE"
    );

    release(&mut f);
    f.state().dismiss_suspended(sid);
}

/// `MIN_SUSPENDED_SIZE` is a floor on the stand-in's *visible* frame, not its
/// stored body: with a border configured, an interactive drag (not just the
/// non-interactive `msg resize` in `resize_ipc.rs`) floors the width further
/// below 120, since the border eats into the body on top of the bar.
#[test]
fn suspended_resize_floors_at_min_size_with_border() {
    let mut f = Fixture::with_config(
        Config::from_toml(
            r#"
        [decorations]
        default_mode = "server"
        border_width = 4
        [mouse.anywhere]
        "super+left" = "resize-window"
    "#,
        )
        .unwrap(),
    );
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    // Right third of the body → a right-edge resize; drag the edge far left.
    start_suspended_resize(&mut f, pt(700.0, 450.0));
    motion(&mut f, pt(400.0, 450.0));

    let s = f.state().find_suspended(sid).unwrap();
    assert_eq!(
        s.size.get(),
        // 120 minus the 4px border on the dragged axis (height is untouched
        // by a pure right-edge drag): 120 - 2×4 = 112.
        Size::from((112, 300)),
        "a shrink past the floor clamps to the border-deflated floor, not the borderless 120"
    );

    release(&mut f);
    f.state().dismiss_suspended(sid);
}

/// A top-left corner drag keeps the opposite (bottom-right) corner fixed: the
/// position shifts by exactly the size change on each dragged edge.
#[test]
fn suspended_top_left_corner_resize_keeps_opposite_corner_fixed() {
    let mut f = Fixture::with_config(config_resize_binding());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    // Top-left third → TopLeft edge; drag the corner inward by (100, 100).
    start_suspended_resize(&mut f, pt(450.0, 350.0));
    motion(&mut f, pt(550.0, 450.0));

    let s = f.state().find_suspended(sid).unwrap();
    let pos = f
        .state()
        .stage
        .position_of(&StageWindow::Suspended(s.clone()))
        .unwrap();
    let size = s.size.get();
    assert_eq!(
        (pos + Point::from((size.w, size.h))),
        Point::from((800, 600)),
        "the bottom-right corner stays fixed while the top-left edge moves"
    );

    release(&mut f);
    f.state().dismiss_suspended(sid);
}

/// Releasing a stand-in resize persists the resized size and tears the grab
/// down (no revert, no lingering grab).
#[test]
fn suspended_resize_release_persists_size_and_ends_grab() {
    let mut f = Fixture::with_config(config_resize_binding());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    start_suspended_resize(&mut f, pt(700.0, 450.0));
    motion(&mut f, pt(900.0, 450.0));
    release(&mut f);

    let s = f.state().find_suspended(sid).unwrap();
    assert_eq!(
        s.size.get(),
        Size::from((600, 300)),
        "the resized size survives release"
    );
    assert!(
        !f.state().seat.get_pointer().unwrap().is_grabbed(),
        "release tears the resize grab down"
    );

    f.state().dismiss_suspended(sid);
}

/// After releasing a stand-in border resize the resize-edge cursor is reset to
/// the default shape.
#[test]
fn releasing_suspended_resize_resets_cursor() {
    let mut f = Fixture::with_config(config_resize_binding());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    start_suspended_resize(&mut f, pt(700.0, 450.0));
    assert!(
        matches!(
            f.state().cursor.cursor_status,
            CursorImageStatus::Named(CursorIcon::EResize)
        ) && f.state().cursor.grab_cursor,
        "precondition: a right-edge resize shows the resize cursor"
    );

    release(&mut f);

    assert!(
        matches!(
            f.state().cursor.cursor_status,
            CursorImageStatus::Named(CursorIcon::Default)
        ),
        "releasing the resize resets the cursor to the default shape"
    );
    assert!(
        !f.state().cursor.grab_cursor,
        "releasing the resize releases cursor ownership"
    );

    f.state().dismiss_suspended(sid);
}

/// A stand-in dismissed mid-resize turns further motion into a pass-through
/// (the pointer keeps tracking) and release tears the pass-through grab down.
#[test]
fn suspended_resize_mid_drag_dismiss_forwards_then_release_cleans_up() {
    let mut f = Fixture::with_config(config_resize_binding());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    start_suspended_resize(&mut f, pt(700.0, 450.0));
    motion(&mut f, pt(800.0, 450.0));
    f.state().dismiss_suspended(sid);

    motion(&mut f, pt(900.0, 600.0));
    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        pt(900.0, 600.0),
        "a dismissed resize still forwards motion so the pointer keeps tracking"
    );

    release(&mut f);
    assert!(
        !f.state().seat.get_pointer().unwrap().is_grabbed(),
        "releasing the button tears the pass-through grab down"
    );
}

/// A stand-in resize dragged past the output's right edge stops at the
/// edge-derived maximum instead of tracking the raw (off-screen) coordinate.
#[test]
fn suspended_resize_clamps_at_output_edge() {
    let mut f = Fixture::with_config(config_resize_binding());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    // Right-edge drag to canvas x = 3000, far past the 1920-wide output. The
    // pointer clamps to screen x = 1919, so the width stops at 400 + (1919-700).
    start_suspended_resize(&mut f, pt(700.0, 450.0));
    motion(&mut f, pt(3000.0, 450.0));

    let s = f.state().find_suspended(sid).unwrap();
    assert_eq!(
        s.size.get().w,
        1619,
        "the width stops at the output-edge maximum, not the raw coordinate"
    );

    release(&mut f);
    f.state().dismiss_suspended(sid);
}

/// The interactive-resize blur bump fires on a size-progressing stand-in tick
/// but not on a no-op tick that leaves the size unchanged.
#[test]
fn suspended_resize_tick_bumps_blur_only_when_size_progresses() {
    let mut f = Fixture::with_config(config_resize_binding());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    start_suspended_resize(&mut f, pt(700.0, 450.0));

    let gen0 = f.state().render.blur_geometry_generation;
    motion(&mut f, pt(800.0, 450.0));
    let gen1 = f.state().render.blur_geometry_generation;
    assert!(
        gen1 > gen0,
        "a size-progressing stand-in resize tick bumps the blur generation"
    );

    // Same location again → same delta → no size change → no bump.
    motion(&mut f, pt(800.0, 450.0));
    let gen2 = f.state().render.blur_geometry_generation;
    assert_eq!(
        gen2, gen1,
        "a no-op stand-in resize tick does not bump the blur generation"
    );

    release(&mut f);
    f.state().dismiss_suspended(sid);
}

/// The client arm shares the same blur bump: it fires on a size-progressing
/// resize tick but not on a no-op tick.
#[test]
fn client_resize_tick_bumps_blur_only_when_size_progresses() {
    let mut f = Fixture::with_config(config_resize_binding());
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    map_window(&mut f, id, "c", (400, 300));
    let window = window_by_app_id(&mut f, "c").unwrap();
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((400, 300)),
        true,
    );

    install_client_resize_grab(
        &mut f,
        &window,
        xdg_toplevel::ResizeEdge::Right,
        pt(800.0, 450.0),
        out,
        ClusterResizeSnapshot::empty(),
    );

    let gen0 = f.state().render.blur_geometry_generation;
    motion(&mut f, pt(900.0, 450.0));
    let gen1 = f.state().render.blur_geometry_generation;
    assert!(
        gen1 > gen0,
        "a size-progressing client resize tick bumps the blur generation"
    );

    motion(&mut f, pt(900.0, 450.0));
    let gen2 = f.state().render.blur_geometry_generation;
    assert_eq!(
        gen2, gen1,
        "a no-op client resize tick does not bump the blur generation"
    );

    release(&mut f);
}

/// A resize commit bumps the blur generation only when it changes the committed
/// size; a damage-only commit at the same size leaves it untouched.
#[test]
fn client_resize_commit_bumps_blur_only_on_size_change() {
    let mut f = Fixture::with_config(config_resize_binding());
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    let csurface = map_window(&mut f, id, "c", (400, 300));
    let window = window_by_app_id(&mut f, "c").unwrap();
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((400, 300)),
        true,
    );

    install_client_resize_grab(
        &mut f,
        &window,
        xdg_toplevel::ResizeEdge::Right,
        pt(800.0, 450.0),
        out,
        ClusterResizeSnapshot::empty(),
    );

    motion(&mut f, pt(900.0, 450.0));
    f.double_roundtrip(id);

    let gen0 = f.state().render.blur_geometry_generation;
    adopt_last_configure(&mut f, id, &csurface);
    let gen1 = f.state().render.blur_geometry_generation;
    assert!(
        gen1 > gen0,
        "a size-changing resize commit bumps the blur generation"
    );

    // A repaint at the same size — a busy client under a held-still border.
    f.client(id).window(&csurface).attach_new_buffer();
    f.client(id).window(&csurface).commit();
    f.double_roundtrip(id);
    let gen2 = f.state().render.blur_geometry_generation;
    assert_eq!(
        gen2, gen1,
        "a damage-only commit at unchanged size does not bump the blur generation"
    );

    release(&mut f);
}

/// A drag resized larger, committed there, then released and committed back at
/// the exact initial size still bumps blur on the settle commit. This pins
/// `finalize()` carrying the *stored* `last_committed_size` (the larger value)
/// into `WaitingForLastCommit`: re-seeding from `initial_window_size` instead
/// would make the settle compare equal to the initial size and skip the bump.
#[test]
fn client_settle_commit_at_initial_size_still_bumps_blur() {
    let mut f = Fixture::with_config(config_resize_binding());
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    let csurface = map_window(&mut f, id, "c", (400, 300));
    let window = window_by_app_id(&mut f, "c").unwrap();
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((400, 300)),
        true,
    );

    install_client_resize_grab(
        &mut f,
        &window,
        xdg_toplevel::ResizeEdge::Right,
        pt(800.0, 450.0),
        out,
        ClusterResizeSnapshot::empty(),
    );

    // Grow to 500 and let the client commit there — write-back records
    // last_committed_size = 500.
    motion(&mut f, pt(900.0, 450.0));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &csurface);

    // Release arms the settle; finalize must carry the 500 forward.
    release(&mut f);
    f.double_roundtrip(id);

    // The client settles back at the *initial* 400×300. current_geo now equals
    // initial_window_size, so the only thing that can make the settle bump is a
    // carried last_committed_size that still differs (the 500).
    let gen_before = f.state().render.blur_geometry_generation;
    f.client(id).window(&csurface).set_size(400, 300);
    f.client(id).window(&csurface).attach_new_buffer();
    f.client(id).window(&csurface).ack_last();
    f.client(id).window(&csurface).commit();
    f.double_roundtrip(id);
    let gen_after = f.state().render.blur_geometry_generation;

    assert!(
        gen_after > gen_before,
        "a settle commit back at the initial size still bumps blur when the \
         drag last committed at a larger size"
    );
}

/// A client resize runs the full grab lifecycle: motion configures the new size
/// with the Resizing state, the ack/commit repositions a left-edge drag, and
/// release then a final commit settles the restore size back to Idle.
#[test]
fn client_resize_configures_repositions_and_settles() {
    let mut f = Fixture::with_config(config_resize_binding());
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    let csurface = map_window(&mut f, id, "c", (400, 300));
    let window = window_by_app_id(&mut f, "c").unwrap();
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((400, 300)),
        true,
    );
    let ssurface = server_surface(&window);

    // Left-edge drag: grab origin at the left edge, dragged 100px left → the
    // width grows by 100 and the left edge (position) must move to compensate.
    install_client_resize_grab(
        &mut f,
        &window,
        xdg_toplevel::ResizeEdge::Left,
        pt(400.0, 450.0),
        out,
        ClusterResizeSnapshot::empty(),
    );

    motion(&mut f, pt(300.0, 450.0));
    f.double_roundtrip(id);

    let configure = f
        .client(id)
        .window(&csurface)
        .configures_received
        .last()
        .unwrap()
        .1
        .clone();
    assert_eq!(
        configure.size,
        (500, 300),
        "the motion configures the new size"
    );
    assert!(
        configure
            .states
            .contains(&wayland_protocols::xdg::shell::client::xdg_toplevel::State::Resizing),
        "the configure carries the Resizing state"
    );

    adopt_last_configure(&mut f, id, &csurface);
    assert_eq!(
        f.state()
            .stage
            .position_of(&StageWindow::Client(window.clone())),
        Some(Point::from((300, 300))),
        "the ack/commit repositions the left edge so the right edge stays fixed"
    );

    release(&mut f);
    assert!(
        matches!(
            resize_state(&ssurface),
            ResizeState::WaitingForLastCommit { .. }
        ),
        "release arms the commit-time settle"
    );

    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &csurface);
    assert!(
        matches!(resize_state(&ssurface), ResizeState::Idle),
        "the final commit settles the resize back to Idle"
    );
    assert_eq!(
        f.state().stage.restore_size(&window),
        Some(Size::from((500, 300))),
        "the settle anchors the restore size to the user's final choice"
    );
}

/// The top/left compensation is measured per commit against the size that
/// commit replaces, so a drag the client acks in several steps lands exactly
/// where one grown-from-the-grab-start step would: the opposite edges never
/// move, however many commits the delta is spread over.
#[test]
fn a_top_left_resize_holds_its_opposite_edges_across_every_commit() {
    let mut f = Fixture::with_config(config_resize_binding());
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    let csurface = map_window(&mut f, id, "c", (400, 300));
    let window = window_by_app_id(&mut f, "c").unwrap();
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((400, 300)),
        true,
    );
    let ssurface = server_surface(&window);

    // Grab the top-left corner: the right edge sits at 800, the bottom at 600,
    // and both must stay there for the whole drag.
    install_client_resize_grab(
        &mut f,
        &window,
        xdg_toplevel::ResizeEdge::TopLeft,
        pt(400.0, 300.0),
        out,
        ClusterResizeSnapshot::empty(),
    );

    for (drag_to, expected) in [
        (pt(300.0, 250.0), Point::from((300, 250))),
        (pt(250.0, 200.0), Point::from((250, 200))),
        (pt(200.0, 150.0), Point::from((200, 150))),
    ] {
        motion(&mut f, drag_to);
        f.double_roundtrip(id);
        adopt_last_configure(&mut f, id, &csurface);
        let pos = f
            .state()
            .stage
            .position_of(&StageWindow::Client(window.clone()))
            .unwrap();
        assert_eq!(
            (
                pos,
                pos + Point::from((window.geometry().size.w, window.geometry().size.h))
            ),
            (expected, Point::from((800, 600))),
            "every commit moves the dragged corner and leaves the opposite one at (800, 600)"
        );
    }

    release(&mut f);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &csurface);
    assert!(
        matches!(resize_state(&ssurface), ResizeState::Idle),
        "the final commit settles the resize back to Idle"
    );
    assert_eq!(
        f.state()
            .stage
            .position_of(&StageWindow::Client(window.clone())),
        Some(Point::from((200, 150))),
        "the settle commit at an unchanged size leaves the window where the drag left it"
    );
    assert_eq!(
        f.state().stage.restore_size(&window),
        Some(Size::from((600, 450))),
        "and anchors the restore size to the user's final choice"
    );
}

/// A client dying mid-resize degrades the grab to a pass-through: the cluster
/// cascade stops (a neighbor no longer moves) and release cleans up with no
/// panic.
#[test]
fn client_dead_mid_resize_stops_cascade_and_cleans_up() {
    let mut f = Fixture::with_config(config_resize_binding());
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let gap = f.state().config.snap_gap as i32;

    let id = f.add_client();
    map_window(&mut f, id, "p", (400, 300));
    let primary = window_by_app_id(&mut f, "p").unwrap();
    f.state().map_window(
        StageWindow::Client(primary.clone()),
        Point::from((400, 300)),
        true,
    );

    let nid = f.state().insert_suspended_for_test(
        2,
        Point::from((800 + gap, 300)),
        Size::from((400, 300)),
        "n",
        "N",
    );

    let cluster = f.state().cluster_snapshot_for_resize(
        &StageWindow::Client(primary.clone()),
        xdg_toplevel::ResizeEdge::Right,
    );
    install_client_resize_grab(
        &mut f,
        &primary,
        xdg_toplevel::ResizeEdge::Right,
        pt(800.0, 450.0),
        out,
        cluster,
    );

    // First tick cascades the downstream neighbor.
    motion(&mut f, pt(900.0, 450.0));
    let n = f.state().find_suspended(nid).unwrap();
    let after_first = f
        .state()
        .stage
        .position_of(&StageWindow::Suspended(n.clone()))
        .unwrap();
    assert_eq!(
        after_first,
        Point::from((900 + gap, 300)),
        "precondition: the live cascade shifted the neighbor"
    );

    f.kill_client(id);
    f.pump(3);

    // The primary is dead → the grab is a pass-through and the cascade stops.
    motion(&mut f, pt(1000.0, 450.0));
    let after_death = f
        .state()
        .stage
        .position_of(&StageWindow::Suspended(n.clone()))
        .unwrap();
    assert_eq!(
        after_death, after_first,
        "a dead primary stops the cascade — the neighbor no longer moves"
    );

    release(&mut f);
    assert!(
        !f.state().seat.get_pointer().unwrap().is_grabbed(),
        "release cleans up the pass-through grab"
    );

    f.state().dismiss_suspended(nid);
}

/// Map a client at (400, 300), fit it, and frame the viewport on it. Returns
/// the client id, its surface, the server-side window, and a grab point inside
/// the window's right edge.
fn fitted_client(
    f: &mut Fixture,
) -> (
    super::client::ClientId,
    WlClientSurface,
    Window,
    Point<f64, Logical>,
) {
    // Moving the camera seeds a per-output blur generation that only clears on
    // output disconnect, so it can't return to the construction baseline.
    f.skip_baseline_check();
    origin_view(f);
    let id = f.add_client();
    let csurface = map_window(f, id, "c", (400, 300));
    let window = window_by_app_id(f, "c").unwrap();
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((400, 300)),
        true,
    );
    let grab_at = fit_and_frame(f, &window, id);
    assert!(
        client_sees_maximized(f, id, &csurface),
        "precondition: the fit told the client it is maximized"
    );
    (id, csurface, window, grab_at)
}

/// Resizing a fitted window clears the compositor's fit state, so the configure
/// that starts the resize has to clear the client's `Maximized` too. A client
/// left holding it has a dead restore button: the `unmaximize_request` that
/// button dispatches finds no fit left and `unfit_window` drops it silently.
/// The pointer arm here; `gesture_resize.rs` covers the trackpad and touch
/// arms, so the four cannot diverge unnoticed.
#[test]
fn pointer_resize_of_a_fitted_window_clears_the_client_maximized_state() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let (id, csurface, window, grab_at) = fitted_client(&mut f);

    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    assert!(
        f.state().start_compositor_resize_with_edge(
            &pointer, &window, grab_at, BTN_LEFT, serial, None, false,
        ),
        "the pointer resize grab was installed"
    );
    motion(&mut f, grab_at + pt(100.0, 0.0));
    f.double_roundtrip(id);

    assert!(
        !client_sees_maximized(&mut f, id, &csurface),
        "the pointer resize told the client it is no longer maximized"
    );
    release(&mut f);
}

/// The fourth arm: a CSD client dragging its own border sends
/// `xdg_toplevel.resize`, which clears fit state like every other arm and so
/// owes the client the same `Maximized` clear — otherwise the window it just
/// dragged still shows a restore button that does nothing.
#[test]
fn client_resize_request_on_a_fitted_window_clears_the_client_maximized_state() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let (id, csurface, window, grab_at) = fitted_client(&mut f);
    let ssurface = server_surface(&window);

    // A press over the window installs smithay's `ClickGrab` with the surface
    // as its focus — the pointer grab `check_grab` gates the request on.
    let pointer = f.state().seat.get_pointer().unwrap();
    let focus = Some((FocusTarget(ssurface.clone()), Point::from((0.0, 0.0))));
    pointer.motion(
        f.state(),
        focus,
        &MotionEvent {
            location: grab_at,
            serial: SERIAL_COUNTER.next_serial(),
            time: 0,
        },
    );
    pointer.button(
        f.state(),
        &ButtonEvent {
            button: BTN_LEFT,
            state: ButtonState::Pressed,
            serial: SERIAL_COUNTER.next_serial(),
            time: 0,
        },
    );

    f.client(id).window(&csurface).resize(
        wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge::Right,
        1,
    );
    f.double_roundtrip(id);
    assert!(
        matches!(resize_state(&ssurface), ResizeState::Resizing { .. }),
        "precondition: the request installed the compositor's resize grab"
    );

    motion(&mut f, grab_at + pt(100.0, 0.0));
    f.double_roundtrip(id);

    assert!(
        !client_sees_maximized(&mut f, id, &csurface),
        "the client's own resize request told it it is no longer maximized"
    );
    release(&mut f);
}

/// A resize that cannot start leaves the window untouched, per
/// `start_compositor_resize_with_edge`'s bail-before-mutation contract: no
/// fit clear, no `ResizeState`, no `Resizing` on the toplevel, no grab
/// cursor, no grab installed.
#[test]
fn a_resize_that_cannot_start_touches_nothing() {
    // No output at all, so `active_output()` is `None` — the bail furthest down
    // the start path, past everything it would otherwise have written.
    let mut f = Fixture::new();
    let id = f.add_client();
    map_window(&mut f, id, "c", (400, 300));
    let window = window_by_app_id(&mut f, "c").expect("mapped with no output attached");
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((400, 300)),
        true,
    );
    let ssurface = server_surface(&window);
    f.state().stage.set_fit(&window, Size::from((400, 300)));

    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    assert!(
        !f.state().start_compositor_resize_with_edge(
            &pointer,
            &window,
            pt(700.0, 450.0),
            BTN_LEFT,
            serial,
            None,
            false,
        ),
        "with no output there is nothing to anchor a resize against"
    );

    assert!(
        f.state().stage.is_fit(&window),
        "the fit state survives a resize that never started"
    );
    assert!(
        with_states(&ssurface, |states| states
            .data_map
            .get::<RefCell<ResizeState>>()
            .is_none_or(|cell| matches!(*cell.borrow(), ResizeState::Idle))),
        "no ResizeState was seeded"
    );
    assert!(
        !window
            .toplevel()
            .unwrap()
            .with_pending_state(|s| s.states.contains(xdg_toplevel::State::Resizing)),
        "the client was never told it is resizing"
    );
    assert!(
        !f.state().cursor.grab_cursor,
        "the cursor was not handed to a resize that never started"
    );
    assert!(
        !f.state().seat.get_pointer().unwrap().is_grabbed(),
        "no grab was installed"
    );
}

/// A pinned move that cannot start reports it, rather than leaving the caller
/// to assume a grab exists. The trackpad's pinned arm keys its raise + focus
/// and its `SwipeMove` latch off exactly this.
#[test]
fn a_pinned_move_on_an_unpinned_window_reports_failure() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    map_window(&mut f, id, "c", (400, 300));
    let window = window_by_app_id(&mut f, "c").unwrap();
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((400, 300)),
        true,
    );
    assert!(
        !f.state().stage.is_pinned(&window),
        "precondition: the window is a plain canvas window"
    );

    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    assert!(
        !f.state()
            .start_pinned_move(&pointer, &window, pt(600.0, 450.0), BTN_LEFT, serial),
        "there is no pin site to move the window within"
    );
    assert!(
        !f.state().seat.get_pointer().unwrap().is_grabbed(),
        "no grab was installed"
    );
}

/// Press the left button over `window` at `loc`, which installs smithay's
/// `ClickGrab` with the window's surface as its focus — the pointer grab
/// `check_grab` gates a client's own `xdg_toplevel.resize` on.
fn press_over(f: &mut Fixture, window: &Window, loc: Point<f64, Logical>) {
    let ssurface = server_surface(window);
    let pointer = f.state().seat.get_pointer().unwrap();
    let focus = Some((FocusTarget(ssurface), Point::from((0.0, 0.0))));
    pointer.motion(
        f.state(),
        focus,
        &MotionEvent {
            location: loc,
            serial: SERIAL_COUNTER.next_serial(),
            time: 0,
        },
    );
    pointer.button(
        f.state(),
        &ButtonEvent {
            button: BTN_LEFT,
            state: ButtonState::Pressed,
            serial: SERIAL_COUNTER.next_serial(),
            time: 0,
        },
    );
}

/// Config pinning `pin` to the screen at a known size, so the pinned half of
/// the entry-point tests has a pin site to anchor against.
fn config_pinned() -> Config {
    Config::from_toml(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [400, 300]
"#,
    )
    .unwrap()
}

/// The whole invariant the pointer entry point establishes, plain and pinned.
/// The four entry points each hand-roll it, so each one is pinned in full here
/// or in `gesture_resize.rs` — `initial_screen_pos` above all, the field that
/// decides whether the commit-time reposition moves a canvas location or a pin
/// site.
#[test]
fn pointer_resize_entry_establishes_the_whole_resize_invariant() {
    {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let id = f.add_client();
        map_window(&mut f, id, "c", (400, 300));
        let window = window_by_app_id(&mut f, "c").unwrap();
        f.state().map_window(
            StageWindow::Client(window.clone()),
            Point::from((400, 300)),
            true,
        );
        // Both memberships set, so clearing either is observable.
        seed_fit_and_fill(&mut f, &window);

        let pointer = f.state().seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        assert!(f.state().start_compositor_resize_with_edge(
            &pointer,
            &window,
            pt(790.0, 450.0),
            BTN_LEFT,
            serial,
            Some(xdg_toplevel::ResizeEdge::Right),
            false,
        ));

        assert_resize_entered(
            &mut f,
            &window,
            xdg_toplevel::ResizeEdge::Right,
            Size::from((400, 300)),
            None,
        );
        release(&mut f);
    }

    {
        let mut f = Fixture::with_config(config_pinned());
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let id = f.add_client();
        map_window(&mut f, id, "pin", (400, 300));
        let window = window_by_app_id(&mut f, "pin").unwrap();
        let site = f.state().stage.pin_of(&window).unwrap().screen_pos;
        seed_fit_and_fill(&mut f, &window);

        let pointer = f.state().seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        assert!(f.state().start_compositor_resize_with_edge(
            &pointer,
            &window,
            pt(site.x as f64 + 390.0, site.y as f64 + 150.0),
            BTN_LEFT,
            serial,
            Some(xdg_toplevel::ResizeEdge::Right),
            false,
        ));

        assert_resize_entered(
            &mut f,
            &window,
            xdg_toplevel::ResizeEdge::Right,
            Size::from((400, 300)),
            Some(site),
        );
        release(&mut f);
    }
}

/// The same invariant through the client's own `xdg_toplevel.resize`. This arm
/// interleaves the pieces differently from the other three — the pin lookup
/// sits between the membership clear and the `ResizeState` seed — so what it
/// ends up writing is worth pinning independently.
#[test]
fn client_resize_request_establishes_the_whole_resize_invariant() {
    {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let id = f.add_client();
        let csurface = map_window(&mut f, id, "c", (400, 300));
        let window = window_by_app_id(&mut f, "c").unwrap();
        f.state().map_window(
            StageWindow::Client(window.clone()),
            Point::from((400, 300)),
            true,
        );
        // Both memberships set, so clearing either is observable.
        seed_fit_and_fill(&mut f, &window);

        press_over(&mut f, &window, pt(790.0, 450.0));
        f.client(id).window(&csurface).resize(
            wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge::Right,
            1,
        );
        f.double_roundtrip(id);

        assert_resize_entered(
            &mut f,
            &window,
            xdg_toplevel::ResizeEdge::Right,
            Size::from((400, 300)),
            None,
        );
        release(&mut f);
    }

    {
        let mut f = Fixture::with_config(config_pinned());
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let id = f.add_client();
        let csurface = map_window(&mut f, id, "pin", (400, 300));
        let window = window_by_app_id(&mut f, "pin").unwrap();
        let site = f.state().stage.pin_of(&window).unwrap().screen_pos;
        seed_fit_and_fill(&mut f, &window);

        press_over(
            &mut f,
            &window,
            pt(site.x as f64 + 390.0, site.y as f64 + 150.0),
        );
        f.client(id).window(&csurface).resize(
            wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge::Right,
            1,
        );
        f.double_roundtrip(id);

        assert_resize_entered(
            &mut f,
            &window,
            xdg_toplevel::ResizeEdge::Right,
            Size::from((400, 300)),
            Some(site),
        );
        release(&mut f);
    }
}
