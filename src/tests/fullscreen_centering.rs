//! A client that commits smaller than its fullscreen output is centred in it
//! rather than left pinned to the top-left corner the park mapped it at.
//! Centring moves the *mapped* stage position, so hit-testing follows it for
//! free, and the settled fullscreen-cull predicate has to keep reading the
//! output as covered even though the window no longer sits exactly on the camera
//! origin.
//!
//! The two answer shapes behave differently on the way in. A client answering at
//! a size it was not already at resolves the fullscreen-entry chase immediately;
//! one answering by re-committing the size it already had does not, because
//! `WindowAnimations::on_window_commit` reads a size-for-size identical commit as
//! "nothing new happened". Both are centred at once, but only the first reads as
//! covering the output straight away — the second waits out the entry
//! animation's real-time endpoint hold, which is why the scenarios asserting
//! coverage for it drive the clock rather than `tick_until_settled`.

use std::time::{Duration, Instant};

use smithay::desktop::Window;
use smithay::output::Output;
use smithay::utils::{Point, Rectangle};

use super::client::ClientId;
use super::input_backend::{FakeDevice, pointer_to};
use super::{Fixture, adopt_last_configure, map_window, tick_until_settled, window_by_app_id};

const TICK: Duration = Duration::from_millis(16);

/// A window mapped at `map_size` on a fresh `1920x1080` output, then made
/// fullscreen on it. The client has not yet answered the fullscreen configure.
fn fullscreen_window(
    f: &mut Fixture,
    map_size: (u16, u16),
) -> (
    ClientId,
    wayland_client::protocol::wl_surface::WlSurface,
    Output,
    Window,
) {
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(f, id, "fs", map_size);
    let window = window_by_app_id(f, "fs").unwrap();
    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    (id, surface, output, window)
}

