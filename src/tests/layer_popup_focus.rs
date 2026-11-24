//! On-demand layer focus versus a grabbed layer popup: `focus_layer_if_on_demand`
//! runs on every press over a layer, and has to tell a press inside the
//! layer's own popup apart from a press that has genuinely left it. Confusing
//! the two used to read a popup click as "elsewhere", dropping the on-demand
//! focus and tearing the popup's grab down under it.
//!
//! A shell can also switch its bar from `None` to `OnDemand` only as it opens a
//! menu on it, after the click that would have granted on-demand focus. The
//! popup grab grants that focus instead, or the menu closes as it opens.

use driftwm::config::{BTN_LEFT, BTN_RIGHT};
use smithay::utils::Point;
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use super::client::{ClientId, LayerConfigureProps, TouchEvent};
use super::input_backend::{
    FakeDevice, key_press, pointer_to_screen, press, release, touch_down, touch_up,
};
use super::popups::{grow_popup, keyboard_enter_serial};
use super::{
    Fixture, adopt_last_configure, keyboard_focus, map_window, pointer_focus, server_surface,
    tick_until_settled, to_canvas, window_by_app_id,
};

/// evdev code of Super, the default `mod_key`.
const KEY_LEFTMETA: u32 = 125;

/// Map a 200×150 top layer with `OnDemand` keyboard interactivity — unanchored,
/// so it centers on the 1920×1080 fixture output at screen (860,465)-(1060,615)
/// — click it to grant on-demand focus, then open a popup on it, grab it, and
/// grow it to 200×100 so it is more than a single hit-testable pixel. Returns
/// the layer's client and server surfaces, the popup's client surface, and the
/// layer's screen-space top-left corner.
fn setup_focused_layer_with_grabbed_popup(
    f: &mut Fixture,
    id: ClientId,
    device: &FakeDevice,
) -> (
    wayland_client::protocol::wl_surface::WlSurface,
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland_client::protocol::wl_surface::WlSurface,
    Point<f64, smithay::utils::Logical>,
) {
    let created = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Top, "widget");
    let layer_surface = created.surface.clone();
    created.set_configure_props(LayerConfigureProps {
        size: Some((200, 150)),
        kb_interactivity: Some(zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand),
        ..Default::default()
    });
    created.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&layer_surface);
    layer.set_size(200, 150);
    layer.attach_new_buffer();
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let layer_topleft = Point::from((860.0, 465.0));
    let output = f.state().space.outputs().next().cloned().unwrap();
    let layer_root = smithay::desktop::layer_map_for_output(&output)
        .layers()
        .next()
        .unwrap()
        .wl_surface()
        .clone();

    // Click the layer itself first, as a real menu owner would before opening
    // its menu, so it holds on-demand keyboard focus going into the grab.
    pointer_to_screen(f, device, layer_topleft + Point::from((100.0, 75.0)));
    press(f, device, BTN_LEFT);
    release(f, device, BTN_LEFT);
    assert_eq!(
        keyboard_focus(f),
        Some(layer_root.clone()),
        "test setup bug: clicking the layer must grant it on-demand focus"
    );

    let popup = f.client(id).create_layer_popup(&layer_surface);
    let popup_surface = popup.surface.clone();
    popup.commit();
    f.roundtrip(id);
    let popup = f.client(id).popup(&popup_surface);
    popup.grab(1);
    popup.attach_new_buffer();
    popup.ack_last_and_commit();
    f.double_roundtrip(id);
    grow_popup(f, id, &popup_surface, (200, 100));
    assert!(
        f.state().popup_grab.is_some(),
        "test setup bug: the popup grab must be installed"
    );

    (layer_surface, layer_root, popup_surface, layer_topleft)
}

