//! Trackpad-gesture and touch resize entry points. Both pick their target with
//! the stand-in-aware `draggable_element_under`, so a suspended stand-in resizes
//! exactly like a live window — except that a stand-in's size is the
//! compositor's own number, so no configure is sent and no surface state is
//! written. Pinned windows keep their screen-space branch and widgets stay
//! grab-proof. Driven through the real entry points (`try_start_gesture_resize`,
//! `build_touch_gesture_resize_grab`) rather than a hand-installed grab, so the
//! pickers are under test too.

use std::cell::RefCell;

use smithay::backend::input::TouchSlot;
use smithay::input::pointer::MotionEvent;
use smithay::input::touch::{
    GrabStartData as TouchGrabStartData, MotionEvent as TouchMotionEvent, UpEvent,
};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};
use smithay::wayland::compositor::with_states;

use crate::grabs::ResizeState;
use crate::state::StageWindow;

use super::{
    Fixture, adopt_last_configure, assert_resize_entered, client_sees_maximized, config,
    fit_and_frame, map_window, seed_fit_and_fill, server_surface, window_by_app_id,
};

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

/// Install the touch resize grab the hold-then-drag handoff would, for fingers
/// landed at canvas-space `at`. `false` when nothing resizable is there (keep
/// panning).
fn start_touch_gesture_resize(f: &mut Fixture, at: Point<f64, Logical>, output: Output) -> bool {
    let start = TouchGrabStartData {
        focus: None,
        slot: slot(),
        location: at,
    };
    let Some(grab) = f
        .state()
        .build_touch_gesture_resize_grab(at, start, output, 1, false)
    else {
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

/// Resize geometry shared by the client and stand-in halves: an element at
/// (400, 300) sized 400×300, grabbed in its right third (→ a `Right` edge on
/// both the 3×3 touch grid and the pointer's edge inference) and dragged 200px
/// right.
const INITIAL: Point<i32, Logical> = Point::new(400, 300);
fn initial_size() -> Size<i32, Logical> {
    Size::from((400, 300))
}
fn grown_size() -> Size<i32, Logical> {
    Size::from((600, 300))
}
fn grab_point() -> Point<f64, Logical> {
    pt(700.0, 450.0)
}
fn drag_to() -> Point<f64, Logical> {
    pt(900.0, 450.0)
}

/// The size the compositor most recently configured on `surface`'s window.
fn last_configured_size(
    f: &mut Fixture,
    id: super::client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) -> (i32, i32) {
    f.client(id)
        .window(surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size
}

/// Read the server-side `ResizeState` on `surface`, `Idle` when the grab never
/// seeded one.
fn resize_state(surface: &WlSurface) -> ResizeState {
    with_states(surface, |states| {
        states
            .data_map
            .get::<RefCell<ResizeState>>()
            .map(|cell| *cell.borrow())
            .unwrap_or(ResizeState::Idle)
    })
}

/// A trackpad resize gesture grows a client and a stand-in alike: the client is
/// configured at the new size, the stand-in's own size follows the drag.
#[test]
fn swipe_resize_grows_client_and_stand_in_alike() {
    {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let id = f.add_client();
        let csurface = map_window(&mut f, id, "c", (400, 300));
        let window = window_by_app_id(&mut f, "c").unwrap();
        f.state()
            .map_window(StageWindow::Client(window), INITIAL, true);

        assert!(
            f.state().try_start_gesture_resize(grab_point(), false),
            "the swipe found the client under the cursor"
        );
        motion(&mut f, drag_to());
        f.double_roundtrip(id);

        assert_eq!(
            last_configured_size(&mut f, id, &csurface),
            (600, 300),
            "the client was configured at the dragged size"
        );
        end_swipe(&mut f);
    }

    {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f
            .state()
            .insert_suspended_for_test(1, INITIAL, initial_size(), "s", "S");

        assert!(
            f.state().try_start_gesture_resize(grab_point(), false),
            "the swipe found the stand-in under the cursor"
        );
        motion(&mut f, drag_to());

        let s = f.state().find_suspended(sid).unwrap();
        assert_eq!(
            s.size.get(),
            grown_size(),
            "the stand-in reached the same size as the client"
        );
        end_swipe(&mut f);
        f.state().dismiss_suspended(sid);
    }
}

/// A touch resize gesture grows a client and a stand-in alike, through the touch
/// picker and `ResizeGrab::new_touch`.
#[test]
fn touch_gesture_resize_grows_client_and_stand_in_alike() {
    {
        let mut f = Fixture::new();
        let out = f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let id = f.add_client();
        let csurface = map_window(&mut f, id, "c", (400, 300));
        let window = window_by_app_id(&mut f, "c").unwrap();
        f.state()
            .map_window(StageWindow::Client(window), INITIAL, true);

        assert!(
            start_touch_gesture_resize(&mut f, grab_point(), out),
            "the touch gesture found the client under the fingers"
        );
        touch_motion(&mut f, drag_to());
        f.double_roundtrip(id);

        assert_eq!(
            last_configured_size(&mut f, id, &csurface),
            (600, 300),
            "the client was configured at the dragged size"
        );
        lift_finger(&mut f);
    }

    {
        let mut f = Fixture::new();
        let out = f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f
            .state()
            .insert_suspended_for_test(1, INITIAL, initial_size(), "s", "S");

        assert!(
            start_touch_gesture_resize(&mut f, grab_point(), out),
            "the touch gesture found the stand-in under the fingers"
        );
        touch_motion(&mut f, drag_to());

        let s = f.state().find_suspended(sid).unwrap();
        assert_eq!(
            s.size.get(),
            grown_size(),
            "the stand-in reached the same size as the client"
        );
        lift_finger(&mut f);
        f.state().dismiss_suspended(sid);
    }
}

/// A left-edge gesture resize keeps the client's opposite edge fixed: the grab
/// seeds `ResizeState::Resizing` on the surface, and the ack/commit uses it to
/// shift the window left by exactly the width it gained. Without that seed the
/// commit has nothing to reposition from and the right edge walks.
#[test]
fn left_edge_gesture_resize_keeps_a_client_right_edge_fixed() {
    // Grab in the left third → a `Left` edge on both the pointer's inference and
    // the 3×3 touch grid; drag 100px further left.
    let grab_at = pt(450.0, 450.0);
    let drag_left = pt(350.0, 450.0);

    {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let id = f.add_client();
        let csurface = map_window(&mut f, id, "c", (400, 300));
        let window = window_by_app_id(&mut f, "c").unwrap();
        f.state()
            .map_window(StageWindow::Client(window.clone()), INITIAL, true);

        assert!(f.state().try_start_gesture_resize(grab_at, false));
        motion(&mut f, drag_left);
        f.double_roundtrip(id);
        assert_eq!(
            last_configured_size(&mut f, id, &csurface),
            (500, 300),
            "the left-edge drag widened the client"
        );
        adopt_last_configure(&mut f, id, &csurface);

        assert_eq!(
            f.state().stage.position_of(&StageWindow::Client(window)),
            Some(Point::from((300, 300))),
            "the trackpad resize moved the left edge, leaving the right edge at 800"
        );
        end_swipe(&mut f);
    }

    {
        let mut f = Fixture::new();
        let out = f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let id = f.add_client();
        let csurface = map_window(&mut f, id, "c", (400, 300));
        let window = window_by_app_id(&mut f, "c").unwrap();
        f.state()
            .map_window(StageWindow::Client(window.clone()), INITIAL, true);

        assert!(start_touch_gesture_resize(&mut f, grab_at, out));
        touch_motion(&mut f, drag_left);
        f.double_roundtrip(id);
        assert_eq!(
            last_configured_size(&mut f, id, &csurface),
            (500, 300),
            "the left-edge drag widened the client"
        );
        adopt_last_configure(&mut f, id, &csurface);

        assert_eq!(
            f.state().stage.position_of(&StageWindow::Client(window)),
            Some(Point::from((300, 300))),
            "the touch resize anchors the right edge the same way"
        );
        lift_finger(&mut f);
    }
}

/// Neither gesture may shrink a stand-in below the usable-chrome floor: a drag
/// far past it stops at `MIN_SUSPENDED_SIZE`.
#[test]
fn gesture_resize_floors_a_stand_in_at_min_size() {
    {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f
            .state()
            .insert_suspended_for_test(1, INITIAL, initial_size(), "s", "S");

        assert!(f.state().try_start_gesture_resize(grab_point(), false));
        motion(&mut f, pt(400.0, 450.0));

        let s = f.state().find_suspended(sid).unwrap();
        assert_eq!(
            s.size.get(),
            Size::from((120, 300)),
            "the trackpad shrink clamps to MIN_SUSPENDED_SIZE"
        );
        end_swipe(&mut f);
        f.state().dismiss_suspended(sid);
    }

    {
        let mut f = Fixture::new();
        let out = f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f
            .state()
            .insert_suspended_for_test(1, INITIAL, initial_size(), "s", "S");

        assert!(start_touch_gesture_resize(&mut f, grab_point(), out));
        touch_motion(&mut f, pt(400.0, 450.0));

        let s = f.state().find_suspended(sid).unwrap();
        assert_eq!(
            s.size.get(),
            Size::from((120, 300)),
            "the touch shrink clamps to MIN_SUSPENDED_SIZE too"
        );
        lift_finger(&mut f);
        f.state().dismiss_suspended(sid);
    }
}

/// A widget is not resizable: both gestures find nothing and the caller falls
/// back to panning the canvas.
#[test]
fn gesture_resize_leaves_a_widget_alone() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "w"
widget = true
"#,
    ));
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    let wsurface = map_window(&mut f, id, "w", (400, 300));
    let widget = window_by_app_id(&mut f, "w").unwrap();
    f.state()
        .map_window(StageWindow::Client(widget.clone()), INITIAL, true);
    f.double_roundtrip(id);
    let before = last_configured_size(&mut f, id, &wsurface);

    assert!(
        !f.state().try_start_gesture_resize(grab_point(), false),
        "the trackpad gesture declines the widget, leaving the caller to pan"
    );
    assert!(
        !start_touch_gesture_resize(&mut f, grab_point(), out),
        "the touch gesture declines it too"
    );
    assert!(
        !f.state().seat.get_pointer().unwrap().is_grabbed(),
        "no resize grab was installed over the widget"
    );
    f.double_roundtrip(id);
    assert_eq!(
        last_configured_size(&mut f, id, &wsurface),
        before,
        "the widget was never configured at a new size"
    );
}

