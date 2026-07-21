//! Shared IPC protocol types for the compositor and the `driftwm msg` client.
//!
//! The transport is line-delimited JSON over a Unix socket: one `Request` per
//! line, one `Reply` per line. Keeping it JSON means the socket is debuggable
//! with `socat` and usable from any scripting language, not just `driftwm msg`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Selects a window: by stable id (JSON number) or case-insensitive `app_id`
/// substring (JSON string). Untagged, so the wire form is just `5` or `"term"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WindowSelector {
    Id(u64),
    AppId(String),
}

/// A command from a client to the compositor. Variants carrying `Option<_>` read
/// when `None` and write when `Some`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// Coordinates are the viewport center, Y-up.
    Camera(Option<(f64, f64)>),
    /// Set value is clamped to the supported range (out to fit-all, in to native
    /// — no magnification).
    Zoom(Option<f64>),
    /// `short` reports the configured `input.keyboard.layout` code for the active
    /// group (e.g. `ru`) instead of the full XKB display name (e.g. `Russian`).
    Layout {
        short: bool,
    },
    State,
    /// Sizes of the compositor's leak-prone internal collections, keyed by
    /// field name. A debug/introspection endpoint — the keys are unstable
    /// implementation detail, not a compatibility surface.
    DebugCounters,
    /// Switches the connection to push mode: after the `Ok` reply the server
    /// writes one [`Event`] line immediately and another on every rendered frame
    /// while state keeps changing. The ~10 Hz throttle governs only the state
    /// file, not these events.
    Subscribe,
    /// Focus a window when `Some` (by [`WindowSelector`]); read the focused
    /// window's `app_id` when `None`.
    Focus(Option<WindowSelector>),
    /// Move or query a window. `window` `None` targets the focused window; `to`
    /// `None` reads the position instead of setting it. Coordinates are
    /// window-center, Y-up (the window-rule convention).
    Move {
        #[serde(default)]
        window: Option<WindowSelector>,
        #[serde(default)]
        to: Option<(i32, i32)>,
    },
    /// Get or set a window's opacity. `window` `None` targets the focused
    /// window; `value` `None` reads instead of setting it. The value is `0.0`
    /// (transparent) to `1.0` (opaque); a window with no stored rule reads `1.0`.
    Opacity {
        #[serde(default)]
        window: Option<WindowSelector>,
        #[serde(default)]
        value: Option<f64>,
    },
    /// Close a window (the focused one when `None`); errors when nothing matches.
    Close(Option<WindowSelector>),
    /// Run a config action by its config-grammar string, e.g. `"switch-layout
    /// next"`. Any keybindable action is reachable, so one-shot ops live here
    /// rather than as their own commands.
    Action(String),
    /// Capture to a PNG at `path` (absolute), at `scale` pixels per canvas unit.
    /// Windows render with full chrome; `region`/`all` include the background, a
    /// `window` capture stays transparent (see [`ScreenshotTarget`]).
    Screenshot {
        target: ScreenshotTarget,
        scale: f64,
        path: String,
    },
}

/// What a [`Request::Screenshot`] captures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScreenshotTarget {
    /// The active output's current viewport on the canvas — what's visible there
    /// (the default for a bare `screenshot`). Panels/layer-shells are excluded.
    Viewport,
    /// A single window (the focused one when `window` is `None`), isolated on
    /// transparency.
    Window {
        #[serde(default)]
        window: Option<WindowSelector>,
    },
    /// The bounding box of all non-widget windows.
    All,
    /// An explicit rectangle. Canvas coords are center/Y-up (the window-rule
    /// convention); with `from_screen`, `(x, y, w, h)` is an output-screen pixel
    /// rect (e.g. from `slurp`) mapped to the canvas via the active viewport.
    Region {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        from_screen: bool,
    },
}

