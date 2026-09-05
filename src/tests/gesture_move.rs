//! Trackpad-gesture and touch move entry points. Both pick their target with
//! the stand-in-aware `draggable_element_under`, so a suspended stand-in drags
//! exactly like a live window; pinned windows keep their own screen-space branch
//! and widgets stay grab-proof. Driven through the real entry points
//! (`try_start_gesture_move`, `build_touch_move_grab`, `touch_suspended_hit`)
//! rather than a hand-installed grab, so the pickers are under test too.

use driftwm::config::Action;
use smithay::backend::input::TouchSlot;
use smithay::desktop::Window;
use smithay::input::pointer::MotionEvent;
use smithay::input::touch::{
    GrabStartData as TouchGrabStartData, MotionEvent as TouchMotionEvent, UpEvent,
};
use smithay::output::Output;
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};

use crate::decorations::DecorationHit;
use crate::state::StageWindow;

use super::real::TempDir;
use super::{Fixture, config, give_ssd, is_activated, map_window, window_by_app_id};

fn pt(x: f64, y: f64) -> Point<f64, Logical> {
    Point::from((x, y))
}

/// Camera at the canvas origin, zoom 1: canvas == screen.
fn origin_view(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });
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

/// End the swipe the way `on_gesture_swipe_end` does — there's no button to
/// release on a gesture.
fn end_swipe(f: &mut Fixture) {
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    pointer.unset_grab(f.state(), serial, 0);
}

fn slot() -> TouchSlot {
    TouchSlot::from(Some(0))
}

/// Install the touch move grab the 3-finger drag handoff would, for a finger at
/// canvas-space `at`. `false` when nothing draggable is there (keep panning).
fn start_touch_gesture_move(f: &mut Fixture, at: Point<f64, Logical>, output: Output) -> bool {
    let start = TouchGrabStartData {
        focus: None,
        slot: slot(),
        location: at,
    };
    let Some(grab) = f.state().build_touch_move_grab(at, start, output, 1, false) else {
        return false;
    };
    let touch = f.state().seat.get_touch().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    touch.set_grab(f.state(), grab, serial);
    true
}

fn touch_motion(f: &mut Fixture, loc: Point<f64, Logical>) {
    let touch = f.state().seat.get_touch().unwrap();
    touch.motion(
        f.state(),
        None,
        &TouchMotionEvent {
            slot: slot(),
            location: loc,
            time: 0,
        },
    );
}

fn lift_finger(f: &mut Fixture) {
    let touch = f.state().seat.get_touch().unwrap();
    touch.up(
        f.state(),
        &UpEvent {
            slot: slot(),
            serial: SERIAL_COUNTER.next_serial(),
            time: 0,
        },
    );
}

/// Drag geometry shared by the client and stand-in halves of the parity tests:
/// an element at (400, 300) grabbed at its center and dragged by (+100, +30).
const INITIAL: Point<i32, Logical> = Point::new(400, 300);
const EXPECTED: Point<i32, Logical> = Point::new(500, 330);
fn grab_point() -> Point<f64, Logical> {
    pt(600.0, 450.0)
}
fn drag_path() -> [Point<f64, Logical>; 3] {
    [pt(650.0, 470.0), pt(680.0, 500.0), pt(700.0, 480.0)]
}

/// A trackpad move gesture lands a client and a stand-in on the same canvas
/// point: the swipe picker resolves both, and both ride the one `MoveGrab`.
#[test]
fn swipe_move_lands_client_and_stand_in_alike() {
    {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let id = f.add_client();
        map_window(&mut f, id, "c", (400, 300));
        let window = window_by_app_id(&mut f, "c").unwrap();
        f.state()
            .map_window(StageWindow::Client(window.clone()), INITIAL, true);

        assert!(
            f.state().try_start_gesture_move(grab_point(), false),
            "the swipe found the client under the cursor"
        );
        for m in drag_path() {
            motion(&mut f, m);
        }

        assert_eq!(
            f.state().stage.position_of(&StageWindow::Client(window)),
            Some(EXPECTED),
            "the client landed at the natural drag destination"
        );
        end_swipe(&mut f);
    }

    {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f
            .state()
            .insert_suspended_for_test(1, INITIAL, Size::from((400, 300)), "s", "S");

        assert!(
            f.state().try_start_gesture_move(grab_point(), false),
            "the swipe found the stand-in under the cursor"
        );
        for m in drag_path() {
            motion(&mut f, m);
        }

        let s = f.state().find_suspended(sid).unwrap();
        assert_eq!(
            f.state().stage.position_of(&StageWindow::Suspended(s)),
            Some(EXPECTED),
            "the stand-in landed at the same destination as the client"
        );
        end_swipe(&mut f);
        f.state().dismiss_suspended(sid);
    }
}

