use std::io::{ErrorKind, Read, Write};
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

use self::protocol::{Reply, Request, Response, ScreenshotTarget, socket_path};

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
        let socket_path = socket_path(wayland_display);

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
            Ok(0) => return PostAction::Remove, // EOF
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if buffer.len() > MAX_COMMAND_SIZE {
                    tracing::warn!("IPC command too large, disconnecting");
                    return PostAction::Remove;
                }
                while let Some(nl) = buffer.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=nl).collect();
                    let reply = process_line(&line[..nl], state);
                    if write_reply(stream, &reply).is_err() {
                        return PostAction::Remove;
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return PostAction::Continue,
            Err(e) => {
                tracing::warn!("IPC read error: {e}");
                return PostAction::Remove;
            }
        }
    }
}

fn process_line(line: &[u8], state: &mut DriftWm) -> Reply {
    let request: Request =
        serde_json::from_slice(line).map_err(|e| format!("invalid request: {e}"))?;
    dispatch(request, state)
}

fn dispatch(request: Request, state: &mut DriftWm) -> Reply {
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
        Request::Focus(arg) => cmd_focus(arg, state),
        Request::Move(arg) => cmd_move(arg, state),
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
            | Request::Move(Some(_))
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
    let windows = state.window_inventory();
    let (fullscreen, pinned) = state.screen_space_inventory();
    Response::State {
        camera: camera_center(state),
        zoom: state.zoom(),
        windows,
        fullscreen,
        pinned,
    }
}

fn cmd_focus(arg: Option<String>, state: &mut DriftWm) -> Reply {
    match arg {
        None => Ok(Response::Focused(
            state.focused_window().and_then(|w| w.app_id_or_class()),
        )),
        Some(target) => {
            let target = target.to_lowercase();
            let found = state
                .space
                .elements()
                .find(|w| {
                    !w.is_widget()
                        && w.app_id_or_class()
                            .is_some_and(|a| a.to_lowercase().contains(&target))
                })
                .cloned();

            match found {
                Some(window) => {
                    let app_id = window.app_id_or_class();
                    // Already on screen: just raise + focus, don't move the
                    // camera. Pinned windows are always on screen and have no
                    // canvas position to navigate to.
                    if state.is_pinned(&window) || state.window_fully_in_viewport(&window) {
                        state.raise_and_focus(&window, SERIAL_COUNTER.next_serial());
                    } else {
                        state.navigate_to_window(&window, state.config.zoom_reset_on_activation);
                    }
                    Ok(Response::Focused(app_id))
                }
                None => Err(format!("no window matching '{target}'")),
            }
        }
    }
}

/// Reuses the config-file parser so the IPC `action` command stays in lockstep
/// with keybindable actions.
fn cmd_action(spec: &str, state: &mut DriftWm) -> Reply {
    let action = driftwm::config::parse_action(spec)?;
    state.execute_action(&action);
    Ok(Response::Ok)
}

fn cmd_move(arg: Option<(i32, i32)>, state: &mut DriftWm) -> Reply {
    let Some(window) = state.focused_window() else {
        return Err("no focused window".to_string());
    };
    let size = window.geometry().size;
    match arg {
        None => {
            let loc = state.space.element_location(&window).unwrap_or_default();
            let (x, y) = driftwm::canvas::internal_to_rule(loc, size);
            Ok(Response::Position { x, y })
        }
        Some((x, y)) => {
            let loc = driftwm::canvas::rule_to_internal(x, y, size);
            state.space.map_element(window, loc, true);
            Ok(Response::Position { x, y })
        }
    }
}

/// Capture a screenshot synchronously to `path`.
///
/// The renderer lives on the backend, so we take it out of `state` to split the
/// borrow (as the render loop does) and put it back on every path.
fn cmd_screenshot(target: &ScreenshotTarget, scale: f64, path: &str, state: &mut DriftWm) -> Reply {
    if !std::path::Path::new(path).is_absolute() {
        return Err("screenshot path must be absolute".to_string());
    }
    let region = resolve_screenshot_region(target, state)?;
    // `window` captures isolate the window on transparency; every other target is
    // a scene capture with the background.
    let include_background = !matches!(target, ScreenshotTarget::Window);

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

/// Resolve a screenshot target to an internal canvas rect (top-left, Y-down).
fn resolve_screenshot_region(
    target: &ScreenshotTarget,
    state: &DriftWm,
) -> Result<Rectangle<i32, Logical>, String> {
    match *target {
        ScreenshotTarget::Viewport => {
            let output = state.active_output().ok_or("no active output to capture")?;
            Ok(crate::state::output_viewport_rect(&output))
        }
        ScreenshotTarget::Window => {
            // Pinned windows render in screen space and are excluded from the
            // canvas-capture path by construction, so there's no canvas region
            // to capture — refuse rather than emit the background behind them.
            let window = state
                .focused_window()
                .filter(|w| !w.is_widget() && !state.is_pinned(w))
                .ok_or("no focused window to capture")?;
            window_visual_rect(state, &window)
                .ok_or_else(|| "focused window has no capturable area".to_string())
        }
        ScreenshotTarget::All => {
            let mut acc: Option<Rectangle<i32, Logical>> = None;
            for w in state
                .space
                .elements()
                .filter(|w| !w.is_widget() && !state.is_pinned(w))
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
            Ok(Rectangle::new(
                Point::<i32, Logical>::from((rect.loc.x - pad, rect.loc.y - pad)),
                Size::<i32, Logical>::from((rect.size.w + 2 * pad, rect.size.h + 2 * pad)),
            ))
        }
        ScreenshotTarget::Region {
            x,
            y,
            w,
            h,
            from_screen,
        } => {
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
                Ok(Rectangle::new(loc, size))
            } else {
                let size = Size::<i32, Logical>::from((w, h));
                let loc = driftwm::canvas::rule_to_internal(x, y, size);
                Ok(Rectangle::new(loc, size))
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
    let loc = state.space.element_location(window)?;
    let size = window.geometry().size;
    if size.w <= 0 || size.h <= 0 {
        return None;
    }
    let wl_surface = window.wl_surface()?;

    let is_fullscreen = state.fullscreen.values().any(|fs| &fs.window == window);
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

/// Serialize and send a reply. Switches to a bounded blocking write so a large
/// reply isn't truncated on `WouldBlock` and a stuck reader can't hang the loop.
fn write_reply(mut stream: &UnixStream, reply: &Reply) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(reply)?;
    bytes.push(b'\n');
    stream.set_nonblocking(false).ok();
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok();
    let res = stream.write_all(&bytes);
    stream.set_nonblocking(true).ok();
    res
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
