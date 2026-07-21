use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::Display;

use driftwm::config::Config;

use crate::state::{ClientState, DriftWm};

/// The real compositor wired onto its own calloop loop with no backend. A test
/// backend is deliberately absent: `DriftWm.backend` stays `None`, so every
/// render/IPC path that would touch a GPU or socket short-circuits, and only
/// pure protocol dispatch (configure, enter/leave, flush) drives the state.
pub struct Server {
    pub event_loop: EventLoop<'static, DriftWm>,
    pub state: DriftWm,
    display: Display<DriftWm>,
}

impl Server {
    pub fn new(config: (Config, Vec<String>)) -> Self {
        let event_loop = EventLoop::try_new().unwrap();
        let handle = event_loop.handle();
        let mut display = Display::<DriftWm>::new().unwrap();
        let dh = display.handle();

        let state = DriftWm::new_with_config(dh, handle.clone(), event_loop.get_signal(), config);

        // Wake-only source: a dup of the display's poll fd keeps pending client
        // data visible to the outer fixture loop (which nests this loop by fd),
        // but the actual dispatch happens unconditionally in `dispatch` below.
        let poll_fd = display.backend().poll_fd().try_clone_to_owned().unwrap();
        let source = Generic::new(poll_fd, Interest::READ, Mode::Level);
        handle
            .insert_source(source, |_, _, _: &mut DriftWm| Ok(PostAction::Continue))
            .unwrap();

        // No listening socket, IPC, xwayland, or render timer — a test drives
        // everything by hand through explicit socket pairs and dispatch pumps.
        Self {
            event_loop,
            state,
            display,
        }
    }

    pub fn dispatch(&mut self) {
        self.event_loop
            .dispatch(Duration::ZERO, &mut self.state)
            .unwrap();

        // Unlike main.rs, client requests are dispatched unconditionally per
        // pump rather than from a readiness-gated calloop source. The fixture
        // nests this loop's epoll fd inside the outer test loop, and readiness
        // propagation across the resulting epoll-in-epoll-in-epoll chain very
        // rarely misses a client disconnect, stranding its resources forever
        // (~1 in 100 full-suite runs). Unconditional dispatch is cheap and
        // makes teardown deterministic.
        self.display.dispatch_clients(&mut self.state).ok();

        // The per-iteration duties main.rs's run-closure performs, minus
        // rendering. `write_state_file_if_dirty` is intentionally not driven —
        // production only calls it from the render loops, and a test must
        // never write $XDG_RUNTIME_DIR/driftwm/state.
        self.state.refresh_and_flush_clients();

        // Every pump cross-checks stage/Space parity (debug builds only, which
        // includes test builds). A violation means some mutation bypassed the
        // stage wrappers.
        #[cfg(debug_assertions)]
        self.state.verify_stage_invariants();
    }

    pub fn insert_client(&mut self, stream: UnixStream) {
        self.state
            .display_handle
            .insert_client(stream, Arc::new(ClientState::default()))
            .unwrap();
    }

    /// Insert a client the compositor treats as its own xwayland-satellite.
    /// Pre-seeds `is_satellite`, standing in for the PID-credential resolution
    /// that runs against a real satellite process, so membership takes the
    /// single-sticky-output path without one.
    pub fn insert_satellite_client(&mut self, stream: UnixStream) {
        let is_satellite = std::sync::OnceLock::new();
        is_satellite.set(true).unwrap();
        let data = ClientState {
            is_satellite,
            ..Default::default()
        };
        self.state
            .display_handle
            .insert_client(stream, Arc::new(data))
            .unwrap();
    }
}