/// Occlusion is a stop, not a skip. A widget covering the grab point makes both
/// resize gestures find nothing at all — they must never reach past it to resize
/// the client underneath, which the user cannot see.
#[test]
fn resize_gestures_do_not_reach_a_client_behind_a_widget() {
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
    let csurface = map_window(&mut f, idc, "c", (400, 300));
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
    f.double_roundtrip(idc);
    let before = last_configured_size(&mut f, idc, &csurface);

    assert!(
        !f.state().try_start_gesture_resize(grab_point(), false),
        "the trackpad gesture stops at the widget"
    );
    assert!(
        !start_touch_gesture_resize(&mut f, grab_point(), out),
        "the touch gesture stops at the widget too"
    );
    f.double_roundtrip(idc);
    assert_eq!(
        last_configured_size(&mut f, idc, &csurface),
        before,
        "the client behind the widget was never resized"
    );
}

/// A stand-in covering a client claims the resize: the gesture sizes the
/// stand-in and the hidden client is never configured.
#[test]
fn swipe_resize_over_a_stand_in_sizes_it_not_the_client_beneath() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);

    let id = f.add_client();
    let csurface = map_window(&mut f, id, "c", (400, 300));
    let client = window_by_app_id(&mut f, "c").unwrap();
    f.state()
        .map_window(StageWindow::Client(client), INITIAL, true);
    let sid = f
        .state()
        .insert_suspended_for_test(1, INITIAL, initial_size(), "s", "S");
    f.double_roundtrip(id);
    let before = last_configured_size(&mut f, id, &csurface);

    assert!(f.state().try_start_gesture_resize(grab_point(), false));
    motion(&mut f, drag_to());

    let s = f.state().find_suspended(sid).unwrap();
    assert_eq!(s.size.get(), grown_size(), "the stand-in took the resize");
    f.double_roundtrip(id);
    assert_eq!(
        last_configured_size(&mut f, id, &csurface),
        before,
        "the client beneath it was never configured at a new size"
    );

    end_swipe(&mut f);
    f.state().dismiss_suspended(sid);
}

