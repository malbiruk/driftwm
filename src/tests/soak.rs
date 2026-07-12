//! Long-running soak: hundreds of windows churned through map, state
//! transitions, and both teardown paths (clean destroy + abrupt kill). The
//! per-cycle counter equality is the attribution win — a leaked collection
//! entry fails inside the cycle that introduced it, not at some distant Drop.
//! Process-wide fd and RSS plateaus catch leaks that don't touch a counter.
//!
//! Gated behind `RUN_SLOW_TESTS=1` so the default lane stays fast.

use std::collections::BTreeMap;

use super::{Fixture, map_window, window_by_app_id};

/// Count the process's open file descriptors. The count includes `read_dir`'s
/// own iterator fd, but that bias is identical in both snapshots and only the
/// delta is asserted.
fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

/// Resident set size in kB, parsed from `/proc/self/status` (`VmRSS:` is
/// reported in kB).
fn rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap()
}

/// Resize the client window to its last received configure (when it names a
/// size) and ack-commit, the way a real app answers a configure.
fn resize_to_last_configure(
    f: &mut Fixture,
    id: super::client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) {
    let window = f.client(id).window(surface);
    let (w, h) = window.configures_received.last().unwrap().1.size;
    if w > 0 && h > 0 {
        window.set_size(w as u16, h as u16);
    }
    window.ack_last_and_commit();
}

/// One soak cycle: fresh client, eight mapped windows, three state
/// round-trips (fullscreen, fit, pin), then half the windows closed cleanly and
/// the rest reaped by killing the client. Ends settled to `settle_target`.
/// Each state transition is asserted mid-cycle — every entry point exercised
/// here has silent early-return guards, and a cycle that quietly stopped
/// churning state would otherwise keep the soak green while testing nothing.
fn run_cycle(f: &mut Fixture, tag: usize, settle_target: &BTreeMap<String, usize>) {
    let id = f.add_client();

    let mut surfaces = Vec::new();
    let mut app_ids = Vec::new();
    for i in 0..8u16 {
        let app_id = format!("soak-{tag}-{i}");
        let size = (300 + i * 37, 200 + i * 29);
        surfaces.push(map_window(f, id, &app_id, size));
        app_ids.push(app_id);
    }

    f.client(id).window(&surfaces[0]).set_fullscreen(None);
    f.double_roundtrip(id);
    f.client(id).window(&surfaces[0]).ack_last_and_commit();
    f.double_roundtrip(id);
    assert_eq!(f.counters()["stage_fullscreen"], 1);
    f.client(id).window(&surfaces[0]).unset_fullscreen();
    f.double_roundtrip(id);
    f.client(id).window(&surfaces[0]).ack_last_and_commit();
    f.double_roundtrip(id);
    assert_eq!(f.counters()["stage_fullscreen"], 0);

    // Fit toggle on + off; resize to each configure before acking so the
    // round-trip mirrors a real app and the unfit recenter path fires
    // mid-cycle instead of only draining at client death.
    let window = window_by_app_id(f, &app_ids[5]).expect("soak window must be mapped");
    f.state().toggle_fit_window(&window);
    f.double_roundtrip(id);
    resize_to_last_configure(f, id, &surfaces[5]);
    f.double_roundtrip(id);
    assert!(f.state().stage.is_fit(&window));
    f.state().toggle_fit_window(&window);
    f.double_roundtrip(id);
    resize_to_last_configure(f, id, &surfaces[5]);
    f.double_roundtrip(id);
    assert!(!f.state().stage.is_fit(&window));

    // Pin toggle on + off through the pin action's real entry point, which
    // operates on the focused window — so focus it first each time.
    let window = window_by_app_id(f, &app_ids[6]).expect("soak window must be mapped");
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);
    f.state()
        .execute_action(&driftwm::config::Action::TogglePinToScreen);
    f.double_roundtrip(id);
    assert_eq!(f.counters()["stage_pinned"], 1);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);
    f.state()
        .execute_action(&driftwm::config::Action::TogglePinToScreen);
    f.double_roundtrip(id);
    assert_eq!(f.counters()["stage_pinned"], 0);

    // Half close cleanly (client-side destroy), the rest die abruptly with the
    // client — both teardown paths must drain to baseline.
    for surface in &surfaces[0..4] {
        f.client(id).window(surface).destroy();
    }
    f.roundtrip(id);
    f.kill_client(id);

    f.settle_to(settle_target);
}

/// Churn hundreds of windows and assert the compositor holds no per-window
/// state, file descriptors, or memory across cycles.
#[test]
fn soak_cycles_hold_no_state() {
    if std::env::var_os("RUN_SLOW_TESTS").is_none() {
        return;
    }

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Empty-state snapshot: the target every cycle must settle back to, and the
    // reference for the warmup cycles before the plateau baselines exist.
    let clean = f.counters();

    // Warmup absorbs allocator/arena growth and lazily-initialized state so the
    // plateau comparison after the loop is honest.
    for tag in 0..3 {
        run_cycle(&mut f, tag, &clean);
    }

    let fd_base = fd_count();
    let rss_base = rss_kb();
    let counter_base = f.counters();

    for cycle in 0..30 {
        run_cycle(&mut f, 3 + cycle, &counter_base);
        assert_eq!(
            f.counters(),
            counter_base,
            "compositor state above baseline after soak cycle {cycle} — a \
             window/surface-keyed collection leaked (see \
             DriftWm::debug_counters)"
        );
    }

    // Slack absorbs unrelated noise: other tests in this process open fds
    // concurrently, and the allocator grows arenas in steps rather than
    // returning every page.
    let fd_now = fd_count();
    assert!(
        fd_now <= fd_base + 8,
        "file descriptors grew across the soak: {fd_base} -> {fd_now}"
    );
    let rss_now = rss_kb();
    assert!(
        rss_now <= rss_base + 24 * 1024,
        "resident memory grew across the soak: {rss_base} kB -> {rss_now} kB"
    );
}
