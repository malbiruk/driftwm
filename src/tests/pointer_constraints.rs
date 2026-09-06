//! Pointer lock coordinate space, and staying quiet while the lock holds.
//!
//! A locked client keeps its own cursor and tells the compositor where it is
//! with `set_cursor_position_hint`, in surface-local coordinates. Convert that
//! from the wrong origin, or hand the client an absolute motion it never made,
//! and a client reading absolute positions reads the difference as camera
//! movement.
//!
//! The silence is conditional in both directions: a confined cursor really
//! moves and is owed its motion, and a lock whose surface the cursor has left
//! has to be let go, or it freezes there with nothing to release it.
//!
//! A constraint carrying a region arms only while the pointer is inside it,
//! and the region is surface-local, so the tests below also pin the origin
//! the compositor measures it from — the path that used to deadlock by
//! re-locking the surface's user-data mutex.

use driftwm::canvas::{CanvasPos, canvas_to_screen};
use smithay::utils::{Logical, Point};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::ZwpConfinedPointerV1;
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::ZwpLockedPointerV1;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1;

use super::client::ClientId;
use super::input_backend::{FakeDevice, pointer_relative_motion, pointer_to};
use super::{Fixture, map_top_layer, map_window, window_by_app_id};

const SHADOW: Point<i32, Logical> = Point::new(26, 23);
const GEOMETRY: (i32, i32) = (800, 600);

/// Surface-local position of the pointer after [`point_at_window_center`]: the
/// geometry centre, measured from the surface origin the shadow pushes out.
const CENTER_IN_SURFACE: (i32, i32) = (GEOMETRY.0 / 2 + SHADOW.x, GEOMETRY.1 / 2 + SHADOW.y);

/// A mapped window whose surface reaches [`SHADOW`] beyond its geometry on
/// every side, like a client drawing its own shadows. Returns the client
/// surface and the canvas-space *surface* origin, which sits [`SHADOW`] above
/// and left of the geometry origin the stage positions the window by.
fn shadowed_window(f: &mut Fixture, id: ClientId) -> (WlSurface, Point<f64, Logical>) {
    let surface = map_window(
        f,
        id,
        "game",
        (
            (GEOMETRY.0 + SHADOW.x * 2) as u16,
            (GEOMETRY.1 + SHADOW.y * 2) as u16,
        ),
    );
    f.client(id)
        .window(&surface)
        .set_geometry(SHADOW.x, SHADOW.y, GEOMETRY.0, GEOMETRY.1);
    f.client(id).window(&surface).commit();
    f.double_roundtrip(id);

    let window = window_by_app_id(f, "game").unwrap();
    assert_eq!(
        window.geometry().loc,
        SHADOW,
        "the window must carry the shadow offset, or this scenario tests nothing"
    );
    let position = f.state().stage.position_of(&window).unwrap();
    (surface, (position - SHADOW).to_f64())
}

/// Put the pointer over the window's center, where a constraint on it can arm.
fn point_at_window_center(f: &mut Fixture, id: ClientId) {
    let window = window_by_app_id(f, "game").unwrap();
    let position = f.state().stage.position_of(&window).unwrap().to_f64();
    pointer_to(
        f,
        &FakeDevice::mouse(),
        position + Point::from((GEOMETRY.0 as f64 / 2.0, GEOMETRY.1 as f64 / 2.0)),
    );
    f.roundtrip(id);
}

/// Put the pointer over the window's center and lock it there.
fn lock_pointer_over(f: &mut Fixture, id: ClientId, surface: &WlSurface) -> ZwpLockedPointerV1 {
    point_at_window_center(f, id);

    let lock = f.client(id).lock_pointer(surface);
    f.double_roundtrip(id);
    assert!(
        f.state().pointer_constraint_active(),
        "the lock must activate with the pointer over its surface, or this \
         scenario tests nothing"
    );
    lock
}

/// Put the pointer over the window's center and confine it to the surface.
fn confine_pointer_over(
    f: &mut Fixture,
    id: ClientId,
    surface: &WlSurface,
) -> ZwpConfinedPointerV1 {
    point_at_window_center(f, id);

    let confine = f.client(id).confine_pointer(surface);
    f.double_roundtrip(id);
    assert!(
        f.state().pointer_constraint_active() && !f.state().pointer_constraint_locked(),
        "the confine must activate with the pointer over its surface and must \
         not read as a lock, or this scenario tests nothing"
    );
    confine
}

