use std::collections::{BTreeMap, HashMap};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

use smithay::output::Output;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};
use smithay::reexports::wayland_server::backend::GlobalId;

use driftwm::config::Config;

use super::client::{Client, ClientId};
use super::headless;
use super::server::Server;
use crate::state::DriftWm;

/// Drives the whole graph: an outer calloop loop that nests the server's loop
/// and every client's loop by their epoll fds. Reading a nested loop's fd means
/// "it has work", so its callback pumps it once — a single [`Fixture::dispatch`]
/// therefore fans out to whichever side has pending events.
pub struct Fixture {
    pub event_loop: EventLoop<'static, FixtureState>,
    pub handle: LoopHandle<'static, FixtureState>,
    pub state: FixtureState,
    /// Outer-loop source token per client, so `kill_client` can unregister the
    /// nested client loop before dropping the client (whose callback would
    /// otherwise fire against a missing client).
    client_tokens: HashMap<ClientId, RegistrationToken>,
    /// `wl_output` global per output name, stashed by `add_output` so
    /// `remove_output` can disable then remove it like the udev backend does.
    output_globals: HashMap<String, GlobalId>,
    /// Counter snapshot taken at construction; `Drop` asserts every counter
    /// returns here once the clients are torn down.
    baseline: BTreeMap<String, usize>,
    /// Opt out of the drop-time baseline check for a scenario that legitimately
    /// ends in a non-baseline state.
    skip_baseline: bool,
}

pub struct FixtureState {
    pub server: Server,
    pub clients: Vec<Client>,
}

impl Fixture {
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    pub fn with_config(config: Config) -> Self {
        let event_loop = EventLoop::try_new().unwrap();
        let handle = event_loop.handle();

        let server = Server::new((config, Vec::new()));
        // Level-triggered so any events still queued after one pump keep the fd
        // readable and get drained on the next outer dispatch.
        let fd = server.event_loop.as_fd().try_clone_to_owned().unwrap();
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        handle
            .insert_source(source, |_, _, state: &mut FixtureState| {
                state.server.dispatch();
                Ok(PostAction::Continue)
            })
            .unwrap();

        let state = FixtureState {
            server,
            clients: Vec::new(),
        };

        let baseline = state.server.state.debug_counters();

        Self {
            event_loop,
            handle,
            state,
            client_tokens: HashMap::new(),
            output_globals: HashMap::new(),
            baseline,
            skip_baseline: false,
        }
    }

    pub fn dispatch(&mut self) {
        self.event_loop
            .dispatch(Duration::ZERO, &mut self.state)
            .unwrap();
    }

    pub fn state(&mut self) -> &mut DriftWm {
        &mut self.state.server.state
    }

    /// Current counter snapshot — for a scenario that wants to assert its own
    /// intermediate baseline.
    pub fn counters(&mut self) -> BTreeMap<String, usize> {
        self.state().debug_counters()
    }