/// A press inside the layer's own grabbed popup must not read as a click
/// elsewhere: it must not drop the layer's on-demand focus, and must not tear
/// the popup's grab down with it.
#[test]
fn press_inside_a_grabbed_layer_popup_keeps_it_open() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    let (layer_surface, layer_root, popup_surface, layer_topleft) =
        setup_focused_layer_with_grabbed_popup(&mut f, id, &device);

    let popup_pos = f.client(id).popup(&popup_surface).pending_configure.pos;
    let inside =
        layer_topleft + Point::from((f64::from(popup_pos.0) + 10.0, f64::from(popup_pos.1) + 10.0));
    pointer_to_screen(&mut f, &device, inside);
    press(&mut f, &device, BTN_LEFT);
    release(&mut f, &device, BTN_LEFT);
    f.double_roundtrip(id);

    assert!(
        !f.client(id).popup(&popup_surface).popup_done,
        "a press inside a layer's own grabbed popup must not dismiss it"
    );
    assert!(
        f.state().popup_grab.is_some(),
        "the popup grab must survive a press inside the popup"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(layer_root),
        "keyboard focus must stay on the on-demand layer that owns the popup"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.client(id).layer(&layer_surface).layer_surface.destroy();
    f.double_roundtrip(id);
}

/// A press genuinely outside the popup and its owning layer must still
/// dismiss it, as before. `BTN_RIGHT` is unbound in the default config's
/// on-canvas context (only plain `left` is), so this reaches the plain
/// forward path and exercises the popup grab's own dismiss-on-outside-click
/// check rather than a mouse binding that would swallow the press itself.
#[test]
fn press_outside_a_grabbed_layer_popup_dismisses_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    let (layer_surface, _layer_root, popup_surface, _layer_topleft) =
        setup_focused_layer_with_grabbed_popup(&mut f, id, &device);

    let outside = Point::from((100.0, 100.0));
    pointer_to_screen(&mut f, &device, outside);
    press(&mut f, &device, BTN_RIGHT);
    release(&mut f, &device, BTN_RIGHT);
    f.double_roundtrip(id);

    assert!(
        f.client(id).popup(&popup_surface).popup_done,
        "a press outside both the popup and its owning layer must dismiss it"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.client(id).layer(&layer_surface).layer_surface.destroy();
    f.double_roundtrip(id);
}

/// While a layer's popup holds the pointer grab, an `anywhere` binding pressed
/// over the layer leaves the press to that grab. A pan grab installed in its
/// place would leave the menu open with nothing to dismiss it.
#[test]
fn an_anywhere_pan_over_a_layer_with_a_grabbed_popup_leaves_the_grab_alone() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    let (layer_surface, layer_root, popup_surface, layer_topleft) =
        setup_focused_layer_with_grabbed_popup(&mut f, id, &device);

    // The layer's bottom row, below the popup.
    let popup_pos = f.client(id).popup(&popup_surface).pending_configure.pos;
    assert!(
        popup_pos.1 + 100 < 145,
        "test setup bug: the popup must leave the layer's bottom row clear"
    );
    let on_layer = layer_topleft + Point::from((100.0, 145.0));
    pointer_to_screen(&mut f, &device, on_layer);
    assert_eq!(
        pointer_focus(&mut f),
        Some(layer_root),
        "test setup bug: the pointer must rest on the layer, clear of its popup"
    );
    let camera = f.state().camera();

    key_press(&mut f, KEY_LEFTMETA);
    press(&mut f, &device, BTN_LEFT);
    pointer_to_screen(&mut f, &device, on_layer - Point::from((200.0, 0.0)));
    release(&mut f, &device, BTN_LEFT);
    f.double_roundtrip(id);

    assert_eq!(
        f.state().camera(),
        camera,
        "the popup grab owns the pointer, so mod+left must not pan"
    );
    assert!(
        f.state().popup_grab.is_some() && !f.client(id).popup(&popup_surface).popup_done,
        "the press landed on the popup's own layer, so the menu stays open under its grab"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.client(id).layer(&layer_surface).layer_surface.destroy();
    f.double_roundtrip(id);
}

/// A `Top` bar on the output the compositor picks.
fn map_bar(f: &mut Fixture, id: ClientId) -> wayland_client::protocol::wl_surface::WlSurface {
    map_layer_on(f, id, None, zwlr_layer_shell_v1::Layer::Top)
}

