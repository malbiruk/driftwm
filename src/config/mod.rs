mod defaults;
mod parse;
mod parse_helpers;
mod toml;
mod types;

pub use parse::{
    parse_action, parse_direction, parse_gesture_binding, parse_gesture_config_entry,
    parse_gesture_trigger, parse_key_combo, parse_mouse_action, parse_mouse_binding,
};
pub use toml::config_path;
pub use types::*;

use std::collections::HashMap;

use smithay::backend::input::AxisSource;
use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::utils::{Logical, Point};
#[cfg(test)]
use smithay::utils::Transform;

use defaults::{default_bindings, default_gesture_bindings, default_mouse_bindings};
use parse_helpers::{
    parse_backend_config, parse_decoration_config, parse_effects_config, parse_output_outline,
    parse_output_rule, parse_window_rule,
};
use toml::{ConfigFile, expand_tilde};

/// Env vars the compositor sets as toolkit fallbacks.
/// `[env]` config entries take precedence over these.
const TOOLKIT_DEFAULTS: &[(&str, &str)] = &[
    ("MOZ_ENABLE_WAYLAND", "1"),
    ("QT_QPA_PLATFORM", "wayland;xcb"),
    ("SDL_VIDEODRIVER", "wayland,x11"),
    ("GDK_BACKEND", "wayland,x11"),
    ("ELECTRON_OZONE_PLATFORM_HINT", "wayland"),
];

pub struct Config {
    pub mod_key: ModKey,
    pub focus_follows_mouse: bool,
    /// Multiplier for trackpad scroll and gesture pan deltas. 1.0 = raw trackpad.
    pub trackpad_speed: f64,
    /// Multiplier for mouse drag pan (Mod+LMB or LMB on canvas). 1.0 = direct.
    pub mouse_speed: f64,
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
    /// Padding (viewport/screen pixels) around the bounding box for ZoomToFit.
    /// Screen-space so the gutter is consistent regardless of the resulting zoom.
    pub zoom_fit_padding: f64,
    /// Animate zoom back to 1.0 when a new window is mapped (true) or preserve current zoom (false).
    pub zoom_reset_on_new_window: bool,
    /// Animate zoom back to 1.0 when an off-screen window requests activation
    /// (xdg-activation or foreign-toplevel click) (true) or just pan to it at current zoom (false).
    pub zoom_reset_on_activation: bool,
    pub snap_enabled: bool,
    pub snap_gap: f64,
    pub snap_distance: f64,
    pub snap_break_force: f64,
    pub snap_same_edge: bool,
    pub background: BackgroundConfig,
    pub trackpad: TrackpadSettings,
    pub mouse_device: MouseDeviceSettings,
    pub gesture_thresholds: GestureThresholds,
    pub layout_independent: bool,
    pub keyboard_layout: KeyboardLayout,
    pub autostart: Vec<String>,
    pub cursor_theme: Option<String>,
    pub cursor_size: Option<u32>,
    /// Cursor opacity on non-active outputs (0.0 = hidden, 1.0 = full).
    pub inactive_cursor_opacity: f64,
    pub decorations: DecorationConfig,
    pub output_outline: OutputOutlineSettings,
    pub nav_anchors: Vec<Point<f64, Logical>>,
    pub backend: BackendConfig,
    pub effects: EffectsConfig,
    pub window_rules: Vec<WindowRule>,
    pub xwayland_enabled: bool,
    pub env: HashMap<String, String>,
    pub output_configs: Vec<OutputConfig>,
    bindings: HashMap<KeyCombo, Action>,
    pub mouse: ContextBindings<MouseBinding, MouseAction>,
    /// When `true`, resizing a window by dragging its edge (SSD border,
    /// CSD border, or X11 border) propagates to every window connected
    /// to it via snap adjacency. Keybinding and gesture resize are
    /// unaffected — bind `resize-window-snapped` explicitly for those.
    pub decoration_resize_snapped: bool,
    /// When `true`, maximize/unmaximize initiated via window decoration
    /// (CSD maximize button, SSD title-bar double-click, xdg/X11/foreign-toplevel
    /// set_maximized) propagates to every window connected via snap adjacency.
    /// Keybinding and gesture fit are unaffected — bind `fit-window-snapped`
    /// explicitly for those.
    pub decoration_fit_snapped: bool,
    pub gestures: ContextBindings<GestureBinding, GestureConfigEntry>,
    pub num_lock: bool,
    pub caps_lock: bool,
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

