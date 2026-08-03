//! Exact configure sequences as the client sees them — the desync class where
//! a toolkit acks one configure while the compositor already believes another.

use driftwm::canvas::Chrome;
use driftwm::config::{Action, Config, DecorationMode, Direction};
use smithay::input::pointer::MotionEvent;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};
use smithay::wayland::shell::wlr_layer::Layer;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1;

use crate::ipc::protocol::{Request, Response, WindowSelector};
use crate::state::StageWindow;

use super::{Fixture, TICK, adopt_last_configure, client_sees_maximized, settle, window_by_app_id};

/// Map one toplevel with a buffer at `size`, settle, and drain the configure
/// cursor so tests only see what happens next.
fn map_settled(
    f: &mut Fixture,
    id: super::client::ClientId,
    app_id: &str,
    size: (u16, u16),
) -> wayland_client::protocol::wl_surface::WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id(app_id);
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.client(id).window(&surface).format_recent_configures();
    surface
}

/// Camera at the canvas origin, zoom 1: canvas == screen.
fn origin_view(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });
}

fn pt(x: f64, y: f64) -> Point<f64, Logical> {
    Point::from((x, y))
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

/// End the gesture the way `on_gesture_swipe_end` does — there's no button to
/// release on a gesture.
fn end_swipe(f: &mut Fixture) {
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    pointer.unset_grab(f.state(), serial, 0);
}

/// A window snapped only to a suspended stand-in reflows when it grows into it,
/// exactly as it would beside a live window: the grow-reflow neighbor set counts
/// stand-ins, so a font-bump next to a stand-in relocates instead of overlapping.
#[test]
fn grow_reflows_off_a_stand_in_neighbor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // A settled live window "a"; pin the camera (mapping pans it) and park it.
    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let a_elem = StageWindow::Client(a.clone());
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state().refresh_stable_snap_rect(&a_elem);

    // A stand-in as "a"'s only neighbor, gap-adjacent to its right edge and
    // y-overlapping (the reflow's "was snapped" anchor precondition).
    let a_frame = f.state().visual_frame_rect(&a_elem).unwrap();
    let gap = f.state().config.snap_gap as i32;
    let bw = f.state().default_border_width();
    let sx = a_frame.x_high as i32 + gap + bw;
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((sx, 300)),
        Size::from((300, 400)),
        "s",
        "S",
    );
    let standin = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let standin_frame = f.state().visual_frame_rect(&standin).unwrap();

    let before = f.state().stage.position_of(&a_elem).unwrap();

    // "a" grows past the stand-in (a font bump would do this): a spontaneous
    // CSD resize larger than its settled width, colliding with the neighbor.
    let win = f.client(id).window(&a_surface);
    win.set_size(1000, 600);
    win.attach_new_buffer();
    win.commit();
    f.double_roundtrip(id);

    // The grow reflowed "a" off the stand-in instead of overlapping it.
    let after = f.state().stage.position_of(&a_elem).unwrap();
    assert_ne!(after, before, "the grown window relocated off the stand-in");
    let a_after = f.state().visual_frame_rect(&a_elem).unwrap();
    let overlaps = a_after.x_low < standin_frame.x_high
        && standin_frame.x_low < a_after.x_high
        && a_after.y_low < standin_frame.y_high
        && standin_frame.y_low < a_after.y_high;
    assert!(!overlaps, "and no longer overlaps the stand-in frame");

    f.state().dismiss_suspended(sid);
}

#[test]
fn initial_burst_is_a_single_configure() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.double_roundtrip(id);

    // Exactly one configure before the first ack — a second uncommitted
    // configure in the initial burst is what desyncs size-tracking toolkits.
    let window = f.client(id).window(&surface);
    assert_eq!(window.recent_configures().count(), 1);
}

#[test]
fn fullscreen_reassert_is_idempotent() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "fs", (800, 600));
    let window = f.client(id).window(&surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);
    f.client(id).window(&surface).format_recent_configures();

    // Toolkits re-assert fullscreen on focus changes; the answer must be the
    // same fullscreen configure again, never an exit/re-enter bounce.
    let window = f.client(id).window(&surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    let configures = window.format_recent_configures();
    for line in configures.lines() {
        assert!(
            line.contains("size: 1920 × 1080") && line.contains("Fullscreen"),
            "re-assert must only repeat the fullscreen configure, got:\n{configures}"
        );
    }
}

#[test]
fn second_fullscreen_displaces_first() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let first = map_settled(&mut f, id, "first", (800, 600));
    let second = map_settled(&mut f, id, "second", (400, 300));

    let window = f.client(id).window(&first);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    f.client(id).window(&first).ack_last_and_commit();
    f.double_roundtrip(id);
    f.client(id).window(&first).format_recent_configures();

    let window = f.client(id).window(&second);
    window.set_fullscreen(None);
    f.double_roundtrip(id);

    // The displaced window is restored to its pre-fullscreen size...
    let first_configures = f.client(id).window(&first).format_recent_configures();
    assert!(
        first_configures.contains("size: 800 × 600") && !first_configures.contains("Fullscreen"),
        "displaced window must get its windowed configure back, got:\n{first_configures}"
    );
    // ...in a single configure whose strip rides the exit — no stale Activated
    // on it, and no separate back-to-back deactivate configure trailing it.
    assert_eq!(
        first_configures.lines().count(),
        1,
        "displaced window must get exactly one configure, got:\n{first_configures}"
    );
    assert!(
        !first_configures.contains("Activated"),
        "displaced window's exit configure must carry the deactivate, got:\n{first_configures}"
    );
    // ...and the new one owns the output.
    let second_configures = f.client(id).window(&second).format_recent_configures();
    assert!(
        second_configures.contains("size: 1920 × 1080") && second_configures.contains("Fullscreen"),
        "takeover window must get the fullscreen configure, got:\n{second_configures}"
    );
    let mapped = window_by_app_id(&mut f, "second").unwrap();
    assert_eq!(
        f.state().stage.fullscreen_output_of(&mapped),
        Some("HEADLESS-1")
    );
}

#[test]
fn fit_round_trip_restores_exact_size() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // Even size on purpose: odd fitted sizes hit a known pre-existing 1px
    // truncation quirk that is not under test here.
    let surface = map_settled(&mut f, id, "fit", (800, 600));
    let window = window_by_app_id(&mut f, "fit").unwrap();

    f.state().toggle_fit_window(&window);
    f.double_roundtrip(id);
    let client_window = f.client(id).window(&surface);
    let fit_configures = client_window.format_recent_configures();
    assert!(
        fit_configures.contains("size:") && !fit_configures.contains("size: 800 × 600"),
        "fit must configure a new (viewport-fitted) size, got:\n{fit_configures}"
    );
    // Commit at the fitted size so the exit path restores from a fit-sized
    // window, as a real client would.
    let (w, h) = client_window.configures_received.last().unwrap().1.size;
    let client_window = f.client(id).window(&surface);
    client_window.set_size(w as u16, h as u16);
    client_window.ack_last_and_commit();
    f.double_roundtrip(id);

    f.state().toggle_fit_window(&window);
    f.double_roundtrip(id);
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 800 × 600"),
        "fit exit must restore the exact pre-fit size, got:\n{configures}"
    );
    assert!(!f.state().stage.is_fit(&window));
}

/// A fit toggled off again before the client ever acks the enter-fit configure
/// makes the exit configure re-send the size the client already has — no
/// commit with a changed size ever follows to complete the exit's recenter, so
/// the entry must not be left owed forever. Left stranded, a later drag (which
/// never touches `pending_recenter`) followed by a resize (whose changed-size
/// commit does trip the stale entry) would teleport the window back toward the
/// fit's center, discarding the drag.
#[test]
fn fast_fit_unfit_does_not_teleport_window_on_next_resize() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);

    // Fit, then unfit before any ack lands: the exit configure re-sends the
    // 800×600 the client already has.
    f.state().toggle_fit_window(&window);
    f.state().toggle_fit_window(&window);
    assert!(!f.state().stage.is_fit(&window));
    f.double_roundtrip(id);

    // The client draws the same size back, as a cell-quantized terminal or a
    // fixed-size dialog would — a genuine equal-size settle, not a straggler.
    adopt_last_configure(&mut f, id, &surface);

    // The user drags the window elsewhere.
    let pos = f.state().stage.position_of(&window).unwrap();
    let center = pt(pos.x as f64 + 400.0, pos.y as f64 + 300.0);
    assert!(f.state().try_start_gesture_move(center, false));
    motion(&mut f, center + pt(100.0, 30.0));
    end_swipe(&mut f);
    let dragged_to = f.state().stage.position_of(&window).unwrap();
    assert_eq!(
        dragged_to,
        pos + Point::from((100, 30)),
        "precondition: the drag landed at its natural destination"
    );

    // The user then resizes it from its right edge.
    let grab_at = pt(dragged_to.x as f64 + 700.0, dragged_to.y as f64 + 300.0);
    assert!(f.state().try_start_gesture_resize(grab_at, false));
    motion(&mut f, grab_at + pt(100.0, 0.0));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    end_swipe(&mut f);

    assert_eq!(
        f.state().stage.position_of(&window),
        Some(dragged_to),
        "the resize must not teleport the window back toward the unfit's center"
    );
}

/// `enter_fullscreen` deliberately preserves fit membership, so a window fit
/// then fullscreened stays fit underneath. Exiting fullscreen without an ack
/// leaves a `pending_recenter` owed (the fullscreen-sized commit differs from
/// the fit-era `saved_size` it restores to); an unfit dispatched right after,
/// before that recenter ever settles, must drop it — not just skip inserting
/// its own. A window mapped at exactly the output's logical size makes the
/// fit's saved (pre-fit) size and the still-fullscreen-sized current geometry
/// coincide, so the unfit hits the equal-size branch that settles in place
/// without ever registering a differing-size commit to complete on. Left
/// untouched, the fullscreen exit's stale entry survives (a drag doesn't
/// touch `pending_recenter` either) and fires on the next differing-size
/// commit — a resize — recentering the window back toward the fullscreen's
/// pre-exit center and discarding the drag in between.
#[test]
fn unfit_after_fullscreen_exit_drops_the_stale_recenter_so_the_next_resize_does_not_teleport() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // Fullscreen below moves the camera, which seeds a per-output blur
    // generation that only clears on output disconnect, so it can never
    // return to the pre-output baseline.
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    // Mapped at exactly the output's logical size — what makes the fit's
    // saved (pre-fit) size and the fullscreen-sized geometry coincide later.
    let surface = map_settled(&mut f, id, "a", (1920, 1080));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((0, 0)), false);

    // Fit, and adopt the fit size as a real client would — enter_fullscreen
    // below reads the fit-era geometry as its own return size.
    f.state().toggle_fit_window(&window);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    assert!(f.state().stage.is_fit(&window), "precondition: fit");

    // Fullscreen, and adopt the viewport size.
    let cw = f.client(id).window(&surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    assert!(
        f.state().stage.is_fullscreen(&window),
        "precondition: fullscreen"
    );

    // Exit fullscreen but never ack the restore configure: a recenter is left
    // owed, and fit membership survives underneath.
    let cw = f.client(id).window(&surface);
    cw.unset_fullscreen();
    f.double_roundtrip(id);
    let root = super::server_surface(&window);
    assert!(
        f.state().pending_recenter.contains_key(&root.id()),
        "precondition: the fullscreen exit left a recenter owed"
    );
    assert!(
        f.state().stage.is_fit(&window),
        "precondition: fit membership survives fullscreen"
    );

    // Unfit before that recenter ever settles: the fit's saved (pre-fit) size
    // and the window's current (still fullscreen-sized) geometry are both
    // 1920×1080, so this hits the equal-size branch.
    f.state().toggle_fit_window(&window);
    assert!(!f.state().stage.is_fit(&window));
    assert!(
        !f.state().pending_recenter.contains_key(&root.id()),
        "the equal-size branch settles in place, so it must drop the owed recenter"
    );
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);

    // The user drags the window elsewhere.
    let pos = f.state().stage.position_of(&window).unwrap();
    let center = pt(pos.x as f64 + 960.0, pos.y as f64 + 540.0);
    assert!(f.state().try_start_gesture_move(center, false));
    motion(&mut f, center + pt(100.0, 30.0));
    end_swipe(&mut f);
    let dragged_to = f.state().stage.position_of(&window).unwrap();
    assert_eq!(
        dragged_to,
        pos + Point::from((100, 30)),
        "precondition: the drag landed at its natural destination"
    );

    // The user then resizes it from its right edge.
    let grab_at = pt(dragged_to.x as f64 + 1900.0, dragged_to.y as f64 + 540.0);
    assert!(f.state().try_start_gesture_resize(grab_at, false));
    motion(&mut f, grab_at + pt(100.0, 0.0));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    end_swipe(&mut f);

    assert_eq!(
        f.state().stage.position_of(&window),
        Some(dragged_to),
        "the resize must not teleport the window back toward the fullscreen exit's stale center"
    );
}

