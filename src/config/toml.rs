use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ConfigFile {
    pub mod_key: Option<String>,
    pub focus_follows_mouse: Option<bool>,
    pub session: SessionFileConfig,
    pub input: InputConfig,
    pub cursor: CursorConfig,
    pub navigation: NavigationConfig,
    pub zoom: ZoomConfig,
    pub snap: SnapConfig,
    pub output: OutputConfig,
    pub background: BackgroundFileConfig,
    pub decorations: DecorationFileConfig,
    pub effects: EffectsFileConfig,
    pub backend: BackendFileConfig,
    pub autostart: Option<Vec<String>>,
    pub bindings: BindingsFileConfig,
    pub keybindings: Option<HashMap<String, String>>,
    pub mouse: MouseFileConfig,
    pub gestures: GestureFileConfig,
    pub touch: TouchFileConfig,
    pub env: HashMap<String, String>,
    pub xwayland: XwaylandConfig,
    /// Placement mode for newly mapped windows: `"center"` (default) or `"cursor"`.
    pub window_placement: Option<String>,
    pub window_rules: Option<Vec<WindowRuleFile>>,
    pub outputs: Option<Vec<OutputRuleFile>>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct BindingsFileConfig {
    pub disable_defaults: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct BackendFileConfig {
    pub wait_for_frame_completion: Option<bool>,
    pub disable_direct_scanout: Option<bool>,
    pub disable_hardware_cursor: Option<bool>,
    pub max_capture_fps: Option<u32>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct EffectsFileConfig {
    pub blur_radius: Option<u32>,
    pub blur_strength: Option<f64>,
    pub animate_blur: Option<bool>,
    pub animate_blur_fps: Option<u32>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct InputConfig {
    pub keyboard: KeyboardConfig,
    pub trackpad: TrackpadConfig,
    pub mouse: MouseDeviceFileConfig,
    pub touch: TouchDeviceFileConfig,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct TouchDeviceFileConfig {
    pub enable: Option<bool>,
    pub map_to_output: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct TrackpadConfig {
    pub tap_to_click: Option<bool>,
    pub natural_scroll: Option<bool>,
    pub tap_and_drag: Option<bool>,
    pub accel_speed: Option<f64>,
    pub accel_profile: Option<String>,
    pub click_method: Option<String>,
    pub disable_while_typing: Option<bool>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct MouseDeviceFileConfig {
    pub accel_speed: Option<f64>,
    pub accel_profile: Option<String>,
    pub natural_scroll: Option<bool>,
    pub left_handed: Option<bool>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct KeyboardConfig {
    pub repeat_rate: Option<i32>,
    pub repeat_delay: Option<i32>,
    pub layout: Option<String>,
    pub variant: Option<String>,
    pub options: Option<String>,
    pub model: Option<String>,
    pub layout_independent: Option<bool>,
    pub num_lock: Option<bool>,
    pub caps_lock: Option<bool>,
    pub remember_layout_per_window: Option<bool>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct CursorConfig {
    pub theme: Option<String>,
    pub size: Option<u32>,
    pub inactive_opacity: Option<f64>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct NavigationConfig {
    pub animation_speed: Option<f64>,
    pub auto_navigate_on_close: Option<bool>,
    pub auto_navigate_on_click: Option<bool>,
    pub nudge_step: Option<i32>,
    pub pan_step: Option<f64>,
    pub trackpad_speed: Option<f64>,
    pub mouse_speed: Option<f64>,
    pub touch_speed: Option<f64>,
    pub drift: Option<f64>,
    /// Renamed to `drift`; kept only so a stale value yields a migration error
    /// instead of failing the whole parse via `deny_unknown_fields`.
    pub friction: Option<f64>,
    pub anchors: Option<Vec<[f64; 2]>>,
    /// Named canvas points (Y-up, window-center convention) that seed the
    /// runtime bookmark registry. Absent → the compiled corner defaults;
    /// present-but-empty → no seeds (explicit-empty escape hatch).
    pub bookmarks: Option<HashMap<String, [f64; 2]>>,
    pub edge_pan: EdgePanConfig,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct EdgePanConfig {
    pub zone: Option<f64>,
    pub speed_min: Option<f64>,
    pub speed_max: Option<f64>,
    /// Enable cursor edge-pan at startup (pan when the bare cursor touches a
    /// screen edge, not just while dragging a window).
    pub cursor_pan: Option<bool>,
    /// Activation zone for cursor edge-pan, px from the edge (kept small so it
    /// doesn't trigger accidentally — distinct from the window-drag `zone`).
    pub cursor_zone: Option<f64>,
    /// Delay before edge-pan starts at an edge bordering another output (ms).
    /// Defaults to 120. Set to 0 to disable.
    pub latency_ms: Option<u64>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct SessionFileConfig {
    /// Convert client-initiated closes into suspended windows instead of
    /// destroying them. Per-window overridable via a `suspend_on_close` rule.
    pub suspend_on_close: Option<bool>,
    /// Save eligible windows on graceful shutdown and bring them back as
    /// suspended windows on the next launch.
    pub restore_windows: Option<bool>,
    /// Seed each output's camera and zoom from the durable session on the next
    /// launch. Off by default, so the default config starts every output at its
    /// centered camera.
    pub restore_camera: Option<bool>,
    /// Restore the bookmark registry from the durable session on the next
    /// launch, overlaying saved bookmarks on top of the config seeds. Off by
    /// default, so runtime bookmark edits don't persist across restarts.
    pub restore_bookmarks: Option<bool>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ZoomConfig {
    pub step: Option<f64>,
    pub fit_padding: Option<f64>,
    pub reset_on_new_window: Option<bool>,
    pub reset_on_activation: Option<bool>,
    pub touch_speed: Option<f64>,
    pub trackpad_speed: Option<f64>,
    pub mouse_speed: Option<f64>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct SnapConfig {
    pub enabled: Option<bool>,
    pub gap: Option<f64>,
    pub distance: Option<f64>,
    pub break_force: Option<f64>,
    pub corners: Option<bool>,
    pub centers: Option<bool>,
    /// Renamed to `corners`; kept only so a stale value yields a migration
    /// warning instead of failing the whole parse via `deny_unknown_fields`.
    pub same_edge: Option<bool>,
    /// Renamed to `centers`; kept only so a stale value yields a migration
    /// warning instead of failing the whole parse via `deny_unknown_fields`.
    pub edge_center: Option<bool>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct OutputConfig {
    pub outline: Option<OutputOutlineConfig>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct OutputOutlineConfig {
    pub color: Option<String>,
    pub thickness: Option<i32>,
    pub opacity: Option<f64>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct BackgroundFileConfig {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub path: Option<String>,
    /// Optional image sampled by a `type = "shader"` background via `tex`.
    pub texture: Option<String>,
    /// Mirror-fold a `type = "tile"` image so non-seamless edges tile cleanly.
    pub mirror_tile: Option<bool>,
    pub cache_shader: Option<bool>,
    pub transparent_shader: Option<bool>,
    pub cache_budget_mb: Option<u32>,
    pub animate_fps: Option<u32>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct DecorationFileConfig {
    pub bg_color: Option<String>,
    pub fg_color: Option<String>,
    pub corner_radius: Option<i32>,
    pub default_mode: Option<String>,
    pub border_width: Option<i32>,
    pub border_color: Option<String>,
    pub border_color_focused: Option<String>,
    pub shadow: Option<bool>,
    pub title_bar_height: Option<i32>,
    pub font: Option<String>,
    pub font_size: Option<u32>,
    pub font_weight: Option<String>,
    pub title_align: Option<String>,
}

/// Flexible `pass_keys` TOML value: `true`/`false` OR a list of key-combo strings.
///
/// Examples:
/// ```toml
/// pass_keys = true                        # forward ALL keys
/// pass_keys = ["mod+q", "ctrl+q"]         # forward only these combos
/// ```
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub(super) enum PassKeysFile {
    Bool(bool),
    Keys(Vec<String>),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WindowRuleFile {
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub position: Option<[i32; 2]>,
    pub size: Option<[i32; 2]>,
    pub fullscreen: Option<bool>,
    /// `false` — don't focus or navigate the camera to this window when it
    /// first maps. Omit to keep the default focus-on-map behavior.
    pub focus_on_open: Option<bool>,
    #[serde(default)]
    pub widget: bool,
    /// Pin the window to one output's screen space: it ignores pan/zoom and
    /// renders above normal windows. `position` is then output-relative
    /// (center, Y-up), not a canvas coordinate.
    #[serde(default)]
    pub pinned_to_screen: bool,
    /// Override the global `suspend_on_close` for matched windows. Escape hatch
    /// for terminals / scratchpads that should always really close (or always
    /// suspend). `None` inherits the global setting.
    pub suspend_on_close: Option<bool>,
    /// Keep the window's aspect ratio during interactive resizes; the ratio is
    /// taken at the start of each resize.
    #[serde(default)]
    pub preserve_aspect_ratio: bool,
    pub decoration: Option<String>,
    pub blur: Option<bool>,
    pub opacity: Option<f64>,
    /// `true` — forward all keys to the app (game-friendly).
    /// `["mod+q", "ctrl+q"]` — forward only those combos; all others stay active.
    /// Omit or `false` — compositor handles everything normally (default).
    pub pass_keys: Option<PassKeysFile>,
    pub border_width: Option<i32>,
    pub border_color: Option<String>,
    pub border_color_focused: Option<String>,
    pub corner_radius: Option<i32>,
    pub shadow: Option<bool>,
    /// Output name (e.g. `"DP-1"`) this window should fullscreen onto, overriding
    /// the client-requested output.
    pub output: Option<String>,
    /// Stacking order among layer-shell surfaces on the same wlr-layer
    /// (higher = on top; ties keep map order).
    pub layer_order: Option<i32>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct MouseFileConfig {
    /// Enable dragging a window's edge/corner to resize it.
    /// See [`super::Config::resize_on_border`].
    pub resize_on_border: Option<bool>,
    /// Propagate edge-drag resize to snapped neighbors.
    /// See [`super::Config::decoration_resize_snapped`].
    pub decoration_resize_snapped: Option<bool>,
    /// Propagate decoration-initiated fit (maximize/unmaximize) to snapped neighbors.
    /// See [`super::Config::decoration_fit_snapped`].
    pub decoration_fit_snapped: Option<bool>,
    #[serde(rename = "on-window")]
    pub on_window: Option<HashMap<String, String>>,
    #[serde(rename = "on-canvas")]
    pub on_canvas: Option<HashMap<String, String>>,
    pub anywhere: Option<HashMap<String, String>>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct GestureFileConfig {
    pub swipe_threshold: Option<f64>,
    pub pinch_in_threshold: Option<f64>,
    pub pinch_out_threshold: Option<f64>,
    #[serde(rename = "on-window")]
    pub on_window: Option<HashMap<String, String>>,
    #[serde(rename = "on-canvas")]
    pub on_canvas: Option<HashMap<String, String>>,
    pub anywhere: Option<HashMap<String, String>>,
}

/// Touch gesture *bindings* (`[touch]`) — distinct from `[input.touch]` device
/// settings (`TouchDeviceFileConfig`). Touch has no modifiers, so no threshold
/// tuning here (it reuses `[gestures]` thresholds); just the three context maps.
#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct TouchFileConfig {
    #[serde(rename = "on-window")]
    pub on_window: Option<HashMap<String, String>>,
    #[serde(rename = "on-canvas")]
    pub on_canvas: Option<HashMap<String, String>>,
    pub anywhere: Option<HashMap<String, String>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OutputRuleFile {
    pub name: String,
    pub scale: Option<f64>,
    pub transform: Option<String>,
    pub position: Option<::toml::Value>,
    pub mode: Option<String>,
    pub hot_corners: Option<HotCornersFile>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct HotCornersFile {
    pub threshold: Option<f64>,
    pub top_left: Option<String>,
    pub top_right: Option<String>,
    pub bottom_left: Option<String>,
    pub bottom_right: Option<String>,
    pub disable_when_fullscreen: Option<bool>,
    pub disable_while_dragging: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct XwaylandConfig {
    pub enabled: bool,
    pub path: String,
}

impl Default for XwaylandConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "xwayland-satellite".to_string(),
        }
    }
}

pub fn config_path() -> std::path::PathBuf {
    // --config <path> sets DRIFTWM_CONFIG at startup
    if let Ok(p) = std::env::var("DRIFTWM_CONFIG") {
        return std::path::PathBuf::from(expand_tilde(&p));
    }
    let config_dir = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.config")
    });
    std::path::PathBuf::from(config_dir).join("driftwm/config.toml")
}

pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}
