use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode};
use smithay::reexports::rustix::net::{
    bind, listen, socket, AddressFamily, SocketAddrUnix, SocketType,
};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::{fs, io, mem};

use crate::state::DriftWm;
use driftwm::canvas;

pub fn socket_path() -> PathBuf {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(runtime).join("driftwm/ipc.sock")
}

pub fn create_ipc_listener() -> io::Result<(RawFd, Generic<OwnedFd>)> {
    let path = socket_path();
    if path.exists() {
        let _ = fs::remove_file(&path);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let sock = socket(AddressFamily::UNIX, SocketType::STREAM, None)?;
    let addr = SocketAddrUnix::new(&path)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    bind(&sock, &addr)?;
    listen(&sock, 8)?;

    let fd: OwnedFd = sock.into();
    let raw = fd.as_raw_fd();
    let source = Generic::new(fd, Interest::READ, Mode::Level);
    Ok((raw, source))
}

fn accept_ipc(listener_fd: RawFd) -> io::Result<StdUnixStream> {
    let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(listener_fd) };
    let (stream, _) = listener.accept()?;
    stream.set_nonblocking(false)?;
    let _ = mem::ManuallyDrop::new(listener);
    Ok(stream)
}

pub fn handle_client(data: &mut DriftWm, listener_fd: RawFd) {
    let stream = match accept_ipc(listener_fd) {
        Ok(s) => s,
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return,
        Err(_) => return,
    };

    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let trimmed = line.trim();
    let (cmd, args) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
    let args = args.trim();

    let mut writer = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };

    match cmd {
        "quit" => {
            let _ = writeln!(writer, "ok");
            data.loop_signal.stop();
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
                    let _ = writeln!(writer, "ok {x:.0},{y:.0}");
                    return;
                }
            }
            let vp = data.get_viewport_size().to_f64();
            let z = data.zoom();
            let cam = data.camera();
            let cx = cam.x + vp.w / (2.0 * z);
            let cy = -(cam.y + vp.h / (2.0 * z));
            let _ = writeln!(writer, "{:.0},{:.0}", cx, cy);
        }

        "zoom" => {
            if !args.is_empty() {
                if let Ok(z) = args.parse::<f64>() {
                    let z = z.clamp(canvas::MIN_ZOOM_FLOOR, canvas::MAX_ZOOM);
                    data.set_zoom_target(Some(z));
                    data.write_state_file_if_dirty();
                    let _ = writeln!(writer, "ok {z:.3}");
                    return;
                }
            }
            let _ = writeln!(writer, "{:.3}", data.zoom());
        }

        "layout" => {
            if !args.is_empty() {
                data.active_layout = args.to_string();
                data.write_state_file_if_dirty();
                let _ = writeln!(writer, "ok {args}");
                return;
            }
            let _ = writeln!(writer, "{}", data.active_layout);
        }

        "state" => {
            let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
            let path = PathBuf::from(runtime).join("driftwm/state");
            match fs::read_to_string(&path) {
                Ok(content) => {
                    let _ = write!(writer, "{content}");
                }
                Err(e) => {
                    let _ = writeln!(writer, "error {e}");
                }
            }
        }

        _ => {
            let _ = writeln!(writer, "unknown: {cmd}");
        }
    }
}
