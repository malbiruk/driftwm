mod backend;
mod focus;
mod grabs;
mod handlers;
mod input;
mod render;
mod state;

use state::{CalloopData, ClientState, log_err};
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging (RUST_LOG=info by default)
    if std::env::var("RUST_LOG").is_err() {
        unsafe { std::env::set_var("RUST_LOG", "info") };
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Parse --backend arg
    let backend_name = std::env::args()
        .skip_while(|a| a != "--backend")
        .nth(1)
        .unwrap_or_else(|| "winit".to_string());

    // Create calloop event loop
    let mut event_loop: smithay::reexports::calloop::EventLoop<CalloopData> =
        smithay::reexports::calloop::EventLoop::try_new()?;

    // Create Wayland display
    let display =
        smithay::reexports::wayland_server::Display::<state::DriftWm>::new()?;

    // Build compositor state
    let compositor_state = state::DriftWm::new(
        display.handle(),
        event_loop.handle(),
        event_loop.get_signal(),
    );

    let mut data = CalloopData {
        state: compositor_state,
        display,
    };

    // Initialize backend BEFORE setting WAYLAND_DISPLAY.
    let drm_device = match backend_name.as_str() {
        "udev" => Some(backend::udev::init_udev(&mut event_loop, &mut data)?),
        _ => {
            backend::winit::init_winit(&mut event_loop, &mut data)?;
            None
        }
    };

    // Register the Wayland display FD so calloop wakes on client messages
    let poll_fd = data.display.backend().poll_fd().try_clone_to_owned()?;
    event_loop.handle().insert_source(
        smithay::reexports::calloop::generic::Generic::new(
            poll_fd,
            smithay::reexports::calloop::Interest::READ,
            smithay::reexports::calloop::Mode::Level,
        ),
        |_, _, data: &mut CalloopData| {
            log_err("dispatch_clients", data.display.dispatch_clients(&mut data.state));
            Ok(smithay::reexports::calloop::PostAction::Continue)
        },
    )?;

    // Now create listening socket and advertise it to child processes
    let listening_socket =
        smithay::wayland::socket::ListeningSocketSource::new_auto()?;
    let socket_name = listening_socket
        .socket_name()
        .to_string_lossy()
        .into_owned();
    tracing::info!("Listening on WAYLAND_DISPLAY={socket_name}");
    // Standard Wayland session env vars for child processes
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &socket_name) };
    unsafe { std::env::set_var("XDG_SESSION_TYPE", "wayland") };
    unsafe { std::env::set_var("XDG_CURRENT_DESKTOP", "driftwm") };
    unsafe { std::env::set_var("MOZ_ENABLE_WAYLAND", "1") };
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "wayland") };
    unsafe { std::env::set_var("SDL_VIDEODRIVER", "wayland") };
    unsafe { std::env::set_var("GDK_BACKEND", "wayland,x11") };

    event_loop
        .handle()
        .insert_source(listening_socket, |stream, _, data: &mut CalloopData| {
            tracing::info!("New client connected");
            log_err("insert_client", data
                .display
                .handle()
                .insert_client(stream, Arc::new(ClientState::default())));

        })?;

    // Config file watcher: poll mtime every 500ms
    {
        let config_path = driftwm::config::config_path();
        data.state.config_file_mtime = std::fs::metadata(&config_path)
            .and_then(|m| m.modified())
            .ok();

        let timer = smithay::reexports::calloop::timer::Timer::from_duration(
            std::time::Duration::from_millis(500),
        );
        event_loop.handle().insert_source(timer, move |_, _, data: &mut CalloopData| {
            let current_mtime = std::fs::metadata(&config_path)
                .and_then(|m| m.modified())
                .ok();
            if current_mtime != data.state.config_file_mtime && current_mtime.is_some() {
                // Debounce: skip if mtime is <100ms old (editor may still be writing)
                let dominated_by_recent_write = current_mtime.is_some_and(|mt| {
                    mt.elapsed().is_ok_and(|age| age.as_millis() < 100)
                });
                if !dominated_by_recent_write {
                    data.state.config_file_mtime = current_mtime;
                    data.state.reload_config();
                }
            }
            smithay::reexports::calloop::timer::TimeoutAction::ToDuration(
                std::time::Duration::from_millis(500),
            )
        })?;
    }

    // Auto-reap child processes — prevents zombies from exec/autostart commands.
    // Must be after backend init: libseat uses waitpid() during session setup.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };

    // Defer autostart until the event loop is running — GTK apps (swaync) need
    // the compositor processing Wayland events before they connect.
    let autostart = data.state.autostart.clone();
    if !autostart.is_empty() {
        event_loop.handle().insert_source(
            smithay::reexports::calloop::timer::Timer::from_duration(
                std::time::Duration::from_millis(100),
            ),
            move |_, _, _data| {
                for cmd in &autostart {
                    tracing::info!("Autostart: {cmd}");
                    state::spawn_command(cmd);
                }
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            },
        )?;
    }

    // Run the event loop
    tracing::info!("Starting event loop — launch apps with: WAYLAND_DISPLAY={socket_name} <app>");
    event_loop.run(None, &mut data, |data| {
        if let Some(ref device) = drm_device {
            backend::udev::render_if_needed(device, data);
        }
        data.state.space.refresh();
        data.state.popups.cleanup();
        log_err("dispatch_clients", data.display.dispatch_clients(&mut data.state));
        log_err("flush_clients", data.display.flush_clients());
    })?;

    state::remove_state_file();

    Ok(())
}
