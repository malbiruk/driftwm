use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size};
use smithay::wayland::seat::WaylandFocus;

use crate::state::DriftWm;
use driftwm::window_ext::WindowExt;

pub mod client;
pub mod protocol;

use self::protocol::{
    Event, OutputInfo, Reply, Request, Response, ScreenshotTarget, StateInfo, WindowSelector,
    socket_path,
};

/// Reject a command line longer than this (without a newline) — bounds the
/// per-connection buffer against a client that never terminates a command.
const MAX_COMMAND_SIZE: usize = 4096;

/// Cap how long a reply write may block, so a stuck reader can't hang the loop.
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

pub struct IpcServer {
    socket_path: PathBuf,
}

impl IpcServer {
    pub fn new(
        event_loop: &LoopHandle<'static, DriftWm>,
        wayland_display: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_at(event_loop, socket_path(wayland_display))
    }

    /// Bind the IPC listener at an explicit path instead of the
    /// `WAYLAND_DISPLAY`-derived one. Tests point this at a private temp dir so
    /// they never touch the live session's socket directory.
    pub fn new_at(
        event_loop: &LoopHandle<'static, DriftWm>,
        socket_path: PathBuf,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        std::fs::remove_file(&socket_path).ok();
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(&socket_path)?;
        listener.set_nonblocking(true)?;
        std::fs::set_permissions(&socket_path, PermissionsExt::from_mode(0o600))?;

        tracing::info!("IPC socket started at {}", socket_path.display());

        let source = Generic::new(listener, Interest::READ, Mode::Level);
        event_loop.insert_source(source, |_, listener, state| {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => accept_client(state, stream),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    // A transient accept error (e.g. fd exhaustion) must never
                    // tear down the compositor — log and keep serving.
                    Err(e) => {
                        tracing::warn!("IPC accept error: {e}");
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        })?;

        Ok(Self { socket_path })
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        std::fs::remove_file(&self.socket_path).ok();
        tracing::debug!("IPC socket cleaned up");
    }
}

/// Register an accepted connection as its own calloop source. The closure owns a
/// per-connection read buffer so partial commands survive across event-loop ticks.
fn accept_client(state: &mut DriftWm, stream: UnixStream) {
    if let Err(e) = stream.set_nonblocking(true) {
        tracing::warn!("IPC: failed to set client nonblocking: {e}");
        return;
    }
    let mut buffer: Vec<u8> = Vec::with_capacity(256);
    let source = Generic::new(stream, Interest::READ, Mode::Level);
    // The callback hands a `&mut NoIoDrop<UnixStream>` which derefs to
    // `&UnixStream`; its Read/Write impls take `&self`, so a shared ref suffices.
    let registered = state
        .loop_handle
        .insert_source(source, move |_, stream, state| {
            Ok(serve_connection(stream, &mut buffer, state))
        });
    if let Err(e) = registered {
        tracing::warn!("IPC: failed to register client connection: {e}");
    }
}

/// Drain everything readable, answering each complete `\n`-terminated command.
fn serve_connection(
    mut stream: &UnixStream,
    buffer: &mut Vec<u8>,
    state: &mut DriftWm,
) -> PostAction {
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return disconnect(stream, state), // EOF
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if buffer.len() > MAX_COMMAND_SIZE {
                    tracing::warn!("IPC command too large, disconnecting");
                    return disconnect(stream, state);
                }
                while let Some(nl) = buffer.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=nl).collect();
                    // A half-written pushed event must be flushed before any
                    // reply, or the reply lands mid-line and corrupts framing.
                    if flush_pending_events(stream, state).is_err() {
                        return disconnect(stream, state);
                    }
                    let written = match serde_json::from_slice::<Request>(&line[..nl]) {
                        // Needs the raw stream to register a push channel, so it's
                        // handled here rather than through the stream-less dispatch.
                        Ok(Request::Subscribe) => subscribe(stream, state),
                        Ok(request) => write_reply(stream, &dispatch(request, state)),
                        Err(e) => write_reply(stream, &Err(format!("invalid request: {e}"))),
                    };
                    if written.is_err() {
                        return disconnect(stream, state);
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return PostAction::Continue,
            Err(e) => {
                tracing::warn!("IPC read error: {e}");
                return disconnect(stream, state);
            }
        }
    }
}