/// The fill twin of the test above: `unfill_window`'s equal-size branch must
/// drop a recenter already owed, not merely skip inserting its own.
///
/// The two saved sizes disagree because they are captured in different eras:
/// the fill records the pre-fill size, the fullscreen entry the filled one. The
/// exit therefore restores a size the client does not have — it is still
/// committing fullscreen-sized frames — and leaves a recenter owed, while the
/// unfill (pre-fill size and still-fullscreen-sized geometry both being the
/// output's 1920×1080) settles in place through the equal-size branch. Left
/// untouched, that stale entry fires on the next differing-size commit (a
/// resize) and discards the drag in between.
///
/// The resize interleave is what makes this route distinct from the twin below
/// rather than what makes it work: the fill lands between a resize release and
/// its settle commit, and the settle re-anchors `restore_size` under it.
#[test]
fn unfill_after_fullscreen_exit_drops_the_stale_recenter_so_the_next_resize_does_not_teleport() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // Fullscreen below moves the camera, which seeds a per-output blur
    // generation that only clears on output disconnect, so it can never
    // return to the pre-output baseline.
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    // Mapped at exactly the output's logical size — what makes the fill's
    // saved (pre-fill) size and the fullscreen-sized geometry coincide later.
    let surface = map_settled(&mut f, id, "a", (1920, 1080));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((0, 0)), false);

    // Resize from the right edge and release, but hold the client's final
    // commit back: the settle that re-anchors `restore_size` is still owed.
    let grab_at = pt(1900.0, 540.0);
    assert!(f.state().try_start_gesture_resize(grab_at, false));
    motion(&mut f, grab_at + pt(-100.0, 0.0));
    end_swipe(&mut f);

    // Fill into that gap: the fill's restore point is still the pre-resize
    // 1920×1080.
    f.state().toggle_fill_window(&window);
    assert!(
        f.state().stage.is_fill(&window),
        "precondition: the fill ran"
    );

    // The client's next commit adopts the fill *and* settles the resize, which
    // anchors `restore_size` to the size the user ended on.
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    assert_eq!(
        f.state().stage.restore_size(&window),
        Some(Size::from((1896, 1056))),
        "precondition: the settle re-anchored restore_size to the filled size"
    );

    // Fullscreen, and adopt the viewport size.
    let cw = f.client(id).window(&surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    assert!(
        f.state().stage.is_fullscreen(&window),
        "precondition: fullscreen"
    );
    assert!(
        f.state().stage.is_fill(&window),
        "precondition: fill membership survives fullscreen"
    );

    // Exit fullscreen but never ack the restore configure: it restores the
    // filled 1896×1056, which the client does not have, so a recenter is left
    // owed.
    let cw = f.client(id).window(&surface);
    cw.unset_fullscreen();
    f.double_roundtrip(id);
    let root = super::server_surface(&window);
    assert!(
        f.state().pending_recenter.contains_key(&root.id()),
        "precondition: the fullscreen exit left a recenter owed"
    );

    // Unfill before that recenter ever settles: the fill's saved (pre-fill)
    // size and the window's current (still fullscreen-sized) geometry are both
    // 1920×1080, so this hits the equal-size branch.
    f.state().toggle_fill_window(&window);
    assert!(!f.state().stage.is_fill(&window));
    assert!(
        !f.state().pending_recenter.contains_key(&root.id()),
        "the equal-size branch settles in place, so it must drop the owed recenter"
    );
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);

    // The user drags the window elsewhere.
    let pos = f.state().stage.position_of(&window).unwrap();
    let center = pt(pos.x as f64 + 960.0, pos.y as f64 + 540.0);
    assert!(f.state().try_start_gesture_move(center, false));
    motion(&mut f, center + pt(100.0, 30.0));
    end_swipe(&mut f);
    let dragged_to = f.state().stage.position_of(&window).unwrap();
    assert_eq!(
        dragged_to,
        pos + Point::from((100, 30)),
        "precondition: the drag landed at its natural destination"
    );

    // The user then resizes it from its right edge.
    let grab_at = pt(dragged_to.x as f64 + 1900.0, dragged_to.y as f64 + 540.0);
    assert!(f.state().try_start_gesture_resize(grab_at, false));
    motion(&mut f, grab_at + pt(100.0, 0.0));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    end_swipe(&mut f);

    assert_eq!(
        f.state().stage.position_of(&window),
        Some(dragged_to),
        "the resize must not teleport the window back toward the fullscreen exit's stale center"
    );
}

/// A resize settle only has to hold the *opposite* edge still across one size
/// change. Anything that placed the window between the release and the settling
/// commit — here a fill, but equally a fit, an exit, an IPC move or a bookmark
/// jump — owns the position, and the settle must compensate from there rather
/// than from where the grab started.
#[test]
fn a_fill_between_a_resize_release_and_its_settle_keeps_its_placement() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "a", (1920, 1080));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((0, 0)), false);

    // Resize from the right edge and release, but hold the client's final
    // commit back: the settle is still owed.
    let grab_at = pt(1900.0, 540.0);
    assert!(f.state().try_start_gesture_resize(grab_at, false));
    motion(&mut f, grab_at + pt(-100.0, 0.0));
    end_swipe(&mut f);

    // Fill into that gap.
    f.state().toggle_fill_window(&window);
    assert!(
        f.state().stage.is_fill(&window),
        "precondition: the fill ran"
    );

    // The client's next commit adopts the fill *and* settles the resize.
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);

    let filled = Point::from((12, 12));
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(filled),
        "the settle must compensate from the filled position, not restore the grab start"
    );
    // The cache is the other half: the settle refreshes it, so a wrong position
    // there and a wrong position on the stage agree with each other and nothing
    // downstream can tell.
    let root = super::server_surface(&window);
    let cached = *f.state().stable_snap_rects.get(&root.id()).unwrap();
    assert_eq!(
        (cached.x_low, cached.y_low, cached.x_high, cached.y_high),
        (12.0, 12.0, 1908.0, 1068.0),
        "and the refreshed snap rect is the fill's frame"
    );
}

/// The same interleave on a top-left drag, which is the only shape that reaches
/// the compensation at all — the right-edge case above leaves both arms unfired.
/// A placement that changed the size as well as the position owns both, so the
/// held-edge delta is not the resize's to apply: measured against the fill's
/// size it is the whole width of the screen.
#[test]
fn a_fill_between_a_top_left_resize_release_and_its_settle_keeps_its_placement() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "a", (500, 400));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((300, 200)), false);

    // Drag the top-left corner outward and let the client commit the dragged
    // size, so the settle's `last_committed_size` is the size the hand ended on
    // rather than the one it started from.
    let grab_at = pt(320.0, 220.0);
    assert!(f.state().try_start_gesture_resize(grab_at, false));
    motion(&mut f, grab_at + pt(-100.0, -100.0));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(Point::from((200, 100))),
        "precondition: the drag held the bottom-right corner still"
    );
    end_swipe(&mut f);

    // Fill into the gap before the client's settling commit.
    f.state().toggle_fill_window(&window);
    assert!(
        f.state().stage.is_fill(&window),
        "precondition: the fill ran"
    );

    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);

    let filled = Point::from((12, 12));
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(filled),
        "the fill owns both the size and the position, so the settle compensates \
         for nothing"
    );
    let root = super::server_surface(&window);
    let cached = *f.state().stable_snap_rects.get(&root.id()).unwrap();
    assert_eq!(
        (cached.x_low, cached.y_low, cached.x_high, cached.y_high),
        (12.0, 12.0, 1908.0, 1068.0),
        "and the refreshed snap rect is the fill's frame"
    );
}

/// A fullscreen taken in the same gap. Its placement is the output itself, so a
/// settle that shifts it by the difference between the drag's size and the
/// viewport's drags the fullscreen window off the screen it is meant to fill.
#[test]
fn a_fullscreen_between_a_resize_release_and_its_settle_stays_on_its_output() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // Fullscreen moves the camera, which seeds a per-output blur generation
    // that only clears on output disconnect.
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "a", (500, 400));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((300, 200)), false);

    let grab_at = pt(320.0, 220.0);
    assert!(f.state().try_start_gesture_resize(grab_at, false));
    motion(&mut f, grab_at + pt(-100.0, -100.0));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    end_swipe(&mut f);

    // Fullscreen before the settling commit, then let the client's adoption of
    // the fullscreen configure be that commit.
    let cw = f.client(id).window(&surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(Point::from((0, 0))),
        "precondition: the fullscreen placed the window on the output origin"
    );

    adopt_last_configure(&mut f, id, &surface);
    assert!(
        f.state().stage.is_fullscreen(&window),
        "precondition: the client adopted the fullscreen configure"
    );
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(Point::from((0, 0))),
        "the fullscreen owns the geometry, so the settle leaves the window on \
         the output instead of shifting it by the viewport's size"
    );
}

/// The same equal-size branch, reached with nothing but fill → fullscreen →
/// exit → unfill — the shorter and likelier production route.
///
/// Nothing here is ever acked, so the client sits at its pre-fill size
/// throughout. The fill saves that size while the fullscreen entry saves the
/// filled one, so the two disagree with no resize settle to split them: the
/// exit restores the filled size, which the client does not have, and leaves a
/// recenter owed, while the unfill restores the size the client has been
/// sitting at all along and settles in place.
#[test]
fn unfill_after_a_plain_fullscreen_exit_drops_the_stale_recenter() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // Fullscreen below moves the camera, which seeds a per-output blur
    // generation that only clears on output disconnect, so it can never
    // return to the pre-output baseline.
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    // Small enough that the fill genuinely grows it, and the size the client
    // stays at for the whole sequence.
    let surface = map_settled(&mut f, id, "a", (400, 80));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((0, 0)), false);

    // Fill, and leave the client on its pre-fill size: the fill's saved size is
    // that raw 400×80, while the fullscreen enter below captures the filled one.
    f.state().toggle_fill_window(&window);
    assert!(
        f.state().stage.is_fill(&window),
        "precondition: the fill ran"
    );

    let cw = f.client(id).window(&surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    assert!(
        f.state().stage.is_fullscreen(&window),
        "precondition: fullscreen"
    );
    assert!(
        f.state().stage.is_fill(&window),
        "precondition: fill membership survives fullscreen"
    );

    // Exit fullscreen: it restores the filled size, which the client never
    // acked, so a recenter is left owed.
    let cw = f.client(id).window(&surface);
    cw.unset_fullscreen();
    f.double_roundtrip(id);
    let root = super::server_surface(&window);
    assert!(
        f.state().pending_recenter.contains_key(&root.id()),
        "precondition: the fullscreen exit left a recenter owed"
    );

    // Unfill: the fill's saved 400×80 is the size the client still has, so this
    // hits the equal-size branch.
    f.state().toggle_fill_window(&window);
    assert!(!f.state().stage.is_fill(&window));
    assert!(
        !f.state().pending_recenter.contains_key(&root.id()),
        "the equal-size branch settles in place, so it must drop the owed recenter"
    );
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);

    // The user drags the window elsewhere.
    let pos = f.state().stage.position_of(&window).unwrap();
    let center = pt(pos.x as f64 + 200.0, pos.y as f64 + 40.0);
    assert!(f.state().try_start_gesture_move(center, false));
    motion(&mut f, center + pt(100.0, 30.0));
    end_swipe(&mut f);
    let dragged_to = f.state().stage.position_of(&window).unwrap();
    assert_eq!(
        dragged_to,
        pos + Point::from((100, 30)),
        "precondition: the drag landed at its natural destination"
    );

    // The user then resizes it from its right edge.
    let grab_at = pt(dragged_to.x as f64 + 390.0, dragged_to.y as f64 + 40.0);
    assert!(f.state().try_start_gesture_resize(grab_at, false));
    motion(&mut f, grab_at + pt(100.0, 0.0));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    end_swipe(&mut f);

    assert_eq!(
        f.state().stage.position_of(&window),
        Some(dragged_to),
        "the resize must not teleport the window back toward the fullscreen exit's stale center"
    );
}