/// A screen-pinned window is claimed by the pinned branch before the canvas
/// picker sees it: the trackpad gesture takes the screen-space resize path
/// (which anchors to the pin site), and the touch canvas picker declines so the
/// touch grab's own pinned branch can handle it.
#[test]
fn gesture_resize_on_a_pinned_window_takes_the_screen_space_path() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [400, 300]
"#,
    ));
    let out = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    map_window(&mut f, id, "pin", (400, 300));
    let window = window_by_app_id(&mut f, "pin").unwrap();
    let site = f.state().stage.pin_of(&window).unwrap().screen_pos;
    let ssurface = server_surface(&window);

    // Canvas == screen here, so the pin site's screen coords double as the
    // gesture's canvas position; land in the window's right third.
    let grab_at = pt(site.x as f64 + 350.0, site.y as f64 + 150.0);

    assert!(
        f.state().draggable_element_under(grab_at).is_none(),
        "precondition: the canvas picker leaves the pinned window alone"
    );
    assert!(
        !start_touch_gesture_resize(&mut f, grab_at, out),
        "the touch canvas picker defers to the touch grab's pinned branch"
    );

    assert!(
        f.state().try_start_gesture_resize(grab_at, false),
        "the trackpad gesture found the pinned window in screen space"
    );
    assert!(
        matches!(
            resize_state(&ssurface),
            ResizeState::Resizing {
                initial_screen_pos: Some(p),
                ..
            } if p == site
        ),
        "the resize is anchored to the pin site, not to a canvas position"
    );

    end_swipe(&mut f);
}

