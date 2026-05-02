//! Graceful shutdown via SIGINT / SIGTERM / SIGHUP.
//!
//! The pattern:
//! 1. `block_early()` — called very early in `main`, before any threads exist.
//!    Uses `pthread_sigmask` to block the signals so the kernel's default
//!    action (process termination) never fires; child threads inherit the
//!    mask so the same applies to them.
//! 2. `listen()` — registers calloop's `Signals` source (signalfd-based).
//!    Blocked signals get queued on the fd and dispatched as normal events;
//!    the handler calls `loop_signal.stop()`, taking the same shutdown path
//!    as the Quit keybind (clean event-loop exit, state-file removal,
//!    Wayland Display drop).
//! 3. `unblock_all()` — runs in `pre_exec` for spawned children so they
//!    don't inherit our blocked sigmask (which would surprise apps that
//!    install their own SIGTERM handlers).

use std::io;
use std::mem::MaybeUninit;

use smithay::reexports::calloop;

use crate::state::DriftWm;

const BLOCKED: &[libc::c_int] = &[libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

pub fn block_early() -> io::Result<()> {
    let mut set = empty_sigset()?;
    for &sig in BLOCKED {
        if unsafe { libc::sigaddset(&mut set, sig) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    set_sigmask(&set)
}

pub fn unblock_all() -> io::Result<()> {
    set_sigmask(&empty_sigset()?)
}

pub fn listen(handle: &calloop::LoopHandle<'static, DriftWm>) {
    use calloop::signals::{Signal, Signals};

    let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP])
        .expect("failed to create signalfd source");
    handle
        .insert_source(signals, |event, _, state| {
            tracing::info!("received {:?} — stopping compositor", event.signal());
            state.loop_signal.stop();
        })
        .expect("failed to register signal source on event loop");
}

fn empty_sigset() -> io::Result<libc::sigset_t> {
    let mut set = MaybeUninit::uninit();
    if unsafe { libc::sigemptyset(set.as_mut_ptr()) } == 0 {
        Ok(unsafe { set.assume_init() })
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_sigmask(set: &libc::sigset_t) -> io::Result<()> {
    if unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, set, std::ptr::null_mut()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
