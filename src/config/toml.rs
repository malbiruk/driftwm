use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ConfigFile {
    pub mod_key: Option<String>,
    pub focus_follows_mouse: Option<bool>,
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
    pub keybindings: Option<HashMap<String, String>>,
    pub mouse: MouseFileConfig,
    pub gestures: GestureFileConfig,
    pub env: HashMap<String, String>,
    pub xwayland: XwaylandConfig,
    /// Placement mode for newly mapped windows: `"center"` (default) or `"cursor"`.
    pub window_placement: Option<String>,
    pub window_rules: Option<Vec<WindowRuleFile>>,
    pub outputs: Option<Vec<OutputRuleFile>>,
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
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct InputConfig {
    pub keyboard: KeyboardConfig,
    pub trackpad: TrackpadConfig,
    pub mouse: MouseDeviceFileConfig,
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
    pub nudge_step: Option<i32>,
    pub pan_step: Option<f64>,
    pub trackpad_speed: Option<f64>,
    pub mouse_speed: Option<f64>,
    pub drift: Option<f64>,
    /// Renamed to `drift`; kept only so a stale value yields a migration error
    /// instead of failing the whole parse via `deny_unknown_fields`.
    pub friction: Option<f64>,
    pub anchors: Option<Vec<[f64; 2]>>,
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
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ZoomConfig {
    pub step: Option<f64>,
    pub fit_padding: Option<f64>,
    pub reset_on_new_window: Option<bool>,
    pub reset_on_activation: Option<bool>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct SnapConfig {
    pub enabled: Option<bool>,
    pub gap: Option<f64>,
    pub distance: Option<f64>,
    pub break_force: Option<f64>,
    pub same_edge: Option<bool>,
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
    pub cache_shader: Option<bool>,
    pub transparent_shader: Option<bool>,
    pub cache_budget_mb: Option<u32>,
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
    #[serde(default)]
    pub widget: bool,
    /// Pin the window to one output's screen space: it ignores pan/zoom and
    /// renders above normal windows. `position` is then output-relative
    /// (center, Y-up), not a canvas coordinate.
    #[serde(default)]
    pub pinned_to_screen: bool,
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OutputRuleFile {
    pub name: String,
    pub scale: Option<f64>,
    pub transform: Option<String>,
    pub position: Option<::toml::Value>,
    pub mode: Option<String>,
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
