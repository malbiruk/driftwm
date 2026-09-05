//! `refresh_pointer_focus`'s dedup guard: a re-seat that would put nothing new
//! on the wire must be skipped, but never at the cost of a delivery the client
//! is actually owed. Related coverage lives in `layer_destroy_focus.rs` (a
//! layer destroy revealing a focused window) and `pointer_constraints.rs`
//! (lock/confine interaction and the CSD-shadow-drop scenario, which needs
//! that module's private `shadowed_window`).
//!
//! Frame grouping under a live grab (motion + `relative_motion` in one frame)
//! is not covered here: the fixture records positions in a `Vec` and frames in
//! a separate counter, losing the interleaving such an assertion would need.
//! The scroll-pan axis path is not covered here either.

use smithay::input::pointer::{Focus, GrabStartData};
use smithay::utils::{Point, SERIAL_COUNTER};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1;

use driftwm::canvas::{ScreenPos, screen_to_canvas};
use driftwm::config::BTN_LEFT;

use crate::grabs::MoveGrab;
use crate::state::StageWindow;

use super::input_backend::{FakeDevice, pointer_to, pointer_to_screen, press, release};
use super::{
    Fixture, assert_click_grab, config, map_top_layer, map_window, pointer_focus, server_surface,
    window_by_app_id,
};

/// A layer surface that maps clear of the cursor and is destroyed must not
/// deliver a spurious motion to the still-focused window underneath: the
/// destroy's re-seat lands on the same target at the same point, so it has
/// nothing new to send.
///
/// A cursor over bare canvas does not exercise this: with nothing focused
/// before or after, smithay's own `PointerInternal::motion` is already a
/// no-op for a `None`-to-`None` transition, independent of this guard.
#[test]
fn a_layer_map_and_destroy_elsewhere_sends_the_focused_window_nothing() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();
    let pos = Point::from((600, 400));
    f.state()
        .map_window(StageWindow::Client(window.clone()), pos, false);

    // Pinned after the window is placed, not before: an earlier pin risks
    // getting silently redefined by whatever `map_window`/placement does
    // internally.
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });

    let center = pos.to_f64() + Point::from((200.0, 150.0));
    pointer_to(&mut f, &device, center);
    f.roundtrip(id);
    assert_eq!(
        pointer_focus(&mut f),
        Some(server_surface(&window)),
        "the cursor must start focused on the window, or this scenario tests nothing"
    );

    let positions_before = f.client(id).state.pointer_positions.len();
    let frames_before = f.client(id).state.pointer_frames;

    // Well clear of the window and the cursor.
    let layer = map_top_layer(
        &mut f,
        id,
        "osd",
        (200, 100),
        Some(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left),
    );
    assert_eq!(
        pointer_focus(&mut f),
        Some(server_surface(&window)),
        "the layer must not cover the cursor, and mapping it must not itself \
         re-seat focus, or this scenario tests nothing"
    );

    f.client(id).layer(&layer).layer_surface.destroy();
    f.client(id).layer(&layer).surface.destroy();
    f.double_roundtrip(id);

    assert_eq!(
        pointer_focus(&mut f),
        Some(server_surface(&window)),
        "the window must still be the one focused after the teardown, or \
         this scenario tests nothing"
    );
    assert_eq!(
        f.client(id).state.pointer_positions.len(),
        positions_before,
        "the destroy's re-seat lands on the same window at the same local \
         point, so it must put nothing on the wire"
    );
    assert_eq!(
        f.client(id).state.pointer_frames,
        frames_before,
        "no motion means no frame either"
    );
}

