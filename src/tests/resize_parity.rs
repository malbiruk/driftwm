//! Resize-grab parity: a suspended stand-in and a client window flow through
//! the one unified [`ResizeGrab`], so a stand-in resize gains the client's
//! constraint floor, output-edge clamp, and cursor reset, while the client's
//! configure/commit settle and the shared interactive-resize blur bump keep
//! their existing behavior.
//!
//! Client grabs are installed directly via the public `ResizeGrab` struct
//! literal (seeding `ResizeState::Resizing` the way the xdg-shell handler
//! would, so `handle_resize_commit` reposition/settle runs instead of
//! early-returning). Suspended grabs run through the real `try_suspended_button`
//! button path so the cursor and cluster install exactly as production drives
//! them — the single-motion precedent in `suspended.rs`.

use std::cell::RefCell;

use smithay::backend::input::ButtonState;
use smithay::desktop::Window;
use smithay::input::keyboard::ModifiersState;
use smithay::input::pointer::{
    ButtonEvent, CursorIcon, CursorImageStatus, Focus, GrabStartData, MotionEvent,
};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};
use smithay::wayland::compositor::with_states;

use driftwm::config::{BTN_LEFT, Config};
use driftwm::layout::snap::SnapState;

use crate::grabs::{ResizeGrab, ResizeState, SizeConstraints};
use crate::state::{ClusterMember, ClusterResizeSnapshot, StageWindow};

use super::{Fixture, adopt_last_configure, map_window, server_surface, window_by_app_id};

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

/// Install a live client [`ResizeGrab`] over `window`, seeding the
/// `ResizeState::Resizing` the xdg-shell handler would have set so
/// `handle_resize_commit` runs its reposition/settle logic. `start` is the
/// canvas-space grab origin; the size delta is measured from there.
fn install_client_resize_grab(
    f: &mut Fixture,
    window: &Window,
    edges: xdg_toplevel::ResizeEdge,
    start: Point<f64, Logical>,
    output: Output,
    cluster: ClusterResizeSnapshot,
) {
    let initial_window_location = f
        .state()
        .stage
        .position_of(&StageWindow::Client(window.clone()))
        .unwrap();
    let initial_window_size = window.geometry().size;

    let surface = server_surface(window);
    with_states(&surface, |states| {
        states
            .data_map
            .get_or_insert(|| RefCell::new(ResizeState::Idle))
            .replace(ResizeState::Resizing {
                edges,
                initial_window_location,
                initial_window_size,
                initial_screen_pos: None,
                last_committed_size: initial_window_size,
            });
    });

    let grab = ResizeGrab {
        start_data: GrabStartData {
            focus: None,
            button: BTN_LEFT,
            location: start,
        },
        target: ClusterMember::Client(window.clone()),
        edges,
        initial_window_location,
        initial_window_size,
        last_window_size: initial_window_size,
        output,
        last_clamped_location: start,
        snap: SnapState::default(),
        constraints: SizeConstraints::for_window(window),
        cluster_resize: cluster,
        pinned_initial_screen_pos: None,
        touch_start: None,
        touch_slots: 0,
        locked_ratio: None,
    };

    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    pointer.set_grab(f.state(), grab, serial, Focus::Clear);
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
