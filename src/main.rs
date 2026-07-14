mod backend;
mod decorations;
mod grabs;
mod handlers;
mod input;
mod ipc;
mod region;
mod render;
mod signals;
mod state;
mod surface_tree;
#[cfg(test)]
mod tests;
mod xwayland;

use clap::Parser;
use state::{ClientState, DriftWm};
use std::sync::Arc;

/// A trackpad-first infinite canvas Wayland compositor.
///
/// With no subcommand, starts the compositor, auto-detecting the backend (udev
/// on a TTY, winit when nested). The `msg` subcommand instead talks to an
/// already-running instance over its IPC socket; see docs/ipc.md for the raw
/// wire protocol.
#[derive(Parser)]
#[command(
    name = "driftwm",
    version,
    after_help = concat!("Documentation & source: ", env!("CARGO_PKG_REPOSITORY"))
)]
struct Cli {
    /// Backend to use [default: udev on a TTY, winit if nested]
    #[arg(long, value_name = "udev|winit")]
    backend: Option<String>,
    /// Use an alternate config file
    #[arg(long, value_name = "PATH")]
    config: Option<std::path::PathBuf>,
    /// Validate the config and exit
    #[arg(long)]
    check_config: bool,
    #[command(subcommand)]
    command: Option<Sub>,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Send a command to the running compositor over its IPC socket.
    ///
    /// Auto-targets the instance named by `WAYLAND_DISPLAY` (override with
    /// `DRIFTWM_SOCKET`). A subcommand with no arguments reads state; with
    /// arguments it writes. Add `--json` for the raw JSON reply. A command that
    /// fails (bad value, no match, no focused window) prints an error to stderr
    /// and exits non-zero, so scripts can branch on it.
    Msg {
        /// Print the raw JSON reply
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        msg: ipc::client::Msg,
    },
}