/// A 300×40 layer on `layer`, anchored to the top edge of `output` (the
/// compositor's pick when `None`): screen (810,0)-(1110,40) on a 1920-wide
/// output.
fn map_layer_on(
    f: &mut Fixture,
    id: ClientId,
    output: Option<&wayland_client::protocol::wl_output::WlOutput>,
    layer: zwlr_layer_shell_v1::Layer,
) -> wayland_client::protocol::wl_surface::WlSurface {
    let created = f.client(id).create_layer(output, layer, "bar");
    let surface = created.surface.clone();
    created.set_configure_props(LayerConfigureProps {
        size: Some((300, 40)),
        anchor: Some(zwlr_layer_surface_v1::Anchor::Top),
        ..Default::default()
    });
    created.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&surface);
    layer.set_size(300, 40);
    layer.attach_new_buffer();
    layer.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

/// A tray menu on a bar that stays `None` the whole time: popup + grab right
/// after a right-click. The grab must install and the popup must stay mapped.
#[test]
fn tray_popup_on_none_interactivity_bar_stays_open() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    let bar = map_bar(&mut f, id);

    // Right-click the bar (the shell opens the menu on release).
    pointer_to_screen(&mut f, &device, Point::from((900.0, 8.0)));
    press(&mut f, &device, BTN_RIGHT);
    release(&mut f, &device, BTN_RIGHT);
    f.double_roundtrip(id);

    let popup = f.client(id).create_layer_popup(&bar);
    let popup_surface = popup.surface.clone();
    popup.commit();
    f.roundtrip(id);
    let popup = f.client(id).popup(&popup_surface);
    popup.grab(1);
    popup.attach_new_buffer();
    popup.ack_last_and_commit();
    f.double_roundtrip(id);

    assert!(
        f.state().popup_grab.is_some(),
        "the popup grab must install on a None-interactivity layer parent"
    );
    assert!(
        !f.client(id).popup(&popup_surface).popup_done,
        "a grabbed tray menu popup on the bar must not be dismissed"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.client(id).layer(&bar).layer_surface.destroy();
    f.double_roundtrip(id);
}

