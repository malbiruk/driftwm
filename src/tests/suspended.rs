//! Suspended-window conformance: the hit contract (§4.3), focus model (§7),
//! obstacle inflation (§4.1), and snap/cluster exclusion. Construction goes
//! through the test-only [`DriftWm::insert_suspended_for_test`] hook —
//! production never builds a suspended element in this chunk.

use driftwm::config::{BTN_LEFT, Config};
use driftwm::layout::snap::SnapState;
use smithay::input::keyboard::ModifiersState;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size};

use crate::decorations::DecorationHit;
use crate::input::DecoTarget;
use crate::state::{FocusIntent, FocusTarget, StageWindow, SuspendedId};

use super::{Fixture, is_activated, map_window, window_by_app_id};

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

/// The padding strip right of the close button is chrome, not a hole: a point
/// there resolves to the stand-in's title bar (drag-to-move) and does not leak
/// to a client whose body lies beneath the drawn bar.
#[test]
fn suspended_bar_right_pad_strip_is_chrome() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    // A client whose body reaches up into the stand-in's title-bar band.
    map_window(&mut f, id, "beneath", (400, 300));
    let client = window_by_app_id(&mut f, "beneath").unwrap();
    origin_view(&mut f);
    f.state()
        .map_window(StageWindow::Client(client), Point::from((500, 400)), true);

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((500, 500)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    // A point in the 8px strip right of the close button, within the bar band:
    // x in [500+400-8, 500+400), y in [500-25, 500).
    let strip = pt(500.0 + 400.0 - 4.0, 500.0 - 13.0);

    // A client is genuinely hittable there (its body reaches into the band)…
    assert!(
        f.state().surface_under(strip, None).is_some(),
        "a client body lies beneath the drawn bar strip"
    );
    // …but the strip resolves to the stand-in's title bar, not a hole.
    assert!(
        matches!(
            f.state().decoration_under(strip),
            Some((DecoTarget::Suspended(_), DecorationHit::TitleBar))
        ),
        "the right-pad strip is a title-bar hit"
    );
    // And the pointer cascade yields nothing over it — no leak to the client.
    assert!(
        f.state().pointer_focus_under(strip, strip).is_none(),
        "the strip gives no pointer focus to the client beneath"
    );

    f.state().dismiss_suspended(sid);
}