/// Wrap the system allocator with Tracy's profiled allocator when the
/// allocations feature is on. Tracks every allocation on the timeline; only
/// useful when chasing allocation hotspots.
#[cfg(feature = "profile-with-tracy-allocations")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(std::alloc::System, 100);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // `driftwm msg ...` runs as a client and exits — before any backend, event
    // loop, or signal blocking.
    if let Some(Sub::Msg { json, msg }) = &cli.command {
        if let Err(e) = ipc::client::run(msg, *json) {
            eprintln!("driftwm msg: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // Block SIGINT/SIGTERM/SIGHUP before any threads spawn so they're
    // delivered via signalfd (see signals::listen) instead of killing the
    // process. Child threads inherit the mask; spawn_command clears it for
    // exec'd children.
    signals::block_early()?;

    // Start Tracy server connection BEFORE other threads spawn so they're
    // captured. No-op without the profile-with-tracy feature.
    #[cfg(feature = "profile-with-tracy")]
    tracy_client::Client::start();

    if std::env::var("RUST_LOG").is_err() {
        unsafe { std::env::set_var("RUST_LOG", "info") };
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    if cli.check_config {
        let _config = driftwm::config::Config::load();
        tracing::info!("Config OK");
        return Ok(());
    }

    // --config <path>: override config file (useful for nested/test sessions).
    if let Some(path) = &cli.config {
        unsafe { std::env::set_var("DRIFTWM_CONFIG", path) };
    }

    // --backend: default udev on bare metal, winit if nested.
    let backend_name = cli.backend.clone().unwrap_or_else(|| {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some() {
            "winit".to_string()
        } else {
            "udev".to_string()
        }
    });

    let mut event_loop: smithay::reexports::calloop::EventLoop<DriftWm> =
        smithay::reexports::calloop::EventLoop::try_new()?;

    // signalfd path so SIGTERM from systemd / `pkill driftwm` goes through
    // the same clean exit as the Quit keybind.
    signals::listen(&event_loop.handle());

    let display = smithay::reexports::wayland_server::Display::<DriftWm>::new()?;

    let mut data = DriftWm::new(
        display.handle(),
        event_loop.handle(),
        event_loop.get_signal(),
    );

    // Scan the desktop-entry database off-thread so the first `suspend-window`
    // (or `suspend_on_close`) never parses hundreds of files on the input path.
    data.warm_desktop_entry_cache();

    // Initialize backend BEFORE setting WAYLAND_DISPLAY.
    match backend_name.as_str() {
        "udev" => {
            let dev = backend::udev::init_udev(&mut event_loop, &mut data)?;
            data.udev_device = Some(dev);
        }
        _ => backend::winit::init_winit(&mut event_loop, &mut data)?,
    }

    // Register Wayland Display as a calloop source for auto client dispatch.
    let display_source = smithay::reexports::calloop::generic::Generic::new(
        display,
        smithay::reexports::calloop::Interest::READ,
        smithay::reexports::calloop::Mode::Level,
    );
    event_loop
        .handle()
        .insert_source(display_source, |_, display, data: &mut DriftWm| {
            // SAFETY: Display is never dropped while the Generic source is alive.
            unsafe { display.get_mut() }.dispatch_clients(data).ok();
            Ok(smithay::reexports::calloop::PostAction::Continue)
        })?;

    let listening_socket = smithay::wayland::socket::ListeningSocketSource::new_auto()?;
    let socket_name = listening_socket
        .socket_name()
        .to_string_lossy()
        .into_owned();
    tracing::info!("Listening on WAYLAND_DISPLAY={socket_name}");
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &socket_name) };
    unsafe { std::env::set_var("XDG_SESSION_TYPE", "wayland") };
    unsafe { std::env::set_var("XDG_CURRENT_DESKTOP", "driftwm") };
    // Toolkit env vars (MOZ_ENABLE_WAYLAND, QT_QPA_PLATFORM, ...) live in
    // Config::load() with user [env] overrides taking precedence.
    unsafe { std::env::set_var("XDG_SESSION_CLASS", "user") };
    unsafe { std::env::set_var("XDG_SESSION_DESKTOP", "driftwm") };

    // Add WAYLAND_DISPLAY to child_env for autostart commands
    data.config
        .child_env
        .insert("WAYLAND_DISPLAY".to_string(), socket_name.clone());

    // The IPC socket name derives from WAYLAND_DISPLAY, so start it now that the
    // wayland display is known — this lets `driftwm msg` auto-target this instance.
    match crate::ipc::IpcServer::new(&event_loop.handle(), &socket_name) {
        Ok(server) => data.ipc_server = Some(server),
        Err(e) => tracing::warn!("IPC server failed to start: {e}"),
    }

    // Export only session-level vars to systemd and D-Bus. Pass them through
    // Command::env() rather than relying on process env — the policy is "don't
    // touch process env at runtime", so the shell-out gets only what we hand it.
    {
        let session_vars = [
            ("WAYLAND_DISPLAY", socket_name.as_str()),
            ("XDG_CURRENT_DESKTOP", "driftwm"),
            ("XDG_SESSION_TYPE", "wayland"),
            ("XDG_SESSION_DESKTOP", "driftwm"),
        ];
        let names = session_vars
            .iter()
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join(" ");
        let cmd = format!(
            "systemctl --user import-environment {names}; \
             hash dbus-update-activation-environment 2>/dev/null && \
             dbus-update-activation-environment {names}"
        );
        match std::process::Command::new("/bin/sh")
            .args(["-c", &cmd])
            .envs(session_vars.iter().copied())
            .spawn()
        {
            Ok(mut child) => {
                if let Err(e) = child.wait() {
                    tracing::warn!("Error waiting for environment import: {e}");
                }
            }
            Err(e) => tracing::warn!("Failed to import environment: {e}"),
        }
    }

    // READY=1 lets graphical-session.target units (e.g. foot-server.socket
    // gated on ConditionEnvironment=WAYLAND_DISPLAY) evaluate post-export.
    // unset_env=true so children don't inherit NOTIFY_SOCKET.
    if let Err(e) = sd_notify::notify(true, &[sd_notify::NotifyState::Ready]) {
        tracing::warn!("Failed to send READY=1 to systemd: {e}");
    }

    event_loop
        .handle()
        .insert_source(listening_socket, |stream, _, data: &mut DriftWm| {
            tracing::info!("New client connected");
            if let Err(e) = data
                .display_handle
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                tracing::warn!("Failed to insert client: {e}");
            }
        })?;

    // inotify watch instead of an mtime poll, so an idle session never wakes
    // the CPU. Watch the directory, not the file: editor atomic-saves replace
    // the file's inode, which would silently kill a file-level watch.
    {
        use inotify::{Inotify, WatchMask};
        use smithay::reexports::calloop::{Interest, Mode, PostAction, generic::Generic};
        use std::os::fd::AsFd;

        let config_path = driftwm::config::config_path();
        data.config_file_mtime = std::fs::metadata(&config_path)
            .and_then(|m| m.modified())
            .ok();
        // Resolve symlinks so a config linked from a dotfiles repo is watched at
        // its real location; fall back to the literal path when it doesn't exist
        // yet, so a later CREATE still fires.
        let watch_path =
            std::fs::canonicalize(&config_path).unwrap_or_else(|_| config_path.clone());
        let config_dir = watch_path.parent().map(|p| p.to_owned());
        let config_name = watch_path.file_name().map(|n| n.to_owned());

        match (config_dir, Inotify::init()) {
            (Some(config_dir), Ok(mut inotify)) => {
                if let Err(e) = inotify.watches().add(
                    &config_dir,
                    WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::CREATE,
                ) {
                    tracing::warn!("Config hot-reload disabled: cannot watch {config_dir:?}: {e}");
                } else {
                    // calloop only hands the callback a shared ref to the Generic's
                    // payload, but read_events needs &mut. Poll a dup'd fd (same
                    // inotify description) and keep the instance in the closure,
                    // where it can be read mutably.
                    match inotify.as_fd().try_clone_to_owned() {
                        Ok(watch_fd) => {
                            let source = Generic::new(watch_fd, Interest::READ, Mode::Level);
                            let registered = event_loop.handle().insert_source(
                                source,
                                move |_, _, data: &mut DriftWm| {
                                    let mut buffer = [0u8; 1024];
                                    let touched = match inotify.read_events(&mut buffer) {
                                        Ok(events) => events
                                            .filter_map(|e| e.name)
                                            .any(|n| Some(n) == config_name.as_deref()),
                                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                            false
                                        }
                                        Err(e) => {
                                            tracing::warn!("inotify read error: {e}");
                                            false
                                        }
                                    };
                                    // One save can emit several events (e.g. CREATE +
                                    // MOVED_TO on an atomic rename); reload only when
                                    // the file's mtime actually advances.
                                    if touched {
                                        let mtime = std::fs::metadata(&config_path)
                                            .and_then(|m| m.modified())
                                            .ok();
                                        if mtime != data.config_file_mtime && mtime.is_some() {
                                            data.config_file_mtime = mtime;
                                            data.reload_config();
                                        }
                                    }
                                    Ok(PostAction::Continue)
                                },
                            );
                            if let Err(e) = registered {
                                tracing::warn!("Config hot-reload disabled: {e}");
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Config hot-reload disabled: fd clone failed: {e}");
                        }
                    }
                }
            }
            (None, _) => {
                tracing::warn!("Config hot-reload disabled: config path has no parent directory");
            }
            (_, Err(e)) => {
                tracing::warn!("Config hot-reload disabled: inotify init failed: {e}");
            }
        }
    }

    // Drives the off-screen frame-callback heartbeat when no rendering is
    // happening (#141); see send_frame_callbacks_fallback.
    event_loop.handle().insert_source(
        smithay::reexports::calloop::timer::Timer::from_duration(std::time::Duration::from_secs(1)),
        |_, _, data: &mut DriftWm| {
            crate::render::send_frame_callbacks_fallback(data);
            smithay::reexports::calloop::timer::TimeoutAction::ToDuration(
                std::time::Duration::from_secs(1),
            )
        },
    )?;

    // After WAYLAND_DISPLAY is set so satellite can connect as a Wayland client.
    xwayland::setup(&mut data);

    // Auto-reap children. Must run after backend init — libseat uses
    // waitpid() during session setup.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };

    // Defer autostart so the event loop is running first — GTK apps (swaync)
    // need Wayland event processing before they connect.
    let autostart = data.autostart.clone();
    if !autostart.is_empty() {
        event_loop.handle().insert_source(
            smithay::reexports::calloop::timer::Timer::from_duration(
                std::time::Duration::from_millis(100),
            ),
            move |_, _, data: &mut DriftWm| {
                for cmd in &autostart {
                    tracing::info!("Autostart: {cmd}");
                    state::spawn_command(cmd, &data.config.child_env);
                }
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            },
        )?;
    }

    tracing::info!("Starting event loop — launch apps with: WAYLAND_DISPLAY={socket_name} <app>");
    event_loop.run(None, &mut data, |data| {
        backend::udev::render_if_needed(data);
        data.refresh_and_flush_clients();
        // Expire suspend / real-close marks a refused close left behind. The
        // fixture drives this with an injected `now`; production uses the wall
        // clock here (the only wall-clock read for mark deadlines).
        data.sweep_marks(std::time::Instant::now());
    })?;

    state::remove_state_file();

    Ok(())
}
