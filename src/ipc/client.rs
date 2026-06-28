//! The `driftwm msg` client: connect to the running compositor's IPC socket,
//! send one request, print the reply. Runs in the same binary but never starts
//! a compositor.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use super::protocol::{Reply, Request, Response, ScreenshotTarget, socket_path};

/// `driftwm msg <...>` subcommands. Variants with optional positionals read when
/// omitted and write when given.
#[derive(clap::Subcommand, Debug)]
pub enum Msg {
    /// Get the camera position, or set it (animated) with `<x> <y>` (viewport center, Y-up).
    #[command(allow_negative_numbers = true)]
    Camera { x: Option<f64>, y: Option<f64> },
    /// Get the zoom level, or set it with `<level>` (clamped to the supported range).
    Zoom { level: Option<f64> },
    /// Print the active keyboard layout (full XKB name, e.g. `English (US)`).
    Layout {
        /// Print the configured layout code instead (e.g. `us`, `ru`).
        #[arg(long)]
        short: bool,
    },
    /// Dump camera, zoom, and the window inventory.
    State,
    /// Print the focused window, or focus a window by app_id substring.
    Focus { app_id: Option<String> },
    /// Get the focused window position, or move it (center, Y-up) with `<x> <y>`.
    #[command(allow_negative_numbers = true)]
    Move { x: Option<i32>, y: Option<i32> },
    /// Run a config action, e.g. `action close-window`, `action quit`, `action switch-layout next`.
    #[command(allow_negative_numbers = true)]
    Action {
        /// Action and arguments, exactly as written in config (e.g. `nudge-window up`).
        #[arg(required = true, trailing_var_arg = true, num_args = 1..)]
        spec: Vec<String>,
    },
    /// Capture a canvas PNG (custom DPI). With no subcommand, captures the active
    /// output's current view of the canvas.
    Screenshot {
        #[command(subcommand)]
        target: Option<ShotTarget>,
        /// Pixels per canvas unit — higher captures more detail than the screen shows.
        #[arg(long, default_value_t = 1.0, global = true)]
        scale: f64,
        /// Output PNG path, or `-` for stdout [default: ./driftwm-screenshot-<time>.png].
        #[arg(short, long, global = true)]
        output: Option<String>,
    },
}

/// What `driftwm msg screenshot` captures.
#[derive(clap::Subcommand, Debug)]
pub enum ShotTarget {
    /// The focused window.
    Window,
    /// The bounding box of all windows.
    All,
    /// A rectangle — `X Y W H` (canvas coords, center/Y-up) or slurp's native
    /// `X,Y WxH`. Commas and the `x` separator are tolerated, so `$(slurp)`
    /// drops in directly. Treated as output-screen pixels with `--from-screen`.
    #[command(allow_negative_numbers = true)]
    Region {
        /// Four ints `X Y W H`, or slurp's `X,Y WxH` (quoted or not).
        #[arg(required = true, num_args = 1..=4)]
        coords: Vec<String>,
        /// Treat the rectangle as output-screen pixels mapped via the active viewport.
        #[arg(long)]
        from_screen: bool,
    },
}

