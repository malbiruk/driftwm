//! Suspended-window conformance: the hit contract (§4.3), focus model (§7),
//! obstacle inflation (§4.1), and snap/cluster exclusion. Construction goes
//! through the test-only [`DriftWm::insert_suspended_for_test`] hook —
//! production never builds a suspended element in this chunk.

use driftwm::config::Config;
use smithay::utils::{Point, Rectangle, SERIAL_COUNTER, Size};

use crate::decorations::DecorationHit;
use crate::input::DecoTarget;
use crate::state::{FocusIntent, FocusTarget, StageWindow};

use super::{Fixture, map_window, window_by_app_id};

/// Server decorations on by default so suspended chrome (and the client bar)
/// resolve, and 1:1 canvas↔screen (camera origin, zoom 1).
fn config_ssd() -> Config {
    Config::from_toml(
        r#"
        [decorations]
        default_mode = "server"
    "#,
    )
    .unwrap()
}

fn origin_view(f: &mut Fixture) {
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });
}

fn pt(x: f64, y: f64) -> Point<f64, smithay::utils::Logical> {
    Point::from((x, y))
}

fn keyboard_focus_none(f: &mut Fixture) -> bool {
    f.state()
        .seat
        .get_keyboard()
        .unwrap()
        .current_focus()
        .is_none()
}

/// A click on a suspended body must not reach the window beneath: the cascade
/// short-circuits with no surface focus and `pointer_over_layer = false`.
#[test]
fn suspended_body_does_not_leak_to_window_beneath() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "beneath", (400, 300));
    let client = window_by_app_id(&mut f, "beneath").unwrap();
    origin_view(&mut f);
    let pos = f.state().stage.position_of(&client).unwrap();

    // Suspended window directly over the client, same rect, raised on top.
    let sid =
        f.state()
            .insert_suspended_for_test(1, pos, Size::from((400, 300)), "beneath", "Beneath");
    let center = pt(pos.x as f64 + 200.0, pos.y as f64 + 150.0);

    // The client is genuinely hittable there…
    assert!(
        f.state().surface_under(center, None).is_some(),
        "the client beneath is hittable"
    );
    // …but the suspended window is the topmost decoration hit.
    assert!(matches!(
        f.state().decoration_under(center),
        Some((DecoTarget::Suspended(_), DecorationHit::Body))
    ));

    // Drive real pointer focus at the body center.
    f.state().warp_pointer(center);
    f.state().refresh_pointer_focus();
    assert!(
        keyboard_focus_none(&mut f)
            || f.state()
                .seat
                .get_pointer()
                .unwrap()
                .current_focus()
                .is_none(),
        "pointer focus must not fall through to the client beneath"
    );
    assert!(
        !f.state().pointer_over_layer,
        "a suspended body is not a layer surface"
    );

    f.state().dismiss_suspended(sid);
}

/// Occlusion for the gesture/action paths: `element_under` / `element_under_raw`
/// (which back the swipe and touch move/resize gestures) find no client through
/// an opaque stand-in, and `FocusCenter` centers the stand-in itself, not the
/// hidden client beneath it.
#[test]
fn suspended_body_occludes_gesture_hit_tests_and_focus_center() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "beneath", (400, 300));
    let client = window_by_app_id(&mut f, "beneath").unwrap();
    origin_view(&mut f);
    let cpos = f.state().stage.position_of(&client).unwrap();

    // A stand-in overlapping the client but offset, raised on top.
    let spos = Point::from((cpos.x + 100, cpos.y + 50));
    let sid = f
        .state()
        .insert_suspended_for_test(1, spos, Size::from((400, 300)), "sus", "Sus");

    // A point inside the stand-in body that also lies within the client's bbox.
    let over = pt(spos.x as f64 + 150.0, spos.y as f64 + 150.0);
    assert!(
        f.state().surface_under(over, None).is_some(),
        "the client beneath is genuinely hittable there"
    );
    // But the client-only hit-tests (gesture move/resize) find nothing — the
    // opaque stand-in occludes it.
    assert!(
        f.state().element_under(over).is_none(),
        "element_under does not reach the client through the stand-in"
    );
    assert!(
        f.state().element_under_raw(over).is_none(),
        "element_under_raw (touch gestures) does not either"
    );

    // FocusCenter over the stand-in centers the stand-in: the focus intent lands
    // on it and the client beneath takes no seat focus.
    f.state().warp_pointer(over);
    f.state()
        .execute_action(&driftwm::config::Action::FocusCenter);
    assert!(
        matches!(f.state().window_focus, Some(FocusIntent::Suspended(s)) if s == sid),
        "FocusCenter acted on the stand-in, not the client"
    );
    assert!(
        keyboard_focus_none(&mut f),
        "the client beneath did not take focus"
    );

    f.state().dismiss_suspended(sid);
}