/// Tear down a connection, forgetting its subscription. Must run while the
/// serving fd is still open: once calloop closes it, the fd number can be
/// reused by a new connection, and a stale registry entry keyed on it would
/// swallow that connection's own `Subscribe`.
fn disconnect(stream: &UnixStream, state: &mut DriftWm) -> PostAction {
    let fd = stream.as_raw_fd();
    state.ipc_subscribers.retain(|s| s.fd != fd);
    PostAction::Remove
}

/// Blocking-flush any pushed-event bytes still pending on this connection's
/// subscription (the client is mid-request, so it's reading).
fn flush_pending_events(stream: &UnixStream, state: &mut DriftWm) -> std::io::Result<()> {
    let fd = stream.as_raw_fd();
    let Some(sub) = state.ipc_subscribers.iter_mut().find(|s| s.fd == fd) else {
        return Ok(());
    };
    let partial = std::mem::take(&mut sub.partial);
    let queued = sub.queued.take();
    if !partial.is_empty() {
        write_line(stream, &partial)?;
    }
    if let Some(event) = queued {
        write_line(stream, &event)?;
    }
    Ok(())
}

pub(crate) fn dispatch(request: Request, state: &mut DriftWm) -> Reply {
    // The animated setters (Camera/Zoom) only stash a target — they don't
    // self-schedule a frame — so every state-changing command needs the kick.
    // Pure queries don't, and shouldn't force a redraw.
    if is_mutating(&request) {
        state.mark_all_dirty();
    }

    match request {
        Request::Camera(arg) => cmd_camera(arg, state),
        Request::Zoom(arg) => cmd_zoom(arg, state),
        Request::Layout { short } => cmd_layout(short, state),
        Request::State => Ok(cmd_state(state)),
        Request::DebugCounters => Ok(Response::DebugCounters(state.debug_counters())),
        // Handled in serve_connection, which has the raw stream Subscribe needs.
        Request::Subscribe => unreachable!("Subscribe is handled before dispatch"),
        Request::Focus(arg) => cmd_focus(arg, state),
        Request::Move { window, to } => cmd_move(window, to, state),
        Request::Opacity { window, value } => cmd_opacity(window, value, state),
        Request::Close(sel) => cmd_close(sel, state),
        Request::Action(spec) => cmd_action(&spec, state),
        Request::Screenshot {
            target,
            scale,
            path,
        } => cmd_screenshot(&target, scale, &path, state),
    }
}

fn is_mutating(request: &Request) -> bool {
    matches!(
        request,
        Request::Camera(Some(_))
            | Request::Zoom(Some(_))
            | Request::Focus(Some(_))
            | Request::Move { to: Some(_), .. }
            | Request::Opacity { value: Some(_), .. }
            | Request::Close(_)
            | Request::Action(_)
    )
}

fn cmd_camera(arg: Option<(f64, f64)>, state: &mut DriftWm) -> Reply {
    match arg {
        None => {
            let (x, y) = camera_center(state);
            Ok(Response::Camera { x, y })
        }
        Some((x, y)) => {
            // (x, y) is the viewport center, Y-up; map it to the internal camera
            // target so the viewport ends up centered there.
            let target =
                driftwm::canvas::camera_for_center(x, y, state.zoom(), state.get_viewport_size());
            state.set_camera_target(Some(target));
            Ok(Response::Camera { x, y })
        }
    }
}

fn cmd_zoom(arg: Option<f64>, state: &mut DriftWm) -> Reply {
    match arg {
        None => Ok(Response::Zoom(state.zoom())),
        Some(zoom) => {
            if !zoom.is_finite() || zoom <= 0.0 {
                return Err("zoom must be a positive number".to_string());
            }
            // Same bounds as keyboard/gesture zoom; reply reports what was applied.
            let clamped = zoom.clamp(state.min_zoom(), driftwm::canvas::MAX_ZOOM);
            // Anchor on the viewport center (like keyboard zoom) so the center
            // stays put — otherwise zoom drifts the camera off the `camera` point.
            state.zoom_to_anchored(clamped);
            Ok(Response::Zoom(clamped))
        }
    }
}