/// A widget is immovable: the move gesture finds nothing to drag and the caller
/// falls back to panning the canvas.
#[test]
fn swipe_move_leaves_a_widget_alone() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "w"
widget = true
"#,
    ));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    map_window(&mut f, id, "w", (400, 300));
    let widget = window_by_app_id(&mut f, "w").unwrap();
    f.state()
        .map_window(StageWindow::Client(widget.clone()), INITIAL, true);

    assert!(
        !f.state().try_start_gesture_move(grab_point(), false),
        "the gesture declines the widget, leaving the caller to pan"
    );
    assert!(
        !f.state().seat.get_pointer().unwrap().is_grabbed(),
        "no move grab was installed over the widget"
    );
    assert_eq!(
        f.state().stage.position_of(&StageWindow::Client(widget)),
        Some(INITIAL),
        "the widget stayed put"
    );
}

/// A screen-pinned window keeps the screen-space branch: the gesture drag
/// rewrites its pin site rather than treating it as canvas content.
#[test]
fn swipe_move_on_a_pinned_window_moves_its_pin_site() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [320, 240]
"#,
    ));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    map_window(&mut f, id, "pin", (320, 240));
    let window = window_by_app_id(&mut f, "pin").unwrap();
    let before = f.state().stage.pin_of(&window).unwrap().screen_pos;

    // Canvas == screen here, so the pin site's screen coords double as the
    // gesture's canvas position.
    let grab_at = pt(before.x as f64 + 10.0, before.y as f64 + 10.0);
    assert!(
        f.state().try_start_gesture_move(grab_at, false),
        "the swipe found the pinned window in screen space"
    );
    motion(&mut f, grab_at + pt(50.0, 30.0));

    assert_eq!(
        f.state().stage.pin_of(&window).unwrap().screen_pos,
        before + Point::from((50, 30)),
        "the pinned window tracked the finger in screen space"
    );
    end_swipe(&mut f);
}

/// A touch move gesture lands a client and a stand-in on the same canvas point,
/// through the touch picker and `MoveGrab::new_touch`.
#[test]
fn touch_gesture_move_lands_client_and_stand_in_alike() {
    {
        let mut f = Fixture::new();
        let out = f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let id = f.add_client();
        map_window(&mut f, id, "c", (400, 300));
        let window = window_by_app_id(&mut f, "c").unwrap();
        f.state()
            .map_window(StageWindow::Client(window.clone()), INITIAL, true);

        assert!(
            start_touch_gesture_move(&mut f, grab_point(), out),
            "the touch gesture found the client under the finger"
        );
        for m in drag_path() {
            touch_motion(&mut f, m);
        }

        assert_eq!(
            f.state().stage.position_of(&StageWindow::Client(window)),
            Some(EXPECTED),
            "the client followed the finger"
        );
        lift_finger(&mut f);
    }

    {
        let mut f = Fixture::new();
        let out = f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f
            .state()
            .insert_suspended_for_test(1, INITIAL, Size::from((400, 300)), "s", "S");

        assert!(
            start_touch_gesture_move(&mut f, grab_point(), out),
            "the touch gesture found the stand-in under the finger"
        );
        for m in drag_path() {
            touch_motion(&mut f, m);
        }

        let s = f.state().find_suspended(sid).unwrap();
        assert_eq!(
            f.state().stage.position_of(&StageWindow::Suspended(s)),
            Some(EXPECTED),
            "the stand-in followed the finger to the same destination"
        );
        lift_finger(&mut f);
        f.state().dismiss_suspended(sid);
    }
}

