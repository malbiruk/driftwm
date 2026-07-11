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
}

impl Server {
    pub fn new(config: (Config, Vec<String>)) -> Self {
        let event_loop = EventLoop::try_new().unwrap();
        let handle = event_loop.handle();
        let display = Display::<DriftWm>::new().unwrap();
        let dh = display.handle();

        let state = DriftWm::new_with_config(dh, handle.clone(), event_loop.get_signal(), config);

        // Register the wayland Display as a calloop source exactly like main.rs
        // so client requests dispatch on the server's own loop. No listening
        // socket, IPC, xwayland, or render timer — a test drives everything by
        // hand through explicit socket pairs and dispatch pumps.
        let source = Generic::new(display, Interest::READ, Mode::Level);
        handle
            .insert_source(source, |_, display, state: &mut DriftWm| {
                // SAFETY: the Display is never dropped while its Generic source
                // is alive; both live for the Server's lifetime. Mirrors main.rs.
                unsafe { display.get_mut() }.dispatch_clients(state).ok();
                Ok(PostAction::Continue)
            })
            .unwrap();

        Self { event_loop, state }
    }

    pub fn dispatch(&mut self) {
        self.event_loop
            .dispatch(Duration::ZERO, &mut self.state)
            .unwrap();

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
}
