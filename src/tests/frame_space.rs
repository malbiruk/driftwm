//! User-facing coordinate boundaries speak the *visual frame* (content plus
//! SSD title bar and border — see [`driftwm::canvas::Chrome`]), not the
//! content rect the compositor stores internally. Every scenario here uses a
//! server-decorated and/or bordered config: on the CSD/borderless default the
//! frame and the content coincide, so a config without either carries no
//! signal for this conversion at all.

use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1;

use crate::ipc::dispatch;
use crate::ipc::protocol::{Reply, Request, Response, WindowSelector};
use crate::state::StageWindow;
use driftwm::config::Action;

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

/// Before this conversion existed, `msg move 0 0` and "navigate to this
/// window" disagreed by half a bar on an SSD window: the IPC path was
/// content-only while navigation already centered the visual frame. Both
/// must now land on the same canvas point.
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

    f.state().navigate_to_window(&window, true);
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
