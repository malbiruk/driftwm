//! In-process compositor test harness.
//!
//! A real [`DriftWm`](crate::state::DriftWm) runs on its own headless calloop
//! loop with no backend (no renderer, no DRM, no sockets). Real wayland test
//! clients connect over socket pairs, and an outer calloop loop nests both the
//! server loop and every client loop by their epoll fds, so one
//! [`Fixture::dispatch`] pumps the whole graph deterministically.
//!
//! Every scenario is leak-checked at teardown: [`Fixture`]'s `Drop` tears down
//! all clients and asserts `debug_counters` return to the construction-time
//! baseline (opt out with `Fixture::skip_baseline_check`).

mod client;
mod fixture;
mod gl;
mod headless;
mod input_backend;
mod real;
mod server;

mod auto_navigate_click;
mod auto_placement;
mod blur_pixels;
mod bookmarks;
mod camera_animation;
mod camera_zoom_ipc;
mod cli_docs;
mod client_teardown;
mod config_reload;
mod configure_sequences;
mod cycle_windows;
mod ext_workspace;
mod focus_placement;
mod focus_timing;
mod frame_space;
mod fullscreen_centering;
mod fullscreen_exit_cost;
mod fullscreen_handoff;
mod gesture_move;
mod gesture_resize;
mod hot_corners;
mod hotplug;
mod hover_focus;
mod input_dispatch;
mod interact_min;
mod layer_destroy_focus;
mod layer_frame_gating;
mod layer_popup_focus;
mod opacity;
mod outer_gap;
mod pass_mouse;
mod pin_ipc;
mod pinned_phantom;
mod pointer_constraints;
mod pointer_motion_dedup;
mod popups;
mod real_clients;
mod relaunch;
mod render_pixels;
mod resize_actions;
mod resize_ipc;
mod resize_parity;
mod send_to_output;
mod session_lock;
mod session_restore;
mod soak;
mod stand_in_parity;
mod suspend_flows;
mod suspended;
mod tablet;
mod trackpad_send_events;
mod translucent_fullscreen;
mod window_animation;
mod window_mode;
mod window_opening;
mod window_rules;
mod zoom_to_fit;

use fixture::Fixture;

use std::time::Duration;

use driftwm::config::Config;
use driftwm::window_ext::WindowExt;
use smithay::desktop::Window;
use smithay::input::pointer::MotionEvent;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::seat::WaylandFocus;
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

const TICK: Duration = Duration::from_millis(16);
const MAX_TICKS: usize = 600;

/// Run both viewport animations to completion, in the order a real frame loop
/// ticks them (zoom first, so the camera uses the recomputed target).
///
/// The per-frame fullscreen disarm is part of that order, not an optimization:
/// `set_zoom` refuses on a fullscreen output, so `apply_zoom_animation` can never
/// retire a `zoom_target` armed there and a loop without the disarm would spin to
/// the panic below instead of settling.
fn settle(f: &mut Fixture) {
    for _ in 0..MAX_TICKS {
        if f.state().camera_target().is_none() && f.state().zoom_target().is_none() {
            return;
        }
        if let Some(output) = f.state().active_output() {
            f.state().disarm_view_flight_on_fullscreen(&output);
        }
        f.state().apply_zoom_animation(TICK);
        f.state().apply_camera_animation(TICK);
    }
    panic!("viewport animation did not converge within {MAX_TICKS} ticks");
}

fn config(toml: &str) -> Config {
    Config::from_toml(toml).unwrap()
}

/// Drive real-time ticks until every window animation prunes, panicking on
/// non-convergence. Only valid for entries that actually settle (position-only,
/// resolved requests, open) — an entry holding an outstanding request would spin
/// to the panic, which is the point of `PAST_HOLD` elsewhere.
fn tick_until_settled(f: &mut Fixture) {
    ticks_to_settle(f);
}

/// As [`tick_until_settled`], returning how many ticks convergence took.
fn ticks_to_settle(f: &mut Fixture) -> usize {
    for n in 0..MAX_TICKS {
        if !f.state().window_animations.is_active() {
            return n;
        }
        f.state().tick_window_animations(TICK);
    }
    panic!("window animations did not converge within {MAX_TICKS} ticks");
}