/// A shell's tray menu sequence: right-click with the bar at `None`, then flip
/// the bar to `OnDemand` (so the popup can carry keyboard), then create + grab
/// the popup.
#[test]
fn tray_popup_after_bar_flip_to_on_demand_stays_open() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    let bar = map_bar(&mut f, id);
    let bar_root = layer_root_on(&output);

    pointer_to_screen(&mut f, &device, Point::from((900.0, 8.0)));
    press(&mut f, &device, BTN_RIGHT);
    release(&mut f, &device, BTN_RIGHT);
    f.double_roundtrip(id);

    // The shell flips the bar to OnDemand before mapping the popup.
    let layer = f.client(id).layer(&bar);
    layer.set_configure_props(LayerConfigureProps {
        kb_interactivity: Some(zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);
    assert_ne!(
        keyboard_focus(&mut f),
        Some(bar_root.clone()),
        "test setup bug: only the grab may give the bar the keyboard"
    );

    let popup = f.client(id).create_layer_popup(&bar);
    let popup_surface = popup.surface.clone();
    popup.commit();
    f.roundtrip(id);
    let popup = f.client(id).popup(&popup_surface);
    popup.grab(1);
    popup.attach_new_buffer();
    popup.ack_last_and_commit();
    f.double_roundtrip(id);

    assert!(
        f.state().popup_grab.is_some(),
        "the popup grab must install after the bar flipped to OnDemand"
    );
    assert!(
        !f.client(id).popup(&popup_surface).popup_done,
        "a grabbed tray menu popup must survive the bar's interactivity flip"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(bar_root),
        "the grab must give the bar on-demand keyboard focus"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.client(id).layer(&bar).layer_surface.destroy();
    f.double_roundtrip(id);
}

/// A bar flipped to `OnDemand` must hold on-demand keyboard focus after a
/// click on it.
#[test]
fn bar_flipped_to_on_demand_gains_focus_after_flip() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    let bar = map_bar(&mut f, id);

    let layer = f.client(id).layer(&bar);
    layer.set_configure_props(LayerConfigureProps {
        kb_interactivity: Some(zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);

    // Move the pointer over the bar and click — an OnDemand layer gains focus
    // on click.
    pointer_to_screen(&mut f, &device, Point::from((900.0, 8.0)));
    press(&mut f, &device, BTN_LEFT);
    release(&mut f, &device, BTN_LEFT);
    f.double_roundtrip(id);

    let output = f.state().space.outputs().next().cloned().unwrap();
    let bar_root = smithay::desktop::layer_map_for_output(&output)
        .layers()
        .next()
        .unwrap()
        .wl_surface()
        .clone();
    assert_eq!(
        keyboard_focus(&mut f),
        Some(bar_root),
        "a clicked OnDemand layer must hold keyboard focus"
    );
}

/// The server-side surface of the one layer mapped on `output`.
fn layer_root_on(
    output: &smithay::output::Output,
) -> smithay::reexports::wayland_server::protocol::wl_surface::WlSurface {
    let map = smithay::desktop::layer_map_for_output(output);
    assert_eq!(
        map.layers().count(),
        1,
        "test setup bug: the output must hold just the layer the test mapped"
    );
    map.layers().next().unwrap().wl_surface().clone()
}

/// Switch `bar`'s keyboard interactivity, as a shell does as it opens a menu.
fn set_interactivity(
    f: &mut Fixture,
    id: ClientId,
    bar: &wayland_client::protocol::wl_surface::WlSurface,
    interactivity: zwlr_layer_surface_v1::KeyboardInteractivity,
) {
    let layer = f.client(id).layer(bar);
    layer.set_configure_props(LayerConfigureProps {
        kb_interactivity: Some(interactivity),
        ..Default::default()
    });
    layer.commit();
    f.double_roundtrip(id);
}

/// Open a popup on `bar` and grab it with `serial` before its first buffer,
/// the order a menu opens in. Returns the popup's client surface.
fn open_grabbed_layer_popup(
    f: &mut Fixture,
    id: ClientId,
    bar: &wayland_client::protocol::wl_surface::WlSurface,
    serial: u32,
) -> wayland_client::protocol::wl_surface::WlSurface {
    let popup = f.client(id).create_layer_popup(bar);
    let popup_surface = popup.surface.clone();
    popup.commit();
    f.roundtrip(id);
    let popup = f.client(id).popup(&popup_surface);
    popup.grab(serial);
    popup.attach_new_buffer();
    popup.ack_last_and_commit();
    f.double_roundtrip(id);
    popup_surface
}

/// Map an 800×600 window and make it fullscreen on the active output. Returns
/// its server-side surface.
fn fullscreen_game(
    f: &mut Fixture,
    id: ClientId,
) -> smithay::reexports::wayland_server::protocol::wl_surface::WlSurface {
    let surface = map_window(f, id, "game", (800, 600));
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(f, id, &surface);
    tick_until_settled(f);
    server_surface(&window_by_app_id(f, "game").unwrap())
}

/// Output `fullscreen` shows the fullscreen window `game`, which holds the
/// keyboard; output `clear` shows none. `bar_client` has mapped nothing yet.
struct TwoOutputs {
    f: Fixture,
    fullscreen: smithay::output::Output,
    clear: smithay::output::Output,
    bar_client: ClientId,
    game: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
}

fn two_outputs() -> TwoOutputs {
    let mut f = Fixture::new();
    let fullscreen = f.add_output(1, (1920, 1080));
    let clear = f.add_output(2, (1920, 1080));
    let game_client = f.add_client();
    let bar_client = f.add_client();
    let game = fullscreen_game(&mut f, game_client);
    assert!(
        f.state().is_output_fullscreen(&fullscreen),
        "test setup bug: the first output must show the fullscreen window"
    );
    assert!(
        !f.state().is_output_fullscreen(&clear),
        "test setup bug: the second output must show no fullscreen window"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(game.clone()),
        "test setup bug: the fullscreen window must hold the keyboard"
    );
    f.double_roundtrip(bar_client);
    TwoOutputs {
        f,
        fullscreen,
        clear,
        bar_client,
        game,
    }
}

/// A popup grab refused for clobbering an unrelated live grab moves no focus:
/// a bar switched to `OnDemand` must not take the keyboard for a menu that
/// never opened.
#[test]
fn a_refused_popup_grab_leaves_keyboard_focus_alone() {
    // Far from the serial counter, which every test in the process advances.
    let earlier_grab_serial = u32::MAX - 1;
    let refused_grab_serial = u32::MAX;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let wid = f.add_client();
    let bid = f.add_client();
    let device = FakeDevice::mouse();

    let window = map_window(&mut f, wid, "app", (400, 300));
    let app = server_surface(&window_by_app_id(&mut f, "app").unwrap());
    assert_eq!(
        keyboard_focus(&mut f),
        Some(app.clone()),
        "test setup bug: the window must hold the keyboard"
    );

    // A grab that has come and gone leaves its serial on the seat, so the
    // bar's grab is checked against a previous serial no live grab holds.
    let earlier = f.client(wid).create_popup(&window);
    let earlier_surface = earlier.surface.clone();
    earlier.commit();
    f.roundtrip(wid);
    let earlier = f.client(wid).popup(&earlier_surface);
    earlier.grab(earlier_grab_serial);
    earlier.attach_new_buffer();
    earlier.ack_last_and_commit();
    f.double_roundtrip(wid);
    assert!(
        f.state().popup_grab.is_some(),
        "test setup bug: the window's popup grab must install"
    );
    f.client(wid).popup(&earlier_surface).destroy();
    f.double_roundtrip(wid);
    pointer_to_screen(&mut f, &device, Point::from((960.0, 540.0)));
    assert!(
        f.state().popup_grab.is_none(),
        "test setup bug: the window's popup grab must end with its popup"
    );
    assert!(
        !f.state().seat.get_keyboard().unwrap().is_grabbed(),
        "test setup bug: the keyboard must be free, so only the pointer grab refuses"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(app.clone()),
        "test setup bug: the window must hold the keyboard again"
    );

    let bar = map_bar(&mut f, bid);
    set_interactivity(
        &mut f,
        bid,
        &bar,
        zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(app.clone()),
        "test setup bug: the flip alone must not move the keyboard"
    );

    // Held on the window: a live click grab under a serial of its own.
    press(&mut f, &device, BTN_LEFT);
    assert!(
        f.state().seat.get_pointer().unwrap().is_grabbed(),
        "test setup bug: the held press must be a live pointer grab"
    );

    let popup = open_grabbed_layer_popup(&mut f, bid, &bar, refused_grab_serial);
    assert!(
        f.client(bid).popup(&popup).popup_done,
        "test setup bug: the bar's grab must be refused"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(app),
        "a refused grab must leave keyboard focus where it was"
    );
    assert!(
        f.state().on_demand_layer.is_none(),
        "a refused grab must not record the bar as the on-demand layer"
    );

    release(&mut f, &device, BTN_LEFT);
}

/// A fullscreen window hides the bar, so the bar's menu must not take the
/// keyboard from it, even with the pointer parked where the bar would be.
#[test]
fn a_bar_hidden_behind_a_fullscreen_window_does_not_take_the_keyboard_for_its_popup() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let wid = f.add_client();
    let bid = f.add_client();
    let device = FakeDevice::mouse();

    let bar = map_bar(&mut f, bid);
    let game = fullscreen_game(&mut f, wid);
    assert!(
        f.state().is_output_fullscreen(&output),
        "test setup bug: the output must show the fullscreen window"
    );
    pointer_to_screen(&mut f, &device, Point::from((900.0, 8.0)));
    f.double_roundtrip(wid);
    assert_eq!(
        keyboard_focus(&mut f),
        Some(game.clone()),
        "test setup bug: the fullscreen window must hold the keyboard"
    );

    set_interactivity(
        &mut f,
        bid,
        &bar,
        zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
    );
    let popup = open_grabbed_layer_popup(&mut f, bid, &bar, 1);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(game),
        "a hidden bar's menu must not take the keyboard from the fullscreen window"
    );
    assert!(
        f.state().on_demand_layer.is_none(),
        "a hidden bar must not be recorded as the on-demand layer"
    );
    assert!(
        f.client(bid).popup(&popup).popup_done,
        "with the keyboard away from its root, the hidden bar's menu closes"
    );
    assert!(
        f.state().popup_grab.is_none(),
        "the hidden bar's menu must leave no popup grab behind"
    );
    assert!(
        !f.state().seat.get_pointer().unwrap().is_grabbed(),
        "the hidden bar's menu must leave the pointer ungrabbed"
    );
    assert!(
        !f.state().seat.get_keyboard().unwrap().is_grabbed(),
        "the hidden bar's menu must leave the keyboard ungrabbed"
    );
}

/// Whether a bar is hidden is read on the bar's own output: a fullscreen
/// window there hides it, though the active output shows none.
#[test]
fn a_bar_hidden_on_its_own_output_does_not_take_the_keyboard_while_another_output_is_active() {
    let TwoOutputs {
        mut f,
        fullscreen,
        clear,
        bar_client,
        game,
    } = two_outputs();
    let device = FakeDevice::mouse();

    let fullscreen_wl = f.client(bar_client).output(&fullscreen.name());
    let bar = map_layer_on(
        &mut f,
        bar_client,
        Some(&fullscreen_wl),
        zwlr_layer_shell_v1::Layer::Top,
    );
    assert_eq!(
        smithay::desktop::layer_map_for_output(&fullscreen)
            .layers()
            .count(),
        1,
        "test setup bug: the bar must map on the fullscreen output"
    );
    f.state().focused_output = Some(clear.clone());
    pointer_to_screen(&mut f, &device, Point::from((900.0, 500.0)));
    f.double_roundtrip(bar_client);
    set_interactivity(
        &mut f,
        bar_client,
        &bar,
        zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
    );
    assert!(
        !f.state().is_fullscreen(),
        "test setup bug: the active output must show no fullscreen window"
    );

    open_grabbed_layer_popup(&mut f, bar_client, &bar, 1);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(game),
        "a bar hidden on its own output must not take the keyboard"
    );
}

