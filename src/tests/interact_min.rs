//! `zoom.interact_min` ("pick mode"): below the threshold a canvas window or
//! stand-in stops receiving pointer input and becomes one uniform click/drag
//! target — a left click centers it on release, a drag past the slop moves it,
//! and pointer focus is suppressed over it in both cascades. Above the
//! threshold (or with the feature off at `0.0`) nothing changes.

use driftwm::canvas::{CanvasPos, canvas_to_screen};
use driftwm::config::{BTN_LEFT, BTN_RIGHT, BindingContext, Config, MouseAction};
use smithay::backend::input::AxisSource;
use smithay::input::keyboard::ModifiersState;
use smithay::input::pointer::{CursorIcon, CursorImageStatus};
use smithay::utils::{IsAlive, Point, Rectangle, SERIAL_COUNTER, Size};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1;

use crate::state::{PickTarget, StageWindow};

use super::{Fixture, map_window, window_by_app_id};

/// Feature armed with a 0.5 threshold; a client window path needs no SSD.
fn config_pick() -> Config {
    Config::from_toml(
        r#"
        [zoom]
        interact_min = 0.5
    "#,
    )
    .unwrap()
}

/// As `config_pick`, plus server decorations so a stand-in's chrome resolves.
fn config_pick_ssd() -> Config {
    Config::from_toml(
        r#"
        [zoom]
        interact_min = 0.5
        [decorations]
        default_mode = "server"
    "#,
    )
    .unwrap()
}

/// Map a client and park it at a known canvas position, so a body point is
/// predictable. Returns the server-side window.
fn map_client_at(
    f: &mut Fixture,
    app_id: &str,
    size: (u16, u16),
    pos: (i32, i32),
) -> smithay::desktop::Window {
    let id = f.add_client();
    map_window(f, id, app_id, size);
    let window = window_by_app_id(f, app_id).unwrap();
    f.state()
        .map_window(StageWindow::Client(window.clone()), Point::from(pos), true);
    window
}

fn pt(x: f64, y: f64) -> Point<f64, smithay::utils::Logical> {
    Point::from((x, y))
}

/// Drop the camera/zoom targets a window's activation left behind, so a later
/// assertion sees only what the pick itself did (as `auto_navigate_click` does).
fn clear_targets(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.camera_target = None;
        os.zoom_target = None;
    });
}

fn pointer(f: &mut Fixture) -> smithay::input::pointer::PointerHandle<crate::state::DriftWm> {
    f.state().seat.get_pointer().unwrap()
}

/// Register an SSD title bar for `window`, exactly as the compositor does on the
/// first sized commit under `default_mode = "server"`. The headless test client
/// never binds xdg-decoration, so that commit path doesn't run — do it directly
/// so `surface_under` reports the window's chrome bands.
fn give_ssd(f: &mut Fixture, window: &smithay::desktop::Window) {
    use smithay::reexports::wayland_server::Resource;
    let width = window.geometry().size.w;
    let id = super::server_surface(window).id();
    let deco =
        crate::decorations::WindowDecoration::new(width, true, &f.state().config.decorations);
    f.state()
        .decorations
        .insert(crate::decorations::DecorationKey::Surface(id), deco);
}