    /// Look up a mouse button action by modifier state, button code, and context.
    pub fn mouse_button_lookup_ctx(
        &self,
        modifiers: &ModifiersState,
        button: u32,
        context: BindingContext,
    ) -> Option<&MouseAction> {
        let binding = MouseBinding {
            modifiers: Modifiers::from_state(modifiers),
            trigger: MouseTrigger::Button(button),
        };
        self.mouse.lookup(&binding, context)
    }

    /// Look up a mouse scroll action by modifier state, axis source, and context.
    pub fn mouse_scroll_lookup_ctx(
        &self,
        modifiers: &ModifiersState,
        source: AxisSource,
        context: BindingContext,
    ) -> Option<&MouseAction> {
        let trigger = match source {
            AxisSource::Finger => MouseTrigger::TrackpadScroll,
            _ => MouseTrigger::WheelScroll,
        };
        let binding = MouseBinding {
            modifiers: Modifiers::from_state(modifiers),
            trigger,
        };
        self.mouse.lookup(&binding, context)
    }

    /// Look up a gesture action by modifier state, trigger, and context.
    pub fn gesture_lookup(
        &self,
        modifiers: &ModifiersState,
        trigger: &GestureTrigger,
        context: BindingContext,
    ) -> Option<&GestureConfigEntry> {
        let binding = GestureBinding {
            modifiers: Modifiers::from_state(modifiers),
            trigger: trigger.clone(),
        };
        self.gestures.lookup(&binding, context)
    }