/// Whether a bar is hidden is read on the bar's own output: with no
/// fullscreen window there, its menu gets the keyboard though the active
/// output shows one.
#[test]
fn a_bar_visible_on_its_own_output_gets_the_keyboard_while_a_fullscreen_output_is_active() {
    let TwoOutputs {
        mut f,
        fullscreen,
        clear,
        bar_client,
        ..
    } = two_outputs();
    let device = FakeDevice::mouse();

    let clear_wl = f.client(bar_client).output(&clear.name());
    let bar = map_layer_on(
        &mut f,
        bar_client,
        Some(&clear_wl),
        zwlr_layer_shell_v1::Layer::Top,
    );
    let root = layer_root_on(&clear);
    f.state().focused_output = Some(clear.clone());
    pointer_to_screen(&mut f, &device, Point::from((900.0, 8.0)));
    press(&mut f, &device, BTN_RIGHT);
    release(&mut f, &device, BTN_RIGHT);
    f.double_roundtrip(bar_client);
    set_interactivity(
        &mut f,
        bar_client,
        &bar,
        zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
    );
    f.state().focused_output = Some(fullscreen.clone());
    assert!(
        f.state().is_fullscreen(),
        "test setup bug: the active output must be the fullscreen one"
    );
    assert_ne!(
        keyboard_focus(&mut f),
        Some(root.clone()),
        "test setup bug: only the grab may give the bar the keyboard"
    );

    let popup = open_grabbed_layer_popup(&mut f, bar_client, &bar, 1);

    assert!(
        !f.client(bar_client).popup(&popup).popup_done,
        "a visible bar's menu must stay open"
    );
    assert!(
        f.state().popup_grab.is_some(),
        "a visible bar's menu must keep its grab"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(root),
        "a visible bar's menu must get the keyboard"
    );
}