/// The hint is surface-local, so it must be measured from the surface origin,
/// not the geometry origin the stage positions the window by.
#[test]
fn cursor_position_hint_lands_at_the_surface_origin() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (surface, surface_origin) = shadowed_window(&mut f, id);
    let lock = lock_pointer_over(&mut f, id, &surface);

    lock.set_cursor_position_hint(400.0, 300.0);
    f.client(id).window(&surface).commit();
    f.double_roundtrip(id);

    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        surface_origin + Point::from((400.0, 300.0)),
        "a surface-local hint must be measured from the surface origin"
    );
}

/// The pointer never moved — the client moved its own cursor — so re-creating
/// the lock must not replay the hinted position back as a `wl_pointer.motion`.
#[test]
fn relocking_does_not_replay_the_hint_as_motion() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (surface, _) = shadowed_window(&mut f, id);
    let lock = lock_pointer_over(&mut f, id, &surface);

    lock.set_cursor_position_hint(400.0, 300.0);
    f.client(id).window(&surface).commit();
    f.double_roundtrip(id);

    f.client(id).state.pointer_positions.clear();
    lock.destroy();
    let _relock = f.client(id).lock_pointer(&surface);
    f.double_roundtrip(id);

    assert_eq!(
        f.client(id).state.pointer_positions,
        Vec::new(),
        "tearing the lock down and putting it back must not deliver absolute \
         motion — the pointer is frozen and the client moved its own cursor"
    );
}

/// A scene change away from the cursor must not re-seat pointer focus through
/// the locked surface: the re-seat carries an absolute motion the locked client
/// cannot tell from a real one.
#[test]
fn a_layer_dying_elsewhere_sends_the_locked_client_nothing() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (surface, _) = shadowed_window(&mut f, id);
    let _lock = lock_pointer_over(&mut f, id, &surface);

    let layer_surface = map_top_layer(
        &mut f,
        id,
        "notification",
        (300, 100),
        Some(zwlr_layer_surface_v1::Anchor::Top),
    );

    f.client(id).state.pointer_positions.clear();
    f.client(id).layer(&layer_surface).layer_surface.destroy();
    f.client(id).layer(&layer_surface).surface.destroy();
    f.double_roundtrip(id);

    assert_eq!(
        f.client(id).state.pointer_positions,
        Vec::new(),
        "a layer teardown elsewhere on screen must not reach a locked client"
    );
}

/// Fullscreening a window that already holds the cursor lock must leave the
/// lock alone: dropping and re-arming it sends an absolute motion.
#[test]
fn fullscreening_a_locked_game_leaves_its_lock_untouched() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (surface, _) = shadowed_window(&mut f, id);
    let _lock = lock_pointer_over(&mut f, id, &surface);

    f.client(id).state.pointer_positions.clear();

    let window = window_by_app_id(&mut f, "game").unwrap();
    f.state().enter_fullscreen(&window, Some(output));
    f.double_roundtrip(id);

    assert!(
        f.state().pointer_constraint_locked(),
        "the game's lock must survive the fullscreen entry"
    );
    assert_eq!(
        f.client(id).state.pointer_positions,
        Vec::new(),
        "fullscreening must not hand the locked client an absolute jump"
    );
}

