//! `post_render` gates layer-map frame callbacks on the renderer's own cull
//! predicates (`renders_lock_frame`, `is_output_visually_fullscreen` sparing
//! only Overlay) instead of sending them unconditionally, throttling to
//! `FRAME_CALLBACK_THROTTLE`. `send_frame_callbacks_fallback` walks the same
//! layer maps, which is what keeps a culled panel throttling instead of
//! freezing.
//!
//! Every test opens with a warm-up `post_render` call: smithay's per-surface
//! throttle state starts `None`, so the first call always delivers regardless
//! of gating — without it, a "silence" assertion would pass for the wrong
//! reason. Warm-ups sit right before the timed assertion since the throttle
//! window is real wall-clock time.
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use smithay::output::Output;
use smithay::utils::Point;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1;

use crate::state::SessionLock;

use super::client::{ClientId, LayerConfigureProps, SyncData};
use super::{Fixture, adopt_last_configure, map_window, tick_until_settled};

/// Map a layer surface on `layer`, give it a buffer, and settle. Returns the
/// client-side surface.
fn map_layer(
    f: &mut Fixture,
    id: ClientId,
    output: Option<&WlOutput>,
    layer: zwlr_layer_shell_v1::Layer,
    namespace: &str,
) -> WlSurface {
    let created = f.client(id).create_layer(output, layer, namespace);
    let surface = created.surface.clone();
    created.set_configure_props(LayerConfigureProps {
        size: Some((200, 40)),
        ..Default::default()
    });
    created.commit();
    f.roundtrip(id);

    let l = f.client(id).layer(&surface);
    l.set_size(200, 40);
    l.attach_new_buffer();
    l.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

/// Request `wl_surface.frame()` on `surface` and roundtrip, so the commit
/// carrying the request has landed server-side before the caller renders.
fn request_frame(f: &mut Fixture, id: ClientId, surface: &WlSurface) -> Arc<SyncData> {
    let data = f.client(id).frame(surface);
    f.roundtrip(id);
    data
}

/// Whether the compositor has delivered the `wl_callback::Event::Done` for a
/// [`request_frame`] request.
fn delivered(data: &Arc<SyncData>) -> bool {
    data.done.load(Ordering::Relaxed)
}

/// Lock the session and confirm it on `output`, landing in `SessionLock::Locked`.
fn lock_and_confirm(f: &mut Fixture, id: ClientId, output: &Output) {
    f.client(id).lock_session();
    f.roundtrip(id);

    let wl_output = f.client(id).output(&output.name());
    let surface = f.client(id).create_lock_surface(&wl_output).surface.clone();
    f.roundtrip(id);

    let lock_surface = f.client(id).lock_surface(&surface);
    let (w, h) = lock_surface.configures_received.last().unwrap().1;
    lock_surface.set_size(w, h);
    lock_surface.attach_new_buffer();
    lock_surface.ack_last_and_commit();
    f.double_roundtrip(id);
}

/// Drive a window to fullscreen and settle the entry animation, so
/// `is_output_visually_fullscreen` reports true (not just `is_output_fullscreen`,
/// which flips the instant the action runs, before the client's redraw lands).
fn settle_fullscreen(f: &mut Fixture, id: ClientId, surface: &WlSurface, output: &Output) {
    f.client(id).window(surface).set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(f, id, surface);
    tick_until_settled(f);
    assert!(f.state().is_output_visually_fullscreen(output));
}

/// Put a settled, visually-fullscreen window on `output`.
fn fullscreen_window_on(f: &mut Fixture, output: &Output) {
    let id = f.add_client();
    let surface = map_window(f, id, "fs", (800, 600));
    settle_fullscreen(f, id, &surface, output);
}

#[test]
fn an_ordinary_layer_surface_gets_a_callback_on_every_post_render() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let bar = map_layer(&mut f, id, None, zwlr_layer_shell_v1::Layer::Top, "bar");

    for _ in 0..3 {
        let frame = request_frame(&mut f, id, &bar);
        crate::render::post_render(f.state(), &output);
        f.roundtrip(id);
        assert!(
            delivered(&frame),
            "an on-screen, unculled layer surface is serviced every render"
        );
    }
}