/// The fullscreen twin of the two tests above: `exit_fullscreen_on`'s
/// equal-size branch must drop a recenter already owed, not merely skip
/// inserting its own.
///
/// No keybind is involved. A fit the client acks, then fullscreen — the entry
/// saves the fit-era size. The client unmaximizes before acking the fullscreen
/// configure, and that unfit is a differing-size exit, so it registers a
/// recenter, aimed at the center of the *fullscreen* rect the window currently
/// occupies. The client then unfullscreens, still without acking: the geometry
/// it commits is the fit-era size the entry saved, so the exit takes the
/// equal-size branch and restores the position outright. Left untouched, the
/// unfit's recenter survives, lies inert while the committed size is unchanged,
/// then fires on the next differing-size commit — a resize — and discards both
/// the exit's placement and the drag in between.
#[test]
fn fullscreen_exit_drops_the_stale_recenter_so_the_next_resize_does_not_teleport() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // Fullscreen below moves the camera, which seeds a per-output blur
    // generation that only clears on output disconnect, so it can never
    // return to the pre-output baseline.
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);

    // Fit, and adopt the fit size as a real client would — enter_fullscreen
    // below reads the fit-era geometry as its own return size.
    f.state().toggle_fit_window(&window);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    assert!(f.state().stage.is_fit(&window), "precondition: fit");

    // Fullscreen, never acked: the committed geometry stays fit-era.
    let cw = f.client(id).window(&surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    assert!(
        f.state().stage.is_fullscreen(&window),
        "precondition: fullscreen"
    );

    // Unmaximize, still without acking. Fit membership survives fullscreen, so
    // this runs a real unfit, and the pre-fit size it restores differs from the
    // fit-era geometry the client still commits — a recenter is left owed.
    let cw = f.client(id).window(&surface);
    cw.unset_maximized();
    f.double_roundtrip(id);
    let root = super::server_surface(&window);
    assert!(
        !f.state().stage.is_fit(&window),
        "precondition: the unfit ran"
    );
    assert!(
        f.state().pending_recenter.contains_key(&root.id()),
        "precondition: the unfit left a recenter owed"
    );

    // Unfullscreen, still without acking: the fit-era geometry the client
    // commits is the size the entry saved, so this is the equal-size branch.
    let cw = f.client(id).window(&surface);
    cw.unset_fullscreen();
    f.double_roundtrip(id);
    assert!(!f.state().stage.is_fullscreen(&window));
    assert!(
        !f.state().pending_recenter.contains_key(&root.id()),
        "the exit restored the window's position outright, so it must drop the \
         recenter that would later undo it"
    );
    adopt_last_configure(&mut f, id, &surface);

    // The user drags the window elsewhere.
    let pos = f.state().stage.position_of(&window).unwrap();
    let center = pt(pos.x as f64 + 948.0, pos.y as f64 + 528.0);
    assert!(f.state().try_start_gesture_move(center, false));
    motion(&mut f, center + pt(100.0, 30.0));
    end_swipe(&mut f);
    let dragged_to = f.state().stage.position_of(&window).unwrap();
    assert_eq!(
        dragged_to,
        pos + Point::from((100, 30)),
        "precondition: the drag landed at its natural destination"
    );

    // The user then resizes it from its right edge.
    let grab_at = pt(dragged_to.x as f64 + 1890.0, dragged_to.y as f64 + 528.0);
    assert!(f.state().try_start_gesture_resize(grab_at, false));
    motion(&mut f, grab_at + pt(100.0, 0.0));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    end_swipe(&mut f);

    assert_eq!(
        f.state().stage.position_of(&window),
        Some(dragged_to),
        "the resize must not teleport the window back toward the unfit's stale center"
    );
}

/// The fit and fill exits refresh the cached snap rect their own entries
/// overwrote. The fullscreen exit has nothing to refresh: `enter_fullscreen`
/// never caches a rect, so the cached one is still the pre-fullscreen rect the
/// exit hands the window back. Moving the window without settling it is what
/// makes that no-op observable — re-deriving the entry from the restored
/// position is not the exit's business.
#[test]
fn fullscreen_exit_leaves_the_cached_snap_rect_alone() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // Fullscreen below moves the camera, which seeds a per-output blur
    // generation that only clears on output disconnect.
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    let root = super::server_surface(&window);

    // Move without settling: the cache keeps the rect from the initial map.
    f.state()
        .map_window(window.clone(), Point::from((700, 500)), false);
    let cached = *f.state().stable_snap_rects.get(&root.id()).unwrap();
    let live = f
        .state()
        .visual_frame_rect(&StageWindow::Client(window.clone()))
        .unwrap();
    assert_ne!(
        (cached.x_low, cached.y_low),
        (live.x_low, live.y_low),
        "precondition: the cached rect is stale against the live position"
    );

    // Fullscreen and back without ever acking: the geometry the client commits
    // is the size the entry saved, so the exit restores the position outright.
    let cw = f.client(id).window(&surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    assert!(
        f.state().stage.is_fullscreen(&window),
        "precondition: fullscreen"
    );
    let cw = f.client(id).window(&surface);
    cw.unset_fullscreen();
    f.double_roundtrip(id);
    assert!(!f.state().stage.is_fullscreen(&window));

    let after = *f.state().stable_snap_rects.get(&root.id()).unwrap();
    assert_eq!(
        (after.x_low, after.y_low, after.x_high, after.y_high),
        (cached.x_low, cached.y_low, cached.x_high, cached.y_high),
        "the fullscreen exit must leave the cached snap rect as it found it"
    );
}

/// The other side of the test above: a snapped fit *does* cache a rect of its
/// own, so its exit has to put the cache back in agreement with the window it
/// restored. Left alone, the window's cluster identity stays the viewport-sized
/// fit rect it no longer occupies.
#[test]
fn unfit_refreshes_the_snap_rect_its_fit_cached() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // Fit moves the camera, which seeds a per-output blur generation that only
    // clears on output disconnect.
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    let _surface = map_settled(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    let root = super::server_surface(&window);
    let element = StageWindow::Client(window.clone());

    // Snapped fit, never acked: the cache holds the fit rect while the
    // window still commits — and still occupies — its pre-fit size.
    f.state().fit_window_snapped(&window);
    let cached = *f.state().stable_snap_rects.get(&root.id()).unwrap();
    let live = f.state().visual_frame_rect(&element).unwrap();
    assert_ne!(
        (cached.x_high - cached.x_low, cached.y_high - cached.y_low),
        (live.x_high - live.x_low, live.y_high - live.y_low),
        "precondition: the fit cached a rect of its own"
    );

    // Unfit, still unacked: the pre-fit size the exit restores is the size the
    // client has all along, so it settles in place.
    f.state().unfit_window_snapped(&window);

    let after = *f.state().stable_snap_rects.get(&root.id()).unwrap();
    let live = f.state().visual_frame_rect(&element).unwrap();
    assert_eq!(
        (after.x_low, after.y_low, after.x_high, after.y_high),
        (live.x_low, live.y_low, live.x_high, live.y_high),
        "the fit exit must leave the cached rect agreeing with the restored window"
    );
}

/// `unfit_window` maps the window to a location truncated out of the visual
/// center it preserves, and records that center *un-truncated* for the settle
/// to finish on. Re-deriving the recorded center from the mapped location
/// instead loses up to half a pixel per axis, which the settle's own truncation
/// then turns into a whole one: with an odd restore width the two disagree by
/// 1 px.
#[test]
fn unfit_settles_on_the_untruncated_center() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // Fit moves the camera, which seeds a per-output blur generation that only
    // clears on output disconnect.
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    // Odd width: the location the unfit restores to is half a pixel off the
    // center it restores around.
    let surface = map_settled(&mut f, id, "a", (801, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);

    // Fit and adopt: the center the unfit preserves is the fit rect's.
    f.state().toggle_fit_window(&window);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    let fit_pos = f.state().stage.position_of(&window).unwrap();
    let (fit_w, fit_h) = f
        .client(id)
        .window(&surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    let center_x = fit_pos.x as f64 + fit_w as f64 / 2.0;
    let center_y = fit_pos.y as f64 + fit_h as f64 / 2.0;

    // Unfit — the fit-era size the client still commits differs from the 801
    // restore, so a recenter is owed — then let the client settle at an even
    // width, which fires it.
    f.state().toggle_fit_window(&window);
    f.double_roundtrip(id);
    let cw = f.client(id).window(&surface);
    cw.set_size(800, 600);
    cw.attach_new_buffer();
    cw.ack_last_and_commit();
    f.double_roundtrip(id);

    assert_eq!(
        f.state().stage.position_of(&window),
        Some(Point::from((
            (center_x - 400.0) as i32,
            (center_y - 300.0) as i32
        ))),
        "the settle must land on the center the unfit recorded, not on the one \
         its truncated restore location describes"
    );
}

/// Fill membership survives fullscreen deliberately, so the exit has to hand
/// the window back the *filled* rect — position and size together. The size it
/// saves has to come from the same era as the position it saves: a pre-fill
/// `restore_size` paired with the current (filled) location brings the window
/// back shrunken in the filled area's top-left corner, still believing it is
/// filled.
///
/// The unfill afterwards is the other half — the pre-fill rect must still be
/// where it goes home to.
#[test]
fn fullscreen_exit_restores_the_filled_rect_and_unfill_still_goes_home() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    // Fullscreen below moves the camera, which seeds a per-output blur
    // generation that only clears on output disconnect, so it can never
    // return to the pre-output baseline.
    f.skip_baseline_check();
    let id = f.add_client();

    // An ordinary window: clear of `MIN_RESTORE_FLOOR` on both axes, and not the
    // output's own size, so the pre-fill and filled rects can't coincide.
    let surface = map_settled(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();
    let pre_fill_loc = Point::from((200, 150));
    f.state().map_window(window.clone(), pre_fill_loc, false);
    origin_view(&mut f);
    let pre_fill = (pre_fill_loc, Size::from((800, 600)));

    // Fill, and let the client adopt the filled size as a real one would.
    f.state().toggle_fill_window(&window);
    assert!(
        f.state().stage.is_fill(&window),
        "precondition: the fill ran"
    );
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    let filled = (
        f.state().stage.position_of(&window).unwrap(),
        window.geometry().size,
    );
    assert_ne!(
        filled, pre_fill,
        "precondition: the fill both moved and grew the window"
    );

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    assert!(
        f.state().stage.is_fill(&window),
        "precondition: fill membership survives fullscreen"
    );

    f.state().exit_fullscreen_on(&output);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);

    assert_eq!(
        (
            f.state().stage.position_of(&window).unwrap(),
            window.geometry().size
        ),
        filled,
        "the exit restores the whole filled rect, not a pre-fill size at the filled corner"
    );
    assert!(
        f.state().stage.is_fill(&window),
        "and the window is still filled"
    );

    // Unfill from there still goes home to the pre-fill rect.
    f.state().toggle_fill_window(&window);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    assert!(!f.state().stage.is_fill(&window));
    assert_eq!(
        (
            f.state().stage.position_of(&window).unwrap(),
            window.geometry().size
        ),
        pre_fill,
        "unfill restores the pre-fill position and size"
    );
}

