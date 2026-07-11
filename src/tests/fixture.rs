use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

use smithay::output::Output;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction};

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

        Self {
            event_loop,
            handle,
            state,
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

    pub fn add_output(&mut self, n: u8, size: (u16, u16)) -> Output {
        headless::add_output(self.state(), n, size)
    }

    pub fn add_client(&mut self) -> ClientId {
        let (sock1, sock2) = UnixStream::pair().unwrap();
        self.state.server.insert_client(sock1);

        let client = Client::new(sock2);
        let id = client.id;

        let fd = client.event_loop.as_fd().try_clone_to_owned().unwrap();
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        self.handle
            .insert_source(source, move |_, _, state: &mut FixtureState| {
                state.client(id).dispatch();
                Ok(PostAction::Continue)
            })
            .unwrap();

        self.state.clients.push(client);
        self.roundtrip(id);
        id
    }

    pub fn client(&mut self, id: ClientId) -> &mut Client {
        self.state.client(id)
    }

    pub fn roundtrip(&mut self, id: ClientId) {
        let client = self.state.client(id);
        let data = client.send_sync();
        while !data.done.load(Ordering::Relaxed) {
            self.dispatch();
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

impl FixtureState {
    pub fn client(&mut self, id: ClientId) -> &mut Client {
        self.clients.iter_mut().find(|c| c.id == id).unwrap()
    }
}