/// Pins the gate to the renderer's per-layer cull — only Overlay survives a
/// visually-fullscreen output — rather than a blanket "fullscreen => silence"
/// that would also starve the Overlay layer.
#[test]
fn fullscreen_throttles_lower_layers_but_spares_overlay() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let layer_id = f.add_client();
    let top = map_layer(
        &mut f,
        layer_id,
        None,
        zwlr_layer_shell_v1::Layer::Top,
        "top-bar",
    );
    let background = map_layer(
        &mut f,
        layer_id,
        None,
        zwlr_layer_shell_v1::Layer::Background,
        "wallpaper",
    );
    let overlay = map_layer(
        &mut f,
        layer_id,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "osd",
    );

    fullscreen_window_on(&mut f, &output);
    crate::render::post_render(f.state(), &output); // warm-up

    let top_frame = request_frame(&mut f, layer_id, &top);
    let background_frame = request_frame(&mut f, layer_id, &background);
    let overlay_frame = request_frame(&mut f, layer_id, &overlay);
    crate::render::post_render(f.state(), &output);
    f.roundtrip(layer_id);

    assert!(
        !delivered(&top_frame),
        "a Top layer is culled under a visually-fullscreen output"
    );
    assert!(
        !delivered(&background_frame),
        "Background shares Top's cull — everything below Overlay goes"
    );
    assert!(
        delivered(&overlay_frame),
        "Overlay survives the fullscreen cull and keeps its callback"
    );
}

/// The fullscreen cull is per-output: only the fullscreen output's layers go
/// quiet, not every layer surface the compositor knows about.
#[test]
fn a_fullscreen_output_leaves_another_output_layers_alone() {
    let mut f = Fixture::new();
    let output_a = f.add_output(1, (1920, 1080));
    let output_b = f.add_output(2, (1280, 720));

    let layer_id = f.add_client();
    f.double_roundtrip(layer_id); // the wl_output names arrive after the bind
    let wl_b = f.client(layer_id).output(&output_b.name());
    let bar = map_layer(
        &mut f,
        layer_id,
        Some(&wl_b),
        zwlr_layer_shell_v1::Layer::Top,
        "bar",
    );

    fullscreen_window_on(&mut f, &output_a);
    assert!(
        !f.state().is_output_visually_fullscreen(&output_b),
        "precondition: only output A went fullscreen"
    );
    crate::render::post_render(f.state(), &output_b); // warm-up

    let frame = request_frame(&mut f, layer_id, &bar);
    crate::render::post_render(f.state(), &output_b);
    f.roundtrip(layer_id);

    assert!(
        delivered(&frame),
        "a layer surface on a non-fullscreen output is still drawn, so still serviced"
    );
}

#[test]
fn locked_session_silences_every_layer_including_overlay() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let layer_id = f.add_client();
    let top = map_layer(
        &mut f,
        layer_id,
        None,
        zwlr_layer_shell_v1::Layer::Top,
        "top-bar",
    );
    let overlay = map_layer(
        &mut f,
        layer_id,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "osd",
    );

    let lock_id = f.add_client();
    lock_and_confirm(&mut f, lock_id, &output);
    assert!(
        matches!(f.state().session_lock, SessionLock::Locked { .. }),
        "precondition: the session reached Locked"
    );
    crate::render::post_render(f.state(), &output); // warm-up

    let top_frame = request_frame(&mut f, layer_id, &top);
    let overlay_frame = request_frame(&mut f, layer_id, &overlay);
    crate::render::post_render(f.state(), &output);
    f.roundtrip(layer_id);

    assert!(!delivered(&top_frame), "no layer draws under a lock frame");
    assert!(
        !delivered(&overlay_frame),
        "the lock cull is total, unlike the fullscreen one"
    );
}

