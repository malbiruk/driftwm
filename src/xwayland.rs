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
    /// Held for the satellite's lifetime; satellite exits when its Wayland
    /// connection closes on drop. SIGCHLD is SIG_IGN so the kernel auto-reaps.
    child: Child,
}

impl Satellite {
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }
}

/// Spawn `xwayland-satellite :N` eagerly, export `DISPLAY=:N`. Fails soft:
/// any error leaves `state.satellite = None` so X11 apps don't work but
/// the compositor runs.
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
        .envs(&state.config.child_env)
        .env_remove("DISPLAY")
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    // stderr inherits so satellite startup messages surface alongside our log.

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

    // Make DISPLAY visible to children via child_env (process env untouched).
    state
        .config
        .child_env
        .insert("DISPLAY".into(), display_name.clone());

    export_display(&display_name);

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

/// Display number with both lock file AND unix socket absent — either
/// artifact present means another X server (SDDM Xorg greeter, etc.) holds
/// it. Non-ENOENT stat errors count as occupied (so a root-owned lock file
/// with no read permission doesn't look free).
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

fn export_display(display_name: &str) {
    let cmd = "systemctl --user import-environment DISPLAY; \
               hash dbus-update-activation-environment 2>/dev/null && \
               dbus-update-activation-environment DISPLAY";
    match Command::new("/bin/sh")
        .args(["-c", cmd])
        .env("DISPLAY", display_name)
        .spawn()
    {
        Ok(mut child) => {
            if let Err(e) = child.wait() {
                tracing::warn!("Error waiting for DISPLAY import: {e}");
            }
        }
        Err(e) => tracing::warn!("Failed to import DISPLAY: {e}"),
    }
}