/// Focusing a suspended window's body raises it and records a `Suspended`
/// intent while leaving seat keyboard focus empty (THE GATE precondition).
#[test]
fn suspended_body_focus_and_raise() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    origin_view(&mut f);

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((600, 400)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    // Map another client on top so the suspended isn't already topmost.
    let id2 = f.add_client();
    map_window(&mut f, id2, "b", (300, 300));

    f.state().focus_and_raise_suspended(sid);

    assert!(matches!(f.state().window_focus, Some(FocusIntent::Suspended(s)) if s == sid));
    assert_eq!(f.state().gated_suspended_focus(), Some(sid));
    assert!(
        keyboard_focus_none(&mut f),
        "a suspended window holds no seat focus"
    );
    // Topmost element on the stage is the suspended one.
    assert!(
        f.state()
            .stage
            .windows()
            .next_back()
            .unwrap()
            .suspended()
            .is_some_and(|s| s.id == sid),
        "focus + raise puts the suspended window on top"
    );

    f.state().dismiss_suspended(sid);
}

/// The label sub-rect is a distinct hit region (relaunch); the rest of the body
/// is `Body` (focus + raise).
#[test]
fn suspended_label_region_is_distinct() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((500, 500)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    // Simulate a rendered label centered in the body (render doesn't run in the
    // headless fixture).
    let s = f.state().find_suspended(sid).unwrap();
    s.chrome.borrow_mut().label_rect = Some(Rectangle::new(
        Point::from((150, 130)),
        Size::from((100, 40)),
    ));

    // A point inside the label rect relaunches.
    assert!(matches!(
        f.state().decoration_under(pt(500.0 + 200.0, 500.0 + 150.0)),
        Some((DecoTarget::Suspended(_), DecorationHit::Label))
    ));
    // A body point outside the label focuses + raises.
    assert!(matches!(
        f.state().decoration_under(pt(500.0 + 20.0, 500.0 + 20.0)),
        Some((DecoTarget::Suspended(_), DecorationHit::Body))
    ));

    f.state().dismiss_suspended(sid);
}

/// THE GATE: a `Suspended` intent only counts while seat keyboard focus is
/// empty. Something else holding focus (a launcher/layer) closes the gate, so
/// Enter would go there instead.
#[test]
fn suspended_focus_gate_closes_when_seat_focus_taken() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "launcher", (300, 300));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((600, 400)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    f.state().focus_and_raise_suspended(sid);
    assert_eq!(
        f.state().gated_suspended_focus(),
        Some(sid),
        "gate open while seat focus is empty"
    );

    // A non-window focus owner (an exclusive layer surface / lock screen) takes
    // seat keyboard focus without clearing the suspended intent — `focus_changed`
    // only tracks windows. Model that: hand the seat a surface, keep the intent.
    let server_surface = super::server_surface(&window_by_app_id(&mut f, "launcher").unwrap());
    let _ = surface;
    let serial = SERIAL_COUNTER.next_serial();
    let kb = f.state().seat.get_keyboard().unwrap();
    kb.set_focus(f.state(), Some(FocusTarget(server_surface)), serial);
    f.state().window_focus = Some(FocusIntent::Suspended(sid));

    assert_eq!(
        f.state().gated_suspended_focus(),
        None,
        "gate closed: another owner holds seat keyboard focus"
    );

    f.state().dismiss_suspended(sid);
}

/// Alt-Tab: a focused suspended window is the cycle anchor (it's never in
/// history), so a fresh cycle returns to the history head client, skipping the
/// suspended window entirely.
#[test]
fn alt_tab_skips_suspended_and_anchors_to_head() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (300, 300));
    map_window(&mut f, id, "b", (300, 300));
    origin_view(&mut f);
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&b, serial); // history head = b

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((900, 400)),
        Size::from((300, 300)),
        "s",
        "S",
    );
    f.state().focus_and_raise_suspended(sid);

    // The anchor is the suspended element…
    let anchor = f.state().cycle_anchor();
    assert!(
        anchor
            .as_ref()
            .and_then(|w| w.suspended())
            .is_some_and(|s| s.id == sid),
        "cycle anchor is the focused suspended window"
    );
    // …and stepping returns to the history head (b), never the suspended one.
    let target = f.state().stage.cycle_step(false, anchor.as_ref());
    assert_eq!(
        target.and_then(|w| w.client().cloned()),
        Some(b),
        "Alt-Tab returns to the history head, skipping the suspended window"
    );
    f.state().end_cycle();

    f.state().dismiss_suspended(sid);
}