fn cmd_layout(short: bool, state: &mut DriftWm) -> Reply {
    if !short {
        return Ok(Response::Layout(state.active_layout.clone()));
    }
    Ok(Response::Layout(active_layout_short(state)))
}

/// The `input.keyboard.layout` token at the active XKB group index (e.g. `ru`),
/// for status bars that want the code rather than the full display name.
///
/// The stored config always matches the loaded keymap (init pins it on the
/// invalid-config fallback, reload swaps both together), so the index resolves;
/// the display-name fallback only guards a malformed token (e.g. a trailing comma).
pub(crate) fn active_layout_short(state: &mut DriftWm) -> String {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return state.active_layout.clone();
    };
    let index =
        keyboard.with_xkb_state(state, |ctx| ctx.xkb().lock().unwrap().active_layout().0) as usize;
    layout_code(&state.config.keyboard_layout.layout, index)
        .unwrap_or_else(|| state.active_layout.clone())
}

/// The `index`-th code in a comma-separated layout list, trimmed; `None` if the
/// index is out of range or the token is empty (e.g. a trailing comma).
fn layout_code(layout_list: &str, index: usize) -> Option<String> {
    layout_list
        .split(',')
        .nth(index)
        .map(str::trim)
        .filter(|code| !code.is_empty())
        .map(str::to_owned)
}

fn cmd_state(state: &mut DriftWm) -> Response {
    Response::State(state_info(state))
}

/// Build the full state snapshot shared by the `state` reply and subscription
/// events, so the two representations can't drift.
pub(crate) fn state_info(state: &mut DriftWm) -> StateInfo {
    let windows = state.window_inventory();
    let (fullscreen, pinned) = state.screen_space_inventory();
    let (layers, canvas_layers) = state.layer_inventory();
    let layout = state.active_layout.clone();
    let layout_short = active_layout_short(state);
    let camera = camera_center(state);
    let zoom = state.zoom();

    let active = state.active_output();
    let outputs = state
        .space
        .outputs()
        .map(|output| {
            let (cam, z) = {
                let os = crate::state::output_state(output);
                (os.camera, os.zoom)
            };
            let logical = crate::state::output_logical_size(output);
            OutputInfo {
                name: output.name(),
                camera: driftwm::canvas::viewport_center(cam, z, logical),
                zoom: z,
                size: [logical.w, logical.h],
                active: active.as_ref() == Some(output),
            }
        })
        .collect();

    StateInfo {
        camera,
        zoom,
        layout,
        layout_short,
        windows,
        fullscreen,
        pinned,
        layers,
        canvas_layers,
        outputs,
    }
}

/// Resolve a selector to a live window. `None` = the focused window. AppId
/// matching is a case-insensitive substring search that skips widgets; id
/// lookup is exact and reaches widgets too.
fn window_by_selector(
    state: &DriftWm,
    selector: Option<&WindowSelector>,
) -> Result<smithay::desktop::Window, String> {
    match selector {
        None => state
            .focused_window()
            .ok_or_else(|| "no focused window".to_string()),
        Some(WindowSelector::Id(n)) => state
            .stage
            .window_by_id(driftwm::stage::ElementId(*n))
            .and_then(|w| w.client())
            .cloned()
            .ok_or_else(|| format!("no window with id {n}")),
        Some(WindowSelector::AppId(s)) => {
            let needle = s.to_lowercase();
            state
                .stage
                .windows()
                .filter_map(|w| w.client())
                .find(|w| {
                    !w.is_widget()
                        && w.app_id_or_class()
                            .is_some_and(|a| a.to_lowercase().contains(&needle))
                })
                .cloned()
                .ok_or_else(|| format!("no window matching '{needle}'"))
        }
    }
}

fn cmd_focus(arg: Option<WindowSelector>, state: &mut DriftWm) -> Reply {
    let Some(selector) = arg else {
        return Ok(Response::Focused(
            state
                .focused_window()
                .map(|w| focused_window_info(state, &w)),
        ));
    };
    let window = window_by_selector(state, Some(&selector))?;
    // Widgets are only reachable by id (the app_id search skips them) and can't
    // take focus.
    if window.is_widget() {
        let id = focused_window_info(state, &window).id;
        return Err(format!("window #{id} is a widget and cannot be focused"));
    }
    let info = focused_window_info(state, &window);
    // Already on screen: just raise + focus, don't move the camera. Pinned
    // windows are always on screen and have no canvas position to navigate to.
    if state.is_pinned(&window) || state.window_fully_in_viewport(&window) {
        state.raise_and_focus(&window, SERIAL_COUNTER.next_serial());
    } else {
        state.navigate_to_window(&window, state.config.zoom_reset_on_activation);
    }
    Ok(Response::Focused(Some(info)))
}

