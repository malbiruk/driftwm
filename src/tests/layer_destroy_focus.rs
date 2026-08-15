//! Pointer-focus invariants when a layer appears or disappears under a
//! stationary cursor: `pointer_over_layer` is only refreshed by pointer
//! motion, so a layer destroyed (or revealed by a fullscreen exit) beneath a
//! resting cursor would route the next press/scroll to the canvas instead of
//! the layer surface under the pointer. The compositor re-seats focus on the
//! scene change itself, and the tests pin the press to the correct grab.

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

/// A panel layer dies over a bar while the cursor rests on the bar. The next
/// press — no pointer motion through the whole teardown — must reach the bar
/// again, not pan the canvas.
#[test]
fn layer_destroyed_under_the_cursor_keeps_pointer_focus() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    // The bar: full-width bottom strip with no exclusive zone.
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

    // A covering layer above the bar, over the cursor's spot: a panel or OSD
    // that opens anchored to the bar, and dies on the click that hits the bar.
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
    // wl_surface after (the teardown order a real client uses).
    f.client(id).layer(&panel).layer_surface.destroy();
    f.double_roundtrip(id);
    assert_eq!(
        pointer_focus(&mut f),
        Some(bar_server.clone()),
        "focus must be re-seated on the bar beneath, not left on the dead \
         panel and not dropped to the canvas"
    );

    // The next press, still without pointer motion, must reach the bar again —
    // not pan the canvas (unmodified BTN_LEFT on empty canvas is pan-viewport
    // in the default config). A press over a layer always installs a
    // ScreenSpaceClickGrab; a PanGrab is the stale-flag regression.
    press(&mut f, &device, BTN_LEFT);
    assert_click_grab(
        &mut f,
        "the press after the teardown must hit the bar, not pan the canvas",
    );
    release(&mut f, &device, BTN_LEFT);

    // Cleanup: the panel's wl_surface, then the bar's layer surface.
    f.client(id).layer(&panel).surface.destroy();
    f.client(id).layer(&bar).layer_surface.destroy();
    f.double_roundtrip(id);
}

/// A layer press delivers a `ScreenSpaceClickGrab`; a `PanGrab` means the
/// press fell through to the canvas. Pressed, not yet released.
fn assert_click_grab(f: &mut Fixture, why: &str) {
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
    assert_eq!((pan_grab, click_grab), (false, true), "{why}");
}

/// Exiting fullscreen can restore a hidden bar beneath a stationary cursor:
/// `enter_fullscreen` forced `pointer_over_layer` to false and nothing
/// refreshed it until the exit. The exit re-seats focus on the revealed bar.
#[test]
fn fullscreen_exit_reveals_a_bar_under_the_stationary_cursor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let device = FakeDevice::mouse();

    let window_surface = super::map_window(&mut f, id, "w", (800, 600));
    let output = f.state().active_output().unwrap();
    let window = super::window_by_app_id(&mut f, "w").unwrap();

    // The bar fullscreen hides and exit restores beneath the cursor.
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

    // Cursor over the bar's strip, then fullscreen over it.
    let over_bar = Point::from((960.0, 1060.0));
    pointer_to_screen(&mut f, &device, over_bar);
    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &window_surface);

    // The fullscreen window owns focus while it covers the bar.
    assert_eq!(
        pointer_focus(&mut f),
        Some(super::server_surface(&window)),
        "the fullscreen window owns focus while the bar is hidden beneath it"
    );

    // Exit with the cursor still over the bar's spot, no motion since.
    f.state().exit_fullscreen_on(&output);
    f.double_roundtrip(id);

    let bar_server = layer_surface_by_namespace(&mut f, "bar");
    assert_eq!(
        pointer_focus(&mut f),
        Some(bar_server),
        "exiting fullscreen must re-seat focus on the bar revealed beneath the cursor"
    );

    press(&mut f, &device, BTN_LEFT);
    assert_click_grab(
        &mut f,
        "the press must hit the bar, not the restored window",
    );
    release(&mut f, &device, BTN_LEFT);

    // Cleanup: the window, then the bar's layer surface.
    f.client(id).window(&window_surface).destroy();
    f.client(id).layer(&bar).layer_surface.destroy();
    f.double_roundtrip(id);
}