/// An `Overlay` layer stays above a fullscreen window, so its menu gets the
/// keyboard there.
#[test]
fn an_overlay_layer_over_a_fullscreen_window_gets_the_keyboard_for_its_popup() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let wid = f.add_client();
    let bid = f.add_client();
    let device = FakeDevice::mouse();

    fullscreen_game(&mut f, wid);
    assert!(
        f.state().is_output_fullscreen(&output),
        "test setup bug: the output must show the fullscreen window"
    );
    let overlay = map_layer_on(&mut f, bid, None, zwlr_layer_shell_v1::Layer::Overlay);
    let root = layer_root_on(&output);
    pointer_to_screen(&mut f, &device, Point::from((900.0, 8.0)));
    press(&mut f, &device, BTN_RIGHT);
    release(&mut f, &device, BTN_RIGHT);
    f.double_roundtrip(bid);
    set_interactivity(
        &mut f,
        bid,
        &overlay,
        zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
    );
    assert_ne!(
        keyboard_focus(&mut f),
        Some(root.clone()),
        "test setup bug: only the grab may give the overlay the keyboard"
    );

    let popup = open_grabbed_layer_popup(&mut f, bid, &overlay, 1);

    assert!(
        !f.client(bid).popup(&popup).popup_done,
        "an overlay's menu over a fullscreen window must stay open"
    );
    assert!(
        f.state().popup_grab.is_some(),
        "an overlay's menu over a fullscreen window must keep its grab"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(root),
        "an overlay's menu over a fullscreen window must get the keyboard"
    );
}