/// Same map/destroy shape as the previous test, but the window the teardown
/// reveals moved *while it was hidden* behind the layer. The guard compares
/// the delivered local point, not just the focus target — keying on the
/// target alone would wrongly call this redundant, since the target itself
/// never changed while the layer was up.
#[test]
fn a_window_whose_origin_moved_behind_a_dying_layer_gets_the_corrected_point() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();
    let pos0 = Point::from((600, 400));
    f.state()
        .map_window(StageWindow::Client(window.clone()), pos0, false);

    // Pinned after the window is placed — see the previous test's comment.
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });

    let center = pos0.to_f64() + Point::from((200.0, 150.0));
    pointer_to(&mut f, &device, center);
    f.roundtrip(id);
    assert_eq!(
        pointer_focus(&mut f),
        Some(server_surface(&window)),
        "the cursor must start over the window, or this scenario tests nothing"
    );

    // A small corner layer, well clear of the window and the cursor, so it
    // never becomes pointer focus itself — only its destroy matters here.
    let trigger = map_top_layer(
        &mut f,
        id,
        "trigger",
        (200, 100),
        Some(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left),
    );
    assert_eq!(
        pointer_focus(&mut f),
        Some(server_surface(&window)),
        "mapping the trigger layer must not itself re-seat focus, or this \
         scenario tests nothing"
    );

    // The window moves while the layer is up — but stays under the same
    // (stationary) cursor point, so this is purely an origin change, not a
    // window sliding out from under the cursor. Repositioning through the
    // stage alone, with no client commit, sends no pointer event of its own.
    let pos1 = Point::from((500, 350));
    f.state()
        .map_window(StageWindow::Client(window.clone()), pos1, false);

    f.client(id).state.pointer_positions.clear();
    let frames_before = f.client(id).state.pointer_frames;

    f.client(id).layer(&trigger).layer_surface.destroy();
    f.client(id).layer(&trigger).surface.destroy();
    f.double_roundtrip(id);

    let expected_local = center - pos1.to_f64();
    assert_eq!(
        f.client(id).state.pointer_positions,
        vec![(expected_local.x, expected_local.y)],
        "the reveal must deliver exactly one motion, carrying the local \
         point measured from the window's new origin"
    );
    assert_eq!(
        f.client(id).state.pointer_frames,
        frames_before + 1,
        "...paired with exactly one frame"
    );
}

/// Regression guard: the dedup key must include the surface origin, not just
/// `(focus, local_point)`. A window and the cursor carried by the *same*
/// delta leave the local point numerically unchanged even though the surface
/// genuinely moved to a different part of the canvas — keying on
/// `(focus, local_point)` alone would misread that as redundant and skip.
/// `fullscreen_centering.rs` covers the same shape via a real fullscreen
/// re-centre.
#[test]
fn a_window_and_cursor_carried_by_the_same_delta_still_gets_the_motion() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();
    let pos0 = Point::from((600, 400));
    f.state()
        .map_window(StageWindow::Client(window.clone()), pos0, false);

    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });

    let local = Point::from((200.0, 150.0));
    let cursor0 = pos0.to_f64() + local;
    pointer_to(&mut f, &device, cursor0);
    f.roundtrip(id);
    assert_eq!(
        pointer_focus(&mut f),
        Some(server_surface(&window)),
        "the cursor must start over the window, or this scenario tests nothing"
    );

    // Window and cursor both move by the same delta: the surface-local point
    // stays numerically identical, but the surface itself really did move —
    // it is a different rectangle of the canvas now under the cursor.
    let delta = Point::from((300, 200));
    let pos1 = Point::from((pos0.x + delta.x, pos0.y + delta.y));
    f.state()
        .map_window(StageWindow::Client(window.clone()), pos1, false);
    f.state().warp_pointer(cursor0 + delta.to_f64());

    f.client(id).state.pointer_positions.clear();
    let frames_before = f.client(id).state.pointer_frames;

    let trigger = map_top_layer(
        &mut f,
        id,
        "trigger",
        (200, 100),
        Some(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left),
    );
    f.client(id).layer(&trigger).layer_surface.destroy();
    f.client(id).layer(&trigger).surface.destroy();
    f.double_roundtrip(id);

    assert_eq!(
        f.client(id).state.pointer_positions,
        vec![(local.x, local.y)],
        "the origin moved even though the local point didn't — a guard keyed \
         only on (focus, local_point) would call this redundant and skip; \
         the shipped key includes the origin so it still delivers exactly \
         one motion, carrying the same numeric point as before"
    );
    assert_eq!(
        f.client(id).state.pointer_frames,
        frames_before + 1,
        "...paired with exactly one frame"
    );
}