/// A settled touch drag is durable session state exactly as a pointer drag is:
/// the canvas point the finger dropped the window on is where a restart has to
/// bring it back, for a live client as much as for a stand-in.
#[test]
fn touch_move_marks_the_session_store_dirty_for_live_and_stand_in() {
    let tmp = TempDir::new();

    {
        let mut f = Fixture::new();
        let out = f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        // Without a path the mark-dirty sites all return early.
        f.state().session_store.path = Some(tmp.path().join("client.json"));

        let id = f.add_client();
        map_window(&mut f, id, "c", (400, 300));
        let window = window_by_app_id(&mut f, "c").unwrap();
        f.state()
            .map_window(StageWindow::Client(window), INITIAL, true);
        assert!(
            start_touch_gesture_move(&mut f, grab_point(), out),
            "precondition: the touch gesture found the client under the finger"
        );
        for m in drag_path() {
            touch_motion(&mut f, m);
        }

        // The map and the grab install's raise-and-focus arm the write as well,
        // so flush here: past this point only the lift can arm it again.
        f.state().session_store_write_now();
        assert!(!f.state().session_store_dirty());

        lift_finger(&mut f);
        assert!(
            f.state().session_store_dirty(),
            "the client's dropped position is queued for the durable write"
        );
        // Cancel the debounce timer the lift armed; `debug_counters` has no
        // entry for event-loop timers, so teardown would not catch it.
        f.state().session_store_write_now();
    }

    {
        let mut f = Fixture::new();
        let out = f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        f.state().session_store.path = Some(tmp.path().join("stand-in.json"));

        let sid = f
            .state()
            .insert_suspended_for_test(1, INITIAL, Size::from((400, 300)), "s", "S");
        assert!(
            start_touch_gesture_move(&mut f, grab_point(), out),
            "precondition: the touch gesture found the stand-in under the finger"
        );
        for m in drag_path() {
            touch_motion(&mut f, m);
        }

        f.state().session_store_write_now();
        assert!(!f.state().session_store_dirty());

        lift_finger(&mut f);
        assert!(
            f.state().session_store_dirty(),
            "the stand-in's dropped position is queued for the same write"
        );

        // Dismiss clears the stand-in; the flush still cancels the debounce
        // timer the lift armed, as in the block above.
        f.state().dismiss_suspended(sid);
        f.state().session_store_write_now();
    }
}

/// The touch gesture picker declines a widget and a pinned window: the widget is
/// immovable, and the pinned window is claimed earlier by the screen-space
/// pinned branch, which the canvas picker must not shadow.
#[test]
fn touch_gesture_move_declines_widgets_and_pinned_windows() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "w"
widget = true
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [320, 240]
"#,
    ));
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);

    let idw = f.add_client();
    map_window(&mut f, idw, "w", (400, 300));
    let widget = window_by_app_id(&mut f, "w").unwrap();
    f.state()
        .map_window(StageWindow::Client(widget), INITIAL, true);

    assert!(
        !start_touch_gesture_move(&mut f, grab_point(), out.clone()),
        "a widget is not draggable by touch either"
    );

    let idp = f.add_client();
    map_window(&mut f, idp, "pin", (320, 240));
    let pin = window_by_app_id(&mut f, "pin").unwrap();
    let site = f.state().stage.pin_of(&pin).unwrap().screen_pos;
    let over_pin = pt(site.x as f64 + 10.0, site.y as f64 + 10.0);

    assert!(
        !start_touch_gesture_move(&mut f, over_pin, out),
        "the canvas picker leaves the pinned window to the pinned branch"
    );
    assert!(
        f.state().pinned_element_under(over_pin).is_some(),
        "…which still finds it in screen space"
    );
}