/// A successful reply payload. Pairs with [`Request`] variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Camera {
        x: f64,
        y: f64,
    },
    Zoom(f64),
    Layout(String),
    State(StateInfo),
    DebugCounters(BTreeMap<String, usize>),
    Focused(Option<FocusedWindow>),
    /// Window-center, Y-up coordinates.
    Position {
        x: i32,
        y: i32,
    },
    /// A window's opacity in `0.0`–`1.0`.
    Opacity(f64),
    /// A written screenshot: absolute `path` and pixel dimensions.
    Screenshot {
        path: String,
        width: u32,
        height: u32,
    },
    Ok,
}

/// The result of a request: `Ok(Response)` or a human-readable error string.
pub type Reply = Result<Response, String>;

/// The focused window in a [`Response::Focused`] reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FocusedWindow {
    pub id: u64,
    pub app_id: Option<String>,
}

/// The full compositor state snapshot: the payload of a [`Response::State`]
/// reply and of a subscription [`Event::State`]. Whole-state, so a subscriber
/// can diff or re-render from each snapshot without tracking granular events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateInfo {
    pub camera: (f64, f64),
    pub zoom: f64,
    /// Active keyboard layout, full XKB name (e.g. `English (US)`).
    #[serde(default)]
    pub layout: String,
    /// The configured layout code for the active group (e.g. `us`).
    #[serde(default)]
    pub layout_short: String,
    pub windows: Vec<WindowInfo>,
    /// Fullscreen + screen-pinned windows, which live in screen space and
    /// are therefore excluded from `windows` (canvas coords). Empty when none.
    #[serde(default)]
    pub fullscreen: Vec<OutputFullscreen>,
    #[serde(default)]
    pub pinned: Vec<OutputPinned>,
    /// Namespaces of screen-space layer-shell surfaces (bars, OSKs,
    /// overlays) — the `app_id` a window rule matches them by.
    #[serde(default)]
    pub layers: Vec<String>,
    #[serde(default)]
    pub canvas_layers: Vec<CanvasLayerInfo>,
    /// Per-output camera state. `camera` is the viewport center, Y-up, like
    /// the top-level `camera`.
    #[serde(default)]
    pub outputs: Vec<OutputInfo>,
}

/// One output's viewport in the `state` reply. `camera` is the viewport
/// center, Y-up; `size` is the output's logical size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub name: String,
    pub camera: (f64, f64),
    pub zoom: f64,
    pub size: [i32; 2],
    pub active: bool,
}

/// A line pushed to a subscribed connection. Not wrapped in a `Reply` —
/// events are one-way and can't fail per-request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    State(StateInfo),
}

/// One window in the canvas inventory (`position` = window center, Y-up).
///
/// Shared by the IPC [`Response::State`] payload and the
/// `$XDG_RUNTIME_DIR/driftwm/state` file so the two representations can't drift.
/// `id` is the compositor-session-stable window handle, usable as
/// [`WindowSelector::Id`] to target this window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub position: [i32; 2],
    pub size: [i32; 2],
    pub is_focused: bool,
    pub is_widget: bool,
}

/// A fullscreen window in the IPC `state` reply — one per fullscreened output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputFullscreen {
    pub id: u64,
    pub output: String,
    pub app_id: String,
    pub title: String,
}

/// A screen-pinned window in the IPC `state` reply. `position` is the window
/// center in rule coordinates (output-center origin, Y-up) — the numbers a
/// `pinned_to_screen` rule's `position` takes; `size` in pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputPinned {
    pub id: u64,
    pub output: String,
    pub app_id: String,
    pub title: String,
    pub position: [i32; 2],
    pub size: [i32; 2],
}

/// A canvas-positioned layer surface, shared by the IPC `state` reply and the
/// state file's `canvas_layers=` line. `app_id` is the layer-shell namespace;
/// `position` uses rule coordinates (Y-up, window-centered), like
/// [`WindowInfo`]. The top-left anchor is frozen at map time while the center
/// is derived from the *current* size, so for a surface that grew after
/// mapping the reported center drifts from the rule that placed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanvasLayerInfo {
    pub app_id: String,
    pub position: [i32; 2],
    pub size: [i32; 2],
}

