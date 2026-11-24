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

use super::client::{ClientId, LayerConfigureProps};
use super::input_backend::{FakeDevice, key_press, pointer_to_screen, press, release};
use super::popups::grow_popup;
use super::{Fixture, keyboard_focus, pointer_focus};

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

/// Anchor to the top edge like the bar does, 300×40.
fn map_bar(f: &mut Fixture, id: ClientId) -> wayland_client::protocol::wl_surface::WlSurface {
    let created = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Top, "bar");
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
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    let bar = map_bar(&mut f, id);

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
