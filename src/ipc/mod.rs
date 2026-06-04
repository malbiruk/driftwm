use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction, LoopHandle};
use smithay::utils::{Point, SERIAL_COUNTER};
use smithay::wayland::seat::WaylandFocus;

use crate::state::{DriftWm, FocusTarget, output_state};
use driftwm::window_ext::WindowExt;

pub struct IpcServer {
    socket_path: PathBuf,
}

impl IpcServer {
    pub fn new(event_loop: &LoopHandle<'static, DriftWm>) -> Result<Self, Box<dyn std::error::Error>> {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| "/tmp".to_string());
        let socket_path = PathBuf::from(format!("{}/driftwm/ipc.sock", runtime_dir));

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
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true)?;
                    let mut client = IpcClient::new(stream);
                    client.process(state);
                    Ok(PostAction::Continue)
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(PostAction::Continue),
                Err(e) => {
                    tracing::warn!("IPC accept error: {}", e);
                    Err(e)
                }
            }
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

struct IpcClient {
    stream: UnixStream,
    buffer: Vec<u8>,
}

impl IpcClient {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            buffer: Vec::with_capacity(256),
        }
    }

    fn process(&mut self, state: &mut DriftWm) {
        let mut buf = [0u8; 512];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    self.buffer.extend_from_slice(&buf[..n]);
                    if self.buffer.len() >= MAX_COMMAND_SIZE {
                        tracing::warn!("IPC command too large, disconnecting");
                        return;
                    }
                    if let Some(newline) = self.buffer.iter().position(|&b| b == b'\n') {
                        let line = String::from_utf8_lossy(&self.buffer[..newline]).to_string();
                        self.buffer.drain(..=newline);
                        let response = process_command(&line, state);
                        let _ = self.stream.write_all(response.as_bytes());
                        let _ = self.stream.write_all(b"\n");
                        let _ = self.stream.flush();
                        if self.buffer.is_empty() {
                            return;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) => {
                    tracing::warn!("IPC read error: {}", e);
                    return;
                }
            }
        }
    }
}

const MAX_COMMAND_SIZE: usize = 4096;

fn process_command(command: &str, state: &mut DriftWm) -> String {
    state.mark_all_dirty();
    let command = command.trim();
    if command.is_empty() {
        return json_response("error", "empty command");
    }

    let parts: Vec<&str> = command.split_whitespace().collect();
    if parts.is_empty() {
        return json_response("error", "empty command");
    }

    match parts[0] {
        "camera" => handle_camera(parts, state),
        "zoom" => handle_zoom(parts, state),
        "layout" => handle_layout(parts, state),
        "state" => handle_state(state),
        "quit" => handle_quit(state),
        "focus" => handle_focus(parts, state),
        "close" => handle_close(state),
        "move" => handle_move(parts, state),
        _ => json_response("error", format!("unknown command: {}", parts[0])),
    }
}

fn handle_camera(args: Vec<&str>, state: &mut DriftWm) -> String {
    if args.len() == 1 {
        let (x, y) = camera_position(state);
        json_response("ok", serde_json::json!({
            "camera": {"x": x, "y": y}
        }))
    } else if args.len() == 3 {
        let x: f64 = match args[1].parse() {
            Ok(v) => v,
            Err(_) => return json_response("error", "x must be number"),
        };
        let y: f64 = match args[2].parse() {
            Ok(v) => v,
            Err(_) => return json_response("error", "y must be number"),
        };

        let target = Point::from((x, y));
        state.set_camera_target(Some(target));

        json_response("ok", serde_json::json!({
            "camera": {"x": x, "y": y}
        }))
    } else {
        json_response("error", "camera takes 0 or 2 args")
    }
}

fn handle_zoom(args: Vec<&str>, state: &mut DriftWm) -> String {
    if args.len() == 1 {
        json_response("ok", serde_json::json!({
            "zoom": zoom_level(state)
        }))
    } else if args.len() == 2 {
        let zoom: f64 = match args[1].parse() {
            Ok(v) => v,
            Err(_) => return json_response("error", "zoom must be number"),
        };

        if !(0.1..=10.0).contains(&zoom) {
            return json_response("error", "zoom must be 0.1-10.0");
        }

        state.set_zoom_target(Some(zoom));

        json_response("ok", serde_json::json!({
            "zoom": zoom
        }))
    } else {
        json_response("error", "zoom takes 0 or 1 arg")
    }
}

