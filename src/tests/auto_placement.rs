//! `placement = "auto"` anchor selection: which element a new window docks
//! against when the focused window can't serve as the anchor, and when it stays
//! centered instead. Every scenario runs at zoom 1 with the camera at the canvas
//! origin, so a 1920×1080 output puts the viewport center at canvas (960, 540)
//! and canvas coordinates read straight off the screen.

use driftwm::layout::cluster::adjacent_side;
use smithay::desktop::Window;
use smithay::utils::{Point, SERIAL_COUNTER, Size};

use crate::state::{StageWindow, SuspendedId};

use super::client::ClientId;
use super::{Fixture, config, map_window, window_by_app_id};

/// Park the camera at `(x, y)` with zoom 1 — the compositor camera and the
/// active output's viewport in step, the way a settled navigation leaves them.
fn view_at(f: &mut Fixture, x: f64, y: f64) {
    f.state().set_camera(Point::from((x, y)));
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((x, y));
    });
}

fn origin_view(f: &mut Fixture) {
    view_at(f, 0.0, 0.0);
}

/// The 200×200 client whose placement every scenario asks about, mapped like a
/// freshly opened app.
fn map_placing(f: &mut Fixture, id: ClientId) -> Window {
    map_window(f, id, "placing", (200, 200));
    window_by_app_id(f, "placing").unwrap()
}

/// Where auto placement spawns `placing`, given the focus `snapshot` production
/// captures when the toplevel appears. `None` means the caller falls back to
/// the viewport center.
fn auto_pos(
    f: &mut Fixture,
    placing: &Window,
    snapshot: Option<StageWindow>,
) -> Option<(i32, i32)> {
    let surface = super::server_surface(placing);
    f.state().auto_anchor_snapshot.insert(surface, snapshot);
    let chrome = f
        .state()
        .element_chrome(&StageWindow::Client(placing.clone()));
    f.state()
        .auto_placement_pos(placing, Size::from((200, 200)), chrome)
}

/// Seat `placing` at `pos` and report whether it lands docked gap-adjacent to
/// `other` without overlapping it — what "auto-placed beside that element"
/// looks like on the canvas.
fn docks_against(f: &mut Fixture, placing: &Window, pos: (i32, i32), other: &StageWindow) -> bool {
    f.state()
        .map_window(StageWindow::Client(placing.clone()), Point::from(pos), true);
    let new = f
        .state()
        .visual_frame_rect(&StageWindow::Client(placing.clone()))
        .unwrap();
    let other = f.state().visual_frame_rect(other).unwrap();
    let overlaps = new.x_low < other.x_high
        && other.x_low < new.x_high
        && new.y_low < other.y_high
        && other.y_low < new.y_high;
    let gap = f.state().config.snap_gap;
    adjacent_side(&new, &other, gap).is_some() && !overlaps
}

/// A stand-in — the element a restored session leaves on screen before its app
/// is relaunched.
fn stand_in(f: &mut Fixture, id: u64, pos: (i32, i32), size: (i32, i32)) -> SuspendedId {
    f.state()
        .insert_suspended_for_test(id, Point::from(pos), Size::from(size), "s", "S")
}

fn element(f: &mut Fixture, sid: SuspendedId) -> StageWindow {
    StageWindow::Suspended(f.state().find_suspended(sid).unwrap())
}

/// Restore-shaped: stand-ins are on screen and nothing holds focus. The new
/// window joins the cluster the user is looking at instead of landing on the
/// bare viewport center.
#[test]
fn elements_in_view_anchor_a_new_window_with_no_focus() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let placing = map_placing(&mut f, id);
    origin_view(&mut f);

    let near = stand_in(&mut f, 1, (600, 400), (200, 200));
    let far = stand_in(&mut f, 2, (200, 100), (200, 200));

    let pos = auto_pos(&mut f, &placing, None)
        .expect("an element in view anchors the placement even with nothing focused");
    let near_elem = element(&mut f, near);
    let far_elem = element(&mut f, far);
    assert!(
        docks_against(&mut f, &placing, pos, &near_elem),
        "the new window docks against the element nearest the viewport center"
    );
    assert!(
        !docks_against(&mut f, &placing, pos, &far_elem),
        "and not against the farther one"
    );

    f.state().dismiss_suspended(near);
    f.state().dismiss_suspended(far);
}