/// Above the threshold, pick mode is inert: a press over a window is not
/// consumed and nothing is armed.
#[test]
fn above_threshold_press_is_not_consumed() {
    let mut f = Fixture::with_config(config_pick());
    f.add_output(1, (1920, 1080));
    let _window = map_client_at(&mut f, "w", (400, 300), (500, 400));
    f.state().set_zoom(0.8);

    let pointer = pointer(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    let consumed = f
        .state()
        .try_pick_button(&pointer, pt(700.0, 550.0), BTN_LEFT, serial, mods());

    assert!(!consumed, "above the threshold a press falls through");
    assert!(f.state().pending_pick.is_none(), "nothing is armed");
}

/// Below the threshold, a left press over a window is consumed and arms a
/// client pick.
#[test]
fn below_threshold_press_over_window_arms() {
    let mut f = Fixture::with_config(config_pick());
    f.add_output(1, (1920, 1080));
    let _window = map_client_at(&mut f, "w", (400, 300), (500, 400));
    f.state().set_zoom(0.3);

    let pointer = pointer(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    let consumed = f
        .state()
        .try_pick_button(&pointer, pt(700.0, 550.0), BTN_LEFT, serial, mods());

    assert!(consumed, "a press over a window in pick mode is consumed");
    let armed = f.state().pending_pick.as_ref().map(|p| p.target.clone());
    assert!(
        matches!(armed, Some(PickTarget::Client(_))),
        "a client pick is armed"
    );
}

/// A held-modifier move binding wins over pick mode, so `alt/super+drag`
/// still moves a single window: the press is not consumed and nothing is armed.
#[test]
fn held_modifier_binding_defers_to_bindings() {
    let mut f = Fixture::with_config(
        Config::from_toml(
            r#"
            [zoom]
            interact_min = 0.5
            [mouse.anywhere]
            "super+left" = "move-window"
        "#,
        )
        .unwrap(),
    );
    f.add_output(1, (1920, 1080));
    let _window = map_client_at(&mut f, "w", (400, 300), (500, 400));
    f.state().set_zoom(0.3);

    let held = ModifiersState {
        logo: true,
        ..Default::default()
    };
    let pointer = pointer(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    let consumed = f
        .state()
        .try_pick_button(&pointer, pt(700.0, 550.0), BTN_LEFT, serial, held);

    assert!(!consumed, "a held-modifier binding wins over pick mode");
    assert!(f.state().pending_pick.is_none(), "no pick armed");
}

/// A press on a stand-in's close button is consumed and arms a suspended
/// pick — the `×` is bypassed, so the stand-in is not dismissed.
#[test]
fn stand_in_press_arms_and_bypasses_close_button() {
    let mut f = Fixture::with_config(config_pick_ssd());
    f.add_output(1, (1920, 1080));
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((500, 500)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    f.state().set_zoom(0.3);

    // The close button sits on the right of the 25px bar band.
    let close = pt(500.0 + 400.0 - 20.0, 500.0 - 12.0);
    let pointer = pointer(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    let consumed = f
        .state()
        .try_pick_button(&pointer, close, BTN_LEFT, serial, mods());

    assert!(consumed, "a press over a stand-in is consumed");
    assert!(
        f.state().find_suspended(sid).is_some(),
        "the close button is bypassed below the threshold — not a dismiss"
    );
    let armed = f.state().pending_pick.as_ref().map(|p| p.target.clone());
    assert!(
        matches!(armed, Some(PickTarget::Suspended(s)) if s == sid),
        "a suspended pick is armed"
    );

    f.state().dismiss_suspended(sid);
}

/// A press on a stand-in's relaunch label is likewise a plain pick — the
/// label is bypassed, so it does not fire the relaunch.
#[test]
fn stand_in_press_bypasses_relaunch_label() {
    let tmp = super::real::TempDir::new();
    std::fs::write(
        tmp.path().join("s.desktop"),
        "[Desktop Entry]\nType=Application\nName=S\nExec=s\n",
    )
    .unwrap();
    let mut f = Fixture::with_config(config_pick_ssd());
    f.add_output(1, (1920, 1080));
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
    // Simulate a rendered label centered in the body (render doesn't run here).
    f.state()
        .find_suspended(sid)
        .unwrap()
        .chrome
        .borrow_mut()
        .label_rect = Some(Rectangle::new(
        Point::from((150, 130)),
        Size::from((100, 40)),
    ));
    f.state().set_zoom(0.3);

    let label = pt(500.0 + 200.0, 500.0 + 150.0);
    let pointer = pointer(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    let consumed = f
        .state()
        .try_pick_button(&pointer, label, BTN_LEFT, serial, mods());

    assert!(consumed, "a press over the label is consumed");
    assert!(
        !f.state().is_suspended_launching(sid),
        "the relaunch label is bypassed below the threshold"
    );

    f.state().dismiss_suspended(sid);
}

/// A press over empty canvas is not consumed — it falls through to the
/// normal on-canvas dispatch.
#[test]
fn press_over_empty_canvas_falls_through() {
    let mut f = Fixture::with_config(config_pick());
    f.add_output(1, (1920, 1080));
    let _window = map_client_at(&mut f, "w", (400, 300), (500, 400));
    f.state().set_zoom(0.3);

    let pointer = pointer(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    // Far from the window.
    let consumed =
        f.state()
            .try_pick_button(&pointer, pt(4000.0, 4000.0), BTN_LEFT, serial, mods());

    assert!(!consumed, "empty canvas falls through");
    assert!(f.state().pending_pick.is_none());
}

/// A resolve within the slop fires the centre: the camera and zoom animate
/// toward the window at 1.0.
#[test]
fn resolve_within_slop_centers_the_window() {
    let mut f = Fixture::with_config(config_pick());
    f.add_output(1, (1920, 1080));
    let window = map_client_at(&mut f, "w", (400, 300), (5000, 400));
    f.state().set_zoom(0.3);
    clear_targets(&mut f);

    let press = pt(5200.0, 550.0);
    f.state()
        .arm_pick(PickTarget::Client(window), press, BTN_LEFT);
    f.state().resolve_pick(BTN_LEFT);

    assert_eq!(
        f.state().zoom_target(),
        Some(1.0),
        "a pick resets zoom to 1.0"
    );
    assert!(
        f.state().camera_target().is_some(),
        "a pick pans toward the window"
    );
}

/// A drag past the slop promotes to a move and cancels the pick, so a
/// later release resolves to nothing.
#[test]
fn drag_past_slop_promotes_and_cancels_the_pick() {
    let mut f = Fixture::with_config(config_pick());
    f.add_output(1, (1920, 1080));
    let window = map_client_at(&mut f, "w", (400, 300), (500, 400));
    f.state().set_zoom(0.3);

    let press = pt(700.0, 550.0);
    f.state()
        .arm_pick(PickTarget::Client(window), press, BTN_LEFT);
    // The real press path records the held button (track_held_button); a promote
    // requires it still held, so mirror that here.
    f.state().held_buttons.insert(BTN_LEFT);

    // 100 canvas px at zoom 0.3 ≈ 30 screen px, well past the 5 px slop.
    let promoted = f.state().maybe_promote_pick(press + pt(100.0, 0.0));

    assert!(promoted, "a drag past the slop promotes to a move");
    assert!(
        f.state().pending_pick.is_none(),
        "promotion cancels the pick, so a release won't also center"
    );
}

/// A resolve drops a pick — no centre — when the target's client has died.
/// Without the alive guard the camera would animate to the canvas origin.
#[test]
fn resolve_drops_when_target_died() {
    let mut f = Fixture::with_config(config_pick());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "w", (400, 300));
    let window = window_by_app_id(&mut f, "w").unwrap();
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((500, 400)),
        true,
    );
    f.state().set_zoom(0.3);
    clear_targets(&mut f);

    f.state()
        .arm_pick(PickTarget::Client(window), pt(700.0, 550.0), BTN_LEFT);
    f.kill_client(id);
    f.pump(10);

    f.state().resolve_pick(BTN_LEFT);

    assert!(
        f.state().camera_target().is_none(),
        "a dead pick target must not pan to origin"
    );
}

/// A resolve on a different output than the press drops the pick — the
/// press coords are incomparable across outputs. Both outputs sit below the
/// threshold, so only the output guard (not the pick-mode guard) can drop it.
#[test]
fn resolve_drops_on_different_output() {
    let mut f = Fixture::with_config(config_pick());
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    let window = map_client_at(&mut f, "w", (400, 300), (500, 400));

    // Put both outputs below the threshold, so out2 is still in pick mode when
    // the release lands there — the pick-mode guard would pass, isolating the
    // cross-output guard as the sole reason the pick drops.
    f.state().focused_output = Some(out2.clone());
    f.state().set_zoom(0.3);
    f.state().focused_output = Some(out1);
    f.state().set_zoom(0.3);

    f.state()
        .arm_pick(PickTarget::Client(window), pt(700.0, 550.0), BTN_LEFT);
    f.state().focused_output = Some(out2);
    clear_targets(&mut f);
    assert!(
        f.state().pick_mode(),
        "the release output is still in pick mode"
    );
    f.state().resolve_pick(BTN_LEFT);

    assert!(
        f.state().camera_target().is_none(),
        "a cross-output release must not center"
    );
}

/// A resolve while a grab is active drops the pick — a grab installed
/// between press and release (gesture, edge-pan) owns the interaction.
#[test]
fn resolve_drops_while_grabbed() {
    let mut f = Fixture::with_config(config_pick_ssd());
    f.add_output(1, (1920, 1080));
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((500, 500)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    f.state().set_zoom(0.3);

    f.state()
        .arm_pick(PickTarget::Suspended(sid), pt(700.0, 650.0), BTN_LEFT);

    // Install a real grab by dragging the stand-in's title bar.
    let bar = pt(500.0 + 50.0, 500.0 - 12.0);
    let pointer = pointer(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    f.state()
        .try_suspended_button(&pointer, bar, BTN_LEFT, serial, mods());
    assert!(pointer.is_grabbed(), "the title-bar drag installed a grab");

    f.state().resolve_pick(BTN_LEFT);

    assert!(
        f.state().camera_target().is_none(),
        "a resolve under an active grab must not center"
    );

    f.state().dismiss_suspended(sid);
}

/// A resolve after zoom rose back above the threshold drops the pick — the
/// deliberate zoom-out must not be stomped back to 1.0.
#[test]
fn resolve_drops_when_zoom_rose_above_threshold() {
    let mut f = Fixture::with_config(config_pick());
    f.add_output(1, (1920, 1080));
    let window = map_client_at(&mut f, "w", (400, 300), (500, 400));
    f.state().set_zoom(0.3);
    clear_targets(&mut f);

    f.state()
        .arm_pick(PickTarget::Client(window), pt(700.0, 550.0), BTN_LEFT);
    // The user zooms back in while holding.
    f.state().set_zoom(0.8);
    f.state().resolve_pick(BTN_LEFT);

    assert!(
        f.state().camera_target().is_none(),
        "a resolve above the threshold must not center"
    );
}

/// Pointer focus is suppressed over a canvas window in pick mode, in both
/// the real-input cascade and the per-frame resync path.
#[test]
fn focus_suppressed_over_window_in_pick_mode() {
    let mut f = Fixture::with_config(config_pick());
    f.add_output(1, (1920, 1080));
    let _window = map_client_at(&mut f, "w", (400, 300), (500, 400));
    f.state().set_zoom(0.3);

    let canvas = pt(700.0, 550.0);
    let camera = f.state().camera();
    let zoom = f.state().zoom();
    let screen = canvas_to_screen(CanvasPos(canvas), camera, zoom).0;
    assert!(
        f.state().pointer_focus_under_pick(screen, canvas).is_none(),
        "a canvas window yields no pointer focus in pick mode"
    );

    // The per-frame resync agrees, so no camera/zoom frame hands the client its
    // enter back.
    f.state().warp_pointer(canvas);
    f.state().flush_pointer_resync();
    assert!(
        f.state()
            .seat
            .get_pointer()
            .unwrap()
            .current_focus()
            .is_none(),
        "a resync over the window sends no enter to the client"
    );
}

/// Suppression is conditional, not blanket: a canvas widget/layer stays
/// interactive and still receives pointer focus in pick mode.
#[test]
fn focus_reaches_canvas_content_in_pick_mode() {
    let mut f = Fixture::with_config(
        Config::from_toml(
            r#"
            [zoom]
            interact_min = 0.5
            [[window_rules]]
            app_id = "widget"
            position = [100, 100]
        "#,
        )
        .unwrap(),
    );
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // A canvas-positioned layer widget at (100,100), size 200×150.
    let layer = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Background, "widget");
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

    f.state().set_zoom(0.3);
    assert!(f.state().pick_mode(), "below the threshold");

    // The rule position is Y-up/window-centered; read the resolved canvas
    // top-left and aim at the widget's center (size 200×150).
    let cl_pos = f.state().canvas_layers[0].position.unwrap();
    let canvas = pt(cl_pos.x as f64 + 100.0, cl_pos.y as f64 + 75.0);
    let camera = f.state().camera();
    let zoom = f.state().zoom();
    let screen = canvas_to_screen(CanvasPos(canvas), camera, zoom).0;
    assert!(
        f.state().pointer_focus_under_pick(screen, canvas).is_some(),
        "a canvas widget stays reachable — suppression is conditional"
    );
}

/// The `0.0` default disables the feature: even at an extreme zoom-out a
/// press behaves as above-threshold and focus is not suppressed.
#[test]
fn default_zero_disables_pick_mode() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let _window = map_client_at(&mut f, "w", (400, 300), (500, 400));
    f.state().set_zoom(0.001);

    assert!(!f.state().pick_mode(), "0.0 threshold is off at any zoom");

    let pointer = pointer(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    let consumed = f
        .state()
        .try_pick_button(&pointer, pt(700.0, 550.0), BTN_LEFT, serial, mods());
    assert!(
        !consumed,
        "the press is not consumed when the feature is off"
    );

    let canvas = pt(700.0, 550.0);
    let camera = f.state().camera();
    let zoom = f.state().zoom();
    let screen = canvas_to_screen(CanvasPos(canvas), camera, zoom).0;
    assert!(
        f.state().pointer_focus_under_pick(screen, canvas).is_some(),
        "focus is not suppressed when the feature is off"
    );
}

/// A press over a canvas window's SSD title-bar band is one uniform pick target
/// below the threshold: it is consumed and arms a client pick — not a close, a
/// move, or a fit — and the affordance shows the Pointer cursor there.
#[test]
fn chrome_below_threshold_is_one_pick_target() {
    let mut f = Fixture::with_config(config_pick_ssd());
    f.add_output(1, (1920, 1080));
    let window = map_client_at(&mut f, "w", (400, 300), (500, 400));
    give_ssd(&mut f, &window);
    f.state().set_zoom(0.3);

    // A point in the SSD title-bar band, above the window rect (bar height 25).
    let bar = pt(550.0, 388.0);
    let pointer = pointer(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    let consumed = f
        .state()
        .try_pick_button(&pointer, bar, BTN_LEFT, serial, mods());

    assert!(consumed, "a chrome press picks below the threshold");
    let armed = f.state().pending_pick.as_ref().map(|p| p.target.clone());
    assert!(
        matches!(armed, Some(PickTarget::Client(_))),
        "the chrome press arms a client pick, not a close/move/fit"
    );
    assert!(window.alive(), "the close button did not close the window");
    assert!(
        !pointer.is_grabbed(),
        "the title bar did not start a move grab"
    );

    // Affordance parity: the same position shows the pick Pointer cursor.
    f.state().update_decoration_cursor(bar);
    assert!(f.state().cursor.decoration_cursor);
    assert!(
        matches!(
            f.state().cursor.cursor_status,
            CursorImageStatus::Named(CursorIcon::Pointer)
        ),
        "a pick target hover shows the Pointer cursor"
    );
}

/// Above the threshold the same chrome press is left to normal chrome handling:
/// `try_pick_button` does not consume it.
#[test]
fn chrome_above_threshold_is_not_a_pick() {
    let mut f = Fixture::with_config(config_pick_ssd());
    f.add_output(1, (1920, 1080));
    let window = map_client_at(&mut f, "w", (400, 300), (500, 400));
    give_ssd(&mut f, &window);
    f.state().set_zoom(0.8);

    let bar = pt(550.0, 388.0);
    let pointer = pointer(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    let consumed = f
        .state()
        .try_pick_button(&pointer, bar, BTN_LEFT, serial, mods());

    assert!(!consumed, "above the threshold chrome behaves normally");
}

/// A pick armed but never held (a release lost while locked / after an output
/// drop) must not promote: `maybe_promote_pick` bails and cancels the pick, so
/// the next drag can't glue the window to the cursor with no button down.
#[test]
fn stale_pick_does_not_promote() {
    let mut f = Fixture::with_config(config_pick());
    f.add_output(1, (1920, 1080));
    let window = map_client_at(&mut f, "w", (400, 300), (500, 400));
    f.state().set_zoom(0.3);

    let press = pt(700.0, 550.0);
    f.state()
        .arm_pick(PickTarget::Client(window), press, BTN_LEFT);
    // BTN_LEFT is deliberately NOT in held_buttons — the lost-release case.

    let promoted = f.state().maybe_promote_pick(press + pt(100.0, 0.0));

    assert!(!promoted, "a pick with no held button must not promote");
    assert!(
        f.state().pending_pick.is_none(),
        "the stale pick is cancelled"
    );
    assert!(
        !f.state().seat.get_pointer().unwrap().is_grabbed(),
        "no move grab was installed"
    );
}

/// A consumed left press records its button in the swallowed set, the
/// precondition for suppressing the matching release forward.
#[test]
fn left_press_records_swallowed_button() {
    let mut f = Fixture::with_config(config_pick());
    f.add_output(1, (1920, 1080));
    let _window = map_client_at(&mut f, "w", (400, 300), (500, 400));
    f.state().set_zoom(0.3);

    let pointer = pointer(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    f.state()
        .try_pick_button(&pointer, pt(700.0, 550.0), BTN_LEFT, serial, mods());

    assert!(
        f.state().pick_swallowed_buttons.contains(&BTN_LEFT),
        "the swallowed press is recorded so its release is swallowed too"
    );
}

/// A non-left press is swallowed and recorded (so its release doesn't forward a
/// phantom to a client that never saw the press), but it arms no center — only
/// a left click picks.
#[test]
fn non_left_press_is_swallowed_without_arming() {
    let mut f = Fixture::with_config(config_pick());
    f.add_output(1, (1920, 1080));
    let _window = map_client_at(&mut f, "w", (400, 300), (500, 400));
    f.state().set_zoom(0.3);

    let pointer = pointer(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    let consumed = f
        .state()
        .try_pick_button(&pointer, pt(700.0, 550.0), BTN_RIGHT, serial, mods());

    assert!(consumed, "a right press over a window is consumed too");
    assert!(
        f.state().pick_swallowed_buttons.contains(&BTN_RIGHT),
        "the right press is recorded for release suppression"
    );
    assert!(
        f.state().pending_pick.is_none(),
        "a non-left press arms no center"
    );
}

/// The scroll fallback gate discriminates by `pick_target_under`: in pick
/// mode a bare scroll over a canvas window would retry the OnCanvas binding
/// (which pans) instead of dying, while a widget/canvas-layer is not a pick
/// target so its scroll path is untouched.
#[test]
fn scroll_fallback_gate_hits_windows_not_widgets() {
    let mut f = Fixture::with_config(
        Config::from_toml(
            r#"
            [zoom]
            interact_min = 0.5
            [[window_rules]]
            app_id = "widget"
            position = [100, 100]
        "#,
        )
        .unwrap(),
    );
    f.add_output(1, (1920, 1080));
    let _window = map_client_at(&mut f, "w", (400, 300), (2000, 400));

    // A canvas-positioned layer widget away from the window.
    let id = f.add_client();
    let layer = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Background, "widget");
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

    f.state().set_zoom(0.3);

    // The gate fires over the window, not over the widget.
    assert!(
        f.state().pick_target_under(pt(2200.0, 550.0)).is_some(),
        "a canvas window is a scroll-fallback target"
    );
    let cl_pos = f.state().canvas_layers[0].position.unwrap();
    assert!(
        f.state()
            .pick_target_under(pt(cl_pos.x as f64 + 100.0, cl_pos.y as f64 + 75.0))
            .is_none(),
        "a canvas widget is not a scroll-fallback target"
    );

    // The fallback target the gate reaches for is real: bare scroll finds no
    // OnWindow binding (it would die), but OnCanvas pans.
    let bare = ModifiersState::default();
    assert!(
        f.state()
            .config
            .mouse_scroll_lookup_ctx(&bare, AxisSource::Finger, BindingContext::OnWindow)
            .is_none(),
        "a bare scroll over a window has no OnWindow binding — it would die"
    );
    assert!(
        matches!(
            f.state().config.mouse_scroll_lookup_ctx(
                &bare,
                AxisSource::Finger,
                BindingContext::OnCanvas
            ),
            Some(MouseAction::PanViewport)
        ),
        "the OnCanvas fallback pans"
    );
}

fn mods() -> ModifiersState {
    ModifiersState::default()
}
