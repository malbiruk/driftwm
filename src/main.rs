mod focus;
mod grabs;
mod handlers;
mod input;
mod state;
mod winit;

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

    // Initialize winit backend BEFORE setting WAYLAND_DISPLAY.
    // winit needs to connect to the parent compositor (e.g. GNOME),
    // not to our own socket.
    winit::init_winit(&mut event_loop, &mut data)?;

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
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &socket_name) };

    event_loop
        .handle()
        .insert_source(listening_socket, |stream, _, data: &mut CalloopData| {
            tracing::info!("New client connected");
            log_err("insert_client", data
                .display
                .handle()
                .insert_client(stream, Arc::new(ClientState::default())));

        })?;

    // Run the event loop
    tracing::info!("Starting event loop — launch apps with: WAYLAND_DISPLAY={socket_name} <app>");
    event_loop.run(None, &mut data, |data| {
        data.state.space.refresh();
        data.state.popups.cleanup();
        log_err("dispatch_clients", data.display.dispatch_clients(&mut data.state));
        log_err("flush_clients", data.display.flush_clients());
    })?;

    Ok(())
}