/// The window being placed is already on the stage at the viewport center, so
/// it out-scores every real candidate by distance. Anchoring it against itself
/// finds no slot, silently reverting the whole fallback to centered placement.
#[test]
fn the_window_being_placed_is_never_its_own_anchor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let placing = map_placing(&mut f, id);
    origin_view(&mut f);

    // The only real candidate sits well off to the side…
    let side = stand_in(&mut f, 1, (300, 200), (200, 200));
    // …while the window being placed straddles the viewport center.
    f.state().map_window(
        StageWindow::Client(placing.clone()),
        Point::from((860, 440)),
        true,
    );

    let pos = auto_pos(&mut f, &placing, None)
        .expect("the picker skips the window being placed and anchors on the element in view");
    let side_elem = element(&mut f, side);
    assert!(
        docks_against(&mut f, &placing, pos, &side_elem),
        "the new window docks against the other element, not the spot it already occupies"
    );

    f.state().dismiss_suspended(side);
}

/// A window rule with `size` re-defers a fresh toplevel's positioning by a
/// client roundtrip, leaving it parked on the viewport-center seed with real,
/// fully-visible geometry — a rect it is about to leave, at distance 0 from the
/// center. Two apps launched together must not dock against that phantom.
#[test]
fn a_window_still_awaiting_placement_is_never_an_anchor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "seeded", (200, 200));
    let seeded = window_by_app_id(&mut f, "seeded").unwrap();
    let placing = map_placing(&mut f, id);
    origin_view(&mut f);

    // Exactly where `new_toplevel` seeds a toplevel, and still awaiting the
    // commit that will move it to its real spot.
    f.state().map_window(
        StageWindow::Client(seeded.clone()),
        Point::from((960, 540)),
        true,
    );
    f.state()
        .pending_center
        .insert(super::server_surface(&seeded));

    let side = stand_in(&mut f, 1, (300, 200), (200, 200));
    let pos = auto_pos(&mut f, &placing, None)
        .expect("the element that is done being placed anchors the new window");
    let side_elem = element(&mut f, side);
    assert!(
        docks_against(&mut f, &placing, pos, &side_elem),
        "the placed element takes the anchor, not the unplaced window over the viewport center"
    );

    f.state().dismiss_suspended(side);
}

/// Clicking bare canvas is a deliberate blank slate: the next window opens
/// centered even though an element is in view and would otherwise anchor it.
#[test]
fn clearing_focus_on_empty_canvas_keeps_the_new_window_centered() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let placing = map_placing(&mut f, id);
    origin_view(&mut f);

    let sid = stand_in(&mut f, 1, (600, 400), (200, 200));
    let serial = SERIAL_COUNTER.next_serial();
    f.state().clear_focus_to_empty_canvas(serial);

    assert!(
        auto_pos(&mut f, &placing, None).is_none(),
        "a blank slate the user asked for centers the new window despite the element in view"
    );

    f.state().dismiss_suspended(sid);
}

/// Panned onto genuinely empty canvas: an element is still technically on
/// screen, but too little of it to count as the cluster being worked on.
#[test]
fn elements_panned_nearly_out_of_view_keep_the_new_window_centered() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let placing = map_placing(&mut f, id);

    let sid = stand_in(&mut f, 1, (600, 400), (200, 200));
    // Only a 20px strip of the 200px-wide stand-in is left on screen — 10%,
    // under the third the anchor fallback demands.
    view_at(&mut f, 780.0, 400.0);
    let elem = element(&mut f, sid);
    assert!(
        f.state().window_visible_at_least(&elem, 0.05)
            && !f.state().window_visible_at_least(&elem, 0.2),
        "the stand-in is still on screen, just barely"
    );

    assert!(
        auto_pos(&mut f, &placing, None).is_none(),
        "a sliver of an element is not the cluster the user is working in"
    );

    f.state().dismiss_suspended(sid);
}