/// The commit hook has no focus gate: window A's geometry-loc change must
/// re-pick focus even when the pointer was resting on a *different* window, B,
/// when A's commit landed. A hook gated on "pointer currently focused on the
/// committing window" would pass every other test in this file — they all
/// commit geometry changes only on the window the cursor is already over —
/// and only fail here.
#[test]
fn a_geometry_loc_change_on_an_unfocused_window_still_repicks_focus() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    map_window(&mut f, id, "b", (200, 200));
    let b = window_by_app_id(&mut f, "b").unwrap();
    f.state().map_window(
        StageWindow::Client(b.clone()),
        Point::from((700, 400)),
        false,
    );

    // A's surface reaches far beyond its declared geometry, like a client
    // drawing a huge shadow. Dropping that inset later — without moving A's
    // stage position at all — expands A's hit-testable area to cover B,
    // purely through the geometry commit.
    let a_surface = map_window(&mut f, id, "a", (1100, 1100));
    let a = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(StageWindow::Client(a.clone()), Point::from((0, 0)), false);
    f.client(id)
        .window(&a_surface)
        .set_geometry(500, 500, 100, 100);
    f.client(id).window(&a_surface).commit();
    f.double_roundtrip(id);

    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });

    let center = Point::from((800.0, 500.0));
    pointer_to(&mut f, &device, center);
    f.roundtrip(id);
    assert_eq!(
        pointer_focus(&mut f),
        Some(server_surface(&b)),
        "the cursor must start focused on B, clear of A's shrunk footprint, \
         or this scenario tests nothing"
    );

    // A drops its inset without moving its stage position — its buffer now
    // reaches over B and the cursor, and A is on top (mapped after B).
    f.client(id).window(&a_surface).set_geometry(0, 0, 100, 100);
    f.client(id).window(&a_surface).commit();
    f.double_roundtrip(id);

    assert_eq!(
        a.geometry().loc,
        Point::from((0, 0)),
        "A's declared geometry must have actually moved, or this scenario \
         tests nothing"
    );
    assert_eq!(
        f.state().stage.position_of(&a),
        Some(Point::from((0, 0))),
        "A must still be at its original stage position — only its geometry \
         moved, or this scenario tests nothing"
    );

    assert_eq!(
        pointer_focus(&mut f),
        Some(server_surface(&a)),
        "A's own commit must re-pick focus onto itself even though the \
         pointer was resting on B, not A, when the commit landed — a focus \
         gate on the committing window would have skipped this re-seat"
    );
}

/// A confine keeps the cursor moving inside its surface, so a camera pan
/// through `warp_pointer` really does relocate it — confines are excluded
/// from the lock guard specifically so a later, unrelated scene change still
/// delivers the corrected point instead of treating the confined surface as
/// an unchanged target.
#[test]
fn a_confined_cursor_panned_by_warp_gets_the_corrected_point_on_the_next_refresh() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    let surface = map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();
    let pos = f.state().stage.position_of(&window).unwrap().to_f64();
    let center = pos + Point::from((200.0, 150.0));
    pointer_to(&mut f, &device, center);
    f.roundtrip(id);

    let _confine = f.client(id).confine_pointer(&surface);
    f.double_roundtrip(id);
    assert!(
        f.state().pointer_constraint_active() && !f.state().pointer_constraint_locked(),
        "the confine must activate over the window and must not read as a \
         lock, or this scenario tests nothing"
    );

    // Stays inside the window, so `warp_pointer` takes the set_location branch
    // that keeps the confine armed — a warp that left the surface would fall
    // through, drop the confine, and set `pending_pointer_resync` instead,
    // and this test would prove nothing.
    let panned = center + Point::from((40.0, 0.0));
    f.state().warp_pointer(panned);
    assert!(
        f.state().pointer_constraint_active(),
        "the warp must stay inside the confined surface and keep the \
         confine armed, or this scenario tests nothing"
    );

    // The unrelated scene change every re-seat follows a real one of — a
    // layer teardown, a window closing, a pin toggle, a fullscreen exit — is
    // called directly, matching the idiom already used for this in
    // `pointer_constraints.rs::a_panel_over_a_locked_cursor_takes_the_pointer`.
    let trigger = map_top_layer(
        &mut f,
        id,
        "trigger",
        (200, 100),
        Some(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left),
    );

    f.client(id).state.pointer_positions.clear();
    let frames_before = f.client(id).state.pointer_frames;

    f.client(id).layer(&trigger).layer_surface.destroy();
    f.client(id).layer(&trigger).surface.destroy();
    f.double_roundtrip(id);

    let expected_local = panned - pos;
    assert_eq!(
        f.client(id).state.pointer_positions,
        vec![(expected_local.x, expected_local.y)],
        "the refresh after a confined pan must deliver exactly one motion \
         carrying the point the pan actually moved the cursor to"
    );
    assert_eq!(
        f.client(id).state.pointer_frames,
        frames_before + 1,
        "...paired with exactly one frame"
    );
}