/// The pinned settle compensates per commit, exactly like the canvas one, so a
/// top-left drag the client acks in several steps holds the pin's right and
/// bottom screen edges still through every one of them.
#[test]
fn a_top_left_pinned_resize_holds_its_opposite_edges_across_every_commit() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [400, 300]
"#,
    ));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    let csurface = map_window(&mut f, id, "pin", (400, 300));
    let window = window_by_app_id(&mut f, "pin").unwrap();

    // No rule `position` centers the pin: (1920/2 - 200, 1080/2 - 150). Its
    // right edge sits at 1160 and its bottom at 690, where they must stay.
    let site = f.state().stage.pin_of(&window).unwrap().screen_pos;
    assert_eq!(site, Point::from((760, 390)), "precondition: the pin site");

    // Canvas == screen here; land in the window's top-left ninth.
    assert!(
        f.state()
            .try_start_gesture_resize(pt(site.x as f64 + 50.0, site.y as f64 + 50.0), false)
    );

    // The grab sits 50px inside the corner, so the corner tracks the cursor at
    // that offset.
    for (drag_to, expected) in [
        (pt(710.0, 390.0), Point::from((660, 340))),
        (pt(660.0, 340.0), Point::from((610, 290))),
        (pt(610.0, 290.0), Point::from((560, 240))),
    ] {
        motion(&mut f, drag_to);
        f.double_roundtrip(id);
        adopt_last_configure(&mut f, id, &csurface);
        let sp = f.state().stage.pin_of(&window).unwrap().screen_pos;
        let size = window.geometry().size;
        assert_eq!(
            (
                sp,
                Point::<i32, Logical>::from((sp.x + size.w, sp.y + size.h))
            ),
            (expected, Point::from((1160, 690))),
            "every commit moves the dragged corner and leaves the opposite one at (1160, 690)"
        );
    }

    end_swipe(&mut f);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &csurface);
    assert_eq!(
        f.state().stage.pin_of(&window).unwrap().screen_pos,
        Point::from((560, 240)),
        "the settle commit at an unchanged size leaves the pin where the drag left it"
    );
}