/// A bookmark jump leaves the focused window far off screen. Focus never
/// changed, but the anchor is unusable, so the placement follows the view.
#[test]
fn an_off_screen_anchor_hands_off_to_the_element_in_view() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "away", (400, 300));
    let away = window_by_app_id(&mut f, "away").unwrap();
    let placing = map_placing(&mut f, id);
    origin_view(&mut f);

    f.state().map_window(
        StageWindow::Client(away.clone()),
        Point::from((6000, 6000)),
        true,
    );
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&away, serial);
    let snapshot = f.state().focused_anchor_element();
    assert_eq!(
        snapshot,
        Some(StageWindow::Client(away.clone())),
        "the off-screen window is the one holding focus"
    );

    let sid = stand_in(&mut f, 1, (600, 400), (200, 200));
    let pos = auto_pos(&mut f, &placing, snapshot)
        .expect("an anchor the user panned away from hands off to what is in view");
    let near = element(&mut f, sid);
    assert!(
        docks_against(&mut f, &placing, pos, &near),
        "the new window docks against the element on screen"
    );

    f.state().dismiss_suspended(sid);
}

/// Proximity is measured to the nearest point of an element, not its center: a
/// large element the viewport center sits inside beats a small one off to the
/// side whose center is nearer.
#[test]
fn an_element_under_the_viewport_center_beats_a_nearer_center_off_to_the_side() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let placing = map_placing(&mut f, id);
    origin_view(&mut f);

    // Spans the viewport center, but its own center is ~649px away.
    let under = stand_in(&mut f, 1, (900, 500), (1200, 800));
    // Sits clear of the center, but its center is only ~269px away.
    let beside = stand_in(&mut f, 2, (1100, 300), (100, 100));

    let pos = auto_pos(&mut f, &placing, None).expect("an element in view anchors the placement");
    let under_elem = element(&mut f, under);
    let beside_elem = element(&mut f, beside);
    assert!(
        docks_against(&mut f, &placing, pos, &under_elem),
        "the element the viewport center falls inside is the nearest one"
    );
    assert!(
        !docks_against(&mut f, &placing, pos, &beside_elem),
        "not the one whose center happens to be closer"
    );

    f.state().dismiss_suspended(under);
    f.state().dismiss_suspended(beside);
}

/// Two overlapping windows both contain the viewport center, so proximity
/// can't separate them. The one on top — what the user is actually looking at —
/// anchors the placement, and raising the other one moves the anchor with it.
#[test]
fn the_top_most_of_two_overlapping_elements_anchors_the_placement() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "lower", (900, 200));
    map_window(&mut f, id, "upper", (900, 200));
    let lower = window_by_app_id(&mut f, "lower").unwrap();
    let upper = window_by_app_id(&mut f, "upper").unwrap();
    let placing = map_placing(&mut f, id);
    origin_view(&mut f);

    // Both straddle the viewport center; `upper` is mapped over `lower`.
    f.state().map_window(
        StageWindow::Client(lower.clone()),
        Point::from((160, 440)),
        true,
    );
    f.state().map_window(
        StageWindow::Client(upper.clone()),
        Point::from((860, 440)),
        true,
    );

    let pos = auto_pos(&mut f, &placing, None).expect("an element in view anchors the placement");
    assert!(
        docks_against(&mut f, &placing, pos, &StageWindow::Client(upper.clone())),
        "the top-most of two overlapping elements takes the placement"
    );
    assert!(
        !docks_against(&mut f, &placing, pos, &StageWindow::Client(lower.clone())),
        "the one underneath does not"
    );

    f.state().raise_window(&lower, false);
    let pos = auto_pos(&mut f, &placing, None).expect("an element in view anchors the placement");
    assert!(
        docks_against(&mut f, &placing, pos, &StageWindow::Client(lower.clone())),
        "raising the other element hands it the anchor"
    );
    assert!(
        !docks_against(&mut f, &placing, pos, &StageWindow::Client(upper.clone())),
        "and takes it from the one now underneath"
    );
}

/// Visibility is judged on the output the camera and viewport center belong to:
/// an element on screen elsewhere is not the cluster being worked in.
#[test]
fn an_element_visible_only_on_another_output_is_not_an_anchor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1920, 1080));
    let id = f.add_client();
    let placing = map_placing(&mut f, id);
    // The first output is the active one; park the second's camera to its right.
    origin_view(&mut f);
    {
        let mut os = crate::state::output_state(&out2);
        os.zoom = 1.0;
        os.camera = Point::from((2000.0, 0.0));
    }

    let sid = stand_in(&mut f, 1, (2400, 400), (200, 200));
    let elem = element(&mut f, sid);
    assert!(
        f.state().window_visible_at_least_on(&elem, &out2, 1.0),
        "the stand-in is fully on screen — on the other output"
    );

    assert!(
        auto_pos(&mut f, &placing, None).is_none(),
        "an element on another output does not anchor a window placed on this one"
    );

    f.state().dismiss_suspended(sid);
}

