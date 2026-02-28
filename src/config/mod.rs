mod defaults;
mod parse;
mod toml;
mod types;

pub use parse::{parse_action, parse_direction, parse_key_combo, parse_mouse_action, parse_mouse_binding};
pub use toml::config_path;
pub use types::*;

use std::collections::HashMap;

use smithay::input::keyboard::{Keysym, ModifiersState};

use defaults::{default_bindings, default_mouse_bindings};
use toml::{ConfigFile, WindowRuleFile, expand_tilde};

pub struct Config {
    pub mod_key: ModKey,
    /// Multiplier for scroll deltas. Higher = faster initial scroll. 1.0 = raw trackpad.
    pub scroll_speed: f64,
    /// Scroll momentum decay factor per frame. 0.92 = snappy, 0.96 = floaty.
    pub friction: f64,
    /// Pixels per keyboard nudge (Mod+Shift+Arrow).
    pub nudge_step: i32,
    /// Pixels per keyboard pan (Mod+Ctrl+Arrow).
    pub pan_step: f64,
    /// Keyboard repeat delay (ms) and rate (keys/sec).
    pub repeat_delay: i32,
    pub repeat_rate: i32,
    /// Edge auto-pan: activation zone width in pixels from viewport edge.
    pub edge_zone: f64,
    /// Edge auto-pan: speed range (px/frame). Quadratic ramp from min to max.
    pub edge_pan_min: f64,
    pub edge_pan_max: f64,
    /// Base lerp factor for camera animation (frame-rate independent). 0.15 = smooth.
    pub animation_speed: f64,
    /// Modifier held during window cycling. Release commits selection.
    pub cycle_modifier: CycleModifier,
    /// Zoom step multiplier per keypress. 1.1 = 10% per press.
    pub zoom_step: f64,
    /// Padding (canvas pixels) around the bounding box for ZoomToFit.
    pub zoom_fit_padding: f64,
    /// Output scale factor for the udev backend (1.0, 1.5, 2.0, etc).
    pub output_scale: f64,
    pub background: BackgroundConfig,
    pub trackpad: TrackpadSettings,
    pub keyboard_layout: KeyboardLayout,
    pub autostart: Vec<String>,
    pub cursor_theme: Option<String>,
    pub cursor_size: Option<u32>,
    pub window_rules: Vec<WindowRule>,
    bindings: HashMap<KeyCombo, Action>,
    pub mouse_bindings: HashMap<MouseBinding, MouseAction>,
}

impl Config {
    pub fn lookup(&self, modifiers: &ModifiersState, sym: Keysym) -> Option<&Action> {
        let mut combo = KeyCombo {
            modifiers: Modifiers::from_state(modifiers),
            sym,
        };
        combo.normalize();
        self.bindings.get(&combo)
    }

    /// Look up a mouse button action by modifier state and button code.
    pub fn mouse_button_lookup(
        &self,
        modifiers: &ModifiersState,
        button: u32,
    ) -> Option<&MouseAction> {
        let binding = MouseBinding {
            modifiers: Modifiers::from_state(modifiers),
            trigger: MouseTrigger::Button(button),
        };
        self.mouse_bindings.get(&binding)
    }

    /// Look up a mouse scroll action by modifier state.
    pub fn mouse_scroll_lookup(&self, modifiers: &ModifiersState) -> Option<&MouseAction> {
        let binding = MouseBinding {
            modifiers: Modifiers::from_state(modifiers),
            trigger: MouseTrigger::Scroll,
        };
        self.mouse_bindings.get(&binding)
    }

    /// Parse a TOML string into a Config. Useful for testing and config reload.
    /// Does NOT set env vars (unlike `load()`).
    pub fn from_toml(toml_str: &str) -> Result<Self, ::toml::de::Error> {
        let raw: ConfigFile = ::toml::from_str(toml_str)?;
        Ok(Self::from_raw(raw))
    }

