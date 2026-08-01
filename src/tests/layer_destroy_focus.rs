//! Pointer-focus invariants around layer-surface teardown: a layer destroyed
//! beneath a stationary cursor must not leave `pointer_over_layer` stale, or
//! the next press/scroll lands on the canvas instead of the surface still
//! under the pointer (the noctalia bar closes its panel — a second layer — on
//! the click that hits it, and the following click was panning the canvas).

use driftwm::config::BTN_LEFT;
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use super::input_backend::{pointer_to_screen, press, release};
use super::{Fixture, client::LayerConfigureProps, input_backend::FakeDevice, pointer_focus};

use smithay::utils::Point;

/// Server-side surface of the layer with `namespace` on the first output.
fn layer_surface_by_namespace(
    f: &mut Fixture,
    namespace: &str,
) -> smithay::reexports::wayland_server::protocol::wl_surface::WlSurface {
    let output = f.state().space.outputs().next().cloned().unwrap();
    smithay::desktop::layer_map_for_output(&output)
        .layers()
        .find(|l| l.namespace() == namespace)
        .unwrap()
        .wl_surface()
        .clone()
}

/// Map a layer, give it a buffer, and settle. Returns the client-side surface.
fn map_layer(
    f: &mut Fixture,
    id: super::client::ClientId,
    layer: zwlr_layer_shell_v1::Layer,
    namespace: &str,
    size: (u32, u32),
    anchor: zwlr_layer_surface_v1::Anchor,
) -> wayland_client::protocol::wl_surface::WlSurface {
    let created = f.client(id).create_layer(None, layer, namespace);
    let surface = created.surface.clone();
    created.set_configure_props(LayerConfigureProps {
        size: Some(size),
        anchor: Some(anchor),
        exclusive_zone: Some(0),
        ..Default::default()
    });
    created.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&surface);
    layer.set_size(size.0 as u16, size.1 as u16);
    layer.attach_new_buffer();
    layer.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

/// dies on the bar's click. The cursor rests over the bar through the whole
/// teardown; the next press must reach the bar again, not pan the canvas.
#[test]
fn layer_destroyed_under_the_cursor_keeps_pointer_focus() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    // The bar: full-width bottom strip. No exclusive zone, like noctalia's
    // `reserve_space = false`.
    let bar = map_layer(
        &mut f,
        id,
        zwlr_layer_shell_v1::Layer::Top,
        "bar",
        (1920, 40),
        zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );

    // The panel `open_near_click_session` opens over the bar: an Overlay layer
    // covering the cursor's spot, above the bar in z-order.
    let panel = map_layer(
        &mut f,
        id,
        zwlr_layer_shell_v1::Layer::Overlay,
        "panel",
        (400, 600),
        zwlr_layer_surface_v1::Anchor::Bottom,
    );

    let over_bar = Point::from((960.0, 1060.0));
    pointer_to_screen(&mut f, &device, over_bar);
    let bar_server = layer_surface_by_namespace(&mut f, "bar");
    let panel_server = layer_surface_by_namespace(&mut f, "panel");
    assert_eq!(
        pointer_focus(&mut f),
        Some(panel_server),
        "the panel sits over the cursor's spot, so it owns focus while alive"
    );

    // The click that hits the bar closes the panel: layer-shell role first,
    // wl_surface after (the teardown order a real panel uses).
    f.client(id).layer(&panel).layer_surface.destroy();
    f.double_roundtrip(id);
    assert_eq!(
        pointer_focus(&mut f),
        Some(bar_server.clone()),
        "focus must be re-seated on the bar beneath, not left on the dead \
         panel (still listed in the layer map until cleanup) and not dropped \
         to the canvas"
    );

    // The next press, still without pointer motion, must reach the bar again —
    // not pan the canvas (unbound BTN_LEFT on the empty canvas is PanViewport
    // in the default config). A press over a layer always installs a
    // ScreenSpaceClickGrab; a PanGrab is the stale-flag regression.
    press(&mut f, &device, BTN_LEFT);
    let (pan_grab, click_grab) = f
        .state()
        .seat
        .get_pointer()
        .unwrap()
        .with_grab(|_, g| {
            (
                g.is::<crate::grabs::PanGrab>(),
                g.is::<crate::grabs::ScreenSpaceClickGrab>(),
            )
        })
        .unwrap_or((false, false));
    assert_eq!(
        (pan_grab, click_grab),
        (false, true),
        "the press after the teardown must hit the bar, not pan the canvas"
    );
    release(&mut f, &device, BTN_LEFT);

    // Cleanup: the panel's wl_surface, then the bar's layer surface.
    f.client(id).layer(&panel).surface.destroy();
    f.client(id).layer(&bar).layer_surface.destroy();
    f.double_roundtrip(id);
}
