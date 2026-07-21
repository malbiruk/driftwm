//! The `driftwm msg` client: connect to the running compositor's IPC socket,
//! send one request, print the reply. Runs in the same binary but never starts
//! a compositor.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use super::protocol::{
    Event, Reply, Request, Response, ScreenshotTarget, StateInfo, WindowSelector, socket_path,
};

/// `driftwm msg <...>` subcommands. Variants with optional positionals read when
/// omitted and write when given.
#[derive(clap::Subcommand, Debug)]
pub enum Msg {
    /// Get the camera position, or set it (animated) with `<x> <y>` (viewport center, Y-up).
    ///
    /// With no arguments, prints the current camera position. With `<x> <y>`,
    /// pans the viewport (animated) to center that canvas point; positive `y` is
    /// up.
    ///
    /// Reply: `{"Ok":{"Camera":{"x":500.0,"y":300.0}}}`.
    #[command(allow_negative_numbers = true)]
    Camera { x: Option<f64>, y: Option<f64> },
    /// Get the zoom level, or set it with `<level>` (clamped to the supported range).
    ///
    /// Setting is animated and clamped to the supported range (out to fit-all,
    /// in to native resolution — no magnification).
    ///
    /// Reply: `{"Ok":{"Zoom":0.5}}`.
    Zoom { level: Option<f64> },
    /// Print the active keyboard layout (full XKB name, e.g. `English (US)`).
    ///
    /// With `--short`, prints the configured layout code for the active group
    /// (e.g. `us`, `ru`) instead — what most status bars want.
    ///
    /// Reply: `{"Ok":{"Layout":"English (US)"}}` (or `"us"` with `--short`).
    Layout {
        /// Print the configured layout code instead (e.g. `us`, `ru`).
        #[arg(long)]
        short: bool,
    },
    /// Dump camera, zoom, and the window inventory.
    ///
    /// Prints camera, zoom, keyboard layout, and every window — each with a
    /// stable `id` usable as a selector for `focus`/`move`/`close`/`screenshot
    /// window` — plus fullscreen, pinned, layer-shell, and per-output details.
    ///
    /// Reply: `{"Ok":{"State":{"camera":[..],"zoom":1.0,"windows":[..],"outputs":[..]}}}`.
    State,
    /// Print internal collection sizes for leak diagnosis (unstable keys).
    ///
    /// An introspection endpoint, not a stable interface: the keys are internal
    /// field names that can change between releases. Meant for leak diagnosis —
    /// a window/surface/client-keyed count should return to its idle baseline
    /// once the windows and clients that raised it are gone (output-keyed
    /// counters follow output lifetimes instead and can persist across hotplug).
    ///
    /// Reply: `{"Ok":{"DebugCounters":{"decorations":2,"stage_entries":2}}}`.
    DebugCounters,
    /// Stream state snapshots as they change (one JSON line per event with --json).
    ///
    /// Turns the connection into a live feed: the server acks, then pushes one
    /// event with the current state immediately and again on every change —
    /// including camera/zoom, the window list, focus, window titles, keyboard
    /// layout, a per-output viewport, the screen-space inventory (pinned and
    /// fullscreen windows — dragging a pinned window pushes events), and
    /// layer/canvas-layer changes. While something animates an event is pushed
    /// per rendered frame (not throttled like the state file), so a pan or drag
    /// streams at the compositor's frame rate. Runs until interrupted.
    ///
    /// Each event is `{"State":{..}}` — the whole snapshot, same shape as the
    /// `state` reply, and not wrapped in `Ok`/`Err`. A slow subscriber never
    /// blocks the compositor: it drops snapshots and catches up in full on the
    /// next change.
    Subscribe,
    /// Print the focused window, or focus a window by app_id substring or `--id`
    /// (the stable id shown in `state`).
    ///
    /// With no argument, prints the focused window's `id` and `app_id`. Given an
    /// `app_id` substring (case-insensitive) or `--id <n>`, focuses that window,
    /// navigating to it only if it is off-screen.
    ///
    /// Reply: `{"Ok":{"Focused":{"id":5,"app_id":"alacritty"}}}` (or `{"Ok":{"Focused":null}}`).
    Focus {
        app_id: Option<String>,
        /// Focus the window with this stable id (from `state`).
        #[arg(long, conflicts_with = "app_id")]
        id: Option<u64>,
    },
    /// Get a window's position, or move it (center, Y-up) with `<x> <y>`. Targets
    /// the focused window, or `--id` (the stable id shown in `state`).
    ///
    /// Positions are a center point with `y` pointing up. Pinned and fullscreen
    /// windows live in screen space, not on the canvas, so `move` refuses to
    /// reposition them.
    ///
    /// Reply: `{"Ok":{"Position":{"x":100,"y":200}}}`.
    #[command(allow_negative_numbers = true)]
    Move {
        x: Option<i32>,
        y: Option<i32>,
        /// Target the window with this stable id (from `state`).
        #[arg(long)]
        id: Option<u64>,
    },
    /// Get a window's opacity, or set it with `<value>` (0.0–1.0). Targets the
    /// focused window, or `--id` (the stable id shown in `state`).
    ///
    /// `0.0` is transparent, `1.0` opaque. Applies to any rendered window, pinned
    /// and fullscreen included; the change takes effect next frame. The value is
    /// runtime-only — seeded from an `opacity` window rule at map time, held for
    /// the session, and never persisted (it resets when the window or compositor
    /// restarts). Values outside `0.0`–`1.0` are rejected, not clamped; a window
    /// no rule touched reads `1`.
    ///
    /// Reply: `{"Ok":{"Opacity":0.85}}`.
    Opacity {
        value: Option<f64>,
        /// Target the window with this stable id (from `state`).
        #[arg(long)]
        id: Option<u64>,
    },
    /// Close the focused window, or a window by app_id substring or `--id`.
    ///
    /// Targets the focused window by default, or a window by `app_id` substring
    /// (case-insensitive) or `--id <n>` (from `state`). Errors when nothing
    /// matches.
    ///
    /// Reply: `{"Ok":"Ok"}`.
    Close {
        app_id: Option<String>,
        /// Close the window with this stable id (from `state`).
        #[arg(long, conflicts_with = "app_id")]
        id: Option<u64>,
    },
    /// Run a config action, e.g. `action close-window`, `action quit`, `action switch-layout next`.
    ///
    /// Runs any compositor action by the same string you would write in a config
    /// keybinding, parsed with the exact config parser, so every keybindable
    /// action is reachable. Replies `Ok` whenever the spec parses — even if it
    /// had no effect (e.g. `close-window` with nothing focused); only an
    /// unparseable spec errors.
    ///
    /// The dedicated `msg` commands are the state you can read or set; every
    /// one-shot operation (close a window, quit, zoom a step) lives here under
    /// `action`. Window actions target the focused window, so to act on a
    /// specific one, `focus` it first — or use the `--id` selector on
    /// `focus`/`move`/`close`/`screenshot window` to target any window without
    /// the focus-first dance.
    ///
    /// The socket is a full control surface: `action` can `exec`/`spawn`, `quit`,
    /// and `reload-config`. It is safe only because the socket is `0600`.
    ///
    /// Reply: `{"Ok":"Ok"}`.
    #[command(allow_negative_numbers = true)]
    Action {
        /// Action and arguments, exactly as written in config (e.g. `nudge-window up`).
        #[arg(required = true, trailing_var_arg = true, num_args = 1..)]
        spec: Vec<String>,
    },
    /// Capture a canvas PNG (custom DPI). With no subcommand, captures the active
    /// output's current view of the canvas.
    ///
    /// A canvas capture, not a screen grab: it re-renders a virtual viewport onto
    /// the canvas, reaching off-screen content at any resolution. Windows get
    /// full chrome (title bar, border, shadow); panels/layer-shells and blur are
    /// not drawn (use `grim` for a literal grab). `-o -` streams the PNG to
    /// stdout (e.g. `screenshot window -o - | wl-copy`).
    ///
    /// Blur caveat: a scene capture (viewport/`all`/`region`) shows a translucent
    /// window over a sharp backdrop, never a blurred one; a `window` capture keeps
    /// the translucency over transparent pixels. A gigapixel TIFF wallpaper uses
    /// a coarse pyramid level, softening at extreme `--scale`. Captures tile
    /// internally but cap at 16384 px/side.
    ///
    /// Reply: `{"Ok":{"Screenshot":{"path":"/abs/shot.png","width":1920,"height":1080}}}`.
    Screenshot {
        #[command(subcommand)]
        target: Option<ShotTarget>,
        /// Pixels per canvas unit — higher captures more detail than the screen shows, independent of zoom.
        #[arg(long, default_value_t = 1.0, global = true)]
        scale: f64,
        /// Output PNG path, or `-` for stdout [default: `./driftwm-screenshot-<time>.png`].
        #[arg(short, long, global = true)]
        output: Option<String>,
    },
}