/// `lock_session()` alone lands in `Pending { keep_lock_frames: false }`,
/// where the desktop is still composed — a blanket "locked => silence" gate
/// would wrongly starve layer surfaces here.
#[test]
fn pending_lock_without_keep_frames_still_delivers_layer_callbacks() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let layer_id = f.add_client();
    let top = map_layer(
        &mut f,
        layer_id,
        None,
        zwlr_layer_shell_v1::Layer::Top,
        "top-bar",
    );

    let lock_id = f.add_client();
    f.client(lock_id).lock_session();
    f.roundtrip(lock_id);
    assert!(
        matches!(
            f.state().session_lock,
            SessionLock::Pending {
                keep_lock_frames: false,
                ..
            }
        ),
        "precondition: lock_session alone lands in Pending without keep_lock_frames"
    );
    crate::render::post_render(f.state(), &output); // warm-up

    let top_frame = request_frame(&mut f, layer_id, &top);
    crate::render::post_render(f.state(), &output);
    f.roundtrip(layer_id);

    assert!(
        delivered(&top_frame),
        "Pending without keep_lock_frames still composes (and renders) the desktop"
    );
}

/// `post_render`'s own throttle (`Some(FRAME_CALLBACK_THROTTLE)`, not
/// `Duration::ZERO`) is what lets a culled surface catch up once overdue,
/// as long as the render loop keeps calling `post_render` at all.
#[test]
fn culled_layer_gets_a_callback_once_the_throttle_elapses() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let layer_id = f.add_client();
    let top = map_layer(
        &mut f,
        layer_id,
        None,
        zwlr_layer_shell_v1::Layer::Top,
        "top-bar",
    );

    fullscreen_window_on(&mut f, &output);
    crate::render::post_render(f.state(), &output); // warm-up

    let top_frame = request_frame(&mut f, layer_id, &top);
    crate::render::post_render(f.state(), &output);
    f.roundtrip(layer_id);
    assert!(
        !delivered(&top_frame),
        "precondition: still culled within the throttle window"
    );

    f.state().start_time -= Duration::from_millis(1000);
    crate::render::post_render(f.state(), &output);
    f.roundtrip(layer_id);
    assert!(
        delivered(&top_frame),
        "the culled surface catches up once its callback is overdue"
    );
}

/// Regression guard: without this fallback, a culled panel stops committing,
/// nothing else marks its output dirty, and `post_render` never runs again to
/// service it.
#[test]
fn idle_fallback_alone_delivers_to_a_culled_layer_surface() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let layer_id = f.add_client();
    let bar = map_layer(
        &mut f,
        layer_id,
        None,
        zwlr_layer_shell_v1::Layer::Top,
        "bar",
    );

    fullscreen_window_on(&mut f, &output);
    crate::render::post_render(f.state(), &output); // warm-up

    let frame = request_frame(&mut f, layer_id, &bar);
    crate::render::post_render(f.state(), &output);
    f.roundtrip(layer_id);
    assert!(
        !delivered(&frame),
        "precondition: post_render alone leaves a culled panel silent"
    );

    f.state().start_time -= Duration::from_millis(1000);
    crate::render::send_frame_callbacks_fallback(f.state());
    f.roundtrip(layer_id);
    assert!(
        delivered(&frame),
        "the idle heartbeat must reach layer maps, and must not re-apply the cull \
         that made the panel need it"
    );
}