/// Occlusion is a stop, not a skip. A widget covering the grab point makes both
/// move gestures find nothing at all — they must never reach past it to drag the
/// client underneath, which the user cannot see.
#[test]
fn move_gestures_do_not_reach_a_client_behind_a_widget() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "w"
widget = true
"#,
    ));
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);

    let idc = f.add_client();
    map_window(&mut f, idc, "c", (400, 300));
    let client = window_by_app_id(&mut f, "c").unwrap();
    f.state()
        .map_window(StageWindow::Client(client.clone()), INITIAL, true);

    // Mapped last over the same rect, so the widget is the topmost element at
    // the grab point.
    let idw = f.add_client();
    map_window(&mut f, idw, "w", (400, 300));
    let widget = window_by_app_id(&mut f, "w").unwrap();
    f.state()
        .map_window(StageWindow::Client(widget.clone()), INITIAL, true);
    assert_eq!(
        f.state().stage.windows().next_back(),
        Some(&StageWindow::Client(widget)),
        "precondition: the widget sits above the client"
    );

    assert!(
        !f.state().try_start_gesture_move(grab_point(), false),
        "the trackpad gesture stops at the widget"
    );
    assert!(
        !start_touch_gesture_move(&mut f, grab_point(), out),
        "the touch gesture stops at the widget too"
    );
    assert_eq!(
        f.state().stage.position_of(&StageWindow::Client(client)),
        Some(INITIAL),
        "the client behind the widget never moved"
    );
}

/// A stand-in covering a client claims the drag: the gesture moves the stand-in
/// and the hidden client stays where it was.
#[test]
fn swipe_move_over_a_stand_in_drags_it_not_the_client_beneath() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);

    let id = f.add_client();
    map_window(&mut f, id, "c", (400, 300));
    let client = window_by_app_id(&mut f, "c").unwrap();
    f.state()
        .map_window(StageWindow::Client(client.clone()), INITIAL, true);
    let sid = f
        .state()
        .insert_suspended_for_test(1, INITIAL, Size::from((400, 300)), "s", "S");

    assert!(f.state().try_start_gesture_move(grab_point(), false));
    for m in drag_path() {
        motion(&mut f, m);
    }

    let s = f.state().find_suspended(sid).unwrap();
    assert_eq!(
        f.state().stage.position_of(&StageWindow::Suspended(s)),
        Some(EXPECTED),
        "the stand-in took the drag"
    );
    assert_eq!(
        f.state().stage.position_of(&StageWindow::Client(client)),
        Some(INITIAL),
        "the client beneath it stayed put"
    );

    end_swipe(&mut f);
    f.state().dismiss_suspended(sid);
}

/// Both gesture moves arm the interactive-move guard for the length of the drag
/// and disarm it on teardown — that's what stops a relaunching app being adopted
/// into the stand-in's slot mid-drag and fighting the grab.
#[test]
fn gesture_moves_arm_the_relaunch_adoption_guard() {
    {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f
            .state()
            .insert_suspended_for_test(1, INITIAL, Size::from((400, 300)), "s", "S");
        let element = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
        assert!(
            !f.state().element_under_interactive_grab(&element),
            "precondition: nothing is armed before the drag"
        );

        assert!(f.state().try_start_gesture_move(grab_point(), false));
        motion(&mut f, drag_path()[0]);
        assert!(
            f.state().element_under_interactive_grab(&element),
            "the trackpad drag is armed against relaunch adoption"
        );

        end_swipe(&mut f);
        assert!(
            !f.state().element_under_interactive_grab(&element),
            "the arm is balanced when the gesture ends"
        );
        f.state().dismiss_suspended(sid);
    }

    {
        let mut f = Fixture::new();
        let out = f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f
            .state()
            .insert_suspended_for_test(1, INITIAL, Size::from((400, 300)), "s", "S");
        let element = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());

        assert!(start_touch_gesture_move(&mut f, grab_point(), out));
        touch_motion(&mut f, drag_path()[0]);
        assert!(
            f.state().element_under_interactive_grab(&element),
            "the touch drag is armed too"
        );

        lift_finger(&mut f);
        assert!(
            !f.state().element_under_interactive_grab(&element),
            "and disarmed when the finger lifts"
        );
        f.state().dismiss_suspended(sid);
    }
}

