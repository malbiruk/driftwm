//! User-facing coordinate boundaries speak the *visual frame* (content plus
//! SSD title bar and border — see [`driftwm::canvas::Chrome`]), not the
//! content rect the compositor stores internally. Every scenario here uses a
//! server-decorated and/or bordered config: on the CSD/borderless default the
//! frame and the content coincide, so a config without either carries no
//! signal for this conversion at all.

use smithay::desktop::Window;
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::decorations::DecorationHit;
use crate::input::DecoTarget;
use crate::ipc::dispatch;
use crate::ipc::protocol::{Reply, Request, Response, WindowSelector};
use crate::state::{NavZoom, StageWindow};
use driftwm::config::Action;
use driftwm::window_ext::WindowExt;

use super::input_backend::{touch_down, touch_motion, touch_up};
use super::{Fixture, config, map_window, settle, window_by_app_id};

fn mv(f: &mut Fixture, window: Option<WindowSelector>, to: Option<(i32, i32)>) -> Reply {
    dispatch(Request::Move { window, to }, f.state())
}

fn resize(f: &mut Fixture, window: Option<WindowSelector>, to: Option<(i32, i32)>) -> Reply {
    dispatch(Request::Resize { window, to }, f.state())
}

/// Camera at the canvas origin, zoom 1: canvas == screen.
fn origin_view(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });
}