/// A bare (modifier-less) mouse move binding does not beat suspended chrome: a
/// close-button click dismisses the stand-in. The same click with the modifier
/// held wins — it grabs the stand-in (focus + raise) without dismissing it.
#[test]
fn suspended_chrome_beats_bare_but_not_modifier_move_binding() {
    let config = Config::from_toml(
        r#"
        [decorations]
        default_mode = "server"
        [mouse.anywhere]
        "left" = "move-window"
        "super+left" = "move-window"
    "#,
    )
    .unwrap();
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);

    // The close button of a stand-in at (500,500), 400x300, bar 25:
    // x in [500+400-25-8, 500+400-8), y in [500-25, 500).
    let close = pt(500.0 + 400.0 - 20.0, 500.0 - 12.0);
    let serial = SERIAL_COUNTER.next_serial();
    let pointer = f.state().seat.get_pointer().unwrap();

    // Bare click: chrome wins, so the close button dismisses the stand-in.
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((500, 500)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    assert!(matches!(
        f.state().decoration_under(close),
        Some((DecoTarget::Suspended(_), DecorationHit::CloseButton))
    ));
    let consumed = f.state().try_suspended_button(
        &pointer,
        close,
        BTN_LEFT,
        serial,
        ModifiersState::default(),
    );
    assert!(consumed, "a click over the opaque stand-in is consumed");
    assert!(
        f.state().find_suspended(sid).is_none(),
        "a bare move binding loses to chrome: the close button dismissed the stand-in"
    );

    // Same click with the modifier held: the binding wins — it grabs the
    // stand-in (focus + raise) without dismissing it.
    let sid2 = f.state().insert_suspended_for_test(
        1,
        Point::from((500, 500)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let held = ModifiersState {
        logo: true,
        ..Default::default()
    };
    let serial = SERIAL_COUNTER.next_serial();
    let consumed = f
        .state()
        .try_suspended_button(&pointer, close, BTN_LEFT, serial, held);
    assert!(consumed);
    assert!(
        f.state().find_suspended(sid2).is_some(),
        "a held-modifier move binding does not dismiss the stand-in"
    );
    assert!(
        matches!(f.state().window_focus, Some(FocusIntent::Suspended(s)) if s == sid2),
        "the held-modifier move focused + raised the stand-in"
    );

    f.state().dismiss_suspended(sid2);
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
    // A deferred pointer resync (camera warp/animation) ending over the stand-in
    // resolves to no pointer focus — the hidden client gets no stray enter.
    f.state().flush_pointer_resync();
    assert!(
        f.state()
            .seat
            .get_pointer()
            .unwrap()
            .current_focus()
            .is_none(),
        "a deferred resync over the stand-in sends no enter to the client beneath"
    );
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

/// A stand-in owns no toplevel to activate, but hovering onto one must still
/// clear the `Activated` hint off whichever real window held it.
#[test]
fn hover_onto_suspended_deactivates_the_previous_window() {
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
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();

    // Well clear of wherever auto-placement put A, so the two never overlap.
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((5000, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    // Inserting the stand-in exclusively activates it too (a no-op on the wire,
    // no toplevel) which already cleared A; re-focus A here so the assertion
    // below isolates the hover path.
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&a, serial);
    assert!(is_activated(&a));

    let center = pt(5200.0, 450.0);
    f.state().warp_pointer(center);
    f.state().maybe_hover_focus(center);

    assert!(
        matches!(f.state().window_focus, Some(FocusIntent::Suspended(s)) if s == sid),
        "hovering the stand-in sets the Suspended intent"
    );
    assert!(
        !is_activated(&a),
        "hovering onto a stand-in must clear the previously-activated window's Activated hint"
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

/// A CSD-origin stand-in (csd=true) wears the same bar as any other: the same
/// CloseButton / TitleBar hits and bar strip above the body. The origin only
/// changes adopt geometry, not chrome.
#[test]
fn csd_stand_in_has_uniform_bar_hits() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_csd_for_test(
        1,
        Point::from((500, 500)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    // Barred like any stand-in: a full bar strip sits above the body.
    let elem = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let bar = f.state().config.decorations.title_bar_height;
    assert_eq!(f.state().window_ssd_bar(&elem), bar);
    let frame = f.state().visual_frame_rect(&elem).unwrap();
    let bw = f.state().default_border_width() as f64;
    assert_eq!(
        frame.y_low,
        500.0 - bar as f64 - bw,
        "frame top includes the bar strip"
    );

    // The close button sits on the right of the bar; the rest of the band drags.
    assert!(matches!(
        f.state()
            .decoration_under(pt(500.0 + 400.0 - 20.0, 500.0 - 12.0)),
        Some((DecoTarget::Suspended(_), DecorationHit::CloseButton))
    ));
    assert!(matches!(
        f.state().decoration_under(pt(500.0 + 50.0, 500.0 - 12.0)),
        Some((DecoTarget::Suspended(_), DecorationHit::TitleBar))
    ));
    // The body itself is a plain Body hit.
    assert!(matches!(
        f.state().decoration_under(pt(500.0 + 200.0, 500.0 + 150.0)),
        Some((DecoTarget::Suspended(_), DecorationHit::Body))
    ));

    f.state().dismiss_suspended(sid);
}

/// A plain-LMB body press on a stand-in is focus-only for both origins — the
/// bar drags, not the body. The centered Label still relaunches.
#[test]
fn stand_in_body_is_focus_only() {
    let body = pt(500.0 + 200.0, 500.0 + 150.0);

    // A body press only focuses, for both an SSD-origin and a CSD-origin
    // stand-in — neither body drags.
    for csd in [false, true] {
        let mut f = Fixture::with_config(config_ssd());
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = if csd {
            f.state().insert_suspended_csd_for_test(
                1,
                Point::from((500, 500)),
                Size::from((400, 300)),
                "s",
                "S",
            )
        } else {
            f.state().insert_suspended_for_test(
                1,
                Point::from((500, 500)),
                Size::from((400, 300)),
                "s",
                "S",
            )
        };
        let pointer = f.state().seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        let consumed = f.state().try_suspended_button(
            &pointer,
            body,
            BTN_LEFT,
            serial,
            ModifiersState::default(),
        );
        assert!(consumed);
        assert!(
            !f.state().seat.get_pointer().unwrap().is_grabbed(),
            "a body press does not start a move (csd={csd})"
        );
        assert!(
            matches!(f.state().window_focus, Some(FocusIntent::Suspended(s)) if s == sid),
            "the body press focused the stand-in (csd={csd})"
        );
        f.state().dismiss_suspended(sid);
    }

    // The centered Label still relaunches — it is not swallowed by the body.
    {
        let tmp = super::real::TempDir::new();
        std::fs::write(
            tmp.path().join("s.desktop"),
            "[Desktop Entry]\nType=Application\nName=S\nExec=s\n",
        )
        .unwrap();
        let mut f = Fixture::with_config(config_ssd());
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        f.state().desktop_entry_cache = Some(driftwm::desktop_entry::DesktopEntryCache::new(vec![
            tmp.path().to_path_buf(),
        ]));
        let sid = f.state().insert_suspended_for_test(
            1,
            Point::from((500, 500)),
            Size::from((400, 300)),
            "s",
            "S",
        );
        // Simulate a rendered label centered in the body (render doesn't run in
        // the headless fixture).
        f.state()
            .find_suspended(sid)
            .unwrap()
            .chrome
            .borrow_mut()
            .label_rect = Some(Rectangle::new(
            Point::from((150, 130)),
            Size::from((100, 40)),
        ));

        let pointer = f.state().seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        let consumed = f.state().try_suspended_button(
            &pointer,
            body,
            BTN_LEFT,
            serial,
            ModifiersState::default(),
        );
        assert!(consumed);
        assert!(
            !f.state().seat.get_pointer().unwrap().is_grabbed(),
            "a Label press relaunches — it does not start the body move"
        );
        assert!(
            f.state().is_suspended_launching(sid),
            "the Label press fired the relaunch"
        );
        f.state().dismiss_suspended(sid);
    }
}

/// A CSD-origin stand-in's bar is a real drag target and its close button
/// really dismisses — the uniform chrome grammar isn't just hit-testing, the
/// interactions it drives are the same as an SSD-origin stand-in's.
#[test]
fn csd_stand_in_bar_drags_and_close_button_dismisses() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let pointer = f.state().seat.get_pointer().unwrap();

    // A bare press on the title-bar band starts a move grab.
    let sid = f.state().insert_suspended_csd_for_test(
        1,
        Point::from((500, 500)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let serial = SERIAL_COUNTER.next_serial();
    let bar_point = pt(500.0 + 50.0, 500.0 - 12.0);
    let consumed = f.state().try_suspended_button(
        &pointer,
        bar_point,
        BTN_LEFT,
        serial,
        ModifiersState::default(),
    );
    assert!(consumed);
    assert!(
        f.state().seat.get_pointer().unwrap().is_grabbed(),
        "a bare press on the bar starts a move grab for a CSD-origin stand-in"
    );
    f.state().dismiss_suspended(sid);

    // The close button dismisses it, same as an SSD-origin stand-in.
    let sid2 = f.state().insert_suspended_csd_for_test(
        1,
        Point::from((500, 500)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let close = pt(500.0 + 400.0 - 20.0, 500.0 - 12.0);
    let serial = SERIAL_COUNTER.next_serial();
    let consumed = f.state().try_suspended_button(
        &pointer,
        close,
        BTN_LEFT,
        serial,
        ModifiersState::default(),
    );
    assert!(consumed);
    assert!(
        f.state().find_suspended(sid2).is_none(),
        "the close button dismisses a CSD-origin stand-in same as an SSD-origin one"
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
    if let Some((x, y)) = f.state().place_adjacent_to(
        &StageWindow::Client(anchor.clone()),
        &placing,
        Size::from((200, 200)),
        25,
    ) {
        let new_top = y - 25; // frame top (above content)
        let overlaps = (x as f64) < frame.x_high
            && frame.x_low < (x + 200) as f64
            && (new_top as f64) < frame.y_high
            && frame.y_low < (y + 200) as f64;
        assert!(!overlaps, "auto-placement avoided the suspended obstacle");
    }

    f.state().dismiss_suspended(sid);
}

/// A stand-in is a full auto-placement citizen, not just an obstacle: with a
/// neighbor of the same frame gap-adjacent to the anchor, a new window lands in
/// the exact same slot whether that neighbor is a live SSD window or a stand-in
/// — so the stand-in is an eligible adjacency target, placed beside, not just
/// avoided.
#[test]
fn auto_placement_treats_a_stand_in_like_a_window() {
    // "nb" is SSD via a rule, so its live frame (bar + default border) matches a
    // stand-in's; every other placement input is identical across the two scenes.
    let toml = "[decorations]\ndefault_mode = \"server\"\n\
                [[window_rules]]\napp_id = \"nb\"\ndecoration = \"server\"\n";

    let place = |stand_in: bool| -> Option<(i32, i32)> {
        let mut f = Fixture::with_config(Config::from_toml(toml).unwrap());
        f.add_output(1, (1920, 1080));
        let id = f.add_client();
        map_window(&mut f, id, "anchor", (200, 200));
        let placing_surface = map_window(&mut f, id, "placing", (200, 200));
        origin_view(&mut f);
        let anchor = window_by_app_id(&mut f, "anchor").unwrap();
        let placing = window_by_app_id(&mut f, "placing").unwrap();
        let _ = placing_surface;
        f.state().map_window(
            StageWindow::Client(anchor.clone()),
            Point::from((500, 500)),
            true,
        );
        // Neighbor gap-adjacent to the anchor's right edge, so it clusters.
        let nb_pos = Point::from((760, 500));
        if stand_in {
            f.state()
                .insert_suspended_for_test(1, nb_pos, Size::from((200, 200)), "nb", "NB");
        } else {
            let nb = f.add_client();
            map_window(&mut f, nb, "nb", (200, 200));
            let nb_win = window_by_app_id(&mut f, "nb").unwrap();
            f.state()
                .map_window(StageWindow::Client(nb_win), nb_pos, true);
        }
        let pos = f.state().place_adjacent_to(
            &StageWindow::Client(anchor.clone()),
            &placing,
            Size::from((200, 200)),
            25,
        );
        // A throwaway measurement fixture — the scene is never torn down.
        f.skip_baseline_check();
        pos
    };

    let with_live = place(false);
    let with_standin = place(true);
    assert!(with_live.is_some(), "the live-neighbor scene finds a slot");
    assert_eq!(
        with_standin, with_live,
        "a stand-in places a new window identically to a live window of the same frame"
    );
}

/// The map-time anchor snapshot captures a focused stand-in the same way it
/// captures a focused live window — under sloppy focus, hovering a stand-in
/// focuses it, so it must be able to anchor a newly-mapped window.
#[test]
fn focused_anchor_element_captures_a_focused_stand_in() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((500, 500)),
        Size::from((200, 200)),
        "s",
        "S",
    );
    f.state().focus_and_raise_suspended(sid);

    let anchor = f.state().focused_anchor_element();
    let expected = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    assert_eq!(
        anchor,
        Some(expected),
        "a focused stand-in is captured as the anchor"
    );

    f.state().dismiss_suspended(sid);
}

/// A new window auto-places beside a focused stand-in: the map-time snapshot
/// anchors on the stand-in, so placement docks the new window gap-adjacent to
/// the stand-in's frame instead of dropping on top of it.
#[test]
fn auto_placement_anchors_a_new_window_against_a_focused_stand_in() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    // A visible stand-in the user is hovering (sloppy focus focuses it).
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((500, 500)),
        Size::from((200, 200)),
        "s",
        "S",
    );
    f.state().focus_and_raise_suspended(sid);
    // new_toplevel snapshots the focus that existed *before* the new window —
    // the stand-in — so capture it now, before mapping steals focus.
    let anchor = f.state().focused_anchor_element();

    let id = f.add_client();
    map_window(&mut f, id, "placing", (200, 200));
    let placing = window_by_app_id(&mut f, "placing").unwrap();
    let surface = super::server_surface(&placing);
    f.state().auto_anchor_snapshot.insert(surface, anchor);

    let bar = f
        .state()
        .window_ssd_bar(&StageWindow::Client(placing.clone()));
    let (x, y) = f
        .state()
        .auto_placement_pos(&placing, Size::from((200, 200)), bar)
        .expect("auto placement anchored on the stand-in");

    // Seat the new window and compare frames: docked gap-adjacent, not overlapping.
    f.state().map_window(
        StageWindow::Client(placing.clone()),
        Point::from((x, y)),
        true,
    );
    let new_frame = f
        .state()
        .visual_frame_rect(&StageWindow::Client(placing.clone()))
        .unwrap();
    let standin = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let standin_frame = f.state().visual_frame_rect(&standin).unwrap();
    let gap = f.state().config.snap_gap;
    assert!(
        driftwm::layout::cluster::adjacent_side(&new_frame, &standin_frame, gap).is_some(),
        "the new window docked gap-adjacent to the stand-in"
    );
    let overlaps = new_frame.x_low < standin_frame.x_high
        && standin_frame.x_low < new_frame.x_high
        && new_frame.y_low < standin_frame.y_high
        && standin_frame.y_low < new_frame.y_high;
    assert!(!overlaps, "and does not overlap the stand-in");

    f.state().dismiss_suspended(sid);
}

/// If the anchored stand-in is dismissed between the snapshot and the placement,
/// the anchor no longer resolves and placement degrades to the same center
/// fallback (`None`) as a closed live anchor — no special-casing.
#[test]
fn auto_placement_falls_back_when_the_anchored_stand_in_is_dismissed() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((500, 500)),
        Size::from((200, 200)),
        "s",
        "S",
    );
    f.state().focus_and_raise_suspended(sid);
    let anchor = f.state().focused_anchor_element();

    let id = f.add_client();
    map_window(&mut f, id, "placing", (200, 200));
    let placing = window_by_app_id(&mut f, "placing").unwrap();
    let surface = super::server_surface(&placing);
    f.state().auto_anchor_snapshot.insert(surface, anchor);

    // The stand-in vanishes before the new window's placement runs.
    f.state().dismiss_suspended(sid);

    let bar = f
        .state()
        .window_ssd_bar(&StageWindow::Client(placing.clone()));
    assert!(
        f.state()
            .auto_placement_pos(&placing, Size::from((200, 200)), bar)
            .is_none(),
        "a dismissed stand-in anchor falls back to center placement"
    );
}

/// Navigation parity: with no focused window, `center-window` (the nearest
/// fallback) considers stand-ins, so it lands on the nearest one and sets the
/// suspended focus intent — Enter then relaunches it.
#[test]
fn center_window_fallback_lands_on_stand_in() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "far", (200, 200));
    origin_view(&mut f);
    let far = window_by_app_id(&mut f, "far").unwrap();
    // Park the live window far from the viewport center.
    f.state()
        .map_window(StageWindow::Client(far), Point::from((5000, 5000)), true);

    // A stand-in straddling the viewport center.
    let vc = f.state().viewport_center_canvas();
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((vc.x as i32 - 100, vc.y as i32 - 100)),
        Size::from((200, 200)),
        "s",
        "S",
    );

    // Clear focus so center-window takes the nearest-fallback path.
    let serial = SERIAL_COUNTER.next_serial();
    f.state().set_window_focus(None, serial);

    f.state()
        .execute_action(&driftwm::config::Action::CenterWindow);
    assert!(
        matches!(f.state().window_focus, Some(FocusIntent::Suspended(s)) if s == sid),
        "center-window landed on the stand-in and set the suspended intent"
    );

    f.state().dismiss_suspended(sid);
}

/// Directional `center-nearest` also treats a stand-in as a candidate: with a
/// stand-in to the right of the origin (and a live window far the other way), a
/// rightward swipe lands on the stand-in with the suspended intent set.
#[test]
fn center_nearest_reaches_a_stand_in() {
    use driftwm::config::{Action, Direction};
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "left", (200, 200));
    origin_view(&mut f);
    let left = window_by_app_id(&mut f, "left").unwrap();
    let vc = f.state().viewport_center_canvas();
    // A live window far to the left of the origin, a stand-in to the right.
    f.state().map_window(
        StageWindow::Client(left),
        Point::from((vc.x as i32 - 2000, vc.y as i32)),
        true,
    );
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((vc.x as i32 + 400, vc.y as i32 - 100)),
        Size::from((200, 200)),
        "s",
        "S",
    );

    let serial = SERIAL_COUNTER.next_serial();
    f.state().set_window_focus(None, serial);

    f.state()
        .execute_action(&Action::CenterNearest(Direction::Right));
    assert!(
        matches!(f.state().window_focus, Some(FocusIntent::Suspended(s)) if s == sid),
        "center-nearest to the right reached the stand-in"
    );

    f.state().dismiss_suspended(sid);
}