/// Tapping a stand-in's title bar and dragging moves it, like a live window's
/// title bar — the bar is a drag target on touch, not just a focus target.
#[test]
fn touch_bar_drag_moves_a_stand_in() {
    let mut f = Fixture::new();
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f
        .state()
        .insert_suspended_for_test(1, INITIAL, Size::from((400, 300)), "s", "S");

    // A point on the bar band, which sits directly above the content rect.
    let bar = pt(INITIAL.x as f64 + 50.0, INITIAL.y as f64 - 4.0);
    let s = f.state().find_suspended(sid).unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    f.state()
        .touch_suspended_hit(&s, DecorationHit::TitleBar, slot(), bar, serial, out);
    assert!(
        f.state().seat.get_touch().unwrap().is_grabbed(),
        "the bar tap started a move grab"
    );

    touch_motion(&mut f, bar + pt(120.0, 60.0));

    assert_eq!(
        f.state().stage.position_of(&StageWindow::Suspended(s)),
        Some(INITIAL + Point::from((120, 60))),
        "the stand-in followed the finger from its bar"
    );

    lift_finger(&mut f);
    f.state().dismiss_suspended(sid);
}

/// A above B: A's SSD title bar (y in `[0, 25)`) sits directly over B's
/// content, while A's own content starts at y = 25 — a point in the bar hits
/// neither window's content, only A's chrome.
fn title_bar_over_lower_content(f: &mut Fixture) -> (Window, Window) {
    let id = f.add_client();
    map_window(f, id, "b", (400, 300));
    let b = window_by_app_id(f, "b").unwrap();
    f.state()
        .map_window(StageWindow::Client(b.clone()), Point::from((0, 0)), false);

    map_window(f, id, "a", (400, 300));
    let a = window_by_app_id(f, "a").unwrap();
    f.state()
        .map_window(StageWindow::Client(a.clone()), Point::from((0, 25)), false);
    give_ssd(f, &a);

    (a, b)
}

/// The interleaved walk must prefer A's chrome over B's content since A is
/// on top — scanning every window's content before any chrome would resolve
/// this point to B instead.
#[test]
fn draggable_element_under_prefers_a_higher_windows_title_bar_over_lower_content() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let (a, _b) = title_bar_over_lower_content(&mut f);

    let point = pt(100.0, 10.0);
    assert_eq!(
        f.state().draggable_element_under(point),
        Some(StageWindow::Client(a)),
        "a point on the topmost window's title bar must resolve to that window"
    );
}

/// `MoveWindow`/`ResizeWindow` resolve their target through
/// `topmost_client_under`, the same interleaved walk `draggable_element_under`
/// uses. The full Mod+drag path isn't reachable here — `on_pointer_button`
/// takes a live `InputBackend` event the fixture can't construct — so this
/// exercises the shared primitive directly: a press on a title bar must
/// resolve to the window instead of finding nothing.
#[test]
fn topmost_client_under_resolves_an_ssd_title_bar_to_its_window() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();

    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();
    f.state()
        .map_window(StageWindow::Client(window.clone()), INITIAL, false);
    give_ssd(&mut f, &window);

    // A point in the title-bar band, above the window's own content.
    let bar = pt(INITIAL.x as f64 + 100.0, INITIAL.y as f64 - 10.0);
    assert_eq!(
        f.state().topmost_client_under(bar),
        Some(window),
        "a title-bar press must resolve to the window, not miss entirely"
    );
}

/// `FocusCenter` resolves through the same unified walk, so pointing at A's
/// title bar targets A even with B's content underneath and B currently
/// holding focus.
#[test]
fn focus_center_on_a_title_bar_targets_that_window_not_the_content_beneath_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let (a, b) = title_bar_over_lower_content(&mut f);

    f.state()
        .set_activated_exclusive(&StageWindow::Client(b.clone()));
    assert!(is_activated(&b));

    f.state().warp_pointer(pt(100.0, 10.0));
    f.state().execute_action(&Action::FocusCenter);

    assert!(
        is_activated(&a),
        "FocusCenter on A's title bar must resolve to A, not the content beneath it"
    );
    assert!(!is_activated(&b));
}