pub fn run(msg: &Msg, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let request = to_request(msg)?;

    // `screenshot -o -`: the compositor writes a temp file (it can't stream over
    // the JSON socket), which we then relay to stdout and delete.
    let stdout_capture = matches!(msg, Msg::Screenshot { output: Some(o), .. } if o == "-");

    // A client launched inside a driftwm session inherits its WAYLAND_DISPLAY, so
    // the derived path targets that instance. DRIFTWM_SOCKET is an explicit
    // override (the server never reads it, so there's no nested-bind footgun).
    let path = match std::env::var_os("DRIFTWM_SOCKET") {
        Some(p) => PathBuf::from(p),
        None => {
            let display = std::env::var("WAYLAND_DISPLAY")
                .map_err(|_| "WAYLAND_DISPLAY is not set — are you in a driftwm session?")?;
            socket_path(&display)
        }
    };

    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("cannot connect to {}: {e}", path.display()))?;

    let mut payload = serde_json::to_vec(&request)?;
    payload.push(b'\n');
    stream.write_all(&payload)?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err("no response from compositor".into());
    }
    let reply: Reply = serde_json::from_str(line.trim_end())?;

    // `-o -` claims stdout for the PNG bytes, so it takes precedence over --json.
    // Clean up the temp file unconditionally, even if the read or write fails.
    if stdout_capture && let Ok(Response::Screenshot { path, .. }) = &reply {
        let bytes = std::fs::read(path);
        let _ = std::fs::remove_file(path);
        let bytes = bytes.map_err(|e| format!("cannot read capture {path}: {e}"))?;
        std::io::stdout().write_all(&bytes)?;
        return Ok(());
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&reply)?);
        // Exit non-zero on a command error too, so scripts can branch on it.
        if reply.is_err() {
            std::process::exit(1);
        }
        return Ok(());
    }

    match reply {
        Ok(response) => {
            print_response(response);
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn to_request(msg: &Msg) -> Result<Request, String> {
    Ok(match msg {
        Msg::Camera { x, y } => match (x, y) {
            (None, None) => Request::Camera(None),
            (Some(x), Some(y)) => Request::Camera(Some((*x, *y))),
            _ => return Err("camera needs both <x> and <y>".to_string()),
        },
        Msg::Zoom { level } => Request::Zoom(*level),
        Msg::Layout { short } => Request::Layout { short: *short },
        Msg::State => Request::State,
        Msg::Focus { app_id } => Request::Focus(app_id.clone()),
        Msg::Move { x, y } => match (x, y) {
            (None, None) => Request::Move(None),
            (Some(x), Some(y)) => Request::Move(Some((*x, *y))),
            _ => return Err("move needs both <x> and <y>".to_string()),
        },
        Msg::Action { spec } => Request::Action(spec.join(" ")),
        Msg::Screenshot {
            target,
            scale,
            output,
        } => {
            let target = match target {
                None => ScreenshotTarget::Viewport,
                Some(ShotTarget::Window) => ScreenshotTarget::Window,
                Some(ShotTarget::All) => ScreenshotTarget::All,
                Some(ShotTarget::Region {
                    coords,
                    from_screen,
                }) => {
                    let (x, y, w, h) = parse_region(coords)?;
                    ScreenshotTarget::Region {
                        x,
                        y,
                        w,
                        h,
                        from_screen: *from_screen,
                    }
                }
            };
            Request::Screenshot {
                target,
                scale: *scale,
                path: resolve_output_path(output)?,
            }
        }
    })
}

/// Parse a region rectangle, accepting both `X Y W H` and slurp's native
/// `X,Y WxH`. The comma and `x` separators are normalized to spaces, so
/// `$(slurp)` drops in whether shell-quoted (one token) or not (two tokens).
fn parse_region(tokens: &[String]) -> Result<(i32, i32, i32, i32), String> {
    let normalized = tokens.join(" ").replace([',', 'x'], " ");
    let nums = normalized
        .split_whitespace()
        .map(|t| t.parse::<i32>().map_err(|_| t.to_string()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|bad| {
            format!("region: '{bad}' is not an integer (expected X Y W H, or slurp's X,Y WxH)")
        })?;
    match nums.as_slice() {
        [x, y, w, h] => Ok((*x, *y, *w, *h)),
        _ => Err(format!(
            "region needs 4 values (X Y W H, or slurp's X,Y WxH), got {}",
            nums.len()
        )),
    }
}

/// Resolve the output path the compositor will write to. It must be absolute —
/// the compositor's working directory differs from the client's.
fn resolve_output_path(output: &Option<String>) -> Result<String, String> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let raw = match output.as_deref() {
        // `-` → a temp file the client streams to stdout, then deletes.
        Some("-") => std::env::temp_dir().join(format!(
            "driftwm-screenshot-{}-{secs}.png",
            std::process::id()
        )),
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(format!("driftwm-screenshot-{secs}.png")),
    };
    let abs = if raw.is_absolute() {
        raw
    } else {
        std::env::current_dir()
            .map_err(|e| format!("cannot resolve current directory: {e}"))?
            .join(raw)
    };
    Ok(abs.to_string_lossy().into_owned())
}

fn print_response(response: Response) {
    match response {
        Response::Camera { x, y } => println!("camera {x} {y}"),
        Response::Zoom(zoom) => println!("zoom {zoom}"),
        Response::Layout(layout) => println!("{layout}"),
        Response::Focused(Some(app_id)) => println!("{app_id}"),
        Response::Focused(None) => println!("(none)"),
        Response::Position { x, y } => println!("{x} {y}"),
        Response::Screenshot { path, .. } => println!("{path}"),
        Response::Ok => println!("ok"),
        Response::State {
            camera,
            zoom,
            windows,
            fullscreen,
            pinned,
        } => {
            println!("camera {} {}", camera.0, camera.1);
            println!("zoom {zoom}");
            println!("windows {}", windows.len());
            for w in windows {
                let mark = if w.is_focused { "*" } else { " " };
                let title = if w.title.is_empty() {
                    String::new()
                } else {
                    format!("  \"{}\"", w.title)
                };
                println!(
                    "  {mark} {} [{}, {}] {}x{}{}",
                    w.app_id, w.position[0], w.position[1], w.size[0], w.size[1], title
                );
            }
            println!("fullscreen {}", fullscreen.len());
            for f in fullscreen {
                let title = if f.title.is_empty() {
                    String::new()
                } else {
                    format!("  \"{}\"", f.title)
                };
                println!("  {} {}{}", f.output, f.app_id, title);
            }
            println!("pinned {}", pinned.len());
            for p in pinned {
                let title = if p.title.is_empty() {
                    String::new()
                } else {
                    format!("  \"{}\"", p.title)
                };
                println!(
                    "  {} {} [{}, {}] {}x{}{}",
                    p.output, p.app_id, p.position[0], p.position[1], p.size[0], p.size[1], title
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_region;

    fn tokens(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn region_four_ints() {
        assert_eq!(
            parse_region(&tokens("0 0 2000 1500")).unwrap(),
            (0, 0, 2000, 1500)
        );
    }

    #[test]
    fn region_negative_canvas_coords() {
        assert_eq!(
            parse_region(&tokens("-100 -200 300 400")).unwrap(),
            (-100, -200, 300, 400)
        );
    }

    #[test]
    fn region_slurp_unquoted() {
        // `$(slurp)` without quotes expands to two tokens.
        assert_eq!(
            parse_region(&tokens("1340,1135 768x361")).unwrap(),
            (1340, 1135, 768, 361)
        );
    }

    #[test]
    fn region_slurp_quoted() {
        // `"$(slurp)"` is a single token containing a space.
        let one = vec!["1340,1135 768x361".to_string()];
        assert_eq!(parse_region(&one).unwrap(), (1340, 1135, 768, 361));
    }

    #[test]
    fn region_wrong_count_errors() {
        assert!(parse_region(&tokens("0 0 100")).is_err());
        assert!(parse_region(&tokens("0 0 100 200 300")).is_err());
    }

    #[test]
    fn region_non_integer_errors() {
        assert!(parse_region(&tokens("a b c d")).is_err());
    }
}