/// The fallback shares FRAME_CALLBACK_THROTTLE with `post_render`, so a surface
/// serviced by a live render loop is skipped rather than serviced twice.
#[test]
fn idle_fallback_dedups_against_a_recent_post_render() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let bar = map_layer(&mut f, id, None, zwlr_layer_shell_v1::Layer::Top, "bar");

    crate::render::post_render(f.state(), &output); // warm-up
    let frame = request_frame(&mut f, id, &bar);
    crate::render::send_frame_callbacks_fallback(f.state());
    f.roundtrip(id);
    assert!(
        !delivered(&frame),
        "the render loop already stamped this surface inside the throttle window"
    );

    f.state().start_time -= Duration::from_millis(1000);
    crate::render::send_frame_callbacks_fallback(f.state());
    f.roundtrip(id);
    assert!(
        delivered(&frame),
        "once the stamp is stale the fallback services it"
    );
}

/// A blanked output costs zero wakeups: the fallback skips it the same way the
/// udev render loop does, so the heartbeat never wakes a dark screen.
#[test]
fn idle_fallback_skips_a_dpms_off_output() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let bar = map_layer(&mut f, id, None, zwlr_layer_shell_v1::Layer::Top, "bar");

    crate::render::post_render(f.state(), &output); // warm-up
    f.state().dpms_off_outputs.insert(output.clone());

    let frame = request_frame(&mut f, id, &bar);
    f.state().start_time -= Duration::from_millis(1000);
    crate::render::send_frame_callbacks_fallback(f.state());
    f.roundtrip(id);

    assert!(
        !delivered(&frame),
        "a layer surface on a DPMS-off output is not worth a wakeup, overdue or not"
    );
}

/// Unplugging the last output leaves only a virtual placeholder, which has no
/// DRM surface: nothing composites, so nothing ever releases what clients draw.
/// Servicing the heartbeat against it makes every client redraw once a second
/// into buffers that accumulate until VRAM is exhausted and the driver spills
/// to system memory.
#[test]
fn idle_fallback_goes_silent_while_only_placeholders_remain() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let bar = map_layer(&mut f, id, None, zwlr_layer_shell_v1::Layer::Top, "bar");
    let window = map_window(&mut f, id, "app", (400, 300));

    crate::render::post_render(f.state(), &output); // warm-up

    f.remove_output(&output);
    assert!(f.state().disconnected_outputs.contains("HEADLESS-1"));

    let bar_frame = request_frame(&mut f, id, &bar);
    let window_frame = request_frame(&mut f, id, &window);
    f.state().start_time -= Duration::from_millis(1000);
    crate::render::send_frame_callbacks_fallback(f.state());
    f.roundtrip(id);

    assert!(
        !delivered(&bar_frame),
        "a layer surface on a placeholder output must not be serviced"
    );
    assert!(
        !delivered(&window_frame),
        "a window must not be driven to redraw while no output can composite it"
    );

    // Reconnecting retires the placeholder, and the heartbeat comes back.
    f.add_output(2, (1920, 1080));
    f.state().start_time -= Duration::from_millis(1000);
    crate::render::send_frame_callbacks_fallback(f.state());
    f.roundtrip(id);

    assert!(
        delivered(&window_frame),
        "a live output must resume servicing the heartbeat"
    );
}

/// Regression guard for the toplevel loop in `post_render`: windows still gate
/// purely on canvas-visibility, not on the lock/fullscreen cull predicates.
#[test]
fn a_window_still_follows_the_on_screen_off_screen_geometry_rule() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "win", (800, 600));

    let on_screen_frame = request_frame(&mut f, id, &surface);
    crate::render::post_render(f.state(), &output);
    f.roundtrip(id);
    assert!(
        delivered(&on_screen_frame),
        "precondition: the window starts on-screen"
    );

    f.state().set_camera(Point::from((100_000.0, 100_000.0)));
    f.state().update_output_from_camera();

    let off_screen_frame = request_frame(&mut f, id, &surface);
    crate::render::post_render(f.state(), &output);
    f.roundtrip(id);
    assert!(
        !delivered(&off_screen_frame),
        "panning the window off-screen throttles it"
    );
}