/// The same restore when fullscreen beats the client's ack of the fill
/// configure — two keypresses inside one frame. Committed geometry is still the
/// pre-fill size there, so reading it would resurrect the very era mismatch the
/// test above pins; the size last *configured* is what the filled position pairs
/// with.
#[test]
fn fullscreen_exit_restores_the_filled_rect_when_it_beats_the_fill_ack() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((200, 150)), false);
    origin_view(&mut f);

    f.state().toggle_fill_window(&window);
    assert!(
        f.state().stage.is_fill(&window),
        "precondition: the fill ran"
    );
    let filled_loc = f.state().stage.position_of(&window).unwrap();
    assert_eq!(
        window.geometry().size,
        Size::from((800, 600)),
        "precondition: the client has not acked the fill configure yet"
    );

    // Fullscreen straight through the un-acked fill, then back.
    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    f.state().exit_fullscreen_on(&output);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);

    // Usable 1920×1080 minus a 12px gap on every side, no SSD bar or border on a
    // default CSD window — the same rect `fill_grows_to_usable_minus_gap` pins.
    assert_eq!(
        (
            f.state().stage.position_of(&window).unwrap(),
            window.geometry().size
        ),
        (filled_loc, Size::from((1896, 1056))),
        "the exit restores the filled rect the fill configured, not the size the client still had"
    );
    assert!(
        f.state().stage.is_fill(&window),
        "and the window is still filled"
    );
}

/// The fit twin of the test above. `fit_window` maps to the fit position in the
/// same breath as it sends the fit configure, so a fullscreen pressed into that
/// gap finds committed geometry at the pre-fit size — pairing it with the fit
/// position would hand the exit a rect the window never held.
#[test]
fn fullscreen_exit_restores_the_fit_rect_when_it_beats_the_fit_ack() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((200, 150)), false);
    origin_view(&mut f);

    f.state().fit_window(&window);
    assert!(f.state().stage.is_fit(&window), "precondition: the fit ran");
    let fit_loc = f.state().stage.position_of(&window).unwrap();
    assert_eq!(
        window.geometry().size,
        Size::from((800, 600)),
        "precondition: the client has not acked the fit configure yet"
    );

    // Fullscreen straight through the un-acked fit, then back.
    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    f.state().exit_fullscreen_on(&output);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);

    // Usable 1920×1080 minus a 12px gap on every side, no SSD bar or border on a
    // default CSD window — the same rect the fit configured.
    assert_eq!(
        (
            f.state().stage.position_of(&window).unwrap(),
            window.geometry().size
        ),
        (fit_loc, Size::from((1896, 1056))),
        "the exit restores the fit rect the fit configured, not the size the client still had"
    );
    assert!(
        f.state().stage.is_fit(&window),
        "and the window is still fit"
    );
}

#[test]
fn fill_grows_to_usable_minus_gap() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // Even size to sidestep the known 1px truncation quirk.
    let surface = map_settled(&mut f, id, "fill", (800, 600));
    let window = window_by_app_id(&mut f, "fill").unwrap();

    f.state().toggle_fill_window(&window);
    f.double_roundtrip(id);

    // Usable 1920×1080 minus a 12px gap on every side, no SSD bar / border on a
    // default CSD window → the content fills 1896×1056.
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 1896 × 1056"),
        "fill must configure the free-space size, got:\n{configures}"
    );
    assert!(f.state().stage.is_fill(&window));
}

#[test]
fn fill_round_trip_restores_size_and_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "fill", (800, 600));
    let window = window_by_app_id(&mut f, "fill").unwrap();
    let pre_pos = f.state().stage.position_of(&window).unwrap();
    let pre_size = window.geometry().size;

    // Fill, then let the client adopt the filled size as a real client would.
    f.state().toggle_fill_window(&window);
    f.double_roundtrip(id);
    let cw = f.client(id).window(&surface);
    let (w, h) = cw.configures_received.last().unwrap().1.size;
    cw.set_size(w as u16, h as u16);
    cw.ack_last_and_commit();
    f.double_roundtrip(id);
    f.client(id).window(&surface).format_recent_configures();
    assert!(f.state().stage.is_fill(&window));

    // Unfill: the exit configure restores the exact pre-fill size, and once the
    // client commits it the pending recenter restores the pre-fill position.
    f.state().toggle_fill_window(&window);
    f.double_roundtrip(id);
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains(&format!("size: {} × {}", pre_size.w, pre_size.h)),
        "unfill must restore the exact pre-fill size, got:\n{configures}"
    );
    let cw = f.client(id).window(&surface);
    let (w, h) = cw.configures_received.last().unwrap().1.size;
    cw.set_size(w as u16, h as u16);
    cw.ack_last_and_commit();
    f.double_roundtrip(id);

    assert!(!f.state().stage.is_fill(&window));
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(pre_pos),
        "unfill must restore the exact pre-fill position"
    );
}

#[test]
fn fill_stops_at_neighbor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Park B to A's right, spanning the usable height, so it caps A's rightward
    // growth regardless of the axis order fill picks.
    f.state()
        .map_window(b.clone(), Point::from((500, -528)), false);
    let b_loc = f.state().stage.position_of(&b).unwrap();

    f.state().toggle_fill_window(&a);
    f.double_roundtrip(id);

    let gap = f.state().config.snap_gap as i32;
    let a_loc = f.state().stage.position_of(&a).unwrap();
    let (w, _h) = f
        .client(id)
        .window(&a_surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    // A's right content edge stops exactly a gap short of B's left edge.
    assert_eq!(a_loc.x + w, b_loc.x - gap);
    assert!(f.state().stage.is_fill(&a));
}

#[test]
fn fill_shrinks_out_of_overlap_with_neighbor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Park B spanning the usable height, then drop A so it overlaps B's left
    // portion. Fill must pull A's right edge back out of B before growing the
    // free sides — the shrink phase, not just growth stopping short.
    f.state()
        .map_window(b.clone(), Point::from((500, -528)), false);
    f.state()
        .map_window(a.clone(), Point::from((300, 0)), false);
    let b_loc = f.state().stage.position_of(&b).unwrap();

    f.state().toggle_fill_window(&a);
    f.double_roundtrip(id);

    let gap = f.state().config.snap_gap as i32;
    let a_loc = f.state().stage.position_of(&a).unwrap();
    let (w, _h) = f
        .client(id)
        .window(&a_surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    // A's right content edge ends exactly a gap short of B's left edge, even
    // though A started overlapping B.
    assert_eq!(a_loc.x + w, b_loc.x - gap);
    assert!(f.state().stage.is_fill(&a));
}

/// Fit `window` and run the handoff to a standstill — ack, tick past the
/// resize freeze the fit parked its pan behind, then let the camera settle
/// onto the fit's target. The fill tests below need the camera and window to
/// agree to the pixel, so nothing short of a genuine settle will do.
fn fit_and_settle(
    f: &mut Fixture,
    window: &smithay::desktop::Window,
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

/// A fit window's rect spans nearly the whole usable area, so any other
/// window in view overlaps it. Fill must shrink the fit window out of that
/// overlap instead of refusing to touch a fit (maximized) window.
#[test]
fn fill_on_fit_window_with_a_neighbor_fires() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let a = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);

    fit_and_settle(&mut f, &a, id, &a_surface);
    assert!(f.state().stage.is_fit(&a), "precondition: fit");
    assert!(
        client_sees_maximized(&mut f, id, &a_surface),
        "precondition: the fit told the client it is maximized"
    );
    let fit_loc = f.state().stage.position_of(&a).unwrap();
    let fit_size = crate::state::configured_window_size(&a);

    // A wall inside the fit rect's left half, spanning most of its height:
    // pulling A's left edge past it is the least-travel escape, and the wall
    // then caps regrowth at the same edge.
    let _b_surface = map_settled(&mut f, id, "b", (300, 800));
    let b = window_by_app_id(&mut f, "b").unwrap();
    f.state().map_window(
        b.clone(),
        Point::from((fit_loc.x + 200, fit_loc.y + 100)),
        false,
    );

    f.state().fill_window(&a);
    f.double_roundtrip(id);

    let gap = f.state().config.snap_gap as i32;
    let a_loc = f.state().stage.position_of(&a).unwrap();
    let b_loc = f.state().stage.position_of(&b).unwrap();
    let b_w = b.geometry().size.w;
    let (w, _h) = f
        .client(id)
        .window(&a_surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    // A's left content edge ends exactly a gap past B's right edge, and its
    // right edge stays where the fit put it — nothing over there to retreat
    // from.
    assert_eq!(a_loc.x, b_loc.x + b_w + gap);
    assert_eq!(a_loc.x + w, fit_loc.x + fit_size.w);
    assert!(!f.state().stage.is_fit(&a));
    assert!(f.state().stage.is_fill(&a));
    assert!(
        !client_sees_maximized(&mut f, id, &a_surface),
        "fill must clear the client's Maximized state"
    );
}

/// With nothing else in view, and the camera genuinely settled on the fit's
/// own target (not merely animating toward it), a fit window already fills
/// its usable space — fill must leave it untouched.
///
/// The exact no-op needs the fit's truncated canvas rect and its untruncated
/// camera to land on the same integers, which needs every input to that
/// centering half-pixel-free: even pre-fit dimensions, an even usable area, an
/// integral snap gap, no SSD bar. See
/// `fill_on_lone_fit_window_nudges_it_when_the_fit_left_a_subpixel_gap` for
/// what one half-pixel does instead.
#[test]
fn fill_on_lone_fit_window_with_settled_camera_is_inert() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);

    fit_and_settle(&mut f, &window, id, &surface);
    assert!(f.state().stage.is_fit(&window), "precondition: fit");
    assert!(
        client_sees_maximized(&mut f, id, &surface),
        "precondition: the fit told the client it is maximized"
    );
    let before_loc = f.state().stage.position_of(&window).unwrap();
    let before_size = crate::state::configured_window_size(&window);

    f.state().fill_window(&window);
    f.double_roundtrip(id);

    assert_eq!(f.state().stage.position_of(&window), Some(before_loc));
    assert_eq!(crate::state::configured_window_size(&window), before_size);
    assert!(
        f.state().stage.is_fit(&window),
        "an inert fill must leave fit membership alone"
    );
    assert!(!f.state().stage.is_fill(&window));
    assert!(
        client_sees_maximized(&mut f, id, &surface),
        "an inert fill must not touch the client's Maximized state"
    );
}