/// Silent is not the same as skipped. The entry parks the viewport at zoom 1,
/// which moves every canvas point on screen, and the frozen cursor has to be
/// carried along or it ends up somewhere it never was — off the fullscreen rect
/// entirely once the pre-fullscreen zoom was small enough.
#[test]
fn fullscreening_a_locked_game_still_relocates_its_cursor() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (surface, _) = shadowed_window(&mut f, id);
    // Zoomed out and panned away from where the entry parks the camera: at the
    // parked zoom 1 with the camera already there the relocation is the
    // identity, and the assertions below would hold whether or not it ran. The
    // pan is relative because the fixture's camera does not start at the canvas
    // origin — an absolute one moves the window off screen, and then the cursor
    // this scenario has to place cannot be reached by any device.
    f.state().set_zoom(0.5);
    let panned = f.state().camera() + Point::from((100.0, 50.0));
    f.state().set_camera(panned);
    let _lock = lock_pointer_over(&mut f, id, &surface);

    let camera_before = f.state().camera();
    let zoom_before = f.state().zoom();
    let frozen = f.state().seat.get_pointer().unwrap().current_location();
    let on_screen = canvas_to_screen(CanvasPos(frozen), camera_before, zoom_before).0;

    let window = window_by_app_id(&mut f, "game").unwrap();
    f.state().enter_fullscreen(&window, Some(output));
    f.double_roundtrip(id);

    let relocated = f.state().seat.get_pointer().unwrap().current_location();
    let camera_after = f.state().camera();
    let zoom_after = f.state().zoom();
    assert_ne!(
        relocated, frozen,
        "the entry rescales the viewport, so the cursor's canvas position has \
         to move with it"
    );
    assert_eq!(
        canvas_to_screen(CanvasPos(relocated), camera_after, zoom_after).0,
        on_screen,
        "a locked cursor must come out of the entry on the screen pixel it went \
         in on"
    );
}

/// A confine is not a freeze — that cursor really does move inside its surface
/// — so the relocation the fullscreen entry applies has to reach the client as
/// motion. Only a lock gets it silently.
#[test]
fn fullscreening_a_confined_client_sends_it_the_motion() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (surface, _) = shadowed_window(&mut f, id);
    let _confine = confine_pointer_over(&mut f, id, &surface);

    f.client(id).state.pointer_positions.clear();

    let window = window_by_app_id(&mut f, "game").unwrap();
    f.state().enter_fullscreen(&window, Some(output));
    f.double_roundtrip(id);

    assert!(
        !f.client(id).state.pointer_positions.is_empty(),
        "a confined cursor moves with the viewport, so the entry owes the \
         client the motion — nothing re-seats it afterwards"
    );
    assert!(
        f.state().pointer_constraint_active(),
        "and the confine must be back in force once the motion is out"
    );
}

/// A CSD client that drops its shadow inset on fullscreen gets a corrected
/// surface-local coordinate on its own commit, without waiting for an
/// unrelated refresh. `fullscreening_a_confined_client_sends_it_the_motion`
/// above doesn't exercise this because its client never changes geometry —
/// this one does, the way GTK drops its CSD margin on fullscreen.
///
/// Subsurface coverage is dropped: the fixture client never binds
/// `wl_subcompositor` and has no subsurface API to exercise.
#[test]
fn a_csd_client_dropping_its_shadow_on_fullscreen_is_corrected_on_its_own_commit() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (surface, _) = shadowed_window(&mut f, id);
    point_at_window_center(&mut f, id);

    let window = window_by_app_id(&mut f, "game").unwrap();
    assert_eq!(
        window.geometry().loc,
        SHADOW,
        "the window must still carry the shadow offset going into \
         fullscreen, or this scenario tests nothing"
    );

    f.state().enter_fullscreen(&window, Some(output));
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);

    // The entry's own dispatch used the geometry the client hadn't dropped
    // yet — captured here for contrast with the corrected delivery below.
    let stale_delivery = f.state().last_pointer_delivery.clone();

    // The client's second commit: same content size, but the geometry origin
    // moves to (0, 0) as the shadow inset is dropped.
    f.client(id)
        .window(&surface)
        .set_geometry(0, 0, GEOMETRY.0, GEOMETRY.1);
    f.client(id).window(&surface).commit();
    f.double_roundtrip(id);

    assert_eq!(
        window.geometry().loc,
        Point::from((0, 0)),
        "the shadow inset must actually be gone, or this scenario tests nothing"
    );

    let true_origin =
        (f.state().stage.position_of(&window).unwrap() - window.geometry().loc).to_f64();
    let cursor = f.state().seat.get_pointer().unwrap().current_location();
    let expected_local = cursor - true_origin;

    assert_eq!(
        f.client(id).state.pointer_positions.last(),
        Some(&(expected_local.x, expected_local.y)),
        "the commit that drops the shadow must itself correct the client's \
         surface-local coordinate, without waiting for an unrelated refresh \
         to run"
    );
    assert_ne!(
        f.state().last_pointer_delivery,
        stale_delivery,
        "the correction must actually differ from the stale, pre-commit \
         delivery, or this scenario tests nothing"
    );
}