/// Where the stand-in's visual center lands on screen once the pending camera
/// animation settles. A centering action puts it at the usable viewport center.
fn settled_center_screen(f: &mut Fixture, sid: SuspendedId) -> Point<f64, Logical> {
    let camera = f.state().camera_target().expect("a camera target was set");
    let zoom = f.state().zoom_target().expect("a zoom target was set");
    let s = f
        .state()
        .find_suspended(sid)
        .expect("the stand-in is alive");
    let rect = f
        .state()
        .visual_frame_rect(&StageWindow::Suspended(s))
        .expect("the stand-in has a visual frame");
    Point::from((
        ((rect.x_low + rect.x_high) / 2.0 - camera.x) * zoom,
        ((rect.y_low + rect.y_high) / 2.0 - camera.y) * zoom,
    ))
}

fn assert_centers_on(f: &mut Fixture, sid: SuspendedId) {
    let settled = settled_center_screen(f, sid);
    let vc = f.state().usable_center_screen();
    assert!(
        (settled.x - vc.x).abs() < 1e-6 && (settled.y - vc.y).abs() < 1e-6,
        "the stand-in settles at the viewport center: {settled:?} vs {vc:?}"
    );
}

/// `focus-center` over a stand-in that is already fully on screen still centers
/// it and resets zoom, matching what it does for a live window. The reveal-only
/// semantics of `msg focus` (pan only when clipped) belong to the IPC verb.
#[test]
fn focus_center_centers_a_fully_visible_stand_in() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    f.state().set_zoom(0.8);

    // Fully on screen at this zoom, but far from the viewport center.
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((100, 100)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let s = f.state().find_suspended(sid).unwrap();
    assert!(
        f.state()
            .window_fully_in_viewport(&StageWindow::Suspended(s)),
        "the stand-in starts fully visible"
    );

    // Canvas coords — outside the body under a screen-space reading, so the
    // hit-test's coordinate space is pinned by the zoom being off 1:1.
    f.state().warp_pointer(pt(450.0, 380.0));
    f.state()
        .execute_action(&driftwm::config::Action::FocusCenter);

    assert_centers_on(&mut f, sid);
    assert_eq!(
        f.state().zoom_target(),
        Some(1.0),
        "focus-center resets zoom on a stand-in as it does on a window"
    );

    f.state().dismiss_suspended(sid);
}

