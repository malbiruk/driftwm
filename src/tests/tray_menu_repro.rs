//! Reproduction of the noctalia tray context-menu flow on a layer surface whose
//! keyboard interactivity is `None` (the bar). noctalia opens the menu popup on
//! the bar's layer surface after a right-click, briefly flipping the bar to
//! `OnDemand` before the popup maps (so the grabbed popup can get keyboard
//! focus). The popup must not be dismissed (`popup_done`) by either the grab or
//! the interactivity flip.

use driftwm::config::{BTN_LEFT, BTN_RIGHT};
use smithay::utils::Point;
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use super::client::{ClientId, LayerConfigureProps};
use super::input_backend::{FakeDevice, pointer_to_screen, press, release};
use super::{Fixture, keyboard_focus};

/// Anchor to the top edge like the bar does, 300×40.
fn map_bar(f: &mut Fixture, id: ClientId) -> wayland_client::protocol::wl_surface::WlSurface {
    let created = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Top, "noctalia-bar");
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

/// Scenario A: bar stays `None` the whole time; popup + grab right after a
/// right-click. The grab must install and the popup must stay mapped.
#[test]
fn tray_popup_on_none_interactivity_bar_stays_open() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    let bar = map_bar(&mut f, id);

    // Right-click the bar (noctalia receives this and opens the menu on release).
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

/// Scenario B: the exact noctalia sequence — right-click with the bar at
/// `None`, then flip the bar to `OnDemand` (so the popup can carry keyboard),
/// then create + grab the popup.
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

    // noctalia flips the bar to OnDemand before mapping the popup.
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

/// Sanity check for scenario B: after the flip the bar must hold on-demand
/// keyboard focus, since that is what `popup_grab_allowed`/keyboard-grab logic
/// reasons about.
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