    /// Load config from `$XDG_CONFIG_HOME/driftwm/config.toml` (or `~/.config/driftwm/config.toml`).
    /// Missing file → all defaults. Parse failure → error log + all defaults.
    pub fn load() -> Self {
        let config_path = config_path();
        let raw = match std::fs::read_to_string(&config_path) {
            Ok(contents) => {
                tracing::info!("Loaded config from {}", config_path.display());
                match ::toml::from_str::<ConfigFile>(&contents) {
                    Ok(cf) => cf,
                    Err(e) => {
                        tracing::error!("Failed to parse config: {e}");
                        ConfigFile::default()
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("No config file found, using defaults");
                ConfigFile::default()
            }
            Err(e) => {
                tracing::error!("Failed to read config: {e}");
                ConfigFile::default()
            }
        };
        // Set cursor env vars before building config (unsafe — process-wide mutation,
        // only safe at startup before threads are spawned)
        if let Some(ref theme) = raw.cursor.theme {
            unsafe { std::env::set_var("XCURSOR_THEME", theme) };
        }
        if let Some(size) = raw.cursor.size {
            unsafe { std::env::set_var("XCURSOR_SIZE", size.to_string()) };
        }

        Self::from_raw(raw)
    }

    /// Build a Config from a parsed (but unvalidated) ConfigFile.
    /// Does not set env vars — that's done in `load()` only.
    fn from_raw(raw: ConfigFile) -> Self {
        let mod_key = match raw.mod_key.as_deref() {
            Some("alt") => ModKey::Alt,
            Some("super") | None => ModKey::Super,
            Some(other) => {
                tracing::warn!("Unknown mod_key '{other}', using super");
                ModKey::Super
            }
        };

        let cycle_modifier = match raw.cycle_modifier.as_deref() {
            Some("ctrl") => CycleModifier::Ctrl,
            Some("alt") | None => CycleModifier::Alt,
            Some(other) => {
                tracing::warn!("Unknown cycle_modifier '{other}', using alt");
                CycleModifier::Alt
            }
        };

        let mut bindings: HashMap<KeyCombo, Action> = default_bindings(mod_key, cycle_modifier)
            .into_iter()
            .map(|(mut k, v)| { k.normalize(); (k, v) })
            .collect();

        if let Some(user_bindings) = raw.keybindings {
            for (key_str, action_str) in &user_bindings {
                match parse_key_combo(key_str, mod_key) {
                    Ok(mut combo) => {
                        combo.normalize();
                        if action_str == "none" {
                            bindings.remove(&combo);
                        } else {
                            match parse_action(action_str) {
                                Ok(action) => {
                                    bindings.insert(combo, action);
                                }
                                Err(e) => {
                                    tracing::warn!("Invalid action '{action_str}': {e}");
                                }
                            }
                        }
                    }
                    Err(e) => tracing::warn!("Invalid key combo '{key_str}': {e}"),
                }
            }
        }

        let mut mouse_bindings = default_mouse_bindings(mod_key);
        if let Some(user_mouse) = raw.mouse {
            for (key_str, action_str) in &user_mouse {
                match parse_mouse_binding(key_str, mod_key) {
                    Ok(binding) => {
                        if action_str == "none" {
                            mouse_bindings.remove(&binding);
                        } else {
                            match parse_mouse_action(action_str) {
                                Ok(action) => {
                                    mouse_bindings.insert(binding, action);
                                }
                                Err(e) => {
                                    tracing::warn!("Invalid mouse action '{action_str}': {e}");
                                }
                            }
                        }
                    }
                    Err(e) => tracing::warn!("Invalid mouse binding '{key_str}': {e}"),
                }
            }
        }

        let background = BackgroundConfig {
            shader_path: raw.background.shader_path.map(|p| expand_tilde(&p)),
            tile_path: raw.background.tile_path.map(|p| expand_tilde(&p)),
        };

        let trackpad = {
            let t = &raw.input.trackpad;
            TrackpadSettings {
                tap_to_click: t.tap_to_click.unwrap_or(true),
                natural_scroll: t.natural_scroll.unwrap_or(true),
                tap_and_drag: t.tap_and_drag.unwrap_or(true),
                accel_speed: t.accel_speed.unwrap_or(0.0).clamp(-1.0, 1.0),
            }
        };

        let keyboard_layout = {
            let k = &raw.input.keyboard;
            KeyboardLayout {
                layout: k.layout.clone().unwrap_or_else(|| "us".into()),
                variant: k.variant.clone().unwrap_or_default(),
                options: k.options.clone().unwrap_or_default(),
                model: k.model.clone().unwrap_or_default(),
            }
        };

        let window_rules = raw
            .window_rules
            .unwrap_or_default()
            .into_iter()
            .map(parse_window_rule)
            .collect();

        Self {
            mod_key,
            scroll_speed: raw.input.scroll.speed.unwrap_or(1.5),
            friction: raw.input.scroll.friction.unwrap_or(0.96),
            nudge_step: raw.navigation.nudge_step.unwrap_or(20),
            pan_step: raw.navigation.pan_step.unwrap_or(100.0),
            repeat_delay: raw.input.keyboard.repeat_delay.unwrap_or(200),
            repeat_rate: raw.input.keyboard.repeat_rate.unwrap_or(25),
            edge_zone: raw.navigation.edge_pan.zone.unwrap_or(100.0),
            edge_pan_min: raw.navigation.edge_pan.speed_min.unwrap_or(4.0),
            edge_pan_max: raw.navigation.edge_pan.speed_max.unwrap_or(30.0),
            animation_speed: raw.navigation.animation_speed.unwrap_or(0.3),
            cycle_modifier,
            zoom_step: raw.zoom.step.unwrap_or(1.1),
            zoom_fit_padding: raw.zoom.fit_padding.unwrap_or(100.0),
            output_scale: raw.output.scale.unwrap_or(1.0),
            background,
            trackpad,
            keyboard_layout,
            cursor_theme: raw.cursor.theme,
            cursor_size: raw.cursor.size,
            autostart: raw.autostart.unwrap_or_default(),
            window_rules,
            bindings,
            mouse_bindings,
        }
    }

    /// Find the first matching window rule for the given `app_id`.
    /// Supports simple glob: `*` anywhere in the pattern.
    pub fn match_window_rule(&self, app_id: &str) -> Option<&WindowRule> {
        self.window_rules.iter().find(|rule| {
            if let Some((prefix, suffix)) = rule.app_id.split_once('*') {
                app_id.len() >= prefix.len() + suffix.len()
                    && app_id.starts_with(prefix)
                    && app_id[prefix.len()..].ends_with(suffix)
            } else {
                rule.app_id == app_id
            }
        })
    }
}

fn parse_window_rule(r: WindowRuleFile) -> WindowRule {
    let decoration = match r.decoration.as_deref() {
        Some("none") => DecorationMode::None,
        Some("server") => DecorationMode::Server,
        Some("client") | None => DecorationMode::Client,
        Some(other) => {
            tracing::warn!("Unknown decoration mode '{other}', using client");
            DecorationMode::Client
        }
    };
    WindowRule {
        app_id: r.app_id,
        position: r.position.map(|[x, y]| (x, y)),
        widget: r.widget,
        no_focus: r.no_focus,
        decoration,
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::from_raw(ConfigFile::default())
    }
}