/// What `driftwm msg screenshot` captures.
#[derive(clap::Subcommand, Debug)]
pub enum ShotTarget {
    /// The focused window, or a window by app_id substring or `--id`.
    ///
    /// Composed alone on transparency, so overlapping windows never appear;
    /// pinned and fullscreen windows capture like any other (a fullscreen window
    /// has no chrome). Reply shape is the shared `Screenshot` reply above.
    Window {
        app_id: Option<String>,
        /// Capture the window with this stable id (from `state`).
        #[arg(long, conflicts_with = "app_id")]
        id: Option<u64>,
    },
    /// The bounding box of all non-widget windows.
    ///
    /// A scene with the canvas background plus every window's chrome, framed with
    /// a `[zoom] fit_padding` margin. Reply shape is the shared `Screenshot`
    /// reply above.
    All,
    /// A rectangle — `X Y W H` (canvas coords, center/Y-up) or slurp's native
    /// `X,Y WxH`. Commas and the `x` separator are tolerated, so `$(slurp)`
    /// drops in directly. Treated as output-screen pixels with `--from-screen`.
    ///
    /// Captures a scene (canvas background plus window chrome) over the rectangle.
    /// Reply shape is the shared `Screenshot` reply above.
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

    // Subscribe switches to push mode: the first reply is just the ack, then the
    // server streams `Event` lines on the same connection until it closes.
    if matches!(msg, Msg::Subscribe) {
        if json && reply.is_err() {
            // Same error surface as every other --json command.
            println!("{}", serde_json::to_string_pretty(&reply)?);
            std::process::exit(1);
        }
        reply?;
        return stream_events(reader, json);
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

/// Build a window selector from a subcommand's `app_id`/`--id` pair; clap's
/// `conflicts_with` guarantees at most one is set (id wins if both are).
fn window_selector(app_id: &Option<String>, id: Option<u64>) -> Option<WindowSelector> {
    match (id, app_id) {
        (Some(n), _) => Some(WindowSelector::Id(n)),
        (None, Some(s)) => Some(WindowSelector::AppId(s.clone())),
        (None, None) => None,
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
        Msg::DebugCounters => Request::DebugCounters,
        Msg::Subscribe => Request::Subscribe,
        Msg::Focus { app_id, id } => Request::Focus(window_selector(app_id, *id)),
        Msg::Move { x, y, id } => {
            let to = match (x, y) {
                (None, None) => None,
                (Some(x), Some(y)) => Some((*x, *y)),
                _ => return Err("move needs both <x> and <y>".to_string()),
            };
            Request::Move {
                window: id.map(WindowSelector::Id),
                to,
            }
        }
        Msg::Opacity { value, id } => {
            // serde_json serializes non-finite floats as null, which the server
            // would read back as a get instead of rejecting — refuse them here.
            if value.is_some_and(|v| !v.is_finite()) {
                return Err("opacity must be between 0.0 and 1.0".to_string());
            }
            Request::Opacity {
                window: id.map(WindowSelector::Id),
                value: *value,
            }
        }
        Msg::Close { app_id, id } => Request::Close(window_selector(app_id, *id)),
        Msg::Action { spec } => Request::Action(spec.join(" ")),
        Msg::Screenshot {
            target,
            scale,
            output,
        } => {
            let target = match target {
                None => ScreenshotTarget::Viewport,
                Some(ShotTarget::Window { app_id, id }) => ScreenshotTarget::Window {
                    window: window_selector(app_id, *id),
                },
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

/// Read pushed `Event` lines until the server closes the connection, printing
/// each one (raw JSON with `--json`, else the human-readable block). Flushes
/// per event so a downstream pipe (jq, a script) sees each snapshot promptly.
fn stream_events(
    mut reader: BufReader<UnixStream>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(()); // server closed the connection
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if json {
            println!("{trimmed}");
        } else {
            let Event::State(info) = serde_json::from_str::<Event>(trimmed)?;
            print_state(&info);
            println!();
        }
        std::io::stdout().flush()?;
    }
}

fn print_response(response: Response) {
    match response {
        Response::Camera { x, y } => println!("camera {x} {y}"),
        Response::Zoom(zoom) => println!("zoom {zoom}"),
        Response::Layout(layout) => println!("{layout}"),
        Response::Focused(Some(w)) => {
            println!("#{} {}", w.id, w.app_id.as_deref().unwrap_or("(no app_id)"))
        }
        Response::Focused(None) => println!("(none)"),
        Response::Position { x, y } => println!("{x} {y}"),
        Response::Opacity(value) => println!("{value}"),
        Response::Screenshot { path, .. } => println!("{path}"),
        Response::Ok => println!("ok"),
        Response::State(info) => print_state(&info),
        Response::DebugCounters(counters) => {
            for (key, value) in counters {
                println!("{key}: {value}");
            }
        }
    }
}

fn print_state(info: &StateInfo) {
    println!("camera {} {}", info.camera.0, info.camera.1);
    println!("zoom {}", info.zoom);
    println!("layout {} ({})", info.layout, info.layout_short);
    println!("windows {}", info.windows.len());
    for w in &info.windows {
        let mark = if w.is_focused { "*" } else { " " };
        let title = if w.title.is_empty() {
            String::new()
        } else {
            format!("  \"{}\"", w.title)
        };
        println!(
            "  {mark} #{} {} [{}, {}] {}x{}{}",
            w.id, w.app_id, w.position[0], w.position[1], w.size[0], w.size[1], title
        );
    }
    println!("fullscreen {}", info.fullscreen.len());
    for f in &info.fullscreen {
        let title = if f.title.is_empty() {
            String::new()
        } else {
            format!("  \"{}\"", f.title)
        };
        println!("  {} #{} {}{}", f.output, f.id, f.app_id, title);
    }
    println!("pinned {}", info.pinned.len());
    for p in &info.pinned {
        let title = if p.title.is_empty() {
            String::new()
        } else {
            format!("  \"{}\"", p.title)
        };
        println!(
            "  {} #{} {} [{}, {}] {}x{}{}",
            p.output, p.id, p.app_id, p.position[0], p.position[1], p.size[0], p.size[1], title
        );
    }
    println!("layers {}", info.layers.len());
    for ns in &info.layers {
        println!("    {ns}");
    }
    println!("canvas-layers {}", info.canvas_layers.len());
    for c in &info.canvas_layers {
        println!(
            "    {} [{}, {}] {}x{}",
            c.app_id, c.position[0], c.position[1], c.size[0], c.size[1]
        );
    }
    println!("outputs {}", info.outputs.len());
    for o in &info.outputs {
        let mark = if o.active { "*" } else { " " };
        println!(
            "  {mark} {} camera {} {} zoom {} {}x{}",
            o.name, o.camera.0, o.camera.1, o.zoom, o.size[0], o.size[1]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::protocol::{Request, ScreenshotTarget, WindowSelector};
    use super::{Msg, ShotTarget, parse_region, to_request};

    fn tokens(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn focus_maps_app_id_and_id() {
        assert_eq!(
            to_request(&Msg::Focus {
                app_id: None,
                id: None
            })
            .unwrap(),
            Request::Focus(None)
        );
        assert_eq!(
            to_request(&Msg::Focus {
                app_id: Some("term".into()),
                id: None
            })
            .unwrap(),
            Request::Focus(Some(WindowSelector::AppId("term".into())))
        );
        assert_eq!(
            to_request(&Msg::Focus {
                app_id: None,
                id: Some(5)
            })
            .unwrap(),
            Request::Focus(Some(WindowSelector::Id(5)))
        );
    }

    #[test]
    fn subscribe_maps_to_request() {
        assert_eq!(to_request(&Msg::Subscribe).unwrap(), Request::Subscribe);
    }

    #[test]
    fn close_maps_default_and_id() {
        assert_eq!(
            to_request(&Msg::Close {
                app_id: None,
                id: None
            })
            .unwrap(),
            Request::Close(None)
        );
        assert_eq!(
            to_request(&Msg::Close {
                app_id: None,
                id: Some(7)
            })
            .unwrap(),
            Request::Close(Some(WindowSelector::Id(7)))
        );
    }

    #[test]
    fn move_maps_id_and_coords() {
        assert_eq!(
            to_request(&Msg::Move {
                x: Some(10),
                y: Some(20),
                id: Some(3)
            })
            .unwrap(),
            Request::Move {
                window: Some(WindowSelector::Id(3)),
                to: Some((10, 20))
            }
        );
        assert_eq!(
            to_request(&Msg::Move {
                x: None,
                y: None,
                id: None
            })
            .unwrap(),
            Request::Move {
                window: None,
                to: None
            }
        );
        // A lone coordinate is still an error.
        assert!(
            to_request(&Msg::Move {
                x: Some(1),
                y: None,
                id: None
            })
            .is_err()
        );
    }

    #[test]
    fn opacity_maps_value_and_id() {
        assert_eq!(
            to_request(&Msg::Opacity {
                value: None,
                id: None
            })
            .unwrap(),
            Request::Opacity {
                window: None,
                value: None
            }
        );
        assert_eq!(
            to_request(&Msg::Opacity {
                value: Some(0.5),
                id: None
            })
            .unwrap(),
            Request::Opacity {
                window: None,
                value: Some(0.5)
            }
        );
        assert_eq!(
            to_request(&Msg::Opacity {
                value: None,
                id: Some(3)
            })
            .unwrap(),
            Request::Opacity {
                window: Some(WindowSelector::Id(3)),
                value: None
            }
        );
        assert_eq!(
            to_request(&Msg::Opacity {
                value: Some(0.25),
                id: Some(3)
            })
            .unwrap(),
            Request::Opacity {
                window: Some(WindowSelector::Id(3)),
                value: Some(0.25)
            }
        );
        assert!(
            to_request(&Msg::Opacity {
                value: Some(f64::NAN),
                id: None
            })
            .is_err()
        );
        assert!(
            to_request(&Msg::Opacity {
                value: Some(f64::INFINITY),
                id: None
            })
            .is_err()
        );
    }

    #[test]
    fn screenshot_window_maps_selector() {
        let req = to_request(&Msg::Screenshot {
            target: Some(ShotTarget::Window {
                app_id: None,
                id: Some(2),
            }),
            scale: 1.0,
            output: Some("/tmp/x.png".into()),
        })
        .unwrap();
        let Request::Screenshot { target, .. } = req else {
            panic!("expected screenshot request");
        };
        assert_eq!(
            target,
            ScreenshotTarget::Window {
                window: Some(WindowSelector::Id(2))
            }
        );
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