/// Hovering a suspended window under focus-follows-mouse sets the `Suspended`
/// intent (focus-only; no seat focus).
#[test]
fn hover_sets_suspended_intent() {
    let mut f = Fixture::with_config(
        Config::from_toml(
            r#"
            focus_follows_mouse = true
            [decorations]
            default_mode = "server"
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

    let center = pt(600.0, 450.0);
    f.state().warp_pointer(center);
    f.state().maybe_hover_focus(center);

    assert!(
        matches!(f.state().window_focus, Some(FocusIntent::Suspended(s)) if s == sid),
        "hovering a suspended window sets the Suspended intent"
    );

    f.state().dismiss_suspended(sid);
}

/// Dismissing a focused suspended window runs a close-style focus-follow back to
/// the most-recent client.
#[test]
fn dismiss_runs_focus_follow() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    origin_view(&mut f);
    let a = window_by_app_id(&mut f, "a").unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((800, 400)),
        Size::from((300, 300)),
        "s",
        "S",
    );
    f.state().focus_and_raise_suspended(sid);
    assert_eq!(f.state().gated_suspended_focus(), Some(sid));

    f.state().dismiss_suspended(sid);

    assert!(
        f.state().find_suspended(sid).is_none(),
        "dismiss removes the suspended window from the stage"
    );
    assert!(
        matches!(f.state().window_focus, Some(FocusIntent::Surface(_))),
        "focus follows to a surface after dismiss"
    );
    assert_eq!(
        f.state().focused_window().as_ref(),
        Some(&a),
        "focus follows back to the most-recent client"
    );
}

/// Auto-placement treats a suspended window as an obstacle, including its title
/// bar strip above the content rect.
#[test]
fn auto_placement_obstacle_includes_bar_strip() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "anchor", (200, 200));
    let placing_surface = map_window(&mut f, id, "placing", (200, 200));
    origin_view(&mut f);
    let anchor = window_by_app_id(&mut f, "anchor").unwrap();
    let placing = window_by_app_id(&mut f, "placing").unwrap();
    f.state().map_window(
        StageWindow::Client(anchor.clone()),
        Point::from((500, 500)),
        true,
    );
    let _ = placing_surface;

    // A suspended obstacle just below the anchor.
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((500, 760)),
        Size::from((200, 200)),
        "s",
        "S",
    );

    // window_ssd_bar treats a suspended window as always-SSD, and its frame
    // (obstacle rect) extends a full bar height above its content.
    let elem = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    assert_eq!(f.state().window_ssd_bar(&elem), 25);
    let frame = f.state().visual_frame_rect(&elem).unwrap();
    assert!(
        frame.y_low <= 760.0 - 25.0,
        "the obstacle rect includes the title-bar strip"
    );

    // Placement adjacent to the anchor must not land on the suspended frame.
    if let Some((x, y)) = f
        .state()
        .place_adjacent_to(&anchor, &placing, Size::from((200, 200)), 25)
    {
        let new_top = y - 25; // frame top (above content)
        let overlaps = (x as f64) < frame.x_high
            && frame.x_low < (x + 200) as f64
            && (new_top as f64) < frame.y_high
            && frame.y_low < (y + 200) as f64;
        assert!(!overlaps, "auto-placement avoided the suspended obstacle");
    }

    f.state().dismiss_suspended(sid);
}

/// Suspended windows are excluded from snap/cluster (surface-gated
/// `snap_rect_for`) but included in navigation extents (`visual_frame_rect`).
#[test]
fn suspended_excluded_from_snap_included_in_navigation() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    origin_view(&mut f);

    let before = f.state().all_windows_with_snap_rects().len();
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((600, 400)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    assert_eq!(
        f.state().all_windows_with_snap_rects().len(),
        before,
        "a suspended window is never a snap target"
    );
    let elem = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    assert!(
        f.state().visual_frame_rect(&elem).is_some(),
        "a suspended window has a navigation frame rect"
    );

    f.state().dismiss_suspended(sid);
}
