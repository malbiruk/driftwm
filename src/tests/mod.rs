//! In-process compositor test harness.
//!
//! A real [`DriftWm`](crate::state::DriftWm) runs on its own headless calloop
//! loop with no backend (no renderer, no DRM, no sockets). Real wayland test
//! clients connect over socket pairs, and an outer calloop loop nests both the
//! server loop and every client loop by their epoll fds, so one
//! [`Fixture::dispatch`] pumps the whole graph deterministically.

mod client;
mod fixture;
mod headless;
mod server;

mod window_opening;

use fixture::Fixture;