/// The odd-dimension twin of the test above — and empirically not a no-op.
/// `fit_window` maps the window at `target_camera.x as i32` while animating
/// the camera onto the untruncated value, and `compute_fill_geometry` reads
/// that camera back as an exact `f64`. An odd pre-fit width/height gives the
/// pre-fit visual center — and so `target_camera` — a `.5` fraction on that
/// axis, so the fit leaves the window half a pixel off the usable area it was
/// meant to fill. With no neighbor to blame, fill closes that gap on its own:
/// it nudges the window, drops fit membership, and clears the client's
/// Maximized state.
///
/// This characterises a known defect. When `fit_window` stops truncating
/// `target_camera`, this test becomes a duplicate of the even-dims inert test
/// above and should be deleted rather than re-aimed.
///
/// `as i32` truncates toward zero, so which way the half-pixel lands follows
/// the sign of `target_camera` — the fit here is off-center enough that x is
/// negative and y positive, so the two axes are nudged opposite ways.
#[test]
fn fill_on_lone_fit_window_nudges_it_when_the_fit_left_a_subpixel_gap() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "a", (801, 601));
    let window = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);

    fit_and_settle(&mut f, &window, id, &surface);
    assert!(f.state().stage.is_fit(&window), "precondition: fit");
    let before_loc = f.state().stage.position_of(&window).unwrap();
    let before_size = crate::state::configured_window_size(&window);

    f.state().fill_window(&window);
    f.double_roundtrip(id);

    let loc = f.state().stage.position_of(&window).unwrap();
    assert_ne!(loc, before_loc, "the sub-pixel gap must be taken up");
    assert!(
        (loc.x - before_loc.x).abs() <= 1 && (loc.y - before_loc.y).abs() <= 1,
        "taking it up must cost at most a pixel per axis, moved {before_loc:?} → {loc:?}"
    );
    assert_eq!(
        crate::state::configured_window_size(&window),
        before_size,
        "half a pixel at each end leaves the size alone"
    );
    assert!(!f.state().stage.is_fit(&window));
    assert!(f.state().stage.is_fill(&window));
    assert!(
        !client_sees_maximized(&mut f, id, &surface),
        "the nudge clears the client's Maximized state too"
    );
}

/// A fill taken straight out of fit inherits the *pre-fit* size as its
/// restore point — not `restore_size`, and not the fit's own viewport-
/// spanning size — paired with the position that size occupied, not the fit
/// rect's top-left. Unfilling must land back on the whole pre-fit rect.
#[test]
fn unfill_after_fill_on_fit_restores_the_pre_fit_rect() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let a = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    // The fit preserves the visual center, so the restore point the fill
    // derives from the fit rect is this position again.
    let pre_fit_loc = f.state().stage.position_of(&a).unwrap();

    fit_and_settle(&mut f, &a, id, &a_surface);
    let fit_loc = f.state().stage.position_of(&a).unwrap();
    f.client(id).window(&a_surface).format_recent_configures();

    // A neighbor inside the fit rect forces a real (non-no-op) fill.
    let _b_surface = map_settled(&mut f, id, "b", (300, 800));
    let b = window_by_app_id(&mut f, "b").unwrap();
    f.state().map_window(
        b.clone(),
        Point::from((fit_loc.x + 200, fit_loc.y + 100)),
        false,
    );

    f.state().fill_window(&a);
    assert!(
        f.state().stage.is_fill(&a),
        "precondition: the fill on the fit window ran"
    );
    adopt_last_configure(&mut f, id, &a_surface);
    f.client(id).window(&a_surface).format_recent_configures();

    f.state().toggle_fill_window(&a);
    f.double_roundtrip(id);
    let configures = f.client(id).window(&a_surface).format_recent_configures();
    adopt_last_configure(&mut f, id, &a_surface);

    assert!(!f.state().stage.is_fill(&a));
    assert!(
        !f.state().stage.is_fit(&a),
        "the fill dropped fit membership and the unfill must not resurrect it"
    );
    assert!(
        configures.contains("size: 800 × 600"),
        "unfill must restore the pre-fit size, not the fit's viewport-spanning \
         size, got:\n{configures}"
    );
    assert_eq!(
        f.state().stage.position_of(&a),
        Some(pre_fit_loc),
        "and the pre-fit position, not the fit rect's corner"
    );
    assert!(
        !client_sees_maximized(&mut f, id, &a_surface),
        "a restored window is not maximized"
    );
}

#[test]
fn fill_on_pinned_window_is_noop() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _surface = map_settled(&mut f, id, "pin", (800, 600));
    let window = window_by_app_id(&mut f, "pin").unwrap();

    f.state().execute_action(&Action::TogglePinToScreen);
    assert!(
        f.state().is_pinned(&window),
        "precondition: window is pinned"
    );

    // The action's is_canvas_window filter drops pinned windows before toggle.
    f.state().execute_action(&Action::FillWindow);
    assert!(!f.state().stage.is_fill(&window));
}

#[test]
fn fill_already_filling_does_not_set_membership() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "fill", (800, 600));
    let window = window_by_app_id(&mut f, "fill").unwrap();

    // Fill and adopt the filled geometry.
    f.state().toggle_fill_window(&window);
    f.double_roundtrip(id);
    let cw = f.client(id).window(&surface);
    let (w, h) = cw.configures_received.last().unwrap().1.size;
    cw.set_size(w as u16, h as u16);
    cw.ack_last_and_commit();
    f.double_roundtrip(id);

    // Drop the restore point (as a manual resize/move would), then fill again:
    // the window already fills its free space, so the geometry is a no-op and no
    // fill membership is recorded.
    f.state().stage.clear_fill(&window);
    f.state().fill_window(&window);
    assert!(!f.state().stage.is_fill(&window));
}

#[test]
fn fill_at_zoom_and_pan_uses_canvas_space_usable_area() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "fill", (800, 600));
    let window = window_by_app_id(&mut f, "fill").unwrap();

    // Zoom to 0.5 and pan the camera: the usable area is a screen rect, so the
    // free canvas region fill grows into is (screen / zoom). Even numbers and
    // zoom 0.5 keep the screen→canvas conversion exact, dodging the 1px quirk.
    f.state().set_zoom(0.5);
    f.state().set_camera(Point::from((5000.0, 5000.0)));
    // Park the window inside the panned viewport so it intersects the bounds.
    f.state()
        .map_window(window.clone(), Point::from((6000, 6000)), false);

    f.state().toggle_fill_window(&window);
    f.double_roundtrip(id);

    // Canvas bounds = camera + screen/zoom = [5000,8840]×[5000,7160]; inset by a
    // 12px gap → free region 3816 × 2136 at canvas top-left (5012, 5012).
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 3816 × 2136"),
        "fill must configure the canvas-space free size, got:\n{configures}"
    );
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(Point::from((5012, 5012))),
        "fill must map the window at the gap-inset canvas top-left"
    );
    assert!(f.state().stage.is_fill(&window));
}

fn config_ssd() -> Config {
    let mut config = Config::default();
    config.decorations.default_mode = DecorationMode::Server;
    config.decorations.border_width = 5;
    config
}

#[test]
fn fill_on_ssd_window_round_trips_bar_and_border() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "fill", (800, 600));
    let window = window_by_app_id(&mut f, "fill").unwrap();
    // Precondition: the window actually carries a server-side title bar.
    assert_eq!(f.state().window_ssd_bar(&window), 25);

    // Pin the camera (mapping a window pans it to center) so the filled canvas
    // position is deterministic; keep the window inside the viewport.
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);

    f.state().toggle_fill_window(&window);
    f.double_roundtrip(id);

    // The free frame region is 1896 × 1056 (usable minus a 12px gap). The client
    // content size is that minus a 5px border per side, and on height also the
    // 25px title bar: 1886 × 1021 — proving the chrome inflation round-trips.
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 1886 × 1021"),
        "fill must deflate the frame by border and bar, got:\n{configures}"
    );
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(Point::from((17, 42))),
        "fill loc must offset the content by border and bar"
    );
    assert!(f.state().stage.is_fill(&window));
}

/// A fit targets the gap-inset usable area with the window's *visual frame*:
/// deflating by the bar alone leaves the frame overflowing by `2 × border_width`
/// per axis.
#[test]
fn fit_on_ssd_window_subtracts_the_border_from_the_target_size() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "fit", (800, 600));
    let window = window_by_app_id(&mut f, "fit").unwrap();
    assert_eq!(f.state().window_ssd_bar(&window), 25);

    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);

    f.state().fit_window(&window);
    f.double_roundtrip(id);

    // Usable area is the full 1920×1080 output; inset by the 12px snap gap
    // gives an 1896×1056 frame budget. The content inside it gives up the
    // whole chrome, borders included (25px bar + 5px border on every side):
    // 1896 - 2×5 = 1886 wide, 1056 - 25 - 2×5 = 1021 tall — the same numbers
    // `fill_on_ssd_window_round_trips_bar_and_border` lands on, since a lone
    // window's fit and fill both target the whole usable area.
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 1886 × 1021"),
        "fit must deflate the target by border and bar, got:\n{configures}"
    );
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(Point::from((-143, 89))),
        "fit loc must offset the content by border and bar"
    );
}

/// Fill must record its rect as the window's settled footprint. Leaving the
/// pre-fill rect cached makes every later commit read as "grew past settled" —
/// a perpetual reflow scan once the fill state is cleared (move-grab start,
/// nudge), and a real translation whenever the fill kept an unresolvable
/// overlap. A commit after the clear must leave the window in place.
#[test]
fn fill_records_settled_footprint() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Pin the camera (mapping pans it) and park A settled and gap-adjacent to
    // B: the settled adjacency is the reflow's anchor precondition.
    let gap = f.state().config.snap_gap as i32;
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state()
        .refresh_stable_snap_rect(&crate::state::StageWindow::Client(a.clone()));
    f.state()
        .map_window(b.clone(), Point::from((1200 + gap, 300)), false);

    f.state().toggle_fill_window(&a);
    assert!(f.state().stage.is_fill(&a), "fill must not silently no-op");
    f.double_roundtrip(id);
    let (w, h) = f
        .client(id)
        .window(&a_surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    let win = f.client(id).window(&a_surface);
    win.set_size(w as u16, h as u16);
    win.attach_new_buffer();
    win.ack_last_and_commit();
    f.double_roundtrip(id);
    let filled_loc = f.state().stage.position_of(&a).unwrap();

    // The settled footprint is the filled frame, not the stale pre-fill rect.
    let a_id = super::server_surface(&a).id();
    let stable = f.state().stable_snap_rects.get(&a_id).copied().unwrap();
    assert_eq!(
        (stable.x_low, stable.y_low, stable.x_high, stable.y_high),
        (12.0, 12.0, 1200.0, 1068.0),
        "fill must cache its target rect as the settled footprint"
    );

    // Re-anchor: every move path (grab start, nudge, send-to-output) funnels
    // through clear_fill, then the app redraws before any grab-end settle.
    f.state().stage.clear_fill(&a);
    let win = f.client(id).window(&a_surface);
    win.attach_new_buffer();
    win.commit();
    f.double_roundtrip(id);

    assert_eq!(
        f.state().stage.position_of(&a),
        Some(filled_loc),
        "a redraw commit after clear_fill must not translate the filled window"
    );
}

/// The plain fullscreen round-trip (no straggler, no neighbor) must restore the
/// exact pre-fullscreen position — the reflow settle-guard must not disturb it.
#[test]
fn fullscreen_round_trip_restores_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    let pre_pos = f.state().stage.position_of(&window).unwrap();

    // Fullscreen, then adopt the fullscreen size as a real client would.
    let cw = f.client(id).window(&surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);

    // Exit and settle at the restored size.
    let cw = f.client(id).window(&surface);
    cw.unset_fullscreen();
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);

    assert!(!f.state().stage.is_fullscreen(&window));
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(pre_pos),
        "fullscreen round-trip must restore the exact pre-fullscreen position"
    );
}