/// Widgets, screen-pinned windows, and fullscreen windows are not canvas
/// windows: none of them anchors a placement, even sitting under the viewport
/// center, so the eligible element off to the side wins.
#[test]
fn widget_pinned_and_fullscreen_elements_are_never_anchors() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "widget"
widget = true

[[window_rules]]
app_id = "pinned"
pinned_to_screen = true
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "widget", (200, 200));
    map_window(&mut f, id, "pinned", (200, 200));
    let fs_surface = map_window(&mut f, id, "fs", (400, 300));
    let widget = window_by_app_id(&mut f, "widget").unwrap();
    let pinned = window_by_app_id(&mut f, "pinned").unwrap();
    let fs = window_by_app_id(&mut f, "fs").unwrap();
    let placing = map_placing(&mut f, id);

    f.client(id).window(&fs_surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert!(f.state().is_window_fullscreen(&fs));
    assert!(f.state().is_pinned(&pinned));

    // Fullscreen owns the camera while it lasts; settle the view afterwards.
    origin_view(&mut f);
    f.state().map_window(
        StageWindow::Client(widget.clone()),
        Point::from((860, 440)),
        true,
    );
    let eligible = stand_in(&mut f, 1, (300, 200), (200, 200));

    let pos = auto_pos(&mut f, &placing, None)
        .expect("the one eligible element in view anchors the placement");
    let eligible_elem = element(&mut f, eligible);
    assert!(
        docks_against(&mut f, &placing, pos, &eligible_elem),
        "the new window docks against the canvas element, not the widget over the center"
    );

    f.state().dismiss_suspended(eligible);
}

/// The blank slate is set by the empty-canvas click alone and lifted by the
/// next focus write, so incidental focus loss never disables the fallback and a
/// deliberate one never outlives the user's next move.
#[test]
fn focus_writes_clear_the_empty_canvas_suppression() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "away", (400, 300));
    let away = window_by_app_id(&mut f, "away").unwrap();
    let placing = map_placing(&mut f, id);
    origin_view(&mut f);
    f.state().map_window(
        StageWindow::Client(away.clone()),
        Point::from((6000, 6000)),
        true,
    );
    let sid = stand_in(&mut f, 1, (600, 400), (200, 200));

    // Losing focus incidentally — a window closed, say — is not a blank slate.
    let serial = SERIAL_COUNTER.next_serial();
    f.state().set_window_focus(None, serial);
    assert!(
        auto_pos(&mut f, &placing, None).is_some(),
        "incidental focus loss still anchors on the element in view"
    );

    let serial = SERIAL_COUNTER.next_serial();
    f.state().clear_focus_to_empty_canvas(serial);
    assert!(
        auto_pos(&mut f, &placing, None).is_none(),
        "the empty-canvas click centers the next window"
    );

    // Focusing anything lifts it, even a window that can't anchor on its own.
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&away, serial);
    let snapshot = f.state().focused_anchor_element();
    assert!(
        auto_pos(&mut f, &placing, snapshot).is_some(),
        "focusing a window lifts the blank slate even when that window is off screen"
    );

    f.state().dismiss_suspended(sid);
}

/// The unchanged path: a focused window on screen still anchors the placement
/// itself, docked on the edge facing the viewport center.
#[test]
fn a_visible_focused_anchor_keeps_the_new_window_beside_itself() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "anchor", (400, 300));
    let anchor = window_by_app_id(&mut f, "anchor").unwrap();
    let placing = map_placing(&mut f, id);
    origin_view(&mut f);

    f.state().map_window(
        StageWindow::Client(anchor.clone()),
        Point::from((100, 200)),
        true,
    );
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&anchor, serial);
    let snapshot = f.state().focused_anchor_element();

    // CSD, so frame == content: the anchor spans x[100,500], y[200,500]. The
    // viewport center (960, 540) lies to its right, so the newcomer takes that
    // edge across the 12px snap gap (x = 500 + 12), centered on the anchor's
    // 300px height (y = 350 - 200/2).
    assert_eq!(auto_pos(&mut f, &placing, snapshot), Some((512, 250)));
}