/// A pinned window's stage entry keeps its stale canvas position (only its
/// screen-space site is live) — the walk must skip past it rather than
/// stopping there, so `under`, genuinely mapped beneath it, stays reachable.
#[test]
fn draggable_element_under_reaches_through_a_pinned_windows_phantom_rect() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [400, 300]
"#,
    ));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();

    map_window(&mut f, id, "pin", (400, 300));
    let pin = window_by_app_id(&mut f, "pin").unwrap();
    let site = f.state().stage.pin_of(&pin).unwrap().screen_pos;

    map_window(&mut f, id, "under", (400, 300));
    let under = window_by_app_id(&mut f, "under").unwrap();
    // Overlap the pin's phantom canvas rect exactly, then re-raise the pin so
    // it stays topmost — the walk must reach `under` by skipping past it, not
    // because `under` happens to be on top.
    f.state()
        .map_window(StageWindow::Client(under.clone()), site, false);
    f.state().raise_window(&pin, false);

    let point = pt(site.x as f64 + 200.0, site.y as f64 + 150.0);
    assert_eq!(
        f.state().draggable_element_under(point),
        Some(StageWindow::Client(under)),
        "the pinned window's phantom rect must be skipped, not a dead end"
    );
}

/// Unlike hover (which rejects a client's own resize border and continues to
/// whatever's beneath it), gesture targeting accepts every band the walk
/// offers — so window A's margin overhanging window B's content resolves to
/// A, the window whose margin it actually is.
#[test]
fn draggable_element_under_resolves_a_resize_margin_to_its_owner() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();

    map_window(&mut f, id, "b", (400, 300));
    let b = window_by_app_id(&mut f, "b").unwrap();
    f.state()
        .map_window(StageWindow::Client(b.clone()), Point::from((0, 0)), false);

    map_window(&mut f, id, "a", (400, 300));
    let a = window_by_app_id(&mut f, "a").unwrap();
    f.state()
        .map_window(StageWindow::Client(a.clone()), Point::from((400, 0)), false);

    // A's left resize margin overhangs B's right edge: x in [392, 400).
    let overhang = pt(396.0, 150.0);
    assert_eq!(
        f.state().draggable_element_under(overhang),
        Some(StageWindow::Client(a)),
        "the margin must resolve to its owner, not the window it overhangs"
    );
}

