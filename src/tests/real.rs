//! Real-client harness: give the in-process fixture a live wayland listening
//! socket and IPC socket so an actual client binary (foot, weston-terminal) can
//! connect and be driven over the wire. Kept apart from the plain fixture so the
//! fast in-process scenarios are unaffected.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction};
use smithay::reexports::wayland_server::ListeningSocket;

use super::fixture::Fixture;
use crate::ipc::protocol::{Reply, Request};
use crate::state::{ClientState, DriftWm};

impl Fixture {
    /// Add a real wayland listening socket to the server loop. Binds inside
    /// `dir` (a private temp dir) with an absolute path so the live session's
    /// `$XDG_RUNTIME_DIR` sockets and lockfiles are never touched; `new_auto`
    /// probes those, so it's deliberately avoided. Returns the absolute socket
    /// path — libwayland reads it verbatim from a child's `WAYLAND_DISPLAY`,
    /// which is never set on our own process env.
    pub fn listen(&mut self, dir: &Path) -> String {
        let path = dir.join("wayland-0");
        let socket = ListeningSocket::bind_absolute(path.clone()).expect("bind wayland socket");
        let source = Generic::new(socket, Interest::READ, Mode::Level);
        let handle = self.state().loop_handle.clone();
        handle
            .insert_source(source, |_, socket, state: &mut DriftWm| {
                while let Some(stream) = socket.accept()? {
                    if let Err(e) = state
                        .display_handle
                        .insert_client(stream, Arc::new(ClientState::default()))
                    {
                        tracing::warn!("test listen: failed to insert client: {e}");
                    }
                }
                Ok(PostAction::Continue)
            })
            .expect("insert listening socket source");
        path.to_string_lossy().into_owned()
    }

    /// Start the IPC server on a socket inside `dir` (a private temp dir, so the
    /// live `$XDG_RUNTIME_DIR/driftwm/` is never written). Stored on the
    /// `DriftWm` so its `Drop` unlinks the socket. Returns the socket path for
    /// the test to connect to.
    pub fn start_ipc(&mut self, dir: &Path) -> PathBuf {
        let path = dir.join("ipc.sock");
        let handle = self.state().loop_handle.clone();
        let server =
            crate::ipc::IpcServer::new_at(&handle, path.clone()).expect("start ipc server");
        self.state().ipc_server = Some(server);
        path
    }

    /// Drive the fixture until `predicate` holds, failing the test on timeout.
    /// A real client blocks on `wl_surface.frame` before drawing its next frame,
    /// and nothing off-screen sends those under the fixture — so each round pumps
    /// the loop, issues the frame-callback heartbeat, then forces a server pump
    /// to flush the callbacks and configures out to the client (queuing them
    /// alone doesn't make the server fd readable, so without this flush the
    /// client deadlocks after its first commit). Wall-clock timing is fine here:
    /// this harness is slow-gated.
    pub fn wait_until(
        &mut self,
        timeout: Duration,
        mut predicate: impl FnMut(&mut DriftWm) -> bool,
    ) {
        let deadline = Instant::now() + timeout;
        loop {
            self.dispatch();
            crate::render::send_frame_callbacks_fallback(self.state());
            self.pump(1);
            if predicate(self.state()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "wait_until timed out after {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Send one request on its own connection and read the reply, pumping the
/// compositor between attempts. A blocking read would deadlock — the test thread
/// *is* the server loop, so the reply is only produced when we pump. A short
/// read timeout bounds each attempt; the retry loop pumps and re-reads until the
/// reply lands or the deadline fails the test.
pub fn ipc_request(f: &mut Fixture, ipc_path: &Path, request: &Request) -> Reply {
    let mut stream = UnixStream::connect(ipc_path).expect("connect ipc socket");
    let mut payload = serde_json::to_vec(request).unwrap();
    payload.push(b'\n');
    stream.write_all(&payload).expect("write ipc request");
    stream
        .set_read_timeout(Some(Duration::from_millis(20)))
        .expect("set read timeout");

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        f.pump(1);
        match reader.read_line(&mut line) {
            Ok(0) => panic!("ipc connection closed before a reply to {request:?}"),
            Ok(_) => break,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                assert!(
                    Instant::now() < deadline,
                    "ipc reply timed out for {request:?}"
                );
            }
            Err(e) => panic!("ipc read error for {request:?}: {e}"),
        }
    }
    serde_json::from_str(line.trim_end()).expect("parse ipc reply")
}

/// A private temp dir removed on drop, holding the test's IPC socket. Named with
/// the pid plus a process-unique counter so concurrent tests never collide.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("driftwm-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).ok();
    }
}