fn handle_layout(args: Vec<&str>, state: &mut DriftWm) -> String {
    if args.len() == 1 {
        json_response("ok", serde_json::json!({
            "layout": state.active_layout
        }))
    } else {
        json_response("error", "layout write not yet implemented (requires XKB switch action)")
    }
}

fn handle_state(state: &mut DriftWm) -> String {
    let mut windows = Vec::new();
    for window in state.space.elements() {
        let bbox = window.bbox();
        let app_id = window.app_id_or_class();
        windows.push(serde_json::json!({
            "app_id": app_id.unwrap_or_else(|| "unknown".to_string()),
            "x": bbox.loc.x,
            "y": bbox.loc.y,
            "width": bbox.size.w,
            "height": bbox.size.h,
        }));
    }

    let (cx, cy) = camera_position(state);
    let zoom = zoom_level(state);

    json_response("ok", serde_json::json!({
        "camera": {"x": cx, "y": cy},
        "zoom": zoom,
        "windows": windows,
        "window_count": windows.len(),
    }))
}

fn handle_quit(state: &mut DriftWm) -> String {
    tracing::info!("IPC quit command received, shutting down");
    state.loop_signal.stop();
    json_response("ok", serde_json::json!({
        "status": "shutting down"
    }))
}

fn handle_focus(args: Vec<&str>, state: &mut DriftWm) -> String {
    if args.len() == 1 {
        if let Some(window) = state.focused_window() {
            let app_id = window.app_id_or_class().unwrap_or_else(|| "unknown".to_string());
            json_response("ok", serde_json::json!({
                "focused": app_id
            }))
        } else {
            json_response("ok", serde_json::json!({
                "focused": null
            }))
        }
    } else if args.len() == 2 {
        let target = args[1].to_lowercase();
        let mut found = None;
        for window in state.space.elements() {
            if let Some(app_id) = window.app_id_or_class()
                && app_id.to_lowercase().contains(&target)
            {
                found = Some((window.clone(), app_id));
                break;
            }
        }

        if let Some((window, app_id)) = found {
            state.space.raise_element(&window, true);
            if let Some(surface) = window.wl_surface() {
                let keyboard = state.seat.get_keyboard().unwrap();
                keyboard.set_focus(
                    state,
                    Some(FocusTarget(surface.into_owned())),
                    SERIAL_COUNTER.next_serial(),
                );
            }
            json_response("ok", serde_json::json!({
                "focused": app_id
            }))
        } else {
            json_response("error", format!("no window matching '{}'", target))
        }
    } else {
        json_response("error", "focus takes 0 or 1 arg (app_id)")
    }
}

fn handle_close(state: &mut DriftWm) -> String {
    if let Some(window) = state.focused_window().filter(|w| !w.is_widget()) {
        let app_id = window.app_id_or_class().unwrap_or_else(|| "unknown".to_string());
        window.send_close();
        json_response("ok", serde_json::json!({
            "closed": app_id
        }))
    } else {
        json_response("error", "no focused window to close")
    }
}

fn handle_move(args: Vec<&str>, state: &mut DriftWm) -> String {
    if args.len() == 1 {
        if let Some(window) = state.focused_window() {
            let bbox = window.bbox();
            json_response("ok", serde_json::json!({
                "x": bbox.loc.x,
                "y": bbox.loc.y
            }))
        } else {
            json_response("error", "no focused window")
        }
    } else if args.len() == 3 {
        let x: i32 = match args[1].parse() {
            Ok(v) => v,
            Err(_) => return json_response("error", "x must be integer"),
        };
        let y: i32 = match args[2].parse() {
            Ok(v) => v,
            Err(_) => return json_response("error", "y must be integer"),
        };

        if let Some(window) = state.focused_window() {
            state.space.map_element(window.clone(), Point::from((x, y)), true);
            json_response("ok", serde_json::json!({
                "x": x,
                "y": y
            }))
        } else {
            json_response("error", "no focused window to move")
        }
    } else {
        json_response("error", "move takes 0 or 2 args (x y)")
    }
}

fn camera_position(state: &mut DriftWm) -> (f64, f64) {
    state
        .active_output()
        .map(|o| {
            let os = output_state(&o);
            (os.camera.x, os.camera.y)
        })
        .unwrap_or((0.0, 0.0))
}

fn zoom_level(state: &mut DriftWm) -> f64 {
    state
        .active_output()
        .map(|o| output_state(&o).zoom)
        .unwrap_or(1.0)
}

fn json_response(status: &str, data: impl Into<serde_json::Value>) -> String {
    serde_json::json!({
        "status": status,
        "data": data.into(),
    })
    .to_string()
}
