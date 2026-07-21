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
mod headless;
mod real;
mod server;

mod auto_navigate_click;
mod camera_animation;
mod cli_docs;
mod client_teardown;
mod config_reload;
mod configure_sequences;
mod focus_timing;
mod hotplug;
mod opacity;
mod popups;
mod real_clients;
mod satellite_membership;
mod send_to_output;
mod soak;
mod window_opening;
mod window_rules;

use fixture::Fixture;

use driftwm::config::Config;
use driftwm::window_ext::WindowExt;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::seat::WaylandFocus;

fn config(toml: &str) -> Config {
    Config::from_toml(toml).unwrap()
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

/// Create an xdg popup on the toplevel backing `parent`, map it (attach a
/// buffer and ack), and settle. Returns the client-side popup surface.
fn map_popup(
    f: &mut Fixture,
    id: client::ClientId,
    parent: &wayland_client::protocol::wl_surface::WlSurface,
) -> wayland_client::protocol::wl_surface::WlSurface {
    let popup = f.client(id).create_popup(parent);
    let surface = popup.surface.clone();
    popup.commit();
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

/// Server-side window matching `app_id` (set client-side before first commit).
fn window_by_app_id(f: &mut Fixture, app_id: &str) -> Option<Window> {
    f.state()
        .stage
        .windows()
        .find(|w| w.app_id_or_class().as_deref() == Some(app_id))
        .cloned()
}

/// The server-side `WlSurface` backing a stage window.
fn server_surface(window: &Window) -> WlSurface {
    window.wl_surface().unwrap().into_owned()
}