/// A focused stand-in holds no seat keyboard focus, so `center-window` can't
/// find it through `focused_window` — it still centers the stand-in rather than
/// falling back to whichever element sits nearest the viewport center.
#[test]
fn center_window_centers_the_focused_stand_in() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "near", (400, 300));
    origin_view(&mut f);

    // The live window sits nearer the viewport center than the stand-in, so the
    // nearest-fallback would claim it.
    let vc = f.state().viewport_center_canvas();
    let near = window_by_app_id(&mut f, "near").unwrap();
    f.state().map_window(
        StageWindow::Client(near),
        Point::from((vc.x as i32 - 200, vc.y as i32 - 150)),
        true,
    );
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((vc.x as i32 + 1500, vc.y as i32 + 900)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    f.state().focus_and_raise_suspended(sid);

    f.state()
        .execute_action(&driftwm::config::Action::CenterWindow);

    assert!(
        matches!(f.state().window_focus, Some(FocusIntent::Suspended(s)) if s == sid),
        "center-window kept the stand-in focused instead of grabbing the client"
    );
    assert_centers_on(&mut f, sid);

    f.state().dismiss_suspended(sid);
}

/// Directional `center-nearest` centers a stand-in it reaches even when that
/// stand-in is already fully visible, and — like the live-window path — leaves
/// zoom where it is.
#[test]
fn center_nearest_centers_a_fully_visible_stand_in() {
    use driftwm::config::{Action, Direction};
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    f.state().set_zoom(0.8);

    let vc = f.state().viewport_center_canvas();
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((vc.x as i32 + 300, vc.y as i32 - 100)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let s = f.state().find_suspended(sid).unwrap();
    assert!(
        f.state()
            .window_fully_in_viewport(&StageWindow::Suspended(s)),
        "the stand-in starts fully visible"
    );

    let serial = SERIAL_COUNTER.next_serial();
    f.state().set_window_focus(None, serial);
    f.state()
        .execute_action(&Action::CenterNearest(Direction::Right));

    assert_centers_on(&mut f, sid);
    assert_eq!(
        f.state().zoom_target(),
        Some(0.8),
        "center-nearest preserves zoom on a stand-in as it does on a window"
    );

    f.state().dismiss_suspended(sid);
}

/// A focused stand-in anchors the directional search the way a focused window
/// does: it becomes the cone origin, so repeating the direction steps past it
/// instead of re-selecting it.
#[test]
fn center_nearest_steps_past_the_focused_stand_in() {
    use driftwm::config::{Action, Direction};
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);

    // Two stand-ins right of the viewport center, both fully visible.
    let near = f.state().insert_suspended_for_test(
        1,
        Point::from((1100, 440)),
        Size::from((300, 200)),
        "near",
        "Near",
    );
    let far = f.state().insert_suspended_for_test(
        2,
        Point::from((1600, 440)),
        Size::from((200, 200)),
        "far",
        "Far",
    );

    let serial = SERIAL_COUNTER.next_serial();
    f.state().set_window_focus(None, serial);
    f.state()
        .execute_action(&Action::CenterNearest(Direction::Right));
    assert!(
        matches!(f.state().window_focus, Some(FocusIntent::Suspended(s)) if s == near),
        "the first press lands on the nearer stand-in"
    );

    // The camera hasn't animated yet, so the viewport center is unchanged — only
    // the anchor can carry the search past the stand-in it just landed on.
    f.state()
        .execute_action(&Action::CenterNearest(Direction::Right));
    assert!(
        matches!(f.state().window_focus, Some(FocusIntent::Suspended(s)) if s == far),
        "the second press stepped past it to the farther stand-in"
    );

    f.state().dismiss_suspended(near);
    f.state().dismiss_suspended(far);
}