    /// Pump until the counters return to `target`, bounded so a genuine leak
    /// leaves the caller's following assertion to fail instead of spinning
    /// forever. The disconnect cascade settles over several dispatch rounds,
    /// so pump one round at a time and re-check. A burst of zero-timeout pumps
    /// completes in microseconds, which is useless against work that lands a
    /// moment later from another thread (a fixture shares its process with
    /// detached threads and 190+ sibling tests) — so after the burst, fall
    /// back to sleep-then-pump until a wall-clock deadline.
    pub fn settle_to(&mut self, target: &BTreeMap<String, usize>) {
        for _ in 0..200 {
            if self.state().debug_counters() == *target {
                return;
            }
            self.pump(1);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if self.state().debug_counters() == *target {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
            self.pump(1);
        }
    }

    /// Opt this fixture out of the drop-time baseline assertion, for a scenario
    /// that legitimately ends off-baseline.
    pub fn skip_baseline_check(&mut self) {
        self.skip_baseline = true;
    }

    pub fn add_output(&mut self, n: u8, size: (u16, u16)) -> Output {
        let (output, global) = headless::add_output(self.state(), n, size);
        self.output_globals.insert(output.name(), global);
        output
    }

    /// Disconnect an output the way the udev backend does: run the backend-
    /// independent disconnect policy, then disable and remove its `wl_output`
    /// global. udev delays the removal 10s for in-flight binds; the fixture pumps
    /// a few rounds between disable and remove so clients observe the disable
    /// deterministically.
    pub fn remove_output(&mut self, output: &Output) {
        let is_last = self.state().space.outputs().count() == 1;
        let dh = self.state().display_handle.clone();
        self.state().output_disconnected(output, is_last);
        let global = self.output_globals.remove(&output.name()).unwrap();
        dh.disable_global::<DriftWm>(global.clone());
        self.pump(3);
        dh.remove_global::<DriftWm>(global);
        self.pump(3);
    }

    pub fn add_client(&mut self) -> ClientId {
        self.add_client_inner(false)
    }

    /// Add a client the compositor treats as its own xwayland-satellite: its
    /// windows get a single sticky output membership instead of full overlap.
    pub fn add_satellite_client(&mut self) -> ClientId {
        self.add_client_inner(true)
    }

    fn add_client_inner(&mut self, satellite: bool) -> ClientId {
        let (sock1, sock2) = UnixStream::pair().unwrap();
        if satellite {
            self.state.server.insert_satellite_client(sock1);
        } else {
            self.state.server.insert_client(sock1);
        }

        let client = Client::new(sock2);
        let id = client.id;

        let fd = client.event_loop.as_fd().try_clone_to_owned().unwrap();
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        let token = self
            .handle
            .insert_source(source, move |_, _, state: &mut FixtureState| {
                state.client(id).dispatch();
                Ok(PostAction::Continue)
            })
            .unwrap();
        self.client_tokens.insert(id, token);

        self.state.clients.push(client);
        self.roundtrip(id);
        id
    }

    /// Simulate abrupt client death: unregister the client's nested loop from
    /// the outer loop, then drop the client so its socket closes and the server
    /// observes the disconnect. Does not settle — the caller pumps and asserts.
    pub fn kill_client(&mut self, id: ClientId) {
        if let Some(token) = self.client_tokens.remove(&id) {
            self.handle.remove(token);
        }
        self.state.clients.retain(|c| c.id != id);
    }

    /// Dispatch the server loop `rounds` times with no client to roundtrip
    /// against. Used after `kill_client` to let the server process the socket
    /// close (destroy handlers, `retain_alive`, invariant check) to completion.
    pub fn pump(&mut self, rounds: usize) {
        for _ in 0..rounds {
            self.state.server.dispatch();
        }
    }

    pub fn client(&mut self, id: ClientId) -> &mut Client {
        self.state.client(id)
    }

    pub fn roundtrip(&mut self, id: ClientId) {
        let client = self.state.client(id);
        let data = client.send_sync();
        while !data.done.load(Ordering::Relaxed) {
            self.dispatch();
            // Also pump both endpoints directly: progress must never depend on
            // readiness propagating through the nested epoll chain, which can
            // (rarely) miss an edge.
            self.state.server.dispatch();
            self.state.client(id).dispatch();
        }
    }

    /// Roundtrip twice in a row. Configures are emitted from the server loop's
    /// commit callback, so they can trail the sync `done` and miss the client
    /// dispatch that observed `done`; a second roundtrip picks them up.
    pub fn double_roundtrip(&mut self, id: ClientId) {
        self.roundtrip(id);
        self.roundtrip(id);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A panicking unwind is already failing the test; asserting here would
        // bury the real cause under a double-panic abort.
        if std::thread::panicking() || self.skip_baseline {
            return;
        }

        // Tear down whatever the scenario left connected before checking for
        // drainage.
        for id in self.client_tokens.keys().copied().collect::<Vec<_>>() {
            self.kill_client(id);
        }

        let baseline = std::mem::take(&mut self.baseline);
        self.settle_to(&baseline);

        assert_eq!(
            self.state().debug_counters(),
            baseline,
            "fixture teardown left compositor state above baseline — a \
             window/surface/client-keyed collection leaked (see \
             DriftWm::debug_counters); tear the scenario down cleanly or call \
             skip_baseline_check"
        );
    }
}

impl FixtureState {
    pub fn client(&mut self, id: ClientId) -> &mut Client {
        self.clients.iter_mut().find(|c| c.id == id).unwrap()
    }
}