fn focused_window_info(
    state: &DriftWm,
    window: &smithay::desktop::Window,
) -> protocol::FocusedWindow {
    protocol::FocusedWindow {
        id: state
            .stage
            .id_of(window)
            .expect("window from the stage has an id")
            .0,
        app_id: window.app_id_or_class(),
    }
}

/// Reuses the config-file parser so the IPC `action` command stays in lockstep
/// with keybindable actions.
fn cmd_action(spec: &str, state: &mut DriftWm) -> Reply {
    let action = driftwm::config::parse_action(spec)?;
    state.execute_action(&action);
    Ok(Response::Ok)
}

fn cmd_move(window: Option<WindowSelector>, to: Option<(i32, i32)>, state: &mut DriftWm) -> Reply {
    let window = window_by_selector(state, window.as_ref())?;
    let size = window.geometry().size;
    match to {
        None => {
            let loc = state.stage.position_of(&window).unwrap_or_default();
            let (x, y) = driftwm::canvas::internal_to_rule(loc, size);
            Ok(Response::Position { x, y })
        }
        Some((x, y)) => {
            // A pinned window renders at its pin, a fullscreen one at its
            // camera park — writing the canvas position would silently do
            // nothing (pinned) or displace the park (fullscreen).
            if !state.is_canvas_window(&window) {
                return Err("pinned and fullscreen windows have no canvas position to move".into());
            }
            let loc = driftwm::canvas::rule_to_internal(x, y, size);
            // Moving re-anchors the window, invalidating any fill restore point.
            state.stage.clear_fill(&window);
            // Activating is only consistent when the target already holds
            // focus; a selector can reach any window.
            let activate = state.focused_window().as_ref() == Some(&window);
            state.map_window(window, loc, activate);
            Ok(Response::Position { x, y })
        }
    }
}

/// Runtime per-window opacity. The stored `AppliedWindowRule` is the single
/// source of truth: seeded from window rules at map, overwritten here. Applies
/// to anything the compositor renders, so no widget/pinned/fullscreen guard.
fn cmd_opacity(window: Option<WindowSelector>, value: Option<f64>, state: &mut DriftWm) -> Reply {
    let window = window_by_selector(state, window.as_ref())?;
    let surface = window
        .wl_surface()
        .ok_or_else(|| "window has no surface".to_string())?;
    match value {
        None => Ok(Response::Opacity(
            driftwm::config::applied_rule(&surface)
                .and_then(|r| r.opacity)
                .unwrap_or(1.0),
        )),
        Some(v) => {
            // Reject rather than clamp: the docs promise commands fail on bad values.
            if !v.is_finite() || !(0.0..=1.0).contains(&v) {
                return Err("opacity must be between 0.0 and 1.0".to_string());
            }
            smithay::wayland::compositor::with_states(&surface, |states| {
                states.data_map.insert_if_missing_threadsafe(|| {
                    std::sync::Mutex::new(driftwm::config::AppliedWindowRule::default())
                });
                states
                    .data_map
                    .get::<std::sync::Mutex<driftwm::config::AppliedWindowRule>>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .opacity = Some(v);
            });
            Ok(Response::Opacity(v))
        }
    }
}

fn cmd_close(sel: Option<WindowSelector>, state: &mut DriftWm) -> Reply {
    let window = window_by_selector(state, sel.as_ref())?;
    window.send_close();
    Ok(Response::Ok)
}