/// A client exiting fullscreen keeps committing viewport-sized frames until it
/// acks the restore configure; those synchronous-exit stragglers read as "grown
/// past settled" against the stale pre-fullscreen rect. A gap-adjacent neighbor
/// must not get shoved aside by that reflow misread.
#[test]
fn fullscreen_exit_straggler_keeps_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Park A settled and gap-adjacent to B in canvas space: the settled
    // adjacency is the reflow's anchor precondition.
    let gap = f.state().config.snap_gap as i32;
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state()
        .refresh_stable_snap_rect(&StageWindow::Client(a.clone()));
    f.state()
        .map_window(b.clone(), Point::from((1200 + gap, 300)), false);
    let pre_pos = f.state().stage.position_of(&a).unwrap();

    // Client-initiated fullscreen, then adopt the fullscreen size.
    let window = f.client(id).window(&a_surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    // Exit runs synchronously server-side; the client has not acked yet.
    let window = f.client(id).window(&a_surface);
    window.unset_fullscreen();
    f.double_roundtrip(id);

    // Straggler: a still-fullscreen-sized frame lands before the restore
    // configure is acked.
    let window = f.client(id).window(&a_surface);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);

    // The client finally acks and settles at the restored size.
    adopt_last_configure(&mut f, id, &a_surface);

    assert_eq!(
        f.state().stage.position_of(&a),
        Some(pre_pos),
        "a straggler fullscreen-sized commit must not relocate the exiting window"
    );
}

/// Real clients (GTK4/celluloid) ack the restore configure as soon as they
/// process it, then keep committing old-fullscreen-sized frames for a frame
/// or two. Once acked, pending configures is empty, so the "unacked configure
/// differs from committed geometry" bail goes blind and the stale-sized
/// commit misreads as a grow-past-settled reflow.
#[test]
fn fullscreen_exit_early_ack_straggler_keeps_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Park A settled and gap-adjacent to B in canvas space: the settled
    // adjacency is the reflow's anchor precondition.
    let gap = f.state().config.snap_gap as i32;
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state()
        .refresh_stable_snap_rect(&StageWindow::Client(a.clone()));
    f.state()
        .map_window(b.clone(), Point::from((1200 + gap, 300)), false);
    let pre_pos = f.state().stage.position_of(&a).unwrap();

    // Client-initiated fullscreen, then adopt the fullscreen size.
    let window = f.client(id).window(&a_surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    // Exit runs synchronously server-side; the compositor sends the restore
    // configure.
    let window = f.client(id).window(&a_surface);
    window.unset_fullscreen();
    f.double_roundtrip(id);

    // Early ack: the client acks the restore configure immediately, before
    // resizing — pending configures is now empty.
    let window = f.client(id).window(&a_surface);
    window.ack_last();

    // Straggler: a still-fullscreen-sized frame (viewport destination
    // untouched) lands after the ack.
    let window = f.client(id).window(&a_surface);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);

    assert_eq!(
        f.state().stage.position_of(&a),
        Some(pre_pos),
        "a stale-sized frame committed after an early ack must not relocate the exiting window"
    );

    // Already acked above (the early ack), so this just draws the resize —
    // re-acking the same serial would be a protocol error.
    let (w, h) = f
        .client(id)
        .window(&a_surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    let window = f.client(id).window(&a_surface);
    window.set_size(w as u16, h as u16);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);

    assert_eq!(
        f.state().stage.position_of(&a),
        Some(pre_pos),
        "the exiting window must settle back at its pre-fullscreen position"
    );
}

/// A client exiting fullscreen may settle at a size the compositor never
/// configured (an aspect-constrained player choosing its own dimensions).
/// The recenter must still land on the pre-fullscreen center, not some
/// adjacent-placement spot, and the settle must be over after that one commit.
#[test]
fn fullscreen_exit_settles_at_client_chosen_size_recentered() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Park A settled and gap-adjacent to B in canvas space: the settled
    // adjacency is the reflow's anchor precondition.
    let gap = f.state().config.snap_gap as i32;
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state()
        .refresh_stable_snap_rect(&StageWindow::Client(a.clone()));
    f.state()
        .map_window(b.clone(), Point::from((1200 + gap, 300)), false);
    let pre_pos = f.state().stage.position_of(&a).unwrap();

    // Client-initiated fullscreen, then adopt the fullscreen size.
    let window = f.client(id).window(&a_surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    // Exit runs synchronously server-side; the compositor sends the restore
    // configure.
    let window = f.client(id).window(&a_surface);
    window.unset_fullscreen();
    f.double_roundtrip(id);

    // Early ack: the client acks the restore configure immediately, before
    // resizing — pending configures is now empty.
    let window = f.client(id).window(&a_surface);
    window.ack_last();

    // The client settles at a size of its own choosing (700 × 500) instead of
    // the configured restore size (800 × 600) — already acked above, so this
    // just draws; re-acking the same serial would be a protocol error.
    let window = f.client(id).window(&a_surface);
    window.set_size(700, 500);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);

    let pre_exit_center = (pre_pos.x as f64 + 400.0, pre_pos.y as f64 + 300.0);
    let settled_pos = f.state().stage.position_of(&a).unwrap();
    let settled_center = (settled_pos.x as f64 + 350.0, settled_pos.y as f64 + 250.0);
    assert!(
        (settled_center.0 - pre_exit_center.0).abs() <= 2.0
            && (settled_center.1 - pre_exit_center.1).abs() <= 2.0,
        "client-chosen settle size must recenter on the pre-fullscreen center, \
         got {settled_center:?}, want {pre_exit_center:?}"
    );

    let window = f.client(id).window(&a_surface);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);
    assert_eq!(
        f.state().stage.position_of(&a),
        Some(settled_pos),
        "a repeat commit at the settled size must leave the position unchanged"
    );
}

/// A re-fullscreen mid-settle (before the client ever resizes down to the
/// restore size) must not let the outstanding recenter fire against the new
/// fullscreen geometry, and must not corrupt the position the window returns
/// to when it eventually exits fullscreen for real.
#[test]
fn refullscreen_during_exit_settle_keeps_return_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Park A settled and gap-adjacent to B in canvas space: the settled
    // adjacency is the reflow's anchor precondition.
    let gap = f.state().config.snap_gap as i32;
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state()
        .refresh_stable_snap_rect(&StageWindow::Client(a.clone()));
    f.state()
        .map_window(b.clone(), Point::from((1200 + gap, 300)), false);
    let pre_pos = f.state().stage.position_of(&a).unwrap();

    // Client-initiated fullscreen, then adopt the fullscreen size.
    let window = f.client(id).window(&a_surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    // Exit runs synchronously server-side; the compositor sends the restore
    // configure.
    let window = f.client(id).window(&a_surface);
    window.unset_fullscreen();
    f.double_roundtrip(id);

    // Early ack: the client acks the restore configure immediately, before
    // resizing — pending configures is now empty.
    let window = f.client(id).window(&a_surface);
    window.ack_last();

    // Stale fullscreen-sized straggler lands after the ack, mid-settle.
    let window = f.client(id).window(&a_surface);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);

    // The client re-fullscreens immediately, before ever resizing down to the
    // restore size — the exit settle is still outstanding.
    let window = f.client(id).window(&a_surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    assert!(
        f.state().stage.is_fullscreen(&a),
        "the re-fullscreen request must take effect despite the outstanding settle"
    );

    // Exit again — a fresh restore configure, safe to ack.
    let window = f.client(id).window(&a_surface);
    window.unset_fullscreen();
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    assert_eq!(
        f.state().stage.position_of(&a),
        Some(pre_pos),
        "the mid-settle re-fullscreen must not corrupt the saved return position"
    );
}

/// A window mid fill-exit settle (a `pending_recenter` outstanding, client not
/// yet resized down) that then enters fullscreen must drop that recenter:
/// otherwise the settle completion fires on the first fullscreen-sized commit
/// and map_windows the now-fullscreen window off its output's camera origin,
/// breaking the fullscreen-parking invariant (`state/viewport.rs`).
#[test]
fn fullscreen_during_fill_exit_settle_stays_at_camera_origin() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // Moving the camera (fill/fullscreen below) seeds a per-output blur
    // generation that only clears on output disconnect, so it can never return
    // to the pre-output baseline.
    f.skip_baseline_check();
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Pin the camera (mapping pans it) and park A gap-adjacent to B so fill has
    // real free space to grow into (a non-noop fill).
    let gap = f.state().config.snap_gap as i32;
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state()
        .refresh_stable_snap_rect(&StageWindow::Client(a.clone()));
    f.state()
        .map_window(b.clone(), Point::from((1200 + gap, 300)), false);

    // Fill A, then let the client adopt the filled size as a real client would.
    f.state().toggle_fill_window(&a);
    assert!(f.state().stage.is_fill(&a), "fill must not silently no-op");
    f.double_roundtrip(id);
    let (w, h) = f
        .client(id)
        .window(&a_surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    let cw = f.client(id).window(&a_surface);
    cw.set_size(w as u16, h as u16);
    cw.ack_last_and_commit();
    f.double_roundtrip(id);

    // Unfill: registers a pending recenter whose pre_exit_size is the filled
    // size. Do NOT settle it — the client never resizes down.
    f.state().toggle_fill_window(&a);
    f.double_roundtrip(id);
    let a_id = super::server_surface(&a).id();
    assert!(
        f.state().pending_recenter.contains_key(&a_id),
        "unfill must register a settle to complete later"
    );

    // Enter fullscreen while that settle is still outstanding, then adopt the
    // fullscreen size. The fullscreen-sized commit differs from the outstanding
    // pre_exit_size, so a surviving recenter would fire against it.
    let cw = f.client(id).window(&a_surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    assert!(f.state().stage.is_fullscreen(&a));
    let origin = f.state().camera().to_i32_round();
    assert_eq!(
        f.state().stage.position_of(&a),
        Some(origin),
        "a fullscreen window must stay parked at its camera origin, not be \
         recentered by a leftover fill-exit settle"
    );
    assert!(
        !f.state().pending_recenter.contains_key(&a_id),
        "entering fullscreen must drop the outstanding fill-exit recenter"
    );
}

/// A window that maps under a fullscreen window and itself requests fullscreen
/// before its first commit takes the deferred `pending_fullscreen` path:
/// background-placed, then fullscreened on the buffer commit. Its fullscreen
/// configure must carry Activated despite the un-activated placement.
#[test]
fn background_window_fullscreen_configure_is_activated() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // A owns the output's fullscreen.
    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let cw = f.client(id).window(&a_surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    // B requests fullscreen before its first commit, so the request is deferred;
    // it background-places under A, then the deferred branch takes it fullscreen.
    let b = f.client(id).create_window();
    let b_surface = b.surface.clone();
    b.set_app_id("b");
    b.set_fullscreen(None);
    b.commit();
    f.roundtrip(id);
    let b = f.client(id).window(&b_surface);
    b.set_size(400, 300);
    b.attach_new_buffer();
    b.ack_last_and_commit();
    f.double_roundtrip(id);

    let mapped = window_by_app_id(&mut f, "b").unwrap();
    assert_eq!(
        f.state().stage.fullscreen_output_of(&mapped),
        Some("HEADLESS-1"),
        "b must have taken over fullscreen via the deferred path"
    );
    let configures = f.client(id).window(&b_surface).format_recent_configures();
    let fs_line = configures
        .lines()
        .find(|l| l.contains("Fullscreen"))
        .unwrap_or("");
    assert!(
        fs_line.contains("Activated"),
        "b's fullscreen configure must carry Activated, got:\n{configures}"
    );
}

/// Fullscreen `surface`'s window, let the client adopt the viewport-sized
/// buffer, then exit and never ack the restore configure — leaving the exit
/// recenter registered and outstanding. Returns the surface's id, the
/// `pending_recenter` key.
fn owe_an_exit_recenter(
    f: &mut Fixture,
    id: super::client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
    window: &smithay::desktop::Window,
) -> smithay::reexports::wayland_server::backend::ObjectId {
    f.client(id).window(surface).set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(f, id, surface);
    f.client(id).window(surface).unset_fullscreen();
    f.double_roundtrip(id);

    let key = super::server_surface(window).id();
    assert!(
        f.state().pending_recenter.contains_key(&key),
        "precondition: the fullscreen exit left a recenter owed"
    );
    key
}

/// Two windows, both mid fullscreen-exit settle, and a placement action on one
/// of them. Returns `(a, a_key, b_key)` — the action's target, its
/// `pending_recenter` key, and the bystander's.
///
/// Every arm that establishes a new placement drops the target's owed recenter;
/// none of them may reach the bystander's. A one-window scenario cannot tell
/// "removed the right key" from "removed every key", and a wrong key is the
/// whole hazard.
fn two_owed_recenters(
    f: &mut Fixture,
    id: super::client::ClientId,
) -> (
    smithay::desktop::Window,
    smithay::reexports::wayland_server::backend::ObjectId,
    smithay::reexports::wayland_server::backend::ObjectId,
) {
    let a_surface = map_settled(f, id, "a", (400, 300));
    let b_surface = map_settled(f, id, "b", (400, 300));
    let a = window_by_app_id(f, "a").unwrap();
    let b = window_by_app_id(f, "b").unwrap();

    let a_key = owe_an_exit_recenter(f, id, &a_surface, &a);
    let b_key = owe_an_exit_recenter(f, id, &b_surface, &b);
    assert!(
        f.state().pending_recenter.contains_key(&a_key),
        "precondition: a's entry survived b's fullscreen round-trip"
    );

    // Park them apart and pin the camera, so the placement actions below have
    // room to work with and neither window obstructs the other.
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state().map_window(a.clone(), Point::from((0, 0)), false);
    f.state()
        .map_window(b.clone(), Point::from((4000, 4000)), false);
    (a, a_key, b_key)
}

/// A fit establishes its own placement, so it drops the recenter its window
/// owes — and only that window's.
#[test]
fn fit_drops_the_fitted_windows_owed_recenter_alone() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // Fullscreen and fit move the camera, which seeds a per-output blur
    // generation that only clears on output disconnect.
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();
    let (a, a_key, b_key) = two_owed_recenters(&mut f, id);

    f.state().fit_window(&a);

    assert!(
        !f.state().pending_recenter.contains_key(&a_key),
        "the fit dropped the recenter that would have yanked it off the fit"
    );
    assert!(
        f.state().pending_recenter.contains_key(&b_key),
        "the other window's owed recenter is none of the fit's business"
    );
}