/// The pinned branch is widget-proof. A window that is both `pinned_to_screen`
/// and a widget is claimed by the screen-space pinned arm before the canvas
/// picker (which rejects widgets) ever sees it, so that arm has to reject it
/// itself — otherwise a wallpaper or panel pinned to the screen becomes
/// gesture-draggable. Covers move and resize together: both go through the one
/// `gesture_target_under`.
#[test]
fn gesture_picker_declines_a_pinned_widget() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pw"
pinned_to_screen = true
widget = true
size = [400, 300]
"#,
    ));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    map_window(&mut f, id, "pw", (400, 300));
    let window = window_by_app_id(&mut f, "pw").unwrap();
    let site = f.state().stage.pin_of(&window).unwrap().screen_pos;

    // Canvas == screen here, so the pin site's screen coords double as the
    // gesture's canvas position; land in the window's right third.
    let over_pin = pt(site.x as f64 + 350.0, site.y as f64 + 150.0);
    assert!(
        f.state().pinned_element_under(over_pin).is_some(),
        "precondition: the pinned arm does find it in screen space"
    );

    assert!(
        !f.state().try_start_gesture_move(over_pin, false),
        "a pinned widget is not gesture-movable"
    );
    assert!(
        !f.state().try_start_gesture_resize(over_pin, false),
        "a pinned widget is not gesture-resizable"
    );
    assert!(
        !f.state().seat.get_pointer().unwrap().is_grabbed(),
        "neither gesture installed a grab over it"
    );
    assert_eq!(
        f.state().stage.pin_of(&window).unwrap().screen_pos,
        site,
        "the pin site is untouched"
    );
}

/// Resizing a fitted window clears the compositor's fit state, so the configure
/// that starts the resize has to clear the client's `Maximized` too. A client
/// left holding it has a dead restore button: the `unmaximize_request` that
/// button dispatches finds no fit left and `unfit_window` drops it silently.
/// The two gesture arms here; `resize_parity.rs` covers the pointer arm and the
/// client's own `xdg_toplevel.resize`, so the four cannot diverge unnoticed.
#[test]
fn gesture_resize_of_a_fitted_window_clears_the_client_maximized_state() {
    {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        // Moving the camera seeds a per-output blur generation that only clears
        // on output disconnect, so it can't return to the construction baseline.
        f.skip_baseline_check();
        origin_view(&mut f);
        let id = f.add_client();
        let csurface = map_window(&mut f, id, "c", (400, 300));
        let window = window_by_app_id(&mut f, "c").unwrap();
        f.state()
            .map_window(StageWindow::Client(window.clone()), INITIAL, true);

        let grab_at = fit_and_frame(&mut f, &window, id);
        assert!(
            client_sees_maximized(&mut f, id, &csurface),
            "precondition: the fit told the client it is maximized"
        );

        assert!(f.state().try_start_gesture_resize(grab_at, false));
        motion(&mut f, grab_at + pt(100.0, 0.0));
        f.double_roundtrip(id);

        assert!(
            !client_sees_maximized(&mut f, id, &csurface),
            "the trackpad resize told the client it is no longer maximized"
        );
        end_swipe(&mut f);
    }

    {
        let mut f = Fixture::new();
        let out = f.add_output(1, (1920, 1080));
        f.skip_baseline_check();
        origin_view(&mut f);
        let id = f.add_client();
        let csurface = map_window(&mut f, id, "c", (400, 300));
        let window = window_by_app_id(&mut f, "c").unwrap();
        f.state()
            .map_window(StageWindow::Client(window.clone()), INITIAL, true);

        let grab_at = fit_and_frame(&mut f, &window, id);
        assert!(client_sees_maximized(&mut f, id, &csurface));

        assert!(start_touch_gesture_resize(&mut f, grab_at, out));
        touch_motion(&mut f, grab_at + pt(100.0, 0.0));
        f.double_roundtrip(id);

        assert!(
            !client_sees_maximized(&mut f, id, &csurface),
            "the touch resize says the same"
        );
        lift_finger(&mut f);
    }
}