/// Ack the fullscreen configure but commit at `size` instead of the offered
/// one — the client-chosen size a fixed-aspect-ratio game or dialog answers
/// with.
fn ack_fullscreen_at(
    f: &mut Fixture,
    id: ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
    size: (u16, u16),
) {
    f.double_roundtrip(id);
    let window = f.client(id).window(surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
}

/// A plain redraw at a new size, with no configure to ack — a client resizing
/// itself after it has already answered the fullscreen offer.
fn commit_at(
    f: &mut Fixture,
    id: ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
    size: (u16, u16),
) {
    let window = f.client(id).window(surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);
}

/// A client that acks the fullscreen configure but takes a size smaller than the
/// output is moved off the parked corner by half the shortfall on each axis — and
/// the settled predicate the render cull gates on must still read the output as
/// covered, because it backs that offset out to recover the park.
#[test]
fn a_smaller_fullscreen_commit_is_centred_and_still_reads_as_covering_the_output() {
    let mut f = Fixture::new();
    let (id, surface, output, window) = fullscreen_window(&mut f, (700, 500));
    let camera = f.state().camera().to_i32_round();

    ack_fullscreen_at(&mut f, id, &surface, (800, 600));
    tick_until_settled(&mut f);

    let position = f.state().stage.position_of(&window).expect("staged");
    assert_eq!(
        position - camera,
        Point::from((560, 240)),
        "an 800x600 answer on a 1920x1080 output sits half the shortfall in on each axis"
    );
    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "the cull gate must still read the output as covered once the window is centred"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The centring moves the mapped stage position, so a pointer at the visual
/// centre of the output resolves to the centred window, and a pointer in the
/// black band it left uncovered resolves to no window at all.
#[test]
fn a_pointer_at_the_output_centre_hits_the_centred_window_and_the_band_hits_nothing() {
    let mut f = Fixture::new();
    let (id, surface, output, window) = fullscreen_window(&mut f, (700, 500));
    let camera = f.state().camera();

    ack_fullscreen_at(&mut f, id, &surface, (800, 600));

    let viewport = crate::state::output_logical_size(&output).to_f64();
    let centre = Point::from((camera.x + viewport.w / 2.0, camera.y + viewport.h / 2.0));
    let hit = f.state().element_under(centre).map(|(w, _)| w.clone());
    assert_eq!(
        hit,
        Some(window.clone()),
        "the output's visual centre hits the centred window"
    );

    let band = Point::from((camera.x + 5.0, camera.y + 5.0));
    assert!(
        f.state().element_under(band).is_none(),
        "the black band the centring left uncovered hits no window"
    );

    f.state().exit_fullscreen_on(&output);
}

/// A client that commits at exactly the offered size is untouched: it keeps
/// sitting on the plain camera-origin park and records no centring offset —
/// the ordinary case must see no behaviour change.
#[test]
fn a_compliant_fullscreen_commit_keeps_the_plain_park_with_a_zero_offset() {
    let mut f = Fixture::new();
    let (id, surface, output, window) = fullscreen_window(&mut f, (800, 600));
    let camera = f.state().camera().to_i32_round();

    adopt_last_configure(&mut f, id, &surface);

    let position = f.state().stage.position_of(&window).expect("staged");
    assert_eq!(
        position, camera,
        "a client that takes the offered size stays parked, not centred"
    );
    assert_eq!(
        f.state()
            .stage
            .fullscreen_on(&output.name())
            .unwrap()
            .centre_offset,
        Point::default(),
        "no offset is recorded for a client that took the offered size"
    );

    f.state().exit_fullscreen_on(&output);
}

/// A commit that answers with a size smaller than the output but never acks the
/// fullscreen configure must not centre the window: acting on geometry that
/// still owes an answer to a live configure would fling the window to the middle
/// and slide it back once the real ack lands.
#[test]
fn an_unacked_fullscreen_commit_does_not_centre_the_window() {
    let mut f = Fixture::new();
    let (id, surface, output, window) = fullscreen_window(&mut f, (700, 500));
    let camera = f.state().camera().to_i32_round();

    // A differently-sized commit with no ack: registers as an answer to the
    // *chase*, but the fullscreen configure itself is still outstanding.
    f.double_roundtrip(id);
    commit_at(&mut f, id, &surface, (800, 600));

    assert_eq!(
        window.geometry().size,
        smithay::utils::Size::from((800, 600)),
        "precondition: the commit actually landed a new size"
    );
    let position = f.state().stage.position_of(&window).expect("staged");
    assert_eq!(
        position, camera,
        "an unacked commit must not move the window off the park"
    );
    assert_eq!(
        f.state()
            .stage
            .fullscreen_on(&output.name())
            .unwrap()
            .centre_offset,
        Point::default(),
        "no offset is recorded while the fullscreen configure is unacked"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The fixed-size client — the whole reason this exists. It answers the
/// fullscreen offer by re-committing the size it already had, which the
/// fullscreen-entry chase reads as no answer at all. Centring must not be gated
/// on that chase, or the clients it is for are the ones it never reaches.
#[test]
fn a_fixed_size_client_that_re_commits_its_own_size_is_still_centred() {
    let mut f = Fixture::new();
    let (id, surface, output, window) = fullscreen_window(&mut f, (800, 600));
    let camera = f.state().camera().to_i32_round();

    ack_fullscreen_at(&mut f, id, &surface, (800, 600));

    let position = f.state().stage.position_of(&window).expect("staged");
    assert_eq!(
        position - camera,
        Point::from((560, 240)),
        "a client that answers with the size it already had is centred like any other"
    );
    assert_eq!(
        f.state()
            .stage
            .fullscreen_on(&output.name())
            .unwrap()
            .centre_offset,
        Point::from((560, 240)),
        "and the move is recorded, without which the cull gate loses the output"
    );

    // Its entry chase never saw an answer, so it sits on the start hold and then
    // the endpoint hold, both of which release on wall-clock deadlines rather
    // than on ticks. `tick_until_settled` cannot outrun them; injecting a `now`
    // that walks past both can.
    let base = Instant::now();
    for step in 0..200 {
        f.state()
            .tick_window_animations_at(TICK, base + TICK * step);
    }
    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "the cull gate reads a centred fixed-size client as covering its output"
    );

    f.state().exit_fullscreen_on(&output);
}

/// Centring moves the window out from under a frozen cursor. A locked cursor
/// cannot follow on its own, and one left in the vacated band re-picks onto no
/// surface at all — the `leave` that carries tears the lock down. So the cursor
/// travels the same distance the window did, and the re-seat then finds it over
/// the surface it was already on.
///
/// The cursor has to start where the centred rect does *not* reach it, which the
/// test asserts rather than assumes: a cursor the move happens to leave inside
/// the rect is already spared by `refresh_pointer_focus`'s own unchanged-focus
/// guard, and would pass whether or not it was carried.
#[test]
fn centring_a_locked_game_carries_the_cursor_and_keeps_its_lock() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();

    let position = f.state().stage.position_of(&window).unwrap().to_f64();
    pointer_to(
        &mut f,
        &FakeDevice::mouse(),
        position + Point::from((20.0, 20.0)),
    );
    f.roundtrip(id);
    let _lock = f.client(id).lock_pointer(&surface);
    f.double_roundtrip(id);
    assert!(
        f.state().pointer_constraint_active(),
        "precondition: the lock must arm, or this scenario tests nothing"
    );

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    f.client(id).state.pointer_positions.clear();
    let before = f.state().seat.get_pointer().unwrap().current_location();

    ack_fullscreen_at(&mut f, id, &surface, (400, 300));

    let centred = centred_rect(&mut f, &window);
    assert!(
        !centred.contains(before),
        "precondition: the frozen cursor must start outside the centred rect \
         (cursor {before:?}, rect {centred:?})"
    );
    let cursor = f.state().seat.get_pointer().unwrap().current_location();
    assert!(
        centred.contains(cursor),
        "the frozen cursor is carried along with the picture it is locked to \
         (cursor {cursor:?}, rect {centred:?})"
    );
    assert!(
        f.state().pointer_constraint_locked(),
        "so the re-seat finds an unchanged focus and the lock survives it"
    );
    assert_eq!(
        f.client(id).state.pointer_positions,
        Vec::new(),
        "and the locked client is handed no absolute jump"
    );

    f.state().exit_fullscreen_on(&output);
}

/// A confined client takes the same carry. Its cursor really moves, so unlike
/// the locked one it is told where to — but it has to arrive *inside* the
/// window, or the re-pick drops the confine exactly as it would a lock.
#[test]
fn centring_a_confined_game_carries_the_cursor_and_keeps_its_confine() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();

    let position = f.state().stage.position_of(&window).unwrap().to_f64();
    pointer_to(
        &mut f,
        &FakeDevice::mouse(),
        position + Point::from((20.0, 20.0)),
    );
    f.roundtrip(id);
    let _confine = f.client(id).confine_pointer(&surface);
    f.double_roundtrip(id);
    assert!(
        f.state().pointer_constraint_active(),
        "precondition: the confine must arm, or this scenario tests nothing"
    );

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    f.client(id).state.pointer_positions.clear();
    let before = f.state().seat.get_pointer().unwrap().current_location();

    ack_fullscreen_at(&mut f, id, &surface, (400, 300));
    f.double_roundtrip(id);

    let centred = centred_rect(&mut f, &window);
    assert!(
        !centred.contains(before),
        "precondition: the cursor must start outside the centred rect \
         (cursor {before:?}, rect {centred:?})"
    );
    let cursor = f.state().seat.get_pointer().unwrap().current_location();
    assert!(
        centred.contains(cursor),
        "the confined cursor is carried into the window its region moved with \
         (cursor {cursor:?}, rect {centred:?})"
    );
    assert!(
        f.state().pointer_constraint_active(),
        "so the confine survives the re-seat"
    );
    let local = *f
        .client(id)
        .state
        .pointer_positions
        .last()
        .expect("a confined cursor really moved, so the client is told where to");
    assert!(
        (0.0..centred.size.w).contains(&local.0) && (0.0..centred.size.h).contains(&local.1),
        "and it is told a position on the window it committed, not the one it left \
         ({local:?} outside {:?})",
        centred.size
    );

    f.state().exit_fullscreen_on(&output);
}

/// The point on the window the cursor sits on in the two scenarios below, near
/// enough the middle of the shrunk 700x500 rect that the carry lands it well
/// inside and the clamp never fires.
const CURSOR_LOCAL: Point<f64, smithay::utils::Logical> = Point::new(200.0, 150.0);

/// Put a locked cursor at [`CURSOR_LOCAL`] on the *parked* fullscreen window and
/// shrink it gently, so the shifted cursor lands inside the new rect on its own.
///
/// The two scenarios above assert *membership*, which the clamp alone
/// guarantees: it bounds the cursor to `[origin, origin + size - 1]`, which
/// `Rectangle::contains` reads as inside for every input, so they hold for a
/// carry that was dropped as much as one that ran. What the carry is for is the
/// point on the window, and only a shift the clamp does not touch can show it.
///
/// 800x600 -> 700x500 on a 1920x1080 output moves the window by
/// `((1920 - 700) / 2, (1080 - 500) / 2)` = (610, 290), so a cursor at
/// surface-local (200, 150) travels to (810, 440) from the park — (200, 150) of
/// the new rect, clear of all four edges. Drop the carry and the clamp drags the
/// unshifted cursor to the rect's origin instead: surface-local (0, 0), which is
/// still inside it, still over the same surface, and still locked.
#[test]
fn centring_a_gently_shrinking_game_holds_the_locked_cursor_on_the_same_point() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();

    // Park first, then aim: the position a window is mapped at says nothing
    // about where on it the cursor sits, and this scenario needs that exact.
    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    let parked = f.state().stage.position_of(&window).unwrap().to_f64();
    pointer_to(&mut f, &FakeDevice::mouse(), parked + CURSOR_LOCAL);
    f.roundtrip(id);
    let _lock = f.client(id).lock_pointer(&surface);
    f.double_roundtrip(id);
    assert!(
        f.state().pointer_constraint_active(),
        "precondition: the lock must arm, or this scenario tests nothing"
    );

    ack_fullscreen_at(&mut f, id, &surface, (700, 500));

    let centred = centred_rect(&mut f, &window);
    assert!(
        CURSOR_LOCAL.x < centred.size.w - 1.0 && CURSOR_LOCAL.y < centred.size.h - 1.0,
        "precondition: the carried cursor must land clear of the clamp's far \
         edge, or this asserts the same tautology the scenarios above do \
         ({CURSOR_LOCAL:?} against {:?})",
        centred.size
    );
    let cursor = f.state().seat.get_pointer().unwrap().current_location();
    assert_eq!(
        cursor - centred.loc,
        CURSOR_LOCAL,
        "the frozen cursor holds the point on the window it was locked over"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The same gentle shrink under a confine, where the preserved point is
/// observable from the client: a confined cursor really moved, so it is handed
/// the surface-local position it arrived at — which must be the one it left.
#[test]
fn centring_a_gently_shrinking_game_tells_a_confined_client_the_same_point() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    let parked = f.state().stage.position_of(&window).unwrap().to_f64();
    pointer_to(&mut f, &FakeDevice::mouse(), parked + CURSOR_LOCAL);
    f.roundtrip(id);
    let _confine = f.client(id).confine_pointer(&surface);
    f.double_roundtrip(id);
    assert!(
        f.state().pointer_constraint_active(),
        "precondition: the confine must arm, or this scenario tests nothing"
    );
    f.client(id).state.pointer_positions.clear();

    ack_fullscreen_at(&mut f, id, &surface, (700, 500));
    f.double_roundtrip(id);

    let local = *f
        .client(id)
        .state
        .pointer_positions
        .last()
        .expect("a confined cursor really moved, so the client is told where to");
    assert_eq!(
        local,
        (CURSOR_LOCAL.x, CURSOR_LOCAL.y),
        "the client is told the point its cursor was already on, not the corner \
         a dropped carry would leave the clamp to pick"
    );

    f.state().exit_fullscreen_on(&output);
}

/// Where the window ended up, in canvas coordinates.
fn centred_rect(f: &mut Fixture, window: &Window) -> Rectangle<f64, smithay::utils::Logical> {
    let origin = f.state().stage.position_of(window).unwrap().to_f64();
    let size = window.geometry().size;
    Rectangle::new(origin, (size.w as f64, size.h as f64).into())
}

/// A second answer at a different size re-centres from the position the first
/// one already moved: the write backs the stored offset out before adding the
/// new one, so the offsets telescope rather than accumulating.
#[test]
fn a_second_smaller_commit_re_centres_rather_than_stacking_offsets() {
    let mut f = Fixture::new();
    let (id, surface, output, window) = fullscreen_window(&mut f, (700, 500));
    let camera = f.state().camera().to_i32_round();

    ack_fullscreen_at(&mut f, id, &surface, (800, 600));
    tick_until_settled(&mut f);
    commit_at(&mut f, id, &surface, (1000, 700));

    let position = f.state().stage.position_of(&window).expect("staged");
    assert_eq!(
        position - camera,
        Point::from((460, 190)),
        "the second answer is centred from the park, not from where the first left it"
    );
    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "and the cull gate survives a re-centre"
    );

    f.state().exit_fullscreen_on(&output);
}