/// `msg state` / `window_inventory` report an SSD+bordered window's *visual
/// frame*, not its content rect.
#[test]
fn window_inventory_reports_the_visual_frame() {
    let mut f = Fixture::with_config(config(
        r#"
[decorations]
border_width = 4

[[window_rules]]
app_id = "term"
decoration = "server"
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((100, 200)), false);

    let inv = f.state().window_inventory();
    let w = inv.iter().find(|w| w.app_id == "term").unwrap();
    // Content sits at (100, 200), sized 400×300. The frame adds a 25px bar
    // above and a 4px border on every side: 408 × 333. Its center in Y-up
    // rule coordinates: frame top-left (96, 171), so x = 96 + 408/2 = 300 and
    // y = -(171 + 333/2) = -337. Literal on purpose — computing it with the
    // same converter the production code uses would track a regression
    // instead of catching it.
    assert_eq!(
        w.size,
        [408, 333],
        "size must be content plus bar and border"
    );
    assert_eq!(
        w.position,
        [300, -337],
        "position must be the frame's center, half a bar above the content's"
    );
}

/// `msg move` with no args, then `msg move` to exactly what it reported,
/// leaves the window's canvas position unchanged — the invariant
/// `docs/window-rules.md` relies on when a user pastes `msg state` output
/// straight into a rule. The reported point is spelled out literally so a
/// bug that drops chrome from *both* the read and the write arm (which would
/// otherwise cancel out and still look like a no-op) cannot hide here.
#[test]
fn msg_move_read_then_write_is_a_no_op_under_ssd() {
    let mut f = Fixture::with_config(config(
        r#"
[decorations]
border_width = 4

[[window_rules]]
app_id = "term"
decoration = "server"
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((100, 200)), false);
    let before = f.state().stage.position_of(&window);

    // Same rect as `window_inventory_reports_the_visual_frame` above, read
    // through `msg move` instead of `msg state` — a distinct call site
    // (`reported_chrome`/`cmd_move`, not `element_chrome`/`window_inventory`).
    assert_eq!(
        mv(&mut f, None, None),
        Ok(Response::Position { x: 300, y: -337 })
    );

    assert_eq!(
        mv(&mut f, None, Some((300, -337))),
        Ok(Response::Position { x: 300, y: -337 })
    );
    assert_eq!(
        f.state().stage.position_of(&window),
        before,
        "writing back exactly what was read must not move the window"
    );
}

/// `msg move`'s reported point and where "navigate to this window" parks the
/// camera are the same canvas point on an SSD window — the visual frame's
/// center. A content-only IPC path would sit half a bar off.
#[test]
fn msg_move_and_navigate_to_window_agree_on_the_frame_center() {
    // An even bar keeps the frame height even too (content 300 + bar 24),
    // so neither side's integer truncation can introduce a half-pixel
    // disagreement that isn't the thing under test.
    let mut f = Fixture::with_config(config(
        r#"
[decorations]
border_width = 4
title_bar_height = 24

[[window_rules]]
app_id = "term"
decoration = "server"
"#,
    ));
    // Navigating moves the camera, which bumps a per-output blur generation
    // that never resets — not a leak this scenario cares about.
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    f.state()
        .map_window(window.clone(), Point::from((100, 200)), false);

    let Ok(Response::Position { x, y }) = mv(&mut f, None, None) else {
        panic!("expected a position reply");
    };
    // `msg move`'s point is Y-up; canvas/camera space is Y-down.
    let reported_center: Point<f64, Logical> = Point::from((x as f64, -y as f64));

    f.state().navigate_to_window(&window, NavZoom::Reset);
    settle(&mut f);
    let landed = f.state().viewport_center_canvas();

    assert!(
        (landed.x - reported_center.x).abs() < 1e-6 && (landed.y - reported_center.y).abs() < 1e-6,
        "navigate_to_window landed on {landed:?}, msg move reported {reported_center:?}"
    );
}

/// The `move`/`resize` *read* arms are reachable on a fullscreen window (they
/// return before the `is_canvas_window` guard the write arms use). A
/// fullscreen window wears no chrome, so both reads must report the bare
/// viewport rect, not one inflated or shifted by the SSD/border config.
#[test]
fn fullscreen_window_reads_report_no_chrome() {
    let mut f = Fixture::with_config(config(
        r#"
[decorations]
border_width = 4

[[window_rules]]
app_id = "term"
decoration = "server"
"#,
    ));
    // Entering fullscreen snaps the camera, which bumps a per-output blur
    // generation that never resets — not a leak this scenario cares about.
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    assert_eq!(
        window.geometry().size,
        Size::from((1920, 1080)),
        "precondition: the client committed the fullscreen viewport"
    );

    assert_eq!(
        resize(&mut f, None, None),
        Ok(Response::Size {
            width: 1920,
            height: 1080
        }),
        "a fullscreen window wears no chrome — the read must not inflate the viewport size"
    );
    assert_eq!(
        mv(&mut f, None, None),
        Ok(Response::Position { x: 960, y: -540 }),
        "nor shift the reported center by half a bar"
    );

    f.state().exit_fullscreen_on(&output);
}

/// A canvas layer never wears a title bar, only a per-rule opt-in border, and
/// a border is symmetric — so its inventory position is unchanged while its
/// size gains `2 × border_width`.
#[test]
fn canvas_layer_inventory_reports_the_border_inflated_frame() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "widget"
position = [0, 0]
border_width = 6
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let layer = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Top, "widget");
    let layer_surface = layer.surface.clone();
    layer.set_configure_props(super::client::LayerConfigureProps {
        size: Some((200, 150)),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&layer_surface);
    layer.set_size(200, 150);
    layer.attach_new_buffer();
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let (_layers, canvas_layers) = f.state().layer_inventory();
    assert_eq!(canvas_layers.len(), 1);
    assert_eq!(
        canvas_layers[0].position,
        [0, 0],
        "a border is symmetric and cancels out of the center"
    );
    assert_eq!(
        canvas_layers[0].size,
        [212, 162],
        "the border adds 2×6 to each axis: 200+12, 150+12"
    );
}

/// A canvas layer's concrete size request is echoed verbatim — the output
/// width never stands in for a dimension the client specified.
#[test]
fn canvas_layer_configure_echoes_a_requested_size() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "widget"
position = [0, 0]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let layer = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Top, "widget");
    let surface = layer.surface.clone();
    layer.set_configure_props(super::client::LayerConfigureProps {
        size: Some((320, 240)),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&surface);
    assert_eq!(
        layer
            .recent_configures()
            .map(|c| c.size)
            .collect::<Vec<_>>(),
        vec![(320, 240)],
    );
}

