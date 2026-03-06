use std::collections::HashMap;

use serde::Deserialize;

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ConfigFile {
    pub mod_key: Option<String>,
    pub cycle_modifier: Option<String>,
    pub input: InputConfig,
    pub cursor: CursorConfig,
    pub navigation: NavigationConfig,
    pub zoom: ZoomConfig,
    pub snap: SnapConfig,
    pub output: OutputConfig,
    pub background: BackgroundFileConfig,
    pub decorations: DecorationFileConfig,
    pub autostart: Option<Vec<String>>,
    pub keybindings: Option<HashMap<String, String>>,
    pub mouse: MouseFileConfig,
    pub gestures: GestureFileConfig,
    pub window_rules: Option<Vec<WindowRuleFile>>,
    pub outputs: Option<Vec<OutputRuleFile>>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct InputConfig {
    pub keyboard: KeyboardConfig,
    pub scroll: ScrollConfig,
    pub trackpad: TrackpadConfig,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct TrackpadConfig {
    pub tap_to_click: Option<bool>,
    pub natural_scroll: Option<bool>,
    pub tap_and_drag: Option<bool>,
    pub accel_speed: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct KeyboardConfig {
    pub repeat_rate: Option<i32>,
    pub repeat_delay: Option<i32>,
    pub layout: Option<String>,
    pub variant: Option<String>,
    pub options: Option<String>,
    pub model: Option<String>,
    pub layout_independent: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ScrollConfig {
    pub speed: Option<f64>,
    pub friction: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct CursorConfig {
    pub theme: Option<String>,
    pub size: Option<u32>,
    pub inactive_opacity: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct NavigationConfig {
    pub animation_speed: Option<f64>,
    pub nudge_step: Option<i32>,
    pub pan_step: Option<f64>,
    pub anchors: Option<Vec<[f64; 2]>>,
    pub edge_pan: EdgePanConfig,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct EdgePanConfig {
    pub zone: Option<f64>,
    pub speed_min: Option<f64>,
    pub speed_max: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ZoomConfig {
    pub step: Option<f64>,
    pub fit_padding: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct SnapConfig {
    pub enabled: Option<bool>,
    pub gap: Option<f64>,
    pub distance: Option<f64>,
    pub break_force: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct OutputConfig {
    pub scale: Option<f64>,
    pub outline: Option<OutputOutlineConfig>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct OutputOutlineConfig {
    pub color: Option<String>,
    pub thickness: Option<i32>,
    pub opacity: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct BackgroundFileConfig {
    pub shader_path: Option<String>,
    pub tile_path: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct DecorationFileConfig {
    pub bg_color: Option<String>,
    pub fg_color: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WindowRuleFile {
    pub app_id: String,
    pub position: Option<[i32; 2]>,
    #[serde(default)]
    pub widget: bool,
    #[serde(default)]
    pub no_focus: bool,
    pub decoration: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct MouseFileConfig {
    #[serde(rename = "on-window")]
    pub on_window: Option<HashMap<String, String>>,
    #[serde(rename = "on-canvas")]
    pub on_canvas: Option<HashMap<String, String>>,
    pub anywhere: Option<HashMap<String, String>>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub(super) struct GestureFileConfig {
    #[serde(rename = "on-window")]
    pub on_window: Option<HashMap<String, String>>,
    #[serde(rename = "on-canvas")]
    pub on_canvas: Option<HashMap<String, String>>,
    pub anywhere: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OutputRuleFile {
    pub name: String,
    pub scale: Option<f64>,
    pub transform: Option<String>,
    pub position: Option<::toml::Value>,
    pub mode: Option<String>,
}

pub fn config_path() -> std::path::PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.config")
    });
    std::path::PathBuf::from(config_dir).join("driftwm/config.toml")
}

pub(super) fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}