/// The other half of the re-seat guard: a scene change that slides a *different*
/// surface under the frozen cursor strands the lock on a surface the cursor has
/// left. That one has to fall through, clearing the lock and handing the motion
/// to whatever the cursor is over now.
#[test]
fn a_panel_over_a_locked_cursor_takes_the_pointer() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let game_id = f.add_client();
    let panel_id = f.add_client();

    let (surface, _) = shadowed_window(&mut f, game_id);
    let _lock = lock_pointer_over(&mut f, game_id, &surface);

    // Output-sized, so it covers the cursor wherever the game was placed.
    // Mapping it doesn't re-seat pointer focus by itself.
    let _panel = map_top_layer(&mut f, panel_id, "panel", (1920, 1080), None);
    f.client(panel_id).state.pointer_positions.clear();

    // The re-seat every scene change ends with — a layer teardown, a window
    // closing, a pin toggle, a fullscreen exit.
    f.state().refresh_pointer_focus();
    f.double_roundtrip(game_id);
    f.double_roundtrip(panel_id);

    assert!(
        !f.state().pointer_constraint_locked(),
        "a lock must not outlive the cursor's surface being covered — the \
         cursor would freeze on a window it is no longer over"
    );
    assert!(
        !f.client(panel_id).state.pointer_positions.is_empty(),
        "the surface that took the cursor must get the motion"
    );
}

/// A camera pan drops a constraint without re-seating focus — that waits for the
/// next frame — so between the two, focus names a surface the cursor has left. A
/// lock re-created in that gap must not arm there: relative motion would take the
/// locked path and the deferred resync bails on an active constraint, leaving the
/// cursor stuck until the next pan.
#[test]
fn a_lock_recreated_after_the_cursor_left_does_not_arm() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (surface, _) = shadowed_window(&mut f, id);
    let lock = lock_pointer_over(&mut f, id, &surface);

    // What a camera pan does: the cursor holds its screen position while the
    // canvas slides beneath it, ending up clear of the window it was locked to.
    // Measured off the window rather than written as a fixed canvas point — the
    // fixture centers the window on a camera that is not at the canvas origin,
    // so a hand-picked constant lands *inside* the window and the warp keeps the
    // lock it is supposed to drop.
    let window = window_by_app_id(&mut f, "game").unwrap();
    let above_left = f.state().stage.position_of(&window).unwrap().to_f64();
    f.state()
        .warp_pointer(above_left - Point::from((100.0, 100.0)));
    assert!(
        !f.state().pointer_constraint_active(),
        "the warp must drop the lock it left behind, or this scenario tests \
         nothing"
    );

    // The relock a warp emulator issues on its next frame, while pointer focus
    // still names the game.
    lock.destroy();
    let _relock = f.client(id).lock_pointer(&surface);
    f.double_roundtrip(id);

    assert!(
        !f.state().pointer_constraint_active(),
        "a lock must not arm on a window the cursor has already left"
    );

    let before = f.state().seat.get_pointer().unwrap().current_location();
    pointer_relative_motion(&mut f, &FakeDevice::mouse(), Point::from((40.0, 25.0)));
    assert_ne!(
        f.state().seat.get_pointer().unwrap().current_location(),
        before,
        "the cursor must still be free to move"
    );
}

#[test]
fn a_locked_pointer_neither_moves_nor_reports_absolute_motion() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (surface, _) = shadowed_window(&mut f, id);
    let _lock = lock_pointer_over(&mut f, id, &surface);

    let frozen = f.state().seat.get_pointer().unwrap().current_location();
    f.client(id).state.pointer_positions.clear();

    pointer_relative_motion(&mut f, &FakeDevice::mouse(), Point::from((40.0, 25.0)));
    f.double_roundtrip(id);

    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        frozen,
        "a locked pointer must not move"
    );
    assert_eq!(
        f.client(id).state.pointer_positions,
        Vec::new(),
        "a locked client must see relative motion only"
    );
}

