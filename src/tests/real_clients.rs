//! Slow-gated real-client conformance: spawn an actual terminal binary against
//! the fixture's live wayland socket, drive it over the IPC wire, then kill it
//! and assert the crash-teardown path drains back to baseline.
//!
//! `#[ignore]`d out of the default lane (spawns a real process against real
//! sockets — not hermetic); run with `cargo test -- --include-ignored`. Also
//! self-skips when neither `foot` nor `weston-terminal` is installed.

use std::process::Command;
use std::time::{Duration, Instant};

use driftwm::window_ext::WindowExt;

use super::Fixture;
use super::real::{TempDir, ipc_request};
use crate::ipc::protocol::{Request, Response, WindowSelector};

/// Kill and reap the child on drop, so a mid-test panic can't orphan a
/// still-running client (`Child::drop` alone neither kills nor waits).
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

/// First of the known terminal binaries present on `PATH`, or `None`.
fn find_client() -> Option<&'static str> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    for bin in ["foot", "weston-terminal"] {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(bin);
            let executable = std::fs::metadata(&candidate)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false);
            if executable {
                return Some(bin);
            }
        }
    }
    None
}

#[test]
#[ignore = "spawns a real client binary; needs foot or weston-terminal installed"]
fn real_client_over_ipc() {
    let Some(bin) = find_client() else {
        eprintln!(
            "skipping real_client_over_ipc: neither foot nor weston-terminal found on PATH \
             (this test is machine-gated)"
        );
        return;
    };
    eprintln!("real_client_over_ipc: using client binary '{bin}'");

    let temp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let socket_name = f.listen(temp.path());
    let ipc_path = f.start_ipc(temp.path());

    // Baseline before the client exists: the crash-teardown path must return
    // here after the child is killed.
    let baseline = f.counters();

    let spawn_start = Instant::now();
    let mut child = KillOnDrop(
        Command::new(bin)
            .env("WAYLAND_DISPLAY", &socket_name)
            .env_remove("DISPLAY")
            .stdout(std::fs::File::create(temp.path().join("client.out")).unwrap())
            .stderr(std::fs::File::create(temp.path().join("client.err")).unwrap())
            .spawn()
            .expect("spawn client binary"),
    );

    // Cold process startup: give it a generous window to connect, map, and set
    // its app_id.
    f.wait_until(Duration::from_secs(15), |s| {
        s.stage
            .windows()
            .any(|w| w.app_id_or_class().is_some_and(|a| !a.is_empty()))
    });
    eprintln!(
        "real_client_over_ipc: window mapped after {:?}",
        spawn_start.elapsed()
    );

    let Ok(Response::State(info)) = ipc_request(&mut f, &ipc_path, &Request::State) else {
        panic!("expected a State reply");
    };
    let window = info
        .windows
        .iter()
        .find(|w| !w.app_id.is_empty())
        .expect("a window with an app_id must be listed");
    let id = window.id;
    eprintln!(
        "real_client_over_ipc: State -> #{id} app_id='{}' at {:?}",
        window.app_id, window.position
    );

    let Ok(Response::Focused(Some(focused))) = ipc_request(
        &mut f,
        &ipc_path,
        &Request::Focus(Some(WindowSelector::Id(id))),
    ) else {
        panic!("expected a Focused reply");
    };
    assert_eq!(focused.id, id, "focus reply must carry the requested id");
    eprintln!("real_client_over_ipc: Focus({id}) -> #{}", focused.id);

    let Ok(Response::Position { x, y }) = ipc_request(
        &mut f,
        &ipc_path,
        &Request::Move {
            window: Some(WindowSelector::Id(id)),
            to: Some((640, 480)),
        },
    ) else {
        panic!("expected a Position reply");
    };
    assert_eq!((x, y), (640, 480), "move reply must echo the target");

    let Ok(Response::State(info)) = ipc_request(&mut f, &ipc_path, &Request::State) else {
        panic!("expected a State reply");
    };
    let window = info
        .windows
        .iter()
        .find(|w| w.id == id)
        .expect("moved window must still be listed");
    assert_eq!(
        window.position,
        [640, 480],
        "state must reflect the moved position"
    );
    eprintln!(
        "real_client_over_ipc: Move -> position {:?}",
        window.position
    );

    let Ok(Response::DebugCounters(counters)) =
        ipc_request(&mut f, &ipc_path, &Request::DebugCounters)
    else {
        panic!("expected a DebugCounters reply");
    };
    assert!(
        counters["stage_entries"] >= 1,
        "stage_entries must be >= 1 while the client is mapped"
    );
    eprintln!(
        "real_client_over_ipc: DebugCounters stage_entries={}",
        counters["stage_entries"]
    );

    child.0.kill().ok();
    child.0.wait().ok();
    let target = baseline.clone();
    f.wait_until(Duration::from_secs(15), |s| s.debug_counters() == target);
    assert_eq!(
        f.counters(),
        baseline,
        "real-client crash teardown left state above baseline"
    );
    eprintln!("real_client_over_ipc: teardown drained to baseline");
}