/// A fill places the window absolutely, so it drops the recenter its window
/// owes — and only that window's.
#[test]
fn fill_drops_the_filled_windows_owed_recenter_alone() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();
    let (a, a_key, b_key) = two_owed_recenters(&mut f, id);

    f.state().fill_window(&a);
    assert!(f.state().stage.is_fill(&a), "precondition: the fill ran");

    assert!(
        !f.state().pending_recenter.contains_key(&a_key),
        "the fill dropped the recenter that would have dragged it off the fill"
    );
    assert!(
        f.state().pending_recenter.contains_key(&b_key),
        "the other window's owed recenter is none of the fill's business"
    );
}

/// A nudge is the window's new position, so it drops the recenter that would
/// undo it — and only that window's.
#[test]
fn nudge_drops_the_nudged_windows_owed_recenter_alone() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();
    let (a, a_key, b_key) = two_owed_recenters(&mut f, id);

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.state()
        .execute_action(&Action::NudgeWindow(Direction::Right));

    assert!(
        !f.state().pending_recenter.contains_key(&a_key),
        "the nudge dropped the recenter that would have undone it"
    );
    assert!(
        f.state().pending_recenter.contains_key(&b_key),
        "the other window's owed recenter is none of the nudge's business"
    );
}

/// A bookmark move asks for a visual center, so instead of dropping the
/// recenter its window owes it re-aims it at the bookmark — and touches no
/// other window's.
#[test]
fn move_to_bookmark_reaims_the_moved_windows_owed_recenter_alone() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();
    let (a, a_key, b_key) = two_owed_recenters(&mut f, id);

    let b_center = f.state().pending_recenter[&b_key].target_center;
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.state().bookmarks.insert("b".into(), [300.0, -200.0]);
    f.state()
        .execute_action(&Action::MoveToBookmark("b".into()));

    assert_eq!(
        f.state()
            .pending_recenter
            .get(&a_key)
            .map(|p| p.target_center),
        Some(Point::from((300.0, 200.0))),
        "the bookmark move re-aimed the owed recenter at the bookmark"
    );
    assert_eq!(
        f.state()
            .pending_recenter
            .get(&b_key)
            .map(|p| p.target_center),
        Some(b_center),
        "the other window's owed recenter is none of the bookmark move's business"
    );
}

/// Pinning decides where the window lives from now on, so it drops the recenter
/// that would re-place it afterwards — and only that window's.
#[test]
fn pin_toggle_drops_the_pinned_windows_owed_recenter_alone() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();
    let (a, a_key, b_key) = two_owed_recenters(&mut f, id);

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.state().execute_action(&Action::TogglePinToScreen);
    assert!(f.state().is_pinned(&a), "precondition: the pin took");

    assert!(
        !f.state().pending_recenter.contains_key(&a_key),
        "the pin dropped the recenter that would have re-placed the window"
    );
    assert!(
        f.state().pending_recenter.contains_key(&b_key),
        "the other window's owed recenter is none of the pin's business"
    );
}

/// Fit `surface`'s window, let the client adopt the fit-sized buffer, then unfit
/// and never ack the restore configure — the fit-exit twin of
/// [`owe_an_exit_recenter`]. Returns the `pending_recenter` key.
fn owe_a_fit_exit_recenter(
    f: &mut Fixture,
    id: super::client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
    window: &smithay::desktop::Window,
) -> smithay::reexports::wayland_server::backend::ObjectId {
    f.state().fit_window(window);
    f.double_roundtrip(id);
    adopt_last_configure(f, id, surface);
    f.state().unfit_window(window);
    f.double_roundtrip(id);

    let key = super::server_surface(window).id();
    assert!(
        f.state().pending_recenter.contains_key(&key),
        "precondition: the fit exit left a recenter owed"
    );
    key
}

/// Fill `surface`'s window, let the client adopt the filled buffer, then unfill
/// and never ack the restore configure. Returns the `pending_recenter` key.
fn owe_a_fill_exit_recenter(
    f: &mut Fixture,
    id: super::client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
    window: &smithay::desktop::Window,
) -> smithay::reexports::wayland_server::backend::ObjectId {
    f.state().fill_window(window);
    assert!(
        f.state().stage.is_fill(window),
        "precondition: the fill was not a no-op"
    );
    f.double_roundtrip(id);
    adopt_last_configure(f, id, surface);
    f.state().unfill_window(window);
    f.double_roundtrip(id);

    let key = super::server_surface(window).id();
    assert!(
        f.state().pending_recenter.contains_key(&key),
        "precondition: the fill exit left a recenter owed"
    );
    key
}

/// The canvas location a window-rule point maps to for a window of content
/// `size` wearing `chrome` — what the settle must land on. Derived straight from
/// the rule convention rather than through the center formula the settle itself
/// runs, so a bar term dropped or flipped on the way into `target_center` shows
/// up here as a half-bar offset instead of cancelling out. Exact for the even
/// sizes used below; an odd one would part company with the settle by the
/// truncation `map_window_to_rule_point` documents.
fn rule_point_loc(x: i32, y: i32, size: Size<i32, Logical>, chrome: Chrome) -> Point<i32, Logical> {
    driftwm::canvas::rule_to_content(x, y, size, chrome)
}

/// Ack the outstanding restore configure and then commit a buffer at a size of
/// the client's own choosing, which is its right — the only settle that can tell
/// a recenter re-aimed at the request from one merely dropped, since any size the
/// compositor could have guessed at placement time is the one it configured.
fn settle_at(
    f: &mut Fixture,
    id: super::client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
    size: (u16, u16),
) {
    f.client(id).window(surface).ack_last();
    let window = f.client(id).window(surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);
}

/// `msg move` dispatched while a window is still settling out of a fullscreen
/// exit must land it on the requested point once the client resizes. Its
/// committed buffer is still viewport-sized, so placing against that size lands
/// it half the size delta away on each axis; the owed recenter is re-aimed at
/// the request rather than dropped, so the settle corrects it.
#[test]
fn ipc_move_mid_fullscreen_exit_settle_lands_where_asked() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let key = owe_an_exit_recenter(&mut f, id, &a_surface, &a);
    assert_eq!(
        a.geometry().size,
        Size::from((1920, 1080)),
        "precondition: the client still commits the fullscreen buffer"
    );

    let ipc_id = f.state().stage.id_of(&a).unwrap().0;
    let reply = crate::ipc::dispatch(
        Request::Move {
            window: Some(WindowSelector::Id(ipc_id)),
            to: Some((1000, -500)),
        },
        f.state(),
    );
    assert!(matches!(reply, Ok(Response::Position { x: 1000, y: -500 })));
    assert_eq!(
        f.state().stage.position_of(&a),
        Some(rule_point_loc(
            1000,
            -500,
            Size::from((400, 300)),
            Chrome::NONE
        )),
        "the provisional placement already uses the size the exit configured, \
         not the fullscreen buffer the client is still committing"
    );

    // The client settles at a size of its own choosing, not the 400x300 the
    // restore configure asked for.
    settle_at(&mut f, id, &a_surface, (700, 500));
    assert_eq!(
        a.geometry().size,
        Size::from((700, 500)),
        "precondition: the settle ran against the client's own size"
    );

    assert!(
        !f.state().pending_recenter.contains_key(&key),
        "the settle consumed the re-aimed recenter"
    );
    assert_eq!(
        f.state().stage.position_of(&a),
        Some(rule_point_loc(
            1000,
            -500,
            Size::from((700, 500)),
            Chrome::NONE
        )),
        "the window landed on the point msg move asked for"
    );
}

/// Once the settle is done nothing is owed, and it's the size the exit
/// configured that is stale: a client that settled at a size of its own is
/// described only by its committed geometry. Placing against the configured size
/// here would miss by half their difference with no settle left to correct it.
#[test]
fn ipc_move_after_a_client_chosen_settle_uses_committed_size() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let key = owe_an_exit_recenter(&mut f, id, &a_surface, &a);
    settle_at(&mut f, id, &a_surface, (700, 500));
    assert!(
        !f.state().pending_recenter.contains_key(&key),
        "precondition: the settle completed, so nothing is owed"
    );
    assert_eq!(
        a.geometry().size,
        Size::from((700, 500)),
        "precondition: the client kept its own size over the configured 400x300"
    );

    let ipc_id = f.state().stage.id_of(&a).unwrap().0;
    let reply = crate::ipc::dispatch(
        Request::Move {
            window: Some(WindowSelector::Id(ipc_id)),
            to: Some((1000, -500)),
        },
        f.state(),
    );
    assert!(matches!(reply, Ok(Response::Position { x: 1000, y: -500 })));

    assert_eq!(
        f.state().stage.position_of(&a),
        Some(rule_point_loc(
            1000,
            -500,
            Size::from((700, 500)),
            Chrome::NONE
        )),
        "the move centered the window on the size it actually committed"
    );
}