/// `MoveGrab` does real work in its own `motion` handler (`apply_move`,
/// edge-pan) and always forwards `None` as focus, so a refresh mid-grab with
/// nothing under the cursor is exactly the shape a guard could wrongly call
/// redundant (`old_focus`, `under`, and the record all `None`) if it didn't
/// special-case a live grab. It must still run the grab's `motion` and move
/// the window. `pointer_positions` can't witness this — `MoveGrab` never
/// names a focus target — so the drag's own effect is the assertion.
#[test]
fn a_refresh_mid_move_grab_over_empty_canvas_still_moves_the_window() {
    let mut f = Fixture::with_config(config("[snap]\nenabled = false\n"));
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "w", (200, 150));
    let window = window_by_app_id(&mut f, "w").unwrap();
    let initial = Point::from((100, 100));
    f.state()
        .map_window(StageWindow::Client(window.clone()), initial, false);

    let start = Point::from((150.0, 125.0));
    let pointer = f.state().seat.get_pointer().unwrap();
    pointer.set_location(start);

    let grab = MoveGrab::new(
        GrabStartData {
            focus: None,
            button: BTN_LEFT,
            location: start,
        },
        window.clone(),
        initial,
        output,
        Vec::new(),
    );
    let serial = SERIAL_COUNTER.next_serial();
    pointer.set_grab(f.state(), grab, serial, Focus::Clear);

    // Establishes `last_pointer_delivery == None` the way a real motion with
    // nothing under the cursor would — the grab forwards `None` regardless of
    // what `under` names, so this also holds when the cursor starts over the
    // window itself.
    f.state().refresh_pointer_focus();
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(initial),
        "the grab must not have moved the window before the cursor moved, \
         or this scenario tests nothing"
    );

    // Off the window, over bare canvas.
    let far = Point::from((1700.0, 900.0));
    let pointer = f.state().seat.get_pointer().unwrap();
    pointer.set_location(far);
    f.state().refresh_pointer_focus();

    // Mirrors `MoveGrab::apply_move`'s own formula (with snap disabled, so
    // the natural destination is the actual one): `initial + (far - start)`,
    // truncated to i32 the same way `apply_move` truncates `new_loc`.
    let delta = far - start;
    let expected = Point::from((
        (initial.x as f64 + delta.x) as i32,
        (initial.y as f64 + delta.y) as i32,
    ));
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(expected),
        "a refresh mid-grab must still run MoveGrab::motion and apply the \
         drag delta, even with nothing under the cursor to re-seat focus onto"
    );

    super::end_grab(&mut f);
}