/// A suspended stand-in is a snap target like any window: its `snap_rect_for`
/// equals the frame the live window had (body + SSD bar + border), and it
/// counts in `all_windows_with_snap_rects`.
#[test]
fn suspended_is_a_snap_target() {
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
        before + 1,
        "a suspended stand-in is a snap target"
    );
    let elem = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let rect = f
        .state()
        .snap_rect_for(&elem)
        .expect("stand-in has a snap rect");
    // Body top-left (600, 400), size 400×300; SSD bar + default border inflate
    // it exactly as the live window's rect would have been.
    let bar = f.state().window_ssd_bar(&elem) as f64;
    let bw = f.state().default_border_width() as f64;
    assert_eq!(rect.x_low, 600.0 - bw);
    assert_eq!(rect.x_high, 600.0 + 400.0 + bw);
    assert_eq!(rect.y_low, 400.0 - bar - bw);
    assert_eq!(rect.y_high, 400.0 + 300.0 + bw);
    // The navigation alias reports the same rect.
    assert_eq!(
        f.state().visual_frame_rect(&elem).map(|r| r.x_low),
        Some(rect.x_low)
    );

    f.state().dismiss_suspended(sid);
}

/// Every stand-in's snap rect includes the title-bar strip above the body,
/// regardless of origin — its top edge is `loc - bar - border`.
#[test]
fn stand_in_snap_rect_includes_bar_strip() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);

    let ssd = f.state().insert_suspended_for_test(
        1,
        Point::from((600, 400)),
        Size::from((400, 300)),
        "b",
        "B",
    );
    let csd = f.state().insert_suspended_csd_for_test(
        2,
        Point::from((600, 900)),
        Size::from((400, 300)),
        "c",
        "C",
    );
    let ssd_elem = StageWindow::Suspended(f.state().find_suspended(ssd).unwrap());
    let csd_elem = StageWindow::Suspended(f.state().find_suspended(csd).unwrap());

    let bw = f.state().default_border_width() as f64;
    let bar = f.state().config.decorations.title_bar_height as f64;
    assert!(
        bar > 0.0,
        "config must actually give a bar for this to bite"
    );
    let sr = f.state().snap_rect_for(&ssd_elem).unwrap();
    let cr = f.state().snap_rect_for(&csd_elem).unwrap();

    assert_eq!(sr.y_low, 400.0 - bar - bw, "SSD-origin keeps its bar");
    assert_eq!(cr.y_low, 900.0 - bar - bw, "CSD-origin is barred too");

    f.state().dismiss_suspended(ssd);
    f.state().dismiss_suspended(csd);
}