/// Exiting fullscreen from a centred window restores the exact pre-fullscreen
/// position, not the centred one — `saved_location` was captured before the
/// centring ever ran, from the same pre-fullscreen geometry every plain exit
/// restores from.
#[test]
fn exiting_fullscreen_from_a_centred_window_restores_the_pre_fullscreen_position() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (600, 400));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    let pre = f.state().stage.position_of(&window).expect("staged");

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    ack_fullscreen_at(&mut f, id, &surface, (650, 450));

    assert_ne!(
        f.state()
            .stage
            .fullscreen_on(&output.name())
            .unwrap()
            .centre_offset,
        Point::default(),
        "the scenario needs a real centring offset to prove the exit undoes it"
    );

    f.state().exit_fullscreen_on(&output);
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(pre),
        "exit restores the exact pre-fullscreen position, not the centred one"
    );
}

/// A commit larger than the output clamps to a zero offset rather than a
/// negative one: `max(0)` on each axis, not a window pushed past the origin.
#[test]
fn an_oversized_fullscreen_commit_yields_a_zero_offset() {
    let mut f = Fixture::new();
    let (id, surface, output, window) = fullscreen_window(&mut f, (400, 300));
    let camera = f.state().camera().to_i32_round();

    ack_fullscreen_at(&mut f, id, &surface, (2200, 1300));

    let position = f.state().stage.position_of(&window).expect("staged");
    assert_eq!(
        position, camera,
        "a commit bigger than the output is not offset at all"
    );
    assert_eq!(
        f.state()
            .stage
            .fullscreen_on(&output.name())
            .unwrap()
            .centre_offset,
        Point::default(),
        "an over-sized commit clamps to a zero offset, not a negative one"
    );

    f.state().exit_fullscreen_on(&output);
}