/// Capture a screenshot synchronously to `path`.
///
/// The renderer lives on the backend, so we take it out of `state` to split the
/// borrow (as the render loop does) and put it back on every path.
fn cmd_screenshot(target: &ScreenshotTarget, scale: f64, path: &str, state: &mut DriftWm) -> Reply {
    if !std::path::Path::new(path).is_absolute() {
        return Err("screenshot path must be absolute".to_string());
    }
    let (region, isolate) = resolve_screenshot_region(target, state)?;
    // `window` captures isolate the window on transparency; every other target is
    // a scene capture with the background.
    let include_background = !matches!(target, ScreenshotTarget::Window { .. });

    let mut backend = state
        .backend
        .take()
        .ok_or("no renderer available for capture")?;
    let result = {
        let renderer = backend.renderer();
        crate::render::capture_region_to_png(
            state,
            renderer,
            region,
            scale,
            include_background,
            isolate.as_ref(),
            std::path::Path::new(path),
        )
    };
    state.backend = Some(backend);

    let cap = result?;
    Ok(Response::Screenshot {
        path: path.to_string(),
        width: cap.width,
        height: cap.height,
    })
}

/// Resolve a screenshot target to an internal canvas rect (top-left, Y-down)
/// and, for a `window` target, the window to compose in isolation (`None` for
/// scene targets, which render every window).
fn resolve_screenshot_region(
    target: &ScreenshotTarget,
    state: &DriftWm,
) -> Result<(Rectangle<i32, Logical>, Option<smithay::desktop::Window>), String> {
    match target {
        ScreenshotTarget::Viewport => {
            let output = state.active_output().ok_or("no active output to capture")?;
            Ok((crate::state::output_viewport_rect(&output), None))
        }
        ScreenshotTarget::Window { window } => {
            // Isolation composes only this window, so pinned and fullscreen
            // capture fine — `window_visual_rect` already returns the right
            // rect for both (camera park / dormant canvas slot).
            let window = window_by_selector(state, window.as_ref())?;
            let rect = window_visual_rect(state, &window)
                .ok_or_else(|| "window has no capturable area".to_string())?;
            Ok((rect, Some(window)))
        }
        ScreenshotTarget::All => {
            let mut acc: Option<Rectangle<i32, Logical>> = None;
            for w in state
                .stage
                .windows()
                .filter_map(|w| w.client())
                .filter(|w| state.is_canvas_window(*w))
            {
                let Some(r) = window_visual_rect(state, w) else {
                    continue;
                };
                acc = Some(match acc {
                    Some(a) => union_rect(a, r),
                    None => r,
                });
            }
            let rect = acc.ok_or_else(|| "no windows to capture".to_string())?;
            // Frame the windows with `fit_padding` canvas units of margin.
            // `fit_padding` is defined as screen px at the fit zoom; applied in
            // canvas units it equals that px count only at `--scale 1`.
            let pad = state.config.zoom_fit_padding.max(0.0).round() as i32;
            Ok((
                Rectangle::new(
                    Point::<i32, Logical>::from((rect.loc.x - pad, rect.loc.y - pad)),
                    Size::<i32, Logical>::from((rect.size.w + 2 * pad, rect.size.h + 2 * pad)),
                ),
                None,
            ))
        }
        ScreenshotTarget::Region {
            x,
            y,
            w,
            h,
            from_screen,
        } => {
            let (x, y, w, h, from_screen) = (*x, *y, *w, *h, *from_screen);
            if w <= 0 || h <= 0 {
                return Err("region width and height must be positive".to_string());
            }
            if from_screen {
                // slurp coords live in the output *layout* space (`current_location()`,
                // what wl_output/xdg-output advertise), NOT the output's position in the
                // canvas Space (which tracks the camera). Map through the output hit.
                let point = Point::<i32, Logical>::from((x, y));
                let output = state
                    .space
                    .outputs()
                    .find(|o| {
                        Rectangle::new(o.current_location(), crate::state::output_logical_size(o))
                            .contains(point)
                    })
                    .cloned()
                    .or_else(|| state.active_output())
                    .ok_or("no output for the screen region")?;
                let layout_pos = output.current_location();
                let (camera, zoom) = {
                    let os = crate::state::output_state(&output);
                    (os.camera, os.zoom)
                };
                let loc = Point::<i32, Logical>::from((
                    (camera.x + (x - layout_pos.x) as f64 / zoom).round() as i32,
                    (camera.y + (y - layout_pos.y) as f64 / zoom).round() as i32,
                ));
                let size = Size::<i32, Logical>::from((
                    (w as f64 / zoom).round() as i32,
                    (h as f64 / zoom).round() as i32,
                ));
                Ok((Rectangle::new(loc, size), None))
            } else {
                let size = Size::<i32, Logical>::from((w, h));
                let loc = driftwm::canvas::rule_to_internal(x, y, size);
                Ok((Rectangle::new(loc, size), None))
            }
        }
    }
}

