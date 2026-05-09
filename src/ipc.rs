use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction};
use smithay::reexports::rustix::net::{bind, listen, socket, AddressFamily, SocketAddrUnix, SocketType};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};
use std::path::PathBuf;
use std::{fs, io, mem};

use crate::state::DriftWm;
use driftwm::canvas;

pub fn socket_path() -> PathBuf {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(runtime).join("driftwm/ipc.sock")
}

pub fn create_ipc_listener() -> io::Result<(Generic<StdUnixListener>, RawFd)> {
    let path = socket_path();
    if path.exists() { let _ = fs::remove_file(&path); }
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }

    let sock = socket(AddressFamily::UNIX, SocketType::STREAM | SocketType::NONBLOCK, None)?;
    let addr = SocketAddrUnix::new(&path).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    bind(&sock, &addr)?;
    listen(&sock, 16)?;

    let fd: OwnedFd = sock.into();
    let raw = fd.as_fd().as_raw_fd();
    let listener = unsafe { StdUnixListener::from_raw_fd(raw) };
    let generic = Generic::new(listener, Interest::READ, Mode::Level);
    Ok((generic, raw))
}

pub struct IpcHandler {
    pub clients: HashMap<RawFd, Vec<u8>>,
}

impl IpcHandler {
    pub fn new() -> Self { Self { clients: HashMap::new() } }

    pub fn accept(&mut self, listener_raw: RawFd, data: &mut DriftWm) {
        let listener = unsafe { StdUnixListener::from_raw_fd(listener_raw) };
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true).ok();
                    let raw = stream.as_fd().as_raw_fd();
                    self.clients.insert(raw, Vec::new());
                    mem::forget(stream);
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        mem::forget(listener);
    }

    pub fn handle_clients(&mut self, data: &mut DriftWm) {
        let mut completed = Vec::new();
        let to_process: Vec<(RawFd, String)>;

        {
            for (&fd, buf) in &mut self.clients {
                let mut stream = unsafe { StdUnixStream::from_raw_fd(fd) };
                let mut chunk = [0u8; 256];
                match stream.read(&mut chunk) {
                    Ok(0) => { completed.push(fd); }
                    Ok(n) => { buf.extend_from_slice(&chunk[..n]); }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => { completed.push(fd); }
                }
                mem::forget(stream);
            }
        }

        to_process = {
            let mut result = Vec::new();
            for (&fd, buf) in &self.clients {
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line = String::from_utf8_lossy(&buf[..pos]).trim().to_string();
                    result.push((fd, line));
                    completed.push(fd);
                }
            }
            result
        };

        for (fd, line) in to_process {
            let cmd = line.trim();
            let (cmd, args) = cmd.split_once(' ').unwrap_or((cmd, ""));
            let args = args.trim();

            let mut stream = unsafe { StdUnixStream::from_raw_fd(fd) };
            let response = handle_command(data, cmd, args);
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.shutdown(std::net::Shutdown::Both);
            mem::forget(stream);
            completed.push(fd);
        }

        for fd in completed {
            self.clients.remove(&fd);
        }
    }
}

fn handle_command(data: &mut DriftWm, cmd: &str, args: &str) -> String {
    match cmd {
        "quit" => {
            data.loop_signal.stop();
            "ok\n".to_string()
        }
        "camera" => {
            let parts: Vec<&str> = args.split_whitespace().collect();
            if parts.len() >= 2 {
                if let (Ok(x), Ok(y)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                    let vp = data.get_viewport_size().to_f64();
                    let z = data.zoom();
                    let cx = x - vp.w / (2.0 * z);
                    let cy = -y + vp.h / (2.0 * z);
                    data.set_camera_target(Some(smithay::utils::Point::from((cx, cy))));
                    data.write_state_file_if_dirty();
                    return format!("ok {x:.0},{y:.0}\n");
                }
            }
            let vp = data.get_viewport_size().to_f64();
            let z = data.zoom();
            let cam = data.camera();
            let cx = cam.x + vp.w / (2.0 * z);
            let cy = -(cam.y + vp.h / (2.0 * z));
            format!("{:.0},{:.0}\n", cx, cy)
        }
        "zoom" => {
            if !args.is_empty() {
                if let Ok(z) = args.parse::<f64>() {
                    let z = z.clamp(canvas::MIN_ZOOM_FLOOR, canvas::MAX_ZOOM);
                    data.set_zoom_target(Some(z));
                    data.write_state_file_if_dirty();
                    return format!("ok {z:.3}\n");
                }
            }
            format!("{:.3}\n", data.zoom())
        }
        "layout" => {
            format!("{}\n", data.active_layout)
        }
        "state" => {
            let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
            let path = PathBuf::from(runtime).join("driftwm/state");
            match fs::read_to_string(&path) {
                Ok(content) => content,
                Err(e) => format!("error {e}\n"),
            }
        }
        "create-section" => {
            data.section_create_mode = true;
            "ok\n".to_string()
        }
        _ => format!("unknown: {cmd}\n"),
    }
}