/// Deliver one pointer motion at canvas-space `loc`, routed through whatever
/// grab is live.
fn motion(f: &mut Fixture, loc: Point<f64, Logical>) {
    let pointer = f.state().seat.get_pointer().unwrap();
    let event = MotionEvent {
        location: loc,
        serial: SERIAL_COUNTER.next_serial(),
        time: 0,
    };
    pointer.motion(f.state(), None, &event);
}

/// Tear the live grab down through its real `unset`, whether or not a physical
/// button installed it — a gesture drag has no button to release.
fn end_grab(f: &mut Fixture) {
    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    pointer.unset_grab(f.state(), serial, 0);
}

/// Map a toplevel with `app_id`, attach a buffer at `size`, and settle.
/// Returns the client-side surface for later lookups.
fn map_window(
    f: &mut Fixture,
    id: client::ClientId,
    app_id: &str,
    size: (u16, u16),
) -> wayland_client::protocol::wl_surface::WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id(app_id);
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

/// [`map_window`] with a real `wl_shm` texture instead of the 1x1 single-pixel
/// buffer: `logical_size` is the viewport destination (what the window measures
/// on the canvas), `texels` the buffer's own grid, and `pixels` its
/// premultiplied little-endian BGRA bytes.
fn map_window_shm(
    f: &mut Fixture,
    id: client::ClientId,
    app_id: &str,
    logical_size: (u16, u16),
    texels: (i32, i32),
    pixels: &[u8],
) -> wayland_client::protocol::wl_surface::WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id(app_id);
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.set_size(logical_size.0, logical_size.1);
    window.attach_shm_buffer(texels, pixels);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

/// [`map_window`] for a toplevel that declares size limits, so the compositor
/// has real client-side bounds to clamp a request against. A zero on either axis
/// of `min`/`max` means unconstrained, smithay's own convention.
fn map_window_with_limits(
    f: &mut Fixture,
    id: client::ClientId,
    app_id: &str,
    size: (u16, u16),
    min: (i32, i32),
    max: (i32, i32),
) -> wayland_client::protocol::wl_surface::WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id(app_id);
    window.set_min_size(min.0, min.1);
    window.set_max_size(max.0, max.1);
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

fn window_position(f: &mut Fixture, window: &Window) -> Point<i32, Logical> {
    f.state().stage.position_of(window).expect("staged")
}

fn configure_count(
    f: &mut Fixture,
    id: client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) -> usize {
    f.double_roundtrip(id);
    f.client(id).window(surface).configures_received.len()
}

/// The size the client was last asked for. Round-trips first, since the
/// configure is only queued when the compositor call returns.
fn last_configured(
    f: &mut Fixture,
    id: client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) -> (i32, i32) {
    f.double_roundtrip(id);
    f.client(id)
        .window(surface)
        .configures_received
        .last()
        .expect("the client was configured")
        .1
        .size
}