/// A window's full visual extent on the canvas: content padded by the title bar
/// (above), border, and shadow radius — must match what `compose_capture_elements`
/// draws. `None` if the window has no location or zero size.
fn window_visual_rect(
    state: &DriftWm,
    window: &smithay::desktop::Window,
) -> Option<Rectangle<i32, Logical>> {
    let loc = state.stage.position_of(window)?;
    let size = window.geometry().size;
    if size.w <= 0 || size.h <= 0 {
        return None;
    }
    let wl_surface = window.wl_surface()?;

    let is_fullscreen = state.stage.is_fullscreen(window);
    let has_ssd = !is_fullscreen && state.decorations.contains_key(&wl_surface.id());
    let applied = driftwm::config::applied_rule(&wl_surface);
    let mode = driftwm::config::effective_decoration_mode(
        applied.as_ref().and_then(|r| r.decoration.as_ref()),
        &state.config.decorations.default_mode,
    );
    let bw = if is_fullscreen {
        0
    } else {
        driftwm::config::effective_border_width(applied.as_ref(), mode, &state.config.decorations)
    };
    let shadow = !is_fullscreen
        && driftwm::config::effective_shadow_enabled(
            applied.as_ref(),
            mode,
            &state.config.decorations,
        );
    let bar = if has_ssd {
        state.config.decorations.title_bar_height
    } else {
        0
    };
    let pad = if shadow {
        driftwm::config::DecorationConfig::SHADOW_RADIUS.ceil() as i32
    } else {
        0
    };
    let edge = bw + pad;
    Some(Rectangle::new(
        Point::<i32, Logical>::from((loc.x - edge, loc.y - bar - edge)),
        Size::<i32, Logical>::from((size.w + 2 * edge, size.h + bar + 2 * edge)),
    ))
}

fn union_rect(a: Rectangle<i32, Logical>, b: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
    let x0 = a.loc.x.min(b.loc.x);
    let y0 = a.loc.y.min(b.loc.y);
    let x1 = (a.loc.x + a.size.w).max(b.loc.x + b.size.w);
    let y1 = (a.loc.y + a.size.h).max(b.loc.y + b.size.h);
    Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into())
}

/// Serialize and send a reply over `stream`.
fn write_reply(stream: &UnixStream, reply: &Reply) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(reply)?;
    bytes.push(b'\n');
    write_line(stream, &bytes)
}

/// Send a pre-serialized line on the request/reply path. Switches to a bounded
/// blocking write so a large payload isn't truncated on `WouldBlock` and a stuck
/// reader can't hang the loop, then restores nonblocking.
fn write_line(mut stream: &UnixStream, bytes: &[u8]) -> std::io::Result<()> {
    stream.set_nonblocking(false).ok();
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok();
    let res = stream.write_all(bytes);
    stream.set_nonblocking(true).ok();
    res
}

/// A subscribed IPC connection: the serving stream's fd (for dedup and
/// disconnect cleanup), a cloned write handle, and at most two buffered
/// events — the unwritten tail of the one in flight, plus the newest queued
/// one (older queued events are superseded, since each is a full snapshot).
/// Writes never block, so a subscriber that stops reading just accumulates a
/// queued event and converges once its socket drains.
pub struct Subscriber {
    fd: std::os::fd::RawFd,
    stream: UnixStream,
    partial: Vec<u8>,
    queued: Option<Vec<u8>>,
}