/// Path to the IPC socket for a given `WAYLAND_DISPLAY` name:
/// `$XDG_RUNTIME_DIR/driftwm/ipc-<wayland_display>.sock` (falls back to `/tmp`).
///
/// Deriving the name from the wayland display lets each compositor instance own
/// a distinct socket and lets `driftwm msg` auto-target the session it runs in.
pub fn socket_path(wayland_display: &str) -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir)
        .join("driftwm")
        .join(format!("ipc-{wayland_display}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(value, &back);
    }

    #[test]
    fn request_roundtrip() {
        for r in [
            Request::Camera(None),
            Request::Camera(Some((100.0, -200.0))),
            Request::Zoom(Some(2.0)),
            Request::Layout { short: false },
            Request::Layout { short: true },
            Request::State,
            Request::DebugCounters,
            Request::Subscribe,
            Request::Focus(None),
            Request::Focus(Some(WindowSelector::AppId("alacritty".into()))),
            Request::Focus(Some(WindowSelector::Id(5))),
            Request::Move {
                window: None,
                to: None,
            },
            Request::Move {
                window: None,
                to: Some((0, 0)),
            },
            Request::Move {
                window: Some(WindowSelector::Id(3)),
                to: None,
            },
            Request::Move {
                window: Some(WindowSelector::AppId("foot".into())),
                to: Some((100, 200)),
            },
            Request::Opacity {
                window: None,
                value: None,
            },
            Request::Opacity {
                window: None,
                value: Some(0.5),
            },
            Request::Opacity {
                window: Some(WindowSelector::Id(3)),
                value: None,
            },
            Request::Opacity {
                window: Some(WindowSelector::AppId("foot".into())),
                value: Some(0.75),
            },
            Request::Close(None),
            Request::Close(Some(WindowSelector::Id(7))),
            Request::Action("switch-layout next".into()),
            Request::Screenshot {
                target: ScreenshotTarget::Viewport,
                scale: 1.0,
                path: "/tmp/view.png".into(),
            },
            Request::Screenshot {
                target: ScreenshotTarget::Window { window: None },
                scale: 2.0,
                path: "/tmp/shot.png".into(),
            },
            Request::Screenshot {
                target: ScreenshotTarget::Window {
                    window: Some(WindowSelector::Id(2)),
                },
                scale: 2.0,
                path: "/tmp/shot.png".into(),
            },
            Request::Screenshot {
                target: ScreenshotTarget::Region {
                    x: -100,
                    y: 200,
                    w: 640,
                    h: 480,
                    from_screen: true,
                },
                scale: 1.0,
                path: "/tmp/region.png".into(),
            },
        ] {
            roundtrip(&r);
        }
    }

    #[test]
    fn selector_wire_forms_parse() {
        // Untagged: a bare string is an app_id, a bare number an id, null none.
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"Focus":"term"}"#).unwrap(),
            Request::Focus(Some(WindowSelector::AppId("term".into())))
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"Focus":5}"#).unwrap(),
            Request::Focus(Some(WindowSelector::Id(5)))
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"Focus":null}"#).unwrap(),
            Request::Focus(None)
        );
        // Move fields both default when omitted.
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"Move":{}}"#).unwrap(),
            Request::Move {
                window: None,
                to: None
            }
        );
        // Opacity fields both default too: bare `{}` is the focused-window read.
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"Opacity":{}}"#).unwrap(),
            Request::Opacity {
                window: None,
                value: None
            }
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"Opacity":{"window":5,"value":0.5}}"#).unwrap(),
            Request::Opacity {
                window: Some(WindowSelector::Id(5)),
                value: Some(0.5)
            }
        );
        // Screenshot window target defaults its selector too.
        assert_eq!(
            serde_json::from_str::<ScreenshotTarget>(r#"{"Window":{}}"#).unwrap(),
            ScreenshotTarget::Window { window: None }
        );
    }

    fn sample_state() -> StateInfo {
        StateInfo {
            camera: (0.0, 0.0),
            zoom: 1.0,
            layout: "English (US)".into(),
            layout_short: "us".into(),
            windows: vec![WindowInfo {
                id: 1,
                app_id: "foo".into(),
                title: "bar".into(),
                position: [10, -20],
                size: [640, 480],
                is_focused: true,
                is_widget: false,
            }],
            fullscreen: vec![OutputFullscreen {
                id: 2,
                output: "DP-1".into(),
                app_id: "mpv".into(),
                title: "video".into(),
            }],
            pinned: vec![OutputPinned {
                id: 3,
                output: "HDMI-A-1".into(),
                app_id: "pavucontrol".into(),
                title: "Volume".into(),
                position: [20, 40],
                size: [320, 240],
            }],
            layers: vec!["waybar".into()],
            canvas_layers: vec![CanvasLayerInfo {
                app_id: "drift-clock".into(),
                position: [0, 200],
                size: [311, 136],
            }],
            outputs: vec![OutputInfo {
                name: "DP-1".into(),
                camera: (-960.0, -600.0),
                zoom: 1.0,
                size: [1920, 1200],
                active: true,
            }],
        }
    }

    #[test]
    fn reply_roundtrip() {
        let replies: Vec<Reply> = vec![
            Ok(Response::Camera { x: 1.0, y: 2.0 }),
            Ok(Response::Zoom(1.5)),
            Ok(Response::State(sample_state())),
            Ok(Response::DebugCounters(
                [("stage_entries".to_string(), 2usize)]
                    .into_iter()
                    .collect(),
            )),
            Ok(Response::Focused(None)),
            Ok(Response::Focused(Some(FocusedWindow {
                id: 5,
                app_id: Some("foot".into()),
            }))),
            Ok(Response::Opacity(0.5)),
            Ok(Response::Ok),
            Err("no focused window".into()),
        ];
        for reply in &replies {
            let json = serde_json::to_string(reply).unwrap();
            let back: Reply = serde_json::from_str(&json).unwrap();
            assert_eq!(reply, &back);
        }
    }

    #[test]
    fn event_roundtrip() {
        roundtrip(&Event::State(sample_state()));
    }

    /// The `State` reply serializes as `{"State":{...}}` with every field present.
    #[test]
    fn state_response_wire_shape() {
        let json = serde_json::to_value(Response::State(sample_state())).unwrap();
        let obj = json
            .get("State")
            .expect("State serializes as {\"State\":{...}}");
        for key in [
            "camera",
            "zoom",
            "layout",
            "layout_short",
            "windows",
            "fullscreen",
            "pinned",
            "layers",
            "canvas_layers",
            "outputs",
        ] {
            assert!(obj.get(key).is_some(), "missing key {key}");
        }
    }

    /// docs/ipc.md promises the pushed `State` event payload is identical to
    /// the `state` reply's. The inner `StateInfo` is shared by construction,
    /// but the `State` wire key comes from two independently-named enum
    /// variants — pin them together.
    #[test]
    fn event_payload_matches_state_reply() {
        let response = serde_json::to_value(Response::State(sample_state())).unwrap();
        let event = serde_json::to_value(Event::State(sample_state())).unwrap();
        assert_eq!(response.get("State"), event.get("State"));
        assert!(response.get("State").is_some());
    }

    /// A reply from a compositor without the `layout`/`layout_short`/`outputs`
    /// fields still parses — that's what their `#[serde(default)]`s are for
    /// (a newer `driftwm msg` against an older compositor).
    #[test]
    fn state_reply_without_new_fields_parses() {
        let old = r#"{"Ok":{"State":{"camera":[0.0,0.0],"zoom":1.0,"windows":[
            {"id":1,"app_id":"foo","title":"","position":[0,0],"size":[1,1],
             "is_focused":false,"is_widget":false}]}}}"#;
        let reply: Reply = serde_json::from_str(old).unwrap();
        let Ok(Response::State(info)) = reply else {
            panic!("expected a State reply");
        };
        assert!(info.layout.is_empty());
        assert!(info.outputs.is_empty());
        assert_eq!(info.windows.len(), 1);
    }

    #[test]
    fn socket_path_uses_wayland_display() {
        // SAFETY: single-threaded test; no other thread reads the env here.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000") };
        assert_eq!(
            socket_path("wayland-1"),
            PathBuf::from("/run/user/1000/driftwm/ipc-wayland-1.sock")
        );
    }
}