/// Adopt the size the compositor most recently configured: set it client-side,
/// attach a buffer, ack, and settle — the buffer-commit ritual a real client
/// runs to acknowledge a configure.
fn adopt_last_configure(
    f: &mut Fixture,
    id: client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) {
    let (w, h) = f
        .client(id)
        .window(surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    let window = f.client(id).window(surface);
    window.set_size(w as u16, h as u16);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
}

/// Ack the configure and commit without taking the size that came with it — a
/// fixed-size dialog's answer. smithay drops the pending configure at *ack*, so
/// afterwards the compositor sees no outstanding configure and a committed
/// geometry that never moved.
fn ack_but_keep_size(
    f: &mut Fixture,
    id: client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) {
    f.double_roundtrip(id);
    let window = f.client(id).window(surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
}

/// Create an xdg popup on the toplevel backing `parent`, map it (attach a
/// buffer and ack), and settle. Returns the client-side popup surface.
fn map_popup(
    f: &mut Fixture,
    id: client::ClientId,
    parent: &wayland_client::protocol::wl_surface::WlSurface,
) -> wayland_client::protocol::wl_surface::WlSurface {
    map_popup_with(f, id, parent, client::PopupProps::default())
}

/// [`map_popup`] with a custom positioner (see [`client::PopupProps`]).
fn map_popup_with(
    f: &mut Fixture,
    id: client::ClientId,
    parent: &wayland_client::protocol::wl_surface::WlSurface,
    props: client::PopupProps,
) -> wayland_client::protocol::wl_surface::WlSurface {
    let surface = f
        .client(id)
        .create_popup_with(parent, props)
        .surface
        .clone();
    settle_popup(f, id, surface)
}

/// [`map_popup_with`] for a popup parented to a layer surface rather than a
/// toplevel (`zwlr_layer_surface_v1.get_popup`).
fn map_layer_popup_with(
    f: &mut Fixture,
    id: client::ClientId,
    parent: &wayland_client::protocol::wl_surface::WlSurface,
    props: client::PopupProps,
) -> wayland_client::protocol::wl_surface::WlSurface {
    let surface = f
        .client(id)
        .create_layer_popup_with(parent, props)
        .surface
        .clone();
    settle_popup(f, id, surface)
}

/// Drive a freshly created popup through its map ritual: initial commit,
/// buffer, ack, settle.
fn settle_popup(
    f: &mut Fixture,
    id: client::ClientId,
    surface: wayland_client::protocol::wl_surface::WlSurface,
) -> wayland_client::protocol::wl_surface::WlSurface {
    f.client(id).popup(&surface).commit();
    f.roundtrip(id);

    let popup = f.client(id).popup(&surface);
    popup.attach_new_buffer();
    popup.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

/// Number of popups the compositor tracks against `root` (a server-side
/// toplevel surface).
fn popups_tracked_on(root: &WlSurface) -> usize {
    smithay::desktop::PopupManager::popups_for_surface(root).count()
}

/// Server-side surface of the first popup tracked against `root`, captured
/// while the parent is still alive so it can be looked up after teardown.
fn first_popup_surface(root: &WlSurface) -> Option<WlSurface> {
    smithay::desktop::PopupManager::popups_for_surface(root)
        .next()
        .map(|(kind, _)| kind.wl_surface().clone())
}

/// Server-side surface that currently holds keyboard focus, if any.
fn keyboard_focus(f: &mut Fixture) -> Option<WlSurface> {
    f.state()
        .seat
        .get_keyboard()
        .unwrap()
        .current_focus()
        .map(|t| t.0)
}

/// Server-side surface that currently holds pointer focus, if any.
fn pointer_focus(f: &mut Fixture) -> Option<WlSurface> {
    f.state()
        .seat
        .get_pointer()
        .unwrap()
        .current_focus()
        .map(|t| t.0)
}

/// Server-side window matching `app_id` (set client-side before first commit).
fn window_by_app_id(f: &mut Fixture, app_id: &str) -> Option<Window> {
    f.state()
        .stage
        .windows()
        .find(|w| w.app_id_or_class().as_deref() == Some(app_id))
        .and_then(|w| w.client())
        .cloned()
}

/// The server-side `WlSurface` backing a stage window.
fn server_surface(window: &Window) -> WlSurface {
    window.wl_surface().unwrap().into_owned()
}

/// Register an SSD title bar for `window`, exactly as the compositor does on the
/// first sized commit under `default_mode = "server"`. The headless test client
/// never binds xdg-decoration, so that commit path doesn't run — do it directly
/// so the hit-tests report the window's chrome bands.
fn give_ssd(f: &mut Fixture, window: &Window) {
    use smithay::reexports::wayland_server::Resource;
    let width = window.geometry().size.w;
    let id = server_surface(window).id();
    let deco =
        crate::decorations::WindowDecoration::new(width, true, &f.state().config.decorations);
    f.state()
        .decorations
        .insert(crate::decorations::DecorationKey::Surface(id), deco);
}

/// Whether the client's most recent configure carried `Maximized` — what a
/// client's own restore button keys off. Shared by every resize arm's test, so
/// the check that keeps the arms in sync isn't itself duplicated.
fn client_sees_maximized(
    f: &mut Fixture,
    id: client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) -> bool {
    f.client(id)
        .window(surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .states
        .contains(&wayland_protocols::xdg::shell::client::xdg_toplevel::State::Maximized)
}

/// Fit `window`, then snap the viewport onto it: the fit only *animates* the
/// camera, while a resize grab clamps the pointer to the output, so an
/// un-settled camera would swallow the drag. Returns a grab point 10px inside
/// the window's right edge — the client hasn't acked the fit size yet, so the
/// rect to aim at is still the pre-fit one.
fn fit_and_frame(
    f: &mut Fixture,
    window: &Window,
    id: client::ClientId,
) -> smithay::utils::Point<f64, smithay::utils::Logical> {
    use crate::state::StageWindow;
    use smithay::utils::Point;

    f.state().fit_window(window);
    let loc = f
        .state()
        .stage
        .position_of(&StageWindow::Client(window.clone()))
        .unwrap();
    f.state().with_output_state(|os| {
        os.camera = Point::from((loc.x as f64, loc.y as f64));
        os.camera_target = None;
        os.zoom = 1.0;
        os.zoom_target = None;
    });
    f.double_roundtrip(id);
    let size = window.geometry().size;
    Point::from((
        loc.x as f64 + size.w as f64 - 10.0,
        loc.y as f64 + size.h as f64 / 2.0,
    ))
}

/// Put `window` into the fit and fill membership a resize entry has to clear,
/// plus the client-visible `Maximized` a fit sets — directly, without the camera
/// move and reposition a real fit action makes, which would shift the very
/// anchors [`assert_resize_entered`] checks.
///
/// The seeded state violates the fit/fill exclusivity `window_inventory`
/// debug-asserts, so a test must not read the inventory (a `state` IPC request
/// or `window_inventory()`) between seeding and the clearing entry point.
fn seed_fit_and_fill(f: &mut Fixture, window: &Window) {
    use driftwm::layout::snap::SnapRect;
    use driftwm::stage::FillSaved;
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

    let loc = f.state().stage.position_of(window).expect("staged");
    let size = window.geometry().size;
    f.state().stage.set_fit(window, size);
    // The viewport rect and its output only matter to the fill-view restore,
    // which no resize entry point reads — a non-degenerate stand-in, with the
    // window seeded where it already is.
    let bounds = SnapRect {
        x_low: 0.0,
        x_high: 1920.0,
        y_low: 0.0,
        y_high: 1080.0,
    };
    let output = f.state().active_output().expect("output").name();
    f.state().stage.set_fill(
        window,
        FillSaved {
            pre_fill_position: loc,
            pre_fill_size: size,
            viewport_bounds: bounds,
            viewport_output: output,
            filled_at: loc,
        },
    );
    window
        .toplevel()
        .expect("toplevel")
        .with_pending_state(|s| s.states.set(xdg_toplevel::State::Maximized));
}

/// Assert the whole invariant a resize entry point establishes: fit and fill
/// membership gone, the `ResizeState` `handle_resize_commit` repositions from
/// seeded field for field, and the toplevel told it is resizing and no longer
/// maximized. Shared by all four entry points, so none can quietly drop a piece
/// of it. `screen_pos` is `Some` exactly for a screen-pinned resize.
fn assert_resize_entered(
    f: &mut Fixture,
    window: &Window,
    edges: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
    size: smithay::utils::Size<i32, smithay::utils::Logical>,
    screen_pos: Option<smithay::utils::Point<i32, smithay::utils::Logical>>,
) {
    use crate::grabs::ResizeState;
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
    use smithay::wayland::compositor::with_states;
    use std::cell::RefCell;

    assert_eq!(
        f.state().stage.fit_saved_size(window),
        None,
        "the resize entry took the window out of fit"
    );
    assert!(
        !f.state().stage.is_fill(window),
        "the resize entry took the window out of fill"
    );

    let state = with_states(&server_surface(window), |states| {
        *states
            .data_map
            .get::<RefCell<ResizeState>>()
            .expect("the resize entry seeded a ResizeState")
            .borrow()
    });
    let ResizeState::Resizing {
        edges: got_edges,
        initial_screen_pos,
        last_committed_size,
    } = state
    else {
        panic!("the resize entry left the surface Resizing");
    };
    assert_eq!(got_edges, edges, "the seeded edge");
    assert_eq!(
        initial_screen_pos, screen_pos,
        "the seeded screen anchor — `Some` only for a pinned resize"
    );
    assert_eq!(
        last_committed_size, size,
        "the settle starts from the size the window already has"
    );

    let toplevel = window.toplevel().expect("toplevel");
    assert!(
        toplevel.with_pending_state(|s| s.states.contains(xdg_toplevel::State::Resizing)),
        "the client was told it is resizing"
    );
    assert!(
        !toplevel.with_pending_state(|s| s.states.contains(xdg_toplevel::State::Maximized)),
        "the fit clear was mirrored to the client, or its restore button is dead"
    );
}

/// Install a live client [`ResizeGrab`] over `window`, entering the resize
/// through the same `begin_client_resize` the real entry points run so
/// `handle_resize_commit` runs its reposition/settle logic. `start` is the
/// canvas-space grab origin; the size delta is measured from there.
fn install_client_resize_grab(
    f: &mut Fixture,
    window: &Window,
    edges: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
    start: Point<f64, Logical>,
    output: smithay::output::Output,
    cluster: crate::state::ClusterResizeSnapshot,
) {
    use crate::grabs::{ResizeGrab, SizeConstraints, resize_screen_anchor};
    use crate::state::{ClusterMember, StageWindow};
    use driftwm::layout::snap::SnapState;
    use smithay::input::pointer::{Focus, GrabStartData};

    let initial_window_location = f
        .state()
        .stage
        .position_of(&StageWindow::Client(window.clone()))
        .unwrap();
    let initial_window_size = window.geometry().size;

    let surface = server_surface(window);
    f.state()
        .begin_client_resize(window, &surface, edges, initial_window_size, None);

    let (start_screen, start_zoom) = resize_screen_anchor(&output, start);
    let grab = ResizeGrab {
        start_data: GrabStartData {
            focus: None,
            button: driftwm::config::BTN_LEFT,
            location: start,
        },
        target: ClusterMember::Client(window.clone()),
        edges,
        initial_window_location,
        initial_window_size,
        last_window_size: initial_window_size,
        output,
        start_screen,
        start_zoom,
        last_clamped_location: start,
        snap: SnapState::default(),
        constraints: SizeConstraints::for_window(window),
        cluster_resize: cluster,
        pinned_initial_screen_pos: None,
        touch_start: None,
        touch_slots: 0,
        locked_ratio: None,
    };

    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    pointer.set_grab(f.state(), grab, serial, Focus::Clear);
}

/// Whether `window`'s toplevel currently carries the xdg `Activated` state
/// (the "focused window" chrome hint the compositor sets exclusively).
fn is_activated(window: &Window) -> bool {
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
    window
        .toplevel()
        .expect("toplevel")
        .with_pending_state(|s| s.states.contains(xdg_toplevel::State::Activated))
}

/// Map a top-anchored panel claiming an exclusive zone, shrinking the output's
/// usable area for real — the layer commit arranges the map, so
/// `non_exclusive_zone` actually changes (an output mode change alone leaves it
/// stale).
fn map_top_panel(f: &mut Fixture, id: client::ClientId, height: u32) {
    let created = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Top, "panel");
    let surface = created.surface.clone();
    created.set_configure_props(client::LayerConfigureProps {
        size: Some((1920, height)),
        anchor: Some(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        ),
        exclusive_zone: Some(height as i32),
        ..Default::default()
    });
    created.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&surface);
    layer.set_size(1920, height as u16);
    layer.attach_new_buffer();
    layer.ack_last_and_commit();
    f.double_roundtrip(id);
}

/// Map a Top layer with `namespace` at `size` and settle. A `None` anchor
/// centers it on the output. Returns the client-side surface.
fn map_top_layer(
    f: &mut Fixture,
    id: client::ClientId,
    namespace: &str,
    size: (u32, u32),
    anchor: Option<zwlr_layer_surface_v1::Anchor>,
) -> wayland_client::protocol::wl_surface::WlSurface {
    let created = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Top, namespace);
    let surface = created.surface.clone();
    created.set_configure_props(client::LayerConfigureProps {
        size: Some(size),
        anchor,
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
