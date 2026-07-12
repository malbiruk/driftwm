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
mod server;

mod client_teardown;
mod config_reload;
mod configure_sequences;
mod focus_timing;
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