/// The same mid-settle placement with an SSD title bar. A rule point names the
/// *visual frame's* center, so the settle has to land the content half a bar
/// below it — carry the bar into the re-aimed center or drop it from the
/// location, and the window sits half a bar off.
#[test]
fn ipc_move_mid_settle_lands_where_asked_with_ssd() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    super::give_ssd(&mut f, &a);
    let bar = f.state().window_ssd_bar(&a);
    assert!(bar > 0, "precondition: the window carries a title bar");

    owe_an_exit_recenter(&mut f, id, &a_surface, &a);

    let ipc_id = f.state().stage.id_of(&a).unwrap().0;
    let reply = crate::ipc::dispatch(
        Request::Move {
            window: Some(WindowSelector::Id(ipc_id)),
            to: Some((1000, -500)),
        },
        f.state(),
    );
    assert!(matches!(reply, Ok(Response::Position { x: 1000, y: -500 })));
    settle_at(&mut f, id, &a_surface, (700, 500));

    let chrome = f.state().element_chrome(&a);
    assert_eq!(
        chrome.bar, bar,
        "the settle used the same bar this test read"
    );
    let expected = rule_point_loc(1000, -500, Size::from((700, 500)), chrome);
    let landed = f.state().stage.position_of(&a).unwrap();
    assert_eq!(landed.x, expected.x);
    // The frame is 525 tall here, so its center sits on a half pixel and the
    // settle's truncation can land one above the direct map — the residual
    // `map_window_to_rule_point` documents. Dropping the bar would be twelve.
    assert!(
        (landed.y - expected.y).abs() <= 1,
        "landed {landed:?}, the direct map says {expected:?}"
    );
}

/// The bookmark binding mid fit-exit settle: a fit exit needs the same recenter
/// compensation a fullscreen exit gets, or the window lands against the
/// still-committed fit size with no recenter left to correct it.
#[test]
fn move_to_bookmark_mid_fit_exit_settle_lands_where_asked() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let key = owe_a_fit_exit_recenter(&mut f, id, &a_surface, &a);
    assert_ne!(
        a.geometry().size,
        Size::from((400, 300)),
        "precondition: the client still commits the fit-sized buffer"
    );

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.state().bookmarks.insert("b".into(), [1000.0, -500.0]);
    f.state()
        .execute_action(&Action::MoveToBookmark("b".into()));

    settle_at(&mut f, id, &a_surface, (700, 500));

    assert!(
        !f.state().pending_recenter.contains_key(&key),
        "the settle consumed the re-aimed recenter"
    );
    assert_eq!(
        f.state().stage.position_of(&a),
        Some(rule_point_loc(
            1000,
            -500,
            Size::from((700, 500)),
            Chrome::NONE
        )),
        "the window landed on the bookmark it was moved to"
    );
}

/// The bookmark binding mid fill-exit settle — the other exit the
/// fullscreen-only compensation never covered.
#[test]
fn move_to_bookmark_mid_fill_exit_settle_lands_where_asked() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let key = owe_a_fill_exit_recenter(&mut f, id, &a_surface, &a);
    assert_ne!(
        a.geometry().size,
        Size::from((400, 300)),
        "precondition: the client still commits the filled buffer"
    );

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.state().bookmarks.insert("b".into(), [1000.0, -500.0]);
    f.state()
        .execute_action(&Action::MoveToBookmark("b".into()));

    settle_at(&mut f, id, &a_surface, (700, 500));

    assert!(
        !f.state().pending_recenter.contains_key(&key),
        "the settle consumed the re-aimed recenter"
    );
    assert_eq!(
        f.state().stage.position_of(&a),
        Some(rule_point_loc(
            1000,
            -500,
            Size::from((700, 500)),
            Chrome::NONE
        )),
        "the window landed on the bookmark it was moved to"
    );
}

/// A move re-anchors the window, so it invalidates the restore point a fill
/// saved — an unfill afterwards would otherwise yank it back to where it was
/// filled from.
#[test]
fn ipc_move_clears_the_fill_restore_point() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    f.state().fill_window(&a);
    assert!(f.state().stage.is_fill(&a), "precondition: the fill took");
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    let ipc_id = f.state().stage.id_of(&a).unwrap().0;
    let reply = crate::ipc::dispatch(
        Request::Move {
            window: Some(WindowSelector::Id(ipc_id)),
            to: Some((1000, -500)),
        },
        f.state(),
    );
    assert!(matches!(reply, Ok(Response::Position { x: 1000, y: -500 })));
    assert!(
        !f.state().stage.is_fill(&a),
        "the move dropped the fill restore point it invalidated"
    );
}

/// A screen-pinned or fullscreen window has no canvas position, so `msg move`
/// refuses to write one rather than silently no-op'ing or displacing the park.
#[test]
fn ipc_move_refuses_pinned_and_fullscreen_windows() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    origin_view(&mut f);
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state()
        .map_window(a.clone(), Point::from((100, 100)), false);
    let ipc_id = f.state().stage.id_of(&a).unwrap().0;
    let move_it = |f: &mut Fixture| {
        crate::ipc::dispatch(
            Request::Move {
                window: Some(WindowSelector::Id(ipc_id)),
                to: Some((1000, -500)),
            },
            f.state(),
        )
    };

    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    f.state().execute_action(&Action::TogglePinToScreen);
    assert!(f.state().is_pinned(&a), "precondition: the pin took");
    assert!(move_it(&mut f).is_err(), "a pinned window refuses the move");

    f.state().execute_action(&Action::TogglePinToScreen);
    let cw = f.client(id).window(&a_surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);
    assert!(
        f.state().stage.is_fullscreen(&a),
        "precondition: fullscreen"
    );
    let parked = f.state().stage.position_of(&a);

    assert!(
        move_it(&mut f).is_err(),
        "a fullscreen window refuses the move"
    );
    assert_eq!(
        f.state().stage.position_of(&a),
        parked,
        "and stays parked at its camera origin"
    );
}

/// A panel that destroys its layer role and takes a fresh one on the same
/// `wl_surface` gets an initial configure for the new role. Left in the output's
/// map, the dead role is what the recreate's first commit finds — lookups match
/// by `wl_surface` in map order — so the configure goes out on the destroyed
/// proxy while marking the shared attributes as configured, and the client waits
/// forever for a size it will never hear.
#[test]
fn a_recreated_layer_role_gets_its_own_initial_configure() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let layer = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Overlay, "panel");
    let surface = layer.surface.clone();
    // Unanchored, so the compositor can't derive a size from anchor edges: every
    // commit needs a non-zero requested size or smithay kills the client.
    layer.set_configure_props(super::client::LayerConfigureProps {
        size: Some((400, 300)),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    layer.set_size(400, 300);
    layer.attach_new_buffer();
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    // The destroy wipes the role's cached state, so the size has to be
    // requested again before this commit.
    let layer =
        f.client(id)
            .recreate_layer(&surface, None, zwlr_layer_shell_v1::Layer::Overlay, "panel");
    layer.set_configure_props(super::client::LayerConfigureProps {
        size: Some((400, 300)),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);

    let configures = f.client(id).layer(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 400 × 300"),
        "the recreated role must be configured in its own right, got:\n{configures}"
    );
    assert_eq!(
        f.state().layers_on_sorted(&output, Layer::Overlay).len(),
        1,
        "and the dead role must not linger in the map, where it would still \
         claim an exclusive zone and sit above the live one for focus"
    );
}

/// An orphaned commit that also attaches a buffer hits smithay's
/// ack-before-attach check on the destroyed role (see
/// `dev/docs/smithay-api.md`'s Layer Shell section) — an OSD re-arming
/// (destroy + recreate the role on the same `wl_surface`) commits a buffer
/// in between and used to get killed.
///
/// Without the fix, this doesn't fail on a caught protocol error —
/// `post_error` on a destroyed proxy is never serialized (its id is already
/// gone from the wire's object map), so the client only sees the socket EOF.
/// That surfaces as `Client::dispatch`'s `self.event_loop.dispatch(...)
/// .unwrap()` (`src/tests/client.rs:359-362`) panicking with a bare
/// `Broken pipe (os error 32)` — not a `protocol_error()`, which maps
/// `Io(_)` to `None` and so never sees it either way. Don't assert on it.
#[test]
fn an_orphaned_layer_commit_with_a_buffer_does_not_kill_the_client() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let layer = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Overlay, "panel");
    let surface = layer.surface.clone();
    // Unanchored, so the compositor can't derive a size from anchor edges: every
    // commit needs a non-zero requested size or smithay kills the client.
    layer.set_configure_props(super::client::LayerConfigureProps {
        size: Some((400, 300)),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    layer.set_size(400, 300);
    layer.attach_new_buffer();
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    // Destroy the role but leave the wl_surface alive, as an OSD re-arming does.
    f.client(id).layer(&surface).layer_surface.destroy();
    f.double_roundtrip(id);

    // An orphaned commit that also attaches a buffer — no live role to ack or
    // configure it.
    let layer = f.client(id).layer(&surface);
    layer.attach_new_buffer();
    layer.commit();
    f.double_roundtrip(id);

    // The client survives: map an unrelated plain toplevel afterwards and
    // confirm it actually reaches the compositor, not just that the call
    // returned.
    map_settled(&mut f, id, "still-alive", (400, 300));
    assert!(
        window_by_app_id(&mut f, "still-alive").is_some(),
        "the client must survive the orphaned buffered commit"
    );
}

/// The buffer-stripping fix above has its own poison: `LayerSurfaceCachedState`
/// never resets `pending` on commit, so the full anchors our hook writes to
/// neutralise the orphaned commit survive into the *next* role taken on the
/// same `wl_surface`. Anchored on all four edges, the recreated role would be
/// sized to the whole output regardless of what it requests — an OSD re-arming
/// would survive the crash only to render fullscreen. The fix must reset the
/// anchor when the new role is taken.
#[test]
fn a_role_recreated_after_an_orphaned_buffered_commit_is_not_poisoned_to_full_output_size() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let layer = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Overlay, "panel");
    let surface = layer.surface.clone();
    layer.set_configure_props(super::client::LayerConfigureProps {
        size: Some((400, 300)),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    layer.set_size(400, 300);
    layer.attach_new_buffer();
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    // Destroy the role, then an orphaned commit that attaches a buffer — this
    // is what writes the full anchors into the shared cached state.
    f.client(id).layer(&surface).layer_surface.destroy();
    f.double_roundtrip(id);
    let layer = f.client(id).layer(&surface);
    layer.attach_new_buffer();
    layer.commit();
    f.double_roundtrip(id);

    // Recreate the role on the same wl_surface and request a small size again,
    // as the destroy wipes the role's own cached state either way.
    // (recreate_layer opens by destroying the proxy it tracks — already dead
    // from the destroy above, so that call is inert; it is not a second
    // destroy the compositor sees.)
    let layer =
        f.client(id)
            .recreate_layer(&surface, None, zwlr_layer_shell_v1::Layer::Overlay, "panel");
    layer.set_configure_props(super::client::LayerConfigureProps {
        size: Some((400, 300)),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);

    let configures = f.client(id).layer(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 400 × 300"),
        "the recreated role must be configured at its own requested size, got:\n{configures}"
    );
    assert!(
        !configures.contains("1920 × 1080"),
        "not resurrected full anchors sizing it to the whole output, got:\n{configures}"
    );
}

/// A layer surface that arrives with no output to host it is closed, not
/// dropped. The role stays live either way, so a client that hears nothing sits
/// on a surface it must not commit to and waits on a configure that is never
/// coming; `closed` is the only thing that lets it retry or exit.
#[test]
fn a_layer_surface_with_no_output_is_closed() {
    let mut f = Fixture::new();
    let id = f.add_client();

    let layer = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Overlay, "panel");
    let surface = layer.surface.clone();
    f.double_roundtrip(id);

    let layer = f.client(id).layer(&surface);
    assert!(
        layer.close_requested,
        "an unhostable layer surface must be told to close"
    );
    assert!(
        layer.configures_received.is_empty(),
        "and must not be configured for an output it was never given"
    );
}