/// A client dragged against a stand-in's edge snaps to it, docking side-by-side
/// with the configured gap — the same magnetic snap it gets against a window.
#[test]
#[allow(clippy::mutable_key_type)]
fn client_snaps_to_a_stand_in() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    origin_view(&mut f);
    let a = window_by_app_id(&mut f, "a").unwrap();
    f.state().map_window(
        StageWindow::Client(a.clone()),
        Point::from((400, 300)),
        true,
    );

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((1000, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let standin = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let target = f.state().snap_rect_for(&standin).unwrap();
    let gap = f.state().config.snap_gap;
    let a_w = a.geometry().size.w as f64;

    // Drag A so its right edge lands just short of the stand-in's left edge.
    let natural = Point::from((target.x_low - gap - a_w + 3.0, 300.0));
    let mut snap = SnapState::default();
    let excludes = std::collections::HashSet::new();
    let snapped =
        f.state()
            .snap_move_location(&StageWindow::Client(a), 1.0, natural, &mut snap, &excludes);

    assert!(
        (snapped.x - (target.x_low - gap - a_w)).abs() < 1.0,
        "client's right edge snapped one gap short of the stand-in ({snapped:?})"
    );

    f.state().dismiss_suspended(sid);
}

/// A stand-in dragged against a client's edge snaps to it — the suspended move
/// grab's snap math is the client move grab's, so it docks the same way.
#[test]
#[allow(clippy::mutable_key_type)]
fn stand_in_snaps_to_a_client() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    origin_view(&mut f);
    let a = window_by_app_id(&mut f, "a").unwrap();
    f.state().map_window(
        StageWindow::Client(a.clone()),
        Point::from((400, 300)),
        true,
    );
    let a_rect = f.state().snap_rect_for(&StageWindow::Client(a)).unwrap();
    let gap = f.state().config.snap_gap;

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((1400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let standin = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());

    // Drag the stand-in so its left edge lands just short of docking to A's right.
    let natural = Point::from((a_rect.x_high + gap - 3.0, 300.0));
    let mut snap = SnapState::default();
    let excludes = std::collections::HashSet::new();
    let snapped = f
        .state()
        .snap_move_location(&standin, 1.0, natural, &mut snap, &excludes);

    assert!(
        (snapped.x - (a_rect.x_high + gap)).abs() < 1.0,
        "stand-in's left edge snapped one gap past the client ({snapped:?})"
    );

    f.state().dismiss_suspended(sid);
}

/// A stand-in is an ordinary cluster member: it bridges two clients it sits
/// between, so the whole run is one snap-cluster — this is what keeps a cluster
/// intact when its middle window is suspended.
#[test]
#[allow(clippy::mutable_key_type)]
fn stand_in_bridges_a_cluster() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let gap = f.state().config.snap_gap as i32;

    let ida = f.add_client();
    map_window(&mut f, ida, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    f.state().map_window(
        StageWindow::Client(a.clone()),
        Point::from((400, 300)),
        true,
    );

    // Stand-in gap-adjacent to A's right (border is 0 in this config).
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((800 + gap, 300)),
        Size::from((200, 300)),
        "s",
        "S",
    );

    let idc = f.add_client();
    map_window(&mut f, idc, "c", (300, 300));
    let c = window_by_app_id(&mut f, "c").unwrap();
    // Gap-adjacent to the stand-in's right edge (800+gap+200).
    f.state().map_window(
        StageWindow::Client(c.clone()),
        Point::from((1000 + 2 * gap, 300)),
        true,
    );

    let rects = f.state().all_windows_with_snap_rects();
    let cluster = driftwm::layout::cluster::cluster_of(
        &StageWindow::Client(a.clone()),
        &rects,
        f.state().config.snap_gap,
    );

    let standin = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    assert_eq!(cluster.len(), 3, "all three form one cluster");
    assert!(cluster.contains(&standin), "the stand-in is a member");
    assert!(cluster.contains(&StageWindow::Client(c)));

    f.state().dismiss_suspended(sid);
}