/// A 0 on an axis anchored at both edges fills from the output; the other
/// axis is echoed. A later commit changing the request gets its own
/// configure — that is how a self-sizing bar grows.
#[test]
fn canvas_layer_configure_fills_an_anchored_axis_and_reconfigures_on_change() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "widget"
position = [0, 0]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let layer = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Top, "widget");
    let surface = layer.surface.clone();
    layer.set_configure_props(super::client::LayerConfigureProps {
        size: Some((0, 240)),
        anchor: Some(zwlr_layer_surface_v1::Anchor::Left | zwlr_layer_surface_v1::Anchor::Right),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&surface);
    assert_eq!(
        layer
            .recent_configures()
            .map(|c| c.size)
            .collect::<Vec<_>>(),
        vec![(1920, 240)],
        "width fills from the output; the requested height is echoed"
    );

    layer.set_configure_props(super::client::LayerConfigureProps {
        size: Some((0, 300)),
        anchor: Some(zwlr_layer_surface_v1::Anchor::Left | zwlr_layer_surface_v1::Anchor::Right),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&surface);
    assert_eq!(
        layer
            .recent_configures()
            .map(|c| c.size)
            .collect::<Vec<_>>(),
        vec![(1920, 300)],
        "a changed request gets a second configure"
    );
    assert_eq!(
        layer.configures_received.len(),
        2,
        "exactly two configures total"
    );
}

fn stand_in_element(f: &mut Fixture, sid: crate::state::SuspendedId) -> StageWindow {
    StageWindow::Suspended(f.state().find_suspended(sid).expect("stand-in"))
}

/// Both `msg move` and `move-to-bookmark` consume rule coordinates for a live
/// client window through the very same `map_window_to_rule_point` — so
/// reading one back through the other is a check that they call the shared
/// path, not two that happen to agree by coincidence.
#[test]
fn move_to_bookmark_and_msg_move_agree_for_a_client_window() {
    let mut f = Fixture::with_config(config(
        r#"
[decorations]
border_width = 4

[[window_rules]]
app_id = "term"
decoration = "server"
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    f.state()
        .bookmarks
        .insert("spot".to_string(), [1000.0, -500.0]);
    f.state()
        .execute_action(&Action::MoveToBookmark("spot".to_string()));

    assert_eq!(
        mv(&mut f, None, None),
        Ok(Response::Position { x: 1000, y: -500 }),
        "msg move reads back exactly where move-to-bookmark put the window"
    );
}

/// The stand-in arm of `move-to-bookmark` resolves its own chrome
/// (`suspended_chrome`) independently of `msg move`'s (`element_chrome`) —
/// unlike the client arm above, nothing forces the two to call the same
/// function, so this is the case that can actually drift.
#[test]
fn move_to_bookmark_and_msg_move_agree_for_a_stand_in() {
    let mut f = Fixture::with_config(config("[decorations]\nborder_width = 4\n"));
    f.add_output(1, (1920, 1080));
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let serial = SERIAL_COUNTER.next_serial();
    f.state().set_suspended_focus(sid, serial);

    f.state()
        .bookmarks
        .insert("spot".to_string(), [1000.0, -500.0]);
    f.state()
        .execute_action(&Action::MoveToBookmark("spot".to_string()));

    let element = stand_in_element(&mut f, sid);
    let window_id = f.state().stage.id_of(&element).unwrap().0;
    assert_eq!(
        mv(&mut f, Some(WindowSelector::Id(window_id)), None),
        Ok(Response::Position { x: 1000, y: -500 }),
        "msg move reads back exactly where move-to-bookmark put the stand-in"
    );

    f.state().dismiss_suspended(sid);
}

/// A widget without `widget_movable` is nailed to the geometry its rule gives
/// it: no titlebar drag, gesture or nudge, and neither `msg move` nor
/// `msg resize` writes it either. Reading both still answers.
#[test]
fn an_immovable_widget_takes_no_geometry_from_anywhere() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "widget"
widget = true

[[window_rules]]
app_id = "normal"
"#,
    ));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();

    map_window(&mut f, id, "widget", (200, 100));
    let widget = window_by_app_id(&mut f, "widget").unwrap();
    let widget_element = StageWindow::Client(widget.clone());

    map_window(&mut f, id, "normal", (400, 300));
    let normal = window_by_app_id(&mut f, "normal").unwrap();
    let normal_element = StageWindow::Client(normal.clone());
    // Park it clear of the widget, so every pick below answers for the widget
    // and not for a neighbour under the same point.
    f.state()
        .map_window(normal_element.clone(), Point::from((3000, 3000)), false);

    let widget_id = f.state().stage.id_of(&widget).unwrap().0;
    let placed = f.state().stage.position_of(&widget).unwrap();

    // Manual moves decline the widget.
    let size = widget.geometry().size;
    let center = Point::from((
        placed.x as f64 + size.w as f64 / 2.0,
        placed.y as f64 + size.h as f64 / 2.0,
    ));
    assert!(
        f.state().draggable_element_under(center).is_none(),
        "draggable_element_under declines widgets"
    );
    assert!(
        !f.state().try_start_gesture_move(center, false),
        "gesture move declines widgets"
    );

    // And so does every IPC arm: the rule's `position` and `size` are the only
    // writers of a widget's geometry.
    let moved = mv(
        &mut f,
        Some(WindowSelector::Id(widget_id)),
        Some((500, -250)),
    );
    assert!(
        moved.is_err(),
        "an immovable widget refuses `msg move`: {moved:?}"
    );
    let resized = resize(
        &mut f,
        Some(WindowSelector::Id(widget_id)),
        Some((400, 200)),
    );
    assert!(
        resized.is_err(),
        "an immovable widget refuses `msg resize`: {resized:?}"
    );
    assert_eq!(
        f.state().stage.position_of(&widget),
        Some(placed),
        "the refused writes left it where the rule put it"
    );

    // Reading is not a write, so both read arms still answer.
    assert!(
        mv(&mut f, Some(WindowSelector::Id(widget_id)), None).is_ok(),
        "`msg move` still reads an immovable widget's position"
    );
    assert!(
        resize(&mut f, Some(WindowSelector::Id(widget_id)), None).is_ok(),
        "`msg resize` still reads its size"
    );

    // Widgets must remain below normal windows in stacking.
    let entries: Vec<_> = f
        .state()
        .stage
        .entries()
        .map(|e| e.window.clone())
        .collect();
    assert!(
        entries.iter().position(|w| w == &widget_element).unwrap()
            < entries.iter().position(|w| w == &normal_element).unwrap(),
        "widget must remain stacked below normal windows"
    );
}