/// Ack a `Subscribe`, push the current snapshot, and register the connection for
/// future pushes. A repeat on an already-subscribed connection just re-sends the
/// snapshot (deduped by the serving stream's fd). A failed `try_clone` replies
/// `Err` and registers nothing.
fn subscribe(stream: &UnixStream, state: &mut DriftWm) -> std::io::Result<()> {
    // Clone the write half before acking, so a clone failure surfaces as an
    // error reply rather than a silently half-registered subscriber.
    let clone = match stream.try_clone() {
        Ok(c) => c,
        Err(e) => return write_reply(stream, &Err(format!("cannot subscribe: {e}"))),
    };
    write_reply(stream, &Ok(Response::Ok))?;
    // The initial snapshot may block (bounded): the client just asked for it,
    // so it's reading — same guarantee a `state` reply gets.
    let mut bytes = serde_json::to_vec(&Event::State(state_info(state)))?;
    bytes.push(b'\n');
    write_line(stream, &bytes)?;
    let fd = stream.as_raw_fd();
    if !state.ipc_subscribers.iter().any(|s| s.fd == fd) {
        state.ipc_subscribers.push(Subscriber {
            fd,
            stream: clone,
            partial: Vec::new(),
            queued: None,
        });
    }
    Ok(())
}

/// Push the current state to every subscriber, dropping the ones whose
/// connection is gone. Writes never block: a subscriber still draining a
/// previous event gets this one queued (superseding any older queued event)
/// and converges once its socket drains.
pub(crate) fn broadcast_state_event(state: &mut DriftWm) {
    if state.ipc_subscribers.is_empty() {
        return;
    }
    let Ok(mut bytes) = serde_json::to_vec(&Event::State(state_info(state))) else {
        return;
    };
    bytes.push(b'\n');
    // A dirty tick that serializes to the same snapshot (e.g. the state-file
    // write failing persistently and retrying) carries no information — don't
    // re-send it. New subscribers got the snapshot at subscribe time.
    let digest = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        h.finish()
    };
    if state.ipc_last_event_hash == Some(digest) {
        return;
    }
    state.ipc_last_event_hash = Some(digest);

    let mut subs = std::mem::take(&mut state.ipc_subscribers);
    subs.retain_mut(|sub| {
        if sub.partial.is_empty() {
            sub.partial = bytes.clone();
        } else {
            sub.queued = Some(bytes.clone());
        }
        drain_subscriber(sub)
    });
    state.ipc_subscribers = subs;
}

/// Retry buffered event writes for subscribers whose socket was full, so a
/// stalled-then-recovered subscriber converges even when no new change fires a
/// broadcast. Called per rendered frame while any subscriber exists.
pub(crate) fn flush_subscriber_outboxes(state: &mut DriftWm) {
    if state
        .ipc_subscribers
        .iter()
        .all(|s| s.partial.is_empty() && s.queued.is_none())
    {
        return;
    }
    state.ipc_subscribers.retain_mut(drain_subscriber);
}

/// Write as much buffered event data as the socket accepts without blocking.
/// `false` means the connection is gone.
fn drain_subscriber(sub: &mut Subscriber) -> bool {
    loop {
        if sub.partial.is_empty() {
            match sub.queued.take() {
                Some(next) => sub.partial = next,
                None => return true,
            }
        }
        if !try_drain(&sub.stream, &mut sub.partial) {
            return false;
        }
        if !sub.partial.is_empty() {
            // Socket full — keep the tail for the next attempt.
            return true;
        }
    }
}

/// Write as much of `pending` as the socket accepts without blocking, keeping
/// the rest. `false` means the connection is gone.
fn try_drain(mut stream: &UnixStream, pending: &mut Vec<u8>) -> bool {
    while !pending.is_empty() {
        match stream.write(pending) {
            Ok(0) => return false,
            Ok(n) => {
                pending.drain(..n);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}

/// The viewport center, Y-up — same representation as the state file, so `camera`,
/// `state`, and the state file all agree.
fn camera_center(state: &DriftWm) -> (f64, f64) {
    driftwm::canvas::viewport_center(state.camera(), state.zoom(), state.get_viewport_size())
}

#[cfg(test)]
mod tests {
    use super::layout_code;

    #[test]
    fn layout_code_indexes_the_list() {
        assert_eq!(layout_code("us,ru", 0).as_deref(), Some("us"));
        assert_eq!(layout_code("us,ru", 1).as_deref(), Some("ru"));
    }

    #[test]
    fn layout_code_trims_whitespace() {
        assert_eq!(layout_code("us, ru", 1).as_deref(), Some("ru"));
    }

    #[test]
    fn layout_code_rejects_out_of_range_and_empty() {
        assert_eq!(layout_code("us,ru", 2), None);
        assert_eq!(layout_code("us,", 1), None);
    }
}