    /// Find the output config for a given connector name (e.g. "eDP-1").
    pub fn output_config(&self, connector_name: &str) -> Option<&OutputConfig> {
        self.output_configs
            .iter()
            .find(|c| c.name == connector_name)
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
        // Set user env vars first (unsafe — process-wide mutation,
        // only safe at startup before threads are spawned)
        for (key, value) in &raw.env {
            unsafe { std::env::set_var(key, value) };
        }
        // Toolkit fallbacks — only set if not already in env (user config wins)
        for &(key, value) in TOOLKIT_DEFAULTS {
            if std::env::var(key).is_err() {
                unsafe { std::env::set_var(key, value) };
            }
        }
        // Cursor env vars
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
            .map(|(mut k, v)| {
                k.normalize();
                (k, v)
            })
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

        let decoration_resize_snapped = raw.mouse.decoration_resize_snapped.unwrap_or(false);
        let decoration_fit_snapped = raw.mouse.decoration_fit_snapped.unwrap_or(false);
        let mut mouse_bindings = default_mouse_bindings(mod_key);
        for (ctx, section) in [
            (BindingContext::OnWindow, raw.mouse.on_window),
            (BindingContext::OnCanvas, raw.mouse.on_canvas),
            (BindingContext::Anywhere, raw.mouse.anywhere),
        ] {
            if let Some(entries) = section {
                for (key_str, action_str) in &entries {
                    match parse_mouse_binding(key_str, mod_key) {
                        Ok(binding) => {
                            if action_str == "none" {
                                mouse_bindings.remove(ctx, &binding);
                            } else {
                                match parse_mouse_action(action_str) {
                                    Ok(action) => {
                                        mouse_bindings.insert(ctx, binding, action);
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
        }

        let mut gesture_bindings = default_gesture_bindings(mod_key);
        for (ctx, section) in [
            (BindingContext::OnWindow, raw.gestures.on_window),
            (BindingContext::OnCanvas, raw.gestures.on_canvas),
            (BindingContext::Anywhere, raw.gestures.anywhere),
        ] {
            if let Some(entries) = section {
                for (key_str, action_str) in &entries {
                    match parse_gesture_binding(key_str, mod_key) {
                        Ok(binding) => {
                            if action_str == "none" {
                                gesture_bindings.remove(ctx, &binding);
                            } else {
                                match parse_gesture_config_entry(&binding.trigger, action_str) {
                                    Ok(entry) => {
                                        gesture_bindings.insert(ctx, binding, entry);
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "Invalid gesture binding '{key_str}' = '{action_str}': {e}"
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => tracing::warn!("Invalid gesture binding '{key_str}': {e}"),
                    }
                }
            }
        }

        let background = BackgroundConfig {
            shader_path: raw.background.shader_path.map(|p| expand_tilde(&p)),
            tile_path: raw.background.tile_path.map(|p| expand_tilde(&p)),
        };

        let trackpad = {
            let t = &raw.input.trackpad;
            let accel_profile = match t.accel_profile.as_deref() {
                Some("flat") => AccelProfile::Flat,
                Some("adaptive") | None => AccelProfile::Adaptive,
                Some(other) => {
                    tracing::warn!("Unknown trackpad accel_profile '{other}', using adaptive");
                    AccelProfile::Adaptive
                }
            };
            TrackpadSettings {
                tap_to_click: t.tap_to_click.unwrap_or(true),
                natural_scroll: t.natural_scroll.unwrap_or(true),
                tap_and_drag: t.tap_and_drag.unwrap_or(true),
                accel_speed: t.accel_speed.unwrap_or(0.0).clamp(-1.0, 1.0),
                accel_profile,
                click_method: t.click_method.clone(),
            }
        };

        let mouse_device = {
            let m = &raw.input.mouse;
            let accel_profile = match m.accel_profile.as_deref() {
                Some("adaptive") => AccelProfile::Adaptive,
                Some("flat") | None => AccelProfile::Flat,
                Some(other) => {
                    tracing::warn!("Unknown mouse accel_profile '{other}', using flat");
                    AccelProfile::Flat
                }
            };
            MouseDeviceSettings {
                accel_speed: m.accel_speed.unwrap_or(0.0).clamp(-1.0, 1.0),
                accel_profile,
                natural_scroll: m.natural_scroll.unwrap_or(false),
            }
        };

        let gesture_thresholds = GestureThresholds {
            swipe_distance: raw.gestures.swipe_threshold.unwrap_or(12.0),
            pinch_in_scale: raw.gestures.pinch_in_threshold.unwrap_or(0.85),
            pinch_out_scale: raw.gestures.pinch_out_threshold.unwrap_or(1.15),
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

        let decorations = parse_decoration_config(raw.decorations);

        let window_rules: Vec<WindowRule> = raw
            .window_rules
            .unwrap_or_default()
            .into_iter()
            .filter_map(parse_window_rule)
            .collect();

        let output_configs = {
            let mut configs: Vec<OutputConfig> = Vec::new();
            for rule in raw.outputs.unwrap_or_default() {
                match parse_output_rule(rule) {
                    Ok(config) => {
                        if configs.iter().any(|c| c.name == config.name) {
                            tracing::warn!(
                                "Duplicate [[outputs]] name '{}', keeping first",
                                config.name
                            );
                        } else {
                            configs.push(config);
                        }
                    }
                    Err(e) => tracing::warn!("Bad [[outputs]] entry: {e}"),
                }
            }
            configs
        };

        let effects = parse_effects_config(raw.effects);
        let backend = parse_backend_config(raw.backend);

        // Deprecation: [input.scroll] → [navigation] trackpad_speed / friction
        let trackpad_speed = if let Some(s) = raw.navigation.trackpad_speed {
            s
        } else if let Some(s) = raw.input.scroll.speed {
            tracing::warn!(
                "[input.scroll] speed is deprecated, use [navigation] trackpad_speed instead"
            );
            s
        } else {
            1.5
        };
        let mouse_speed = raw.navigation.mouse_speed.unwrap_or(1.0);
        let friction = if let Some(f) = raw.navigation.friction {
            f
        } else if let Some(f) = raw.input.scroll.friction {
            tracing::warn!(
                "[input.scroll] friction is deprecated, use [navigation] friction instead"
            );
            f
        } else {
            0.94
        };

        Self {
            mod_key,
            focus_follows_mouse: raw.focus_follows_mouse.unwrap_or(false),
            trackpad_speed,
            mouse_speed,
            friction,
            nudge_step: raw.navigation.nudge_step.unwrap_or(20),
            pan_step: raw.navigation.pan_step.unwrap_or(100.0),
            repeat_delay: raw.input.keyboard.repeat_delay.unwrap_or(200),
            repeat_rate: raw.input.keyboard.repeat_rate.unwrap_or(25),
            edge_zone: raw.navigation.edge_pan.zone.unwrap_or(100.0),
            edge_pan_min: raw.navigation.edge_pan.speed_min.unwrap_or(4.0),
            edge_pan_max: raw.navigation.edge_pan.speed_max.unwrap_or(10.0),
            animation_speed: raw.navigation.animation_speed.unwrap_or(0.3),
            cycle_modifier,
            zoom_step: raw.zoom.step.unwrap_or(1.1),
            zoom_fit_padding: raw.zoom.fit_padding.unwrap_or(80.0),
            zoom_reset_on_new_window: raw.zoom.reset_on_new_window.unwrap_or(true),
            zoom_reset_on_activation: raw.zoom.reset_on_activation.unwrap_or(true),
            snap_enabled: raw.snap.enabled.unwrap_or(true),
            snap_gap: raw.snap.gap.unwrap_or(12.0),
            snap_distance: raw.snap.distance.unwrap_or(24.0),
            snap_break_force: raw.snap.break_force.unwrap_or(32.0),
            snap_same_edge: raw.snap.same_edge.unwrap_or(false),
            background,
            decorations,
            effects,
            backend,
            trackpad,
            mouse_device,
            gesture_thresholds,
            layout_independent: raw.input.keyboard.layout_independent.unwrap_or(true),
            keyboard_layout,
            cursor_theme: raw.cursor.theme,
            cursor_size: raw.cursor.size,
            inactive_cursor_opacity: raw.cursor.inactive_opacity.unwrap_or(0.5).clamp(0.0, 1.0),
            output_outline: parse_output_outline(raw.output.outline.unwrap_or_default()),
            nav_anchors: raw
                .navigation
                .anchors
                .unwrap_or_else(|| vec![[0.0, 0.0]])
                .into_iter()
                .map(|[x, y]| Point::from((x, -y)))
                .collect(),
            autostart: raw.autostart.unwrap_or_default(),
            env: raw.env,
            window_rules,
            xwayland_enabled: raw.xwayland.enabled,
            output_configs,
            bindings,
            mouse: mouse_bindings,
            decoration_resize_snapped,
            decoration_fit_snapped,
            gestures: gesture_bindings,
            num_lock: raw.input.keyboard.num_lock.unwrap_or(true),
            caps_lock: raw.input.keyboard.caps_lock.unwrap_or(false),
        }
    }

    /// Find the first matching window rule for the given `app_id` and `title`.
    pub fn match_window_rule(&self, app_id: &str, title: &str) -> Option<&WindowRule> {
        self.window_rules
            .iter()
            .find(|rule| Self::rule_matches(rule, app_id, title))
    }

    /// Find the Nth matching window rule (with position) for the given `app_id` and `title`.
    /// Used by layer shell to assign different rules to successive surfaces with
    /// the same namespace (e.g. two waybar instances at different positions).
    pub fn match_window_rule_nth(
        &self,
        app_id: &str,
        title: &str,
        n: usize,
    ) -> Option<&WindowRule> {
        self.window_rules
            .iter()
            .filter(|rule| rule.position.is_some() && Self::rule_matches(rule, app_id, title))
            .nth(n)
    }

    fn glob_matches(pattern: &str, value: &str) -> bool {
        if let Some((prefix, suffix)) = pattern.split_once('*') {
            value.len() >= prefix.len() + suffix.len()
                && value.starts_with(prefix)
                && value[prefix.len()..].ends_with(suffix)
        } else {
            pattern == value
        }
    }

    fn rule_matches(rule: &WindowRule, app_id: &str, title: &str) -> bool {
        let app_ok = rule
            .app_id
            .as_ref()
            .is_none_or(|pat| Self::glob_matches(pat, app_id));
        let title_ok = rule
            .title
            .as_ref()
            .is_none_or(|pat| Self::glob_matches(pat, title));
        app_ok && title_ok
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::from_raw(ConfigFile::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_output_rule_negative_scale() {
        let toml_str = r#"
            [[outputs]]
            name = "eDP-1"
            scale = -1.0
        "#;
        let config = Config::from_toml(toml_str).unwrap();
        assert!(config.output_configs.is_empty());
    }

    #[test]
    fn parse_output_rule_zero_scale() {
        let toml_str = r#"
            [[outputs]]
            name = "eDP-1"
            scale = 0.0
        "#;
        let config = Config::from_toml(toml_str).unwrap();
        assert!(config.output_configs.is_empty());
    }

    #[test]
    fn parse_output_rule_valid() {
        let toml_str = r#"
            [[outputs]]
            name = "eDP-1"
            scale = 1.5
            transform = "90"
            mode = "2560x1440@144"
            position = [1920, 0]
        "#;
        let config = Config::from_toml(toml_str).unwrap();
        assert_eq!(config.output_configs.len(), 1);
        let oc = &config.output_configs[0];
        assert_eq!(oc.name, "eDP-1");
        assert_eq!(oc.scale, Some(1.5));
        assert_eq!(oc.transform, Some(Transform::_90));
        assert_eq!(oc.mode, OutputMode::SizeRefresh(2560, 1440, 144));
        assert_eq!(oc.position, OutputPosition::Fixed(1920, 0));
    }

    #[test]
    fn parse_output_rule_defaults() {
        let toml_str = r#"
            [[outputs]]
            name = "HDMI-A-1"
        "#;
        let config = Config::from_toml(toml_str).unwrap();
        assert_eq!(config.output_configs.len(), 1);
        let oc = &config.output_configs[0];
        assert_eq!(oc.scale, None);
        assert_eq!(oc.transform, None);
        assert_eq!(oc.mode, OutputMode::Preferred);
        assert_eq!(oc.position, OutputPosition::Auto);
    }

    #[test]
    fn duplicate_output_names_keeps_first() {
        let toml_str = r#"
            [[outputs]]
            name = "eDP-1"
            scale = 1.5

            [[outputs]]
            name = "eDP-1"
            scale = 2.0
        "#;
        let config = Config::from_toml(toml_str).unwrap();
        assert_eq!(config.output_configs.len(), 1);
        assert_eq!(config.output_configs[0].scale, Some(1.5));
    }

    #[test]
    fn output_config_lookup() {
        let toml_str = r#"
            [[outputs]]
            name = "eDP-1"
            scale = 1.5

            [[outputs]]
            name = "HDMI-A-1"
            scale = 1.0
        "#;
        let config = Config::from_toml(toml_str).unwrap();
        assert!(config.output_config("eDP-1").is_some());
        assert!(config.output_config("HDMI-A-1").is_some());
        assert!(config.output_config("DP-2").is_none());
    }

    #[test]
    fn no_outputs_section_produces_empty_vec() {
        let config = Config::from_toml("").unwrap();
        assert!(config.output_configs.is_empty());
    }
}