/// `widget_movable = true` hands a widget's geometry back without changing what
/// it is: drags and `msg resize` reach it, while it stays stacked below normal
/// windows and out of the focus chain.
#[test]
fn a_movable_widget_takes_geometry_back_but_keeps_its_class() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "widget"
widget = true
widget_movable = true

[[window_rules]]
app_id = "normal"
"#,
    ));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();

    map_window(&mut f, id, "normal", (400, 300));
    let normal = window_by_app_id(&mut f, "normal").unwrap();
    f.state().map_window(
        StageWindow::Client(normal.clone()),
        Point::from((3000, 3000)),
        false,
    );

    map_window(&mut f, id, "widget", (200, 100));
    let widget = window_by_app_id(&mut f, "widget").unwrap();
    let widget_element = StageWindow::Client(widget.clone());
    f.state()
        .map_window(widget_element.clone(), Point::from((0, 0)), true);
    let widget_id = f.state().stage.id_of(&widget).unwrap().0;

    // The widget still *is* a widget: focusable, never in the switcher.
    assert!(widget.is_widget(), "the widget flag is untouched");
    assert!(
        !f.state().stage.focus_history().iter().any(|w| w == &widget),
        "a movable widget stays out of the focus chain"
    );

    // Where the plain widget was refused both arms, it now answers both.
    assert_eq!(
        resize(
            &mut f,
            Some(WindowSelector::Id(widget_id)),
            Some((400, 200))
        ),
        Ok(Response::Size {
            width: 400,
            height: 200
        }),
        "a movable widget has a canvas size to set"
    );
    assert_eq!(
        mv(
            &mut f,
            Some(WindowSelector::Id(widget_id)),
            Some((500, -250))
        ),
        Ok(Response::Position { x: 500, y: -250 }),
        "a movable widget has a canvas position to set"
    );
    assert_eq!(
        mv(&mut f, Some(WindowSelector::Id(widget_id)), None),
        Ok(Response::Position { x: 500, y: -250 }),
        "and reads back the point it was given"
    );

    // And so do the pickers the plain widget stopped. Grab the live rect: the
    // resize and move above relocated it, and its client has not committed.
    let loc = f.state().stage.position_of(&widget_element).unwrap();
    let size = widget.geometry().size;
    let center = Point::from((
        loc.x as f64 + size.w as f64 / 2.0,
        loc.y as f64 + size.h as f64 / 2.0,
    ));
    assert!(
        f.state().draggable_element_under(center).is_some(),
        "draggable_element_under offers the movable widget"
    );
    assert!(
        f.state().try_start_gesture_move(center, false),
        "gesture move accepts it"
    );
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    pointer.unset_grab(f.state(), serial, 0);

    // Stacking survived the resize's raise, the move's, and the move grab's.
    let entries: Vec<_> = f
        .state()
        .stage
        .entries()
        .map(|e| e.window.clone())
        .collect();
    assert!(
        entries.iter().position(|w| *w == widget_element).unwrap()
            < entries
                .iter()
                .position(|w| *w == StageWindow::Client(normal.clone()))
                .unwrap(),
        "the widget stayed below the normal window"
    );
}

