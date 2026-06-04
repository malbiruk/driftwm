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
mod xwayland;

use state::{ClientState, DriftWm};
use std::sync::Arc;

/// Wrap the system allocator with Tracy's profiled allocator when the
/// allocations feature is on. Tracks every allocation on the timeline; only
/// useful when chasing allocation hotspots.
#[cfg(feature = "profile-with-tracy-allocations")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(std::alloc::System, 100);

fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "driftwm {} — {}\n\
             \n\
             USAGE:\n    \
                 driftwm [OPTIONS]\n\
             \n\
             OPTIONS:\n    \
                 --backend <udev|winit>   Backend (default: udev on TTY, winit if nested)\n    \
                 --config <path>          Use an alternate config file\n    \
                 --check-config           Validate config and exit\n    \
                 -V, --version            Print version\n    \
                 -h, --help               Print this help\n\
             \n\
             {}",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_DESCRIPTION"),
            env!("CARGO_PKG_REPOSITORY"),
        );
        return Ok(());
    }

    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("driftwm {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if std::env::args().any(|a| a == "--check-config") {
        let _config = driftwm::config::Config::load();
        tracing::info!("Config OK");
        return Ok(());
    }

    // --config <path>: override config file (useful for nested/test sessions).
    if let Some(path) = std::env::args().skip_while(|a| a != "--config").nth(1) {
        unsafe { std::env::set_var("DRIFTWM_CONFIG", &path) };
    }

    // --backend: default udev on bare metal, winit if nested.
    let backend_name = std::env::args()
        .skip_while(|a| a != "--backend")
        .nth(1)
        .unwrap_or_else(|| {
            if std::env::var_os("WAYLAND_DISPLAY").is_some()
                || std::env::var_os("DISPLAY").is_some()
            {
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

    // Initialize IPC server
    match crate::ipc::IpcServer::new(&event_loop.handle()) {
        Ok(server) => data.ipc_server = Some(server),
        Err(e) => tracing::warn!("IPC server failed to start: {}", e),
    }

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

    // Config watcher: poll mtime every 500ms.
    {
        let config_path = driftwm::config::config_path();
        data.config_file_mtime = std::fs::metadata(&config_path)
            .and_then(|m| m.modified())
            .ok();

        let timer = smithay::reexports::calloop::timer::Timer::from_duration(
            std::time::Duration::from_millis(500),
        );
        event_loop
            .handle()
            .insert_source(timer, move |_, _, data: &mut DriftWm| {
                let current_mtime = std::fs::metadata(&config_path)
                    .and_then(|m| m.modified())
                    .ok();
                if current_mtime != data.config_file_mtime && current_mtime.is_some() {
                    // Debounce: skip if <100ms old (editor may still be writing).
                    let dominated_by_recent_write = current_mtime
                        .is_some_and(|mt| mt.elapsed().is_ok_and(|age| age.as_millis() < 100));
                    if !dominated_by_recent_write {
                        data.config_file_mtime = current_mtime;
                        data.reload_config();
                    }
                }
                smithay::reexports::calloop::timer::TimeoutAction::ToDuration(
                    std::time::Duration::from_millis(500),
                )
            })?;
    }

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
        data.space.refresh();
        data.popups.cleanup();
        data.display_handle.flush_clients().ok();
    })?;

    state::remove_state_file();

    Ok(())
}
