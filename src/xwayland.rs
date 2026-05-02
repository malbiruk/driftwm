//! xwayland-satellite integration.
//!
//! Spawns `xwayland-satellite` eagerly at compositor startup so X11 apps
//! can connect immediately.
//!
//! Background: the on-demand `-listenfd` pattern (compositor pre-binds the
//! X11 socket and hands the FD to satellite on first X11 connection) has an
//! interop bug with Xwayland 24.x and multi-layout XKB configs (e.g.
//! `layout = "us,ru"` + `options = "grp:win_space_toggle"`): the queued X11
//! connection on the pre-bound socket triggers Xwayland's keyboard
//! initialization before the `wl_keyboard.keymap` event arrives, causing
//! `XKB: Failed to compile keymap` and a satellite panic. Vanilla mode
//! (satellite binds its own X11 socket on startup) avoids the race entirely.
//! Trade-off: ~30MB resident even if no X11 client ever runs. Acceptable;
//! if upstream fixes the listenfd path we can revisit.

use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use smithay::reexports::rustix;
use smithay::reexports::rustix::io::Errno;

use crate::state::DriftWm;

const MAX_DISPLAY: u32 = 50;

pub struct Satellite {
    /// The child satellite process. Held for its lifetime; on driftwm
    /// shutdown the handle is dropped and satellite exits when its Wayland
    /// connection to us closes. SIGCHLD is SIG_IGN in driftwm so the kernel
    /// auto-reaps.
    #[allow(dead_code)]
    child: Child,
}

/// Spawn `xwayland-satellite :N` eagerly, export `DISPLAY=:N`. Fails soft on
/// any error: logs a warning and leaves `state.satellite` as `None`, meaning
/// X11 apps just won't work but the compositor runs.
pub fn setup(state: &mut DriftWm) {
    if state.satellite.is_some() {
        return;
    }
    if !state.config.xwayland_enabled {
        return;
    }

    let path = state.config.xwayland_path.clone();
    if !probe_satellite(&path) {
        return;
    }

    let display = match find_free_display() {
        Some(n) => n,
        None => {
            tracing::warn!("no free X11 display number found, disabling xwayland-satellite");
            return;
        }
    };
    let display_name = format!(":{display}");

    let mut process = Command::new(&path);
    process
        .arg(&display_name)
        .env_remove("DISPLAY")
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    // stderr inherits so satellite's startup messages and any errors surface
    // alongside driftwm's log. Most noise is xkbcomp warnings; if those are
    // too distracting, redirect driftwm's stderr to a file.

    unsafe {
        process.pre_exec(|| {
            // Don't pass the compositor's blocked sigmask to satellite.
            crate::signals::unblock_all()?;
            // SIGCHLD inherits as SIG_IGN from driftwm — satellite needs
            // SIG_DFL so it can wait() on its own children (Xwayland).
            libc::signal(libc::SIGCHLD, libc::SIG_DFL);
            Ok(())
        });
    }

    let child = match process.spawn() {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!("failed to spawn xwayland-satellite ({path:?}): {err}");
            return;
        }
    };

    tracing::info!(
        "spawned xwayland-satellite pid={} on {display_name}",
        child.id()
    );

    // SAFETY: setup() runs at startup before any threads are spawned that
    // mutate env vars; matches the existing DRIFTWM_CONFIG/WAYLAND_DISPLAY
    // pattern in main.rs.
    unsafe {
        std::env::set_var("DISPLAY", &display_name);
    }
    export_display();

    state.satellite = Some(Satellite { child });
}

/// Probe whether the binary at `path` is launchable and supports our
/// expected protocol. Uses `--test-listenfd-support` because it's a cheap
/// "binary exists and responds to argv" check that all satellites since 0.7
/// implement and exit zero on. We don't actually use listenfd downstream.
fn probe_satellite(path: &str) -> bool {
    let mut process = Command::new(path);
    process
        .args([":0", "--test-listenfd-support"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("DISPLAY")
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE");

    let mut child = match process.spawn() {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(
                "xwayland-satellite not found at {path:?}: {err} — X11 apps disabled \
                 (install xwayland-satellite, or set [xwayland] enabled = false to silence)"
            );
            return false;
        }
    };
    match child.wait() {
        Ok(s) if s.success() => true,
        Ok(_) => {
            tracing::warn!(
                "xwayland-satellite at {path:?} is too old (need >= 0.7) — X11 apps disabled"
            );
            false
        }
        Err(err) => {
            tracing::warn!("error waiting for xwayland-satellite probe: {err}");
            false
        }
    }
}

/// Find a display number whose lock file *and* unix socket are both absent.
/// Satellite's internal Xwayland will create the lock and bind
/// `/tmp/.X11-unix/X{N}`, so either artifact already existing means another
/// X server (e.g. an SDDM-managed Xorg greeter on the same seat) holds the
/// number. Stat errors other than ENOENT are treated as "occupied" — a
/// root-owned lock file with no read permission would otherwise look free.
fn find_free_display() -> Option<u32> {
    (0..MAX_DISPLAY).find(|&n| !display_in_use(n))
}

fn display_in_use(n: u32) -> bool {
    path_present(&format!("/tmp/.X{n}-lock")) || path_present(&format!("/tmp/.X11-unix/X{n}"))
}

fn path_present(path: &str) -> bool {
    match rustix::fs::lstat(path) {
        Ok(_) => true,
        Err(Errno::NOENT) => false,
        Err(_) => true,
    }
}

fn export_display() {
    let cmd = "systemctl --user import-environment DISPLAY; \
               hash dbus-update-activation-environment 2>/dev/null && \
               dbus-update-activation-environment DISPLAY";
    match Command::new("/bin/sh").args(["-c", cmd]).spawn() {
        Ok(mut child) => {
            if let Err(e) = child.wait() {
                tracing::warn!("Error waiting for DISPLAY import: {e}");
            }
        }
        Err(e) => tracing::warn!("Failed to import DISPLAY: {e}"),
    }
}