/// Mid-height of a window's right-hand resize band: four pixels outside its
/// content rect, so inside the eight-pixel margin the compositor drags by.
fn right_band(f: &mut Fixture, w: &Window) -> Point<f64, Logical> {
    let loc = f.state().stage.position_of(w).unwrap();
    let size = w.geometry().size;
    Point::from((
        loc.x as f64 + size.w as f64 + 4.0,
        loc.y as f64 + size.h as f64 / 2.0,
    ))
}

/// A borderless widget is sized by hand through the compositor's resize margin,
/// so that band is where `widget_movable` shows up for a CSD window: it answers
/// for a movable widget and stays inert over an immovable one.
#[test]
fn a_movable_widget_answers_to_its_resize_margin() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "movable"
widget = true
widget_movable = true

[[window_rules]]
app_id = "nailed"
widget = true
"#,
    ));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();

    map_window(&mut f, id, "movable", (200, 100));
    let movable = window_by_app_id(&mut f, "movable").unwrap();
    map_window(&mut f, id, "nailed", (200, 100));
    let nailed = window_by_app_id(&mut f, "nailed").unwrap();
    f.state()
        .map_window(StageWindow::Client(movable.clone()), (0, 0).into(), false);
    f.state()
        .map_window(StageWindow::Client(nailed.clone()), (1000, 0).into(), false);

    let movable_band = right_band(&mut f, &movable);
    let nailed_band = right_band(&mut f, &nailed);

    assert!(
        matches!(
            f.state().decoration_under(movable_band),
            Some((DecoTarget::Client(w), DecorationHit::ResizeBorder(_))) if w == movable
        ),
        "the band outside a movable widget is chrome the pointer can drag"
    );
    assert!(
        !matches!(
            f.state().decoration_under(nailed_band),
            Some((DecoTarget::Client(w), DecorationHit::ResizeBorder(_))) if w == nailed
        ),
        "the same band outside an immovable widget stays inert"
    );
}

/// A finger drags a title bar, a movable widget's included. An immovable
/// widget's bar answers with focus instead, exactly as the pointer's does.
#[test]
fn a_finger_drags_a_movable_widgets_title_bar_and_not_an_immovable_ones() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "movable"
widget = true
widget_movable = true
decoration = "server"

[[window_rules]]
app_id = "nailed"
widget = true
decoration = "server"
"#,
    ));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();

    map_window(&mut f, id, "movable", (200, 100));
    let movable = window_by_app_id(&mut f, "movable").unwrap();
    map_window(&mut f, id, "nailed", (200, 100));
    let nailed = window_by_app_id(&mut f, "nailed").unwrap();
    // Low enough that a title bar — 25px above the content — stays on the
    // viewport a device could report.
    let movable_at = Point::from((300, 400));
    let nailed_at = Point::from((1200, 400));
    f.state()
        .map_window(StageWindow::Client(movable.clone()), movable_at, false);
    f.state()
        .map_window(StageWindow::Client(nailed.clone()), nailed_at, false);

    let title_bar = |f: &mut Fixture, w: &Window| {
        let loc = f.state().stage.position_of(w).unwrap();
        Point::from((loc.x as f64 + 40.0, loc.y as f64 - 6.0))
    };
    let movable_bar = title_bar(&mut f, &movable);
    let nailed_bar = title_bar(&mut f, &nailed);
    assert!(
        matches!(
            f.state().decoration_under(movable_bar),
            Some((DecoTarget::Client(w), DecorationHit::TitleBar)) if w == movable
        ),
        "setup: the probe sits on the widget's own title bar"
    );

    for start in [movable_bar, nailed_bar] {
        touch_down(&mut f, start, 0);
        touch_motion(&mut f, Point::from((start.x + 120.0, start.y + 60.0)), 0);
        touch_up(&mut f, 0);
        f.double_roundtrip(id);
    }

    assert_ne!(
        f.state().stage.position_of(&movable),
        Some(movable_at),
        "a movable widget follows the finger"
    );
    assert_eq!(
        f.state().stage.position_of(&nailed),
        Some(nailed_at),
        "an immovable widget stays where its rule put it"
    );
}