/// A grab that keeps the same focus target but supplies its own location —
/// `ScreenSpaceClickGrab`, installed the only way a test can reach it, by a
/// real press over a layer — must clear the delivery record rather than write
/// `under` into it. A `MoveGrab` can't exercise this: it always passes
/// `None`, so `focus_unchanged` is already false and dispatch is forced
/// regardless of where the clear lives — this needs a grab that keeps the
/// target the same.
#[test]
fn a_screen_space_click_grab_clears_the_delivery_record() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    let bar = map_top_layer(
        &mut f,
        id,
        "bar",
        (1920, 40),
        Some(
            zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        ),
    );
    let over_bar = Point::from((960.0, 1060.0));
    pointer_to_screen(&mut f, &device, over_bar);
    f.roundtrip(id);

    // An ordinary motion over the bar must itself write the delivery record
    // (not only `refresh_pointer_focus`), or this would still read as
    // `init.rs`'s `None`.
    let canvas_pos = screen_to_canvas(ScreenPos(over_bar), f.state().camera(), f.state().zoom()).0;
    let (focus, origin) = f
        .state()
        .pointer_focus_under_pick(over_bar, canvas_pos)
        .expect("the bar must be under the cursor, or this scenario tests nothing");
    let expected_local = canvas_pos - origin;
    assert_eq!(
        f.state().last_pointer_delivery,
        Some((focus, origin, expected_local)),
        "an ordinary motion over the bar must record what was actually \
         delivered, or the negative check below is vacuous"
    );

    press(&mut f, &device, BTN_LEFT);
    assert_click_grab(
        &mut f,
        "the press over the bar must install a ScreenSpaceClickGrab, or this \
         scenario tests nothing",
    );

    // Still over the bar, so the caller's own `under` names the same target
    // the grab started with — exactly the case the record's clear has to
    // cover, since `focus_unchanged` alone can't tell this apart from a truly
    // redundant re-seat.
    pointer_to_screen(&mut f, &device, over_bar + Point::from((30.0, 0.0)));

    assert!(
        f.state().last_pointer_delivery.is_none(),
        "a live grab must clear the delivery record: ScreenSpaceClickGrab \
         changes only the loc and keeps the focus target it started with, so \
         recording `under` here would describe a delivery the grab never made"
    );

    release(&mut f, &device, BTN_LEFT);
    f.client(id).layer(&bar).layer_surface.destroy();
    f.client(id).layer(&bar).surface.destroy();
    f.double_roundtrip(id);
}

/// `restore_fullscreen_view` clears `pending_pointer_resync` on the premise
/// that its own `refresh_pointer_focus` call is the resync. Under the guard,
/// that call can now legitimately produce nothing — this pins that the skip
/// stays sound when it does: the pointer is still seated on the right
/// surface, at the right point, not merely an unchanged count against a
/// record that could itself be stale.
///
/// The flag's own clear is not asserted: `restore_fullscreen_view` clears it
/// unconditionally, so that half can't fail regardless of the guard — only
/// the skip's soundness, checked below, can.
#[test]
fn restore_fullscreen_view_leaves_the_client_seated_even_when_its_own_refresh_is_skipped() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    let surface = map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((0, 0)),
        false,
    );

    // An exact-identity view, pinned after the window is placed: the
    // fullscreen entry parks the camera at the (rounded) pre-entry camera, so
    // with both at the canvas origin already, and the window itself at the
    // origin, entry and exit round-trip the window's stage position and the
    // viewport back to themselves precisely — no rounding noise for the skip
    // to hide behind.
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });

    let center = Point::from((200.0, 150.0));
    pointer_to(&mut f, &device, center);
    f.roundtrip(id);
    assert_eq!(
        pointer_focus(&mut f),
        Some(server_surface(&window)),
        "the cursor must start over the window, or this scenario tests nothing"
    );

    assert_eq!(
        f.state().camera(),
        Point::from((0.0, 0.0)),
        "the skip below depends on the parked camera landing back at exactly \
         this point, or this scenario tests nothing"
    );
    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);

    let positions_before = f.client(id).state.pointer_positions.len();
    let frames_before = f.client(id).state.pointer_frames;

    f.state().exit_fullscreen_on(&output);
    f.double_roundtrip(id);

    assert_eq!(
        f.client(id).state.pointer_positions.len(),
        positions_before,
        "the exit's own refresh must recognize the client already has the \
         correct point and skip — an unmoved cursor over a window that came \
         back exactly where it started delivers nothing new, or this \
         scenario doesn't exercise the guard this test is about"
    );
    assert_eq!(
        f.client(id).state.pointer_frames,
        frames_before,
        "...and send no frame either"
    );

    let true_origin =
        (f.state().stage.position_of(&window).unwrap() - window.geometry().loc).to_f64();
    let cursor = f.state().seat.get_pointer().unwrap().current_location();
    let expected_local = cursor - true_origin;
    assert_eq!(
        f.client(id).state.pointer_positions.last(),
        Some(&(expected_local.x, expected_local.y)),
        "even with nothing new sent, the client's last known point must \
         already be the correct one — a skip against a stale record would \
         pass the count check above but leave the client with a wrong \
         coordinate forever"
    );
    assert_eq!(
        pointer_focus(&mut f),
        Some(server_surface(&window)),
        "despite the skip, the pointer must still be seated on the window"
    );
}