/// The grant does not depend on where the mouse pointer is: a menu opened by
/// a touch tap on the bar stays open with the pointer on another client's
/// window.
#[test]
fn a_tray_menu_opened_by_a_touch_tap_stays_open_while_the_pointer_rests_on_another_window() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let wid = f.add_client();
    let bid = f.add_client();
    let device = FakeDevice::mouse();

    map_window(&mut f, wid, "app", (400, 300));
    let app = server_surface(&window_by_app_id(&mut f, "app").unwrap());
    let bar = map_bar(&mut f, bid);
    let root = layer_root_on(&output);
    pointer_to_screen(&mut f, &device, Point::from((960.0, 540.0)));
    f.double_roundtrip(wid);

    let tap = to_canvas(&mut f, Point::from((900.0, 8.0)));
    touch_down(&mut f, tap, 0);
    touch_up(&mut f, 0);
    f.double_roundtrip(bid);
    set_interactivity(
        &mut f,
        bid,
        &bar,
        zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
    );
    assert!(
        f.client(bid).state.touch_events.contains(&TouchEvent::Down),
        "test setup bug: the tap must reach the bar"
    );
    assert_eq!(
        pointer_focus(&mut f),
        Some(app),
        "test setup bug: the pointer must rest on the other client's window"
    );
    assert_ne!(
        keyboard_focus(&mut f),
        Some(root.clone()),
        "test setup bug: only the grab may give the bar the keyboard"
    );

    let popup = open_grabbed_layer_popup(&mut f, bid, &bar, 1);

    assert!(
        !f.client(bid).popup(&popup).popup_done,
        "a touch-opened tray menu must stay open"
    );
    assert!(
        f.state().popup_grab.is_some(),
        "a touch-opened tray menu must keep its grab"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(root),
        "a touch-opened tray menu must get the keyboard"
    );
}

/// The grab's serial names the client's input event; the keyboard enter the
/// grant sends takes a serial of the compositor's own.
#[test]
fn the_grants_keyboard_enter_carries_the_compositors_own_serial_not_the_grabs() {
    // Far from the serial counter, which every test in the process advances.
    let grab_serial = u32::MAX - 2;

    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    let bar = map_bar(&mut f, id);
    let root = layer_root_on(&output);
    pointer_to_screen(&mut f, &device, Point::from((900.0, 8.0)));
    press(&mut f, &device, BTN_RIGHT);
    release(&mut f, &device, BTN_RIGHT);
    f.double_roundtrip(id);
    set_interactivity(
        &mut f,
        id,
        &bar,
        zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
    );

    open_grabbed_layer_popup(&mut f, id, &bar, grab_serial);
    assert_eq!(
        keyboard_focus(&mut f),
        Some(root),
        "test setup bug: the grab must give the bar the keyboard"
    );

    assert_ne!(
        keyboard_enter_serial(&mut f),
        grab_serial,
        "the grant's keyboard enter must not reuse the grab's serial"
    );
}