#[test]
fn a_region_around_the_pointer_arms_the_lock() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (surface, _) = shadowed_window(&mut f, id);
    point_at_window_center(&mut f, id);

    // Reaches less than `SHADOW` from the pointer, so measuring from the
    // geometry origin instead of the surface origin would put it outside.
    let _lock = f.client(id).lock_pointer_with_region(
        &surface,
        &[(
            CENTER_IN_SURFACE.0 - SHADOW.x + 1,
            CENTER_IN_SURFACE.1 - SHADOW.y + 1,
            SHADOW.x * 2 - 2,
            SHADOW.y * 2 - 2,
        )],
    );
    f.double_roundtrip(id);

    assert!(
        f.state().pointer_constraint_active(),
        "a lock whose region covers the pointer must arm"
    );
}

#[test]
fn a_lock_stays_disarmed_until_the_pointer_enters_its_region() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (surface, surface_origin) = shadowed_window(&mut f, id);
    point_at_window_center(&mut f, id);

    // Its centre sits less than `SHADOW` from the near edges, so the same wrong
    // origin would miss it on the way back in — which needs the offset to clear
    // both `SHADOW` components.
    let region = (30, 30, SHADOW.x * 2 - 2, SHADOW.y * 2 - 2);
    let _lock = f.client(id).lock_pointer_with_region(&surface, &[region]);
    f.double_roundtrip(id);

    assert!(
        !f.state().pointer_constraint_active(),
        "a lock must not arm with the pointer outside its region"
    );

    pointer_to(
        &mut f,
        &FakeDevice::mouse(),
        surface_origin
            + Point::from((
                (region.0 + region.2 / 2) as f64,
                (region.1 + region.3 / 2) as f64,
            )),
    );
    f.double_roundtrip(id);

    assert!(
        f.state().pointer_constraint_active(),
        "the lock must arm once the pointer moves inside its region"
    );
}

/// A confine's region is surface-local like a lock's, but unlike a lock the
/// cursor inside it really moves — so the compositor has to hold it in the
/// region itself. Refused, not clamped: clamping to the region's bounding box
/// can slide the cursor onto another output, where the confine could never
/// re-establish.
#[test]
fn a_confine_with_a_region_holds_the_cursor_inside_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let (surface, _) = shadowed_window(&mut f, id);
    point_at_window_center(&mut f, id);

    // Reaches less than `SHADOW` from the pointer on every side, so measuring
    // from the geometry origin instead of the surface origin would read it
    // as outside.
    let region = (
        CENTER_IN_SURFACE.0 - SHADOW.x + 1,
        CENTER_IN_SURFACE.1 - SHADOW.y + 1,
        SHADOW.x * 2 - 2,
        SHADOW.y * 2 - 2,
    );
    let _confine = f
        .client(id)
        .confine_pointer_with_region(&surface, &[region]);
    f.double_roundtrip(id);

    assert!(
        f.state().pointer_constraint_active() && !f.state().pointer_constraint_locked(),
        "a confine whose region covers the pointer must arm and must not read \
         as a lock, or this scenario tests nothing"
    );

    // Past the region's right edge, well short of the window's. A confined move
    // is refused for leaving the surface *or* the region, so a destination off
    // the window would be refused even by a compositor that lost the region.
    let out_of_region = Point::from((40.0, 0.0));
    assert!(
        f64::from(CENTER_IN_SURFACE.0) + out_of_region.x > f64::from(region.0 + region.2)
            && f64::from(CENTER_IN_SURFACE.0) + out_of_region.x < f64::from(SHADOW.x + GEOMETRY.0),
        "the refused destination must land outside the region but still inside \
         the window, or this scenario tests nothing"
    );

    let start = f.state().seat.get_pointer().unwrap().current_location();
    pointer_relative_motion(&mut f, &FakeDevice::mouse(), out_of_region);
    f.double_roundtrip(id);

    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        start,
        "a move out of the confine's region must be refused outright, not \
         clamped to the region's edge"
    );

    // The refusal left the cursor at `start`, so this move sets off from the
    // region's centre exactly like the last one did.
    let in_region = Point::from((10.0, 10.0));
    pointer_relative_motion(&mut f, &FakeDevice::mouse(), in_region);
    f.double_roundtrip(id);

    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        start + in_region,
        "a move that stays inside the region must go through untouched"
    );
}