/// A group move carries a stand-in member along at its frozen offset, and the
/// move never leaks the stand-in into the fit / fullscreen / pin sets.
#[test]
#[allow(clippy::mutable_key_type)]
fn group_move_carries_stand_in_without_membership_leak() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let gap = f.state().config.snap_gap as i32;

    let ida = f.add_client();
    map_window(&mut f, ida, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    f.state().map_window(
        StageWindow::Client(a.clone()),
        Point::from((400, 300)),
        true,
    );
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((800 + gap, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );

    let members = f
        .state()
        .cluster_snapshot_for_drag(&StageWindow::Client(a.clone()), Point::from((400, 300)));
    let standin = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    assert_eq!(members.len(), 1, "the stand-in is captured as a member");
    let (member, offset) = &members[0];
    assert_eq!(*member, standin);
    assert_eq!(*offset, Point::from((400 + gap, 0)));

    // Apply the move the grab would: primary to a new spot, member at + offset.
    let new_a = Point::from((900, 500));
    for (m, off) in &members {
        f.state().map_window(m.clone(), new_a + *off, false);
    }
    f.state().map_window(StageWindow::Client(a), new_a, true);

    assert_eq!(
        f.state().stage.position_of(&standin),
        Some(new_a + Point::from((400 + gap, 0))),
        "the stand-in rode along with the group"
    );
    assert!(!f.state().stage.is_fit(&standin), "no fit-set leak");
    assert!(
        !f.state().is_window_fullscreen(&standin),
        "no fullscreen leak"
    );
    assert!(!f.state().is_pinned(&standin), "no pin leak");

    f.state().dismiss_suspended(sid);
}

/// A `move-snapped-windows` binding initiated ON a stand-in group-moves its
/// cluster — the stand-in is the primary and a client partner rides along —
/// instead of dragging the stand-in out of its cluster (plain `move-window`
/// stays single-window). Drives the grab entry points: press then one motion.
#[test]
fn group_move_on_stand_in_carries_cluster_partner() {
    use smithay::input::pointer::MotionEvent;

    let config = Config::from_toml(
        r#"
        [decorations]
        default_mode = "server"
        [mouse.anywhere]
        "super+left" = "move-snapped-windows"
    "#,
    )
    .unwrap();
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let gap = f.state().config.snap_gap as i32;

    // A stand-in with a client partner adjacent on its right → one cluster.
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let idc = f.add_client();
    map_window(&mut f, idc, "c", (400, 300));
    let client = window_by_app_id(&mut f, "c").unwrap();
    f.state().map_window(
        StageWindow::Client(client.clone()),
        Point::from((800 + gap, 300)),
        true,
    );

    // Held-modifier group-move press over the stand-in body starts the grab.
    let pointer = f.state().seat.get_pointer().unwrap();
    let held = ModifiersState {
        logo: true,
        ..Default::default()
    };
    let serial = SERIAL_COUNTER.next_serial();
    let consumed =
        f.state()
            .try_suspended_button(&pointer, pt(500.0, 450.0), BTN_LEFT, serial, held);
    assert!(consumed);
    assert!(
        f.state().seat.get_pointer().unwrap().is_grabbed(),
        "a group-move binding on the stand-in started a grab"
    );

    let before = f
        .state()
        .stage
        .position_of(&StageWindow::Client(client.clone()))
        .unwrap();

    // Drag by (+100, +50); the client partner rides the same delta.
    let event = MotionEvent {
        location: pt(600.0, 500.0),
        serial: SERIAL_COUNTER.next_serial(),
        time: 0,
    };
    pointer.motion(f.state(), None, &event);

    let after = f
        .state()
        .stage
        .position_of(&StageWindow::Client(client.clone()))
        .unwrap();
    assert_eq!(
        after - before,
        Point::from((100, 50)),
        "the client partner moved with the group, not left behind"
    );

    f.state().dismiss_suspended(sid);
}

/// A suspended resize derives cluster participation from the BINDING variant,
/// like a client's — not from the SSD-border config flag. `resize-window-snapped`
/// cascades a snapped neighbor even with `decoration_resize_snapped = false`, and
/// plain `resize-window` leaves it alone even with the flag on.
#[test]
fn suspended_resize_cluster_follows_binding_variant() {
    use smithay::input::pointer::MotionEvent;

    // Snapped variant + flag OFF → the neighbor still cascades.
    {
        let config = Config::from_toml(
            r#"
            [decorations]
            default_mode = "server"
            [mouse]
            decoration_resize_snapped = false
            [mouse.anywhere]
            "super+left" = "resize-window-snapped"
        "#,
        )
        .unwrap();
        let mut f = Fixture::with_config(config);
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let gap = f.state().config.snap_gap as i32;

        let sid = f.state().insert_suspended_for_test(
            1,
            Point::from((400, 300)),
            Size::from((400, 300)),
            "s",
            "S",
        );
        let idc = f.add_client();
        map_window(&mut f, idc, "c", (400, 300));
        let client = window_by_app_id(&mut f, "c").unwrap();
        f.state().map_window(
            StageWindow::Client(client.clone()),
            Point::from((800 + gap, 300)),
            true,
        );

        let pointer = f.state().seat.get_pointer().unwrap();
        let held = ModifiersState {
            logo: true,
            ..Default::default()
        };
        let serial = SERIAL_COUNTER.next_serial();
        // Right third of the stand-in → a right-edge resize.
        f.state()
            .try_suspended_button(&pointer, pt(700.0, 450.0), BTN_LEFT, serial, held);
        let before = f
            .state()
            .stage
            .position_of(&StageWindow::Client(client.clone()))
            .unwrap();
        let event = MotionEvent {
            location: pt(800.0, 450.0),
            serial: SERIAL_COUNTER.next_serial(),
            time: 0,
        };
        pointer.motion(f.state(), None, &event);
        let after = f
            .state()
            .stage
            .position_of(&StageWindow::Client(client.clone()))
            .unwrap();
        assert_eq!(
            after.x - before.x,
            100,
            "the snapped variant cascaded the neighbor despite the flag being off"
        );
        f.state().dismiss_suspended(sid);
    }

    // Plain variant + flag ON → the neighbor stays put.
    {
        let config = Config::from_toml(
            r#"
            [decorations]
            default_mode = "server"
            [mouse]
            decoration_resize_snapped = true
            [mouse.anywhere]
            "super+left" = "resize-window"
        "#,
        )
        .unwrap();
        let mut f = Fixture::with_config(config);
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let gap = f.state().config.snap_gap as i32;

        let sid = f.state().insert_suspended_for_test(
            1,
            Point::from((400, 300)),
            Size::from((400, 300)),
            "s",
            "S",
        );
        let idc = f.add_client();
        map_window(&mut f, idc, "c", (400, 300));
        let client = window_by_app_id(&mut f, "c").unwrap();
        f.state().map_window(
            StageWindow::Client(client.clone()),
            Point::from((800 + gap, 300)),
            true,
        );

        let pointer = f.state().seat.get_pointer().unwrap();
        let held = ModifiersState {
            logo: true,
            ..Default::default()
        };
        let serial = SERIAL_COUNTER.next_serial();
        f.state()
            .try_suspended_button(&pointer, pt(700.0, 450.0), BTN_LEFT, serial, held);
        let before = f
            .state()
            .stage
            .position_of(&StageWindow::Client(client.clone()))
            .unwrap();
        let event = MotionEvent {
            location: pt(800.0, 450.0),
            serial: SERIAL_COUNTER.next_serial(),
            time: 0,
        };
        pointer.motion(f.state(), None, &event);
        let after = f
            .state()
            .stage
            .position_of(&StageWindow::Client(client.clone()))
            .unwrap();
        assert_eq!(
            after, before,
            "plain resize left the neighbor put despite the flag being on"
        );
        f.state().dismiss_suspended(sid);
    }
}

/// Two adjacent restored stand-ins (all a session materialize produces) form a
/// cluster with no live client involved — membership is geometry-derived, so it
/// comes for free.
#[test]
#[allow(clippy::mutable_key_type)]
fn restored_adjacent_stand_ins_form_a_cluster() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let gap = f.state().config.snap_gap as i32;

    let a = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "a",
        "A",
    );
    let b = f.state().insert_suspended_for_test(
        2,
        Point::from((800 + gap, 300)),
        Size::from((400, 300)),
        "b",
        "B",
    );
    let a_elem = StageWindow::Suspended(f.state().find_suspended(a).unwrap());
    let b_elem = StageWindow::Suspended(f.state().find_suspended(b).unwrap());

    let rects = f.state().all_windows_with_snap_rects();
    let cluster = driftwm::layout::cluster::cluster_of(&a_elem, &rects, f.state().config.snap_gap);

    assert_eq!(cluster.len(), 2);
    assert!(cluster.contains(&b_elem), "adjacent stand-ins cluster");

    f.state().dismiss_suspended(a);
    f.state().dismiss_suspended(b);
}