/// Both gesture resizes arm the interactive-move guard for the length of the
/// drag and disarm it on teardown — that's what stops a relaunching app being
/// adopted into the stand-in's slot mid-resize and fighting the grab.
#[test]
fn gesture_resizes_arm_the_relaunch_adoption_guard() {
    {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f
            .state()
            .insert_suspended_for_test(1, INITIAL, initial_size(), "s", "S");
        let element = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
        assert!(
            !f.state().element_under_interactive_grab(&element),
            "precondition: nothing is armed before the drag"
        );

        assert!(f.state().try_start_gesture_resize(grab_point(), false));
        motion(&mut f, drag_to());
        assert!(
            f.state().element_under_interactive_grab(&element),
            "the trackpad resize is armed against relaunch adoption"
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
            .insert_suspended_for_test(1, INITIAL, initial_size(), "s", "S");
        let element = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());

        assert!(start_touch_gesture_resize(&mut f, grab_point(), out));
        touch_motion(&mut f, drag_to());
        assert!(
            f.state().element_under_interactive_grab(&element),
            "the touch resize is armed too"
        );

        lift_finger(&mut f);
        assert!(
            !f.state().element_under_interactive_grab(&element),
            "and disarmed when the finger lifts"
        );
        f.state().dismiss_suspended(sid);
    }
}

/// The whole invariant the touch entry point establishes, plain and pinned.
/// The pinned half calls `build_touch_resize_grab` the way the touch gesture
/// grab's own pinned branch does — the canvas picker declines pinned windows,
/// so the gesture helper above can't reach it.
#[test]
fn touch_resize_entry_establishes_the_whole_resize_invariant() {
    {
        let mut f = Fixture::new();
        let out = f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let id = f.add_client();
        map_window(&mut f, id, "c", (400, 300));
        let window = window_by_app_id(&mut f, "c").unwrap();
        f.state()
            .map_window(StageWindow::Client(window.clone()), INITIAL, true);
        // Both memberships set, so clearing either is observable.
        seed_fit_and_fill(&mut f, &window);

        assert!(start_touch_gesture_resize(&mut f, grab_point(), out));

        assert_resize_entered(
            &mut f,
            &window,
            xdg_toplevel::ResizeEdge::Right,
            initial_size(),
            None,
        );
        lift_finger(&mut f);
    }

    {
        let mut f = Fixture::with_config(config(
            r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [400, 300]
"#,
        ));
        let out = f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let id = f.add_client();
        map_window(&mut f, id, "pin", (400, 300));
        let window = window_by_app_id(&mut f, "pin").unwrap();
        let site = f.state().stage.pin_of(&window).unwrap().screen_pos;
        let element = StageWindow::Client(window.clone());
        seed_fit_and_fill(&mut f, &window);

        let start = TouchGrabStartData {
            focus: None,
            slot: slot(),
            location: pt(site.x as f64 + 390.0, site.y as f64 + 150.0),
        };
        assert!(
            f.state()
                .build_touch_resize_grab(
                    &element,
                    xdg_toplevel::ResizeEdge::Right,
                    start,
                    out,
                    1,
                    false,
                )
                .is_some(),
            "the pinned branch builds a grab"
        );

        assert_resize_entered(
            &mut f,
            &window,
            xdg_toplevel::ResizeEdge::Right,
            initial_size(),
            Some(site),
        );
    }
}

/// The whole invariant the trackpad entry point establishes. Only the canvas
/// arm writes it: a pinned window is handed to the pointer path instead, which
/// `gesture_resize_on_a_pinned_window_takes_the_screen_space_path` pins — so
/// this arm's `initial_screen_pos` is `None` by construction.
#[test]
fn swipe_resize_entry_establishes_the_whole_resize_invariant() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    map_window(&mut f, id, "c", (400, 300));
    let window = window_by_app_id(&mut f, "c").unwrap();
    f.state()
        .map_window(StageWindow::Client(window.clone()), INITIAL, true);
    // Both memberships set, so clearing either is observable.
    seed_fit_and_fill(&mut f, &window);

    assert!(f.state().try_start_gesture_resize(grab_point(), false));

    assert_resize_entered(
        &mut f,
        &window,
        xdg_toplevel::ResizeEdge::Right,
        initial_size(),
        None,
    );
    end_swipe(&mut f);
}
