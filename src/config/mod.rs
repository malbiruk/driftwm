mod defaults;

mod parse;
mod parse_helpers;
mod toml;
mod types;

pub use parse::{
    parse_action, parse_direction, parse_gesture_binding, parse_gesture_config_entry,
    parse_gesture_trigger, parse_key_combo, parse_mouse_action, parse_mouse_binding,
    parse_tap_combo,
};
pub use toml::config_path;
pub use types::*;

use std::collections::HashMap;

use smithay::backend::input::AxisSource;
use smithay::input::keyboard::{Keysym, ModifiersState};
#[cfg(test)]
use smithay::utils::Transform;
use smithay::utils::{Logical, Point};

use defaults::{default_bindings, default_gesture_bindings, default_mouse_bindings};
use parse_helpers::{
    Warnings, clamp_warn, collect_warn, non_negative, parse_backend_config,
    parse_decoration_config, parse_effects_config, parse_output_outline, parse_output_rule,
    parse_window_rule,
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

#[derive(Debug, PartialEq)]
pub struct Config {
    pub mod_key: ModKey,
    pub focus_follows_mouse: bool,
    /// Multiplier for trackpad scroll and gesture pan deltas. 1.0 = raw trackpad.
    pub trackpad_speed: f64,
    /// Multiplier for mouse drag pan (Mod+LMB or LMB on canvas). 1.0 = direct.
    pub mouse_speed: f64,
    /// Scroll momentum on a 0–1 scale: 0 = off, 0.5 = default, 1 = floatiest.
    pub drift: f64,
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
    /// Cursor edge-pan: pan the viewport when the bare cursor touches a screen
    /// edge (toggle with `toggle-cursor-pan`). This is the startup default.
    pub edge_pan_cursor: bool,
    /// Cursor edge-pan activation zone, px from the edge.
    pub edge_pan_cursor_zone: f64,
    /// Base lerp factor for camera animation (frame-rate independent), in (0, 1].
    /// Lower = smoother; 1 = instant; 0 would freeze the camera.
    pub animation_speed: f64,
    /// On close, pan the camera to the newly focused window (true). When false,
    /// focus only moves to an already-visible window — never off-screen.
    pub auto_navigate_on_close: bool,
    /// Modifiers held during Alt-Tab window cycling, derived from the
    /// `cycle-windows forward` binding. Releasing them commits the selection.
    pub cycle_hold: Modifiers,
    /// Zoom step multiplier per keypress. 1.1 = 10% per press.
    pub zoom_step: f64,
    /// Padding (viewport/screen pixels) around the bounding box for ZoomToFit.
    /// Screen-space so the gutter is consistent regardless of the resulting zoom.
    pub zoom_fit_padding: f64,
    /// Animate zoom back to 1.0 when a new window is mapped
    /// (true) or preserve current zoom (false).
    pub zoom_reset_on_new_window: bool,
    /// Animate zoom back to 1.0 when an off-screen window requests activation
    /// (xdg-activation or foreign-toplevel click) (true) or just pan to it at current zoom (false).
    pub zoom_reset_on_activation: bool,
    pub snap_enabled: bool,
    pub snap_gap: f64,
    pub snap_distance: f64,
    pub snap_break_force: f64,
    pub snap_corners: bool,
    pub snap_centers: bool,
    pub background: BackgroundConfig,
    pub trackpad: TrackpadSettings,
    pub mouse_device: MouseDeviceSettings,
    pub touch: TouchSettings,
    pub gesture_thresholds: GestureThresholds,
    pub layout_independent: bool,
    pub keyboard_layout: KeyboardLayout,
    /// Restore each window's last-used keyboard layout when it regains focus.
    pub remember_layout_per_window: bool,
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
    pub xwayland_path: String,
    pub window_placement: WindowPlacement,
    pub env: HashMap<String, String>,
    /// Pre-merged env passed to spawned child processes via `Command::envs()`.
    /// Layers (later wins): toolkit defaults → XCURSOR_* → user `[env]`. Built
    /// once in `from_raw` so we never touch process env at runtime.
    pub child_env: HashMap<String, String>,
    pub output_configs: Vec<OutputConfig>,
    bindings: HashMap<KeyCombo, Action>,
    /// Tap-modifier bindings: a bare modifier chord (e.g. `alt+shift`) that
    /// fires its action when the chord is pressed and released with no other
    /// key on top. Keyed by the exact modifier set.
    tap_bindings: HashMap<Modifiers, Action>,
    pub mouse: ContextBindings<MouseBinding, MouseAction>,
    /// When `true` (default), dragging a window's edge or corner resizes it
    /// via the invisible resize border (SSD frame or CSD margin). When `false`,
    /// that border is inert — resize only through explicit bindings (e.g.
    /// `alt+RMB`) or gestures.
    pub resize_on_border: bool,
    /// When `true`, resizing a window by dragging its edge (SSD or CSD
    /// border) propagates to every window connected to it via snap
    /// adjacency. Keybinding and gesture resize are unaffected — bind
    /// `resize-window-snapped` explicitly for those.
    pub decoration_resize_snapped: bool,
    /// When `true`, maximize/unmaximize initiated via window decoration
    /// (CSD maximize button, SSD title-bar double-click, xdg/foreign-toplevel
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

    /// Look up a tap-modifier binding by the completed chord's modifier set.
    pub fn tap_lookup(&self, mods: &Modifiers) -> Option<&Action> {
        self.tap_bindings.get(mods)
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
        Self::from_toml_collect(toml_str).map(|(c, _)| c)
    }

    /// Parse a TOML string into a Config, collecting validation warnings
    /// (out-of-bounds values, deprecated fields, etc.) alongside.
    pub fn from_toml_collect(toml_str: &str) -> Result<(Self, Vec<String>), ::toml::de::Error> {
        let raw: ConfigFile = ::toml::from_str(toml_str)?;
        Ok(Self::from_raw_collect(raw))
    }

    /// Load config from `$XDG_CONFIG_HOME/driftwm/config.toml` (or `~/.config/driftwm/config.toml`).
    /// Missing file → all defaults. Parse failure → error log + all defaults.
    pub fn load() -> Self {
        Self::load_from(&config_path())
    }

    /// Like `load()` but returns config file warnings (out-of-bounds values, etc.)
    /// alongside the config so callers can surface them in the on-screen error bar.
    pub fn load_collect() -> (Self, Vec<String>) {
        Self::load_from_collect(&config_path())
    }

    /// Load config from an explicit path. Used by `--config <path>` CLI arg.
    /// Missing file → all defaults. Parse failure → error log + all defaults.
    pub fn load_from(config_path: &std::path::Path) -> Self {
        Self::load_from_collect(config_path).0
    }

    /// Like `load_from` but returns config file warnings alongside the config.
    pub fn load_from_collect(config_path: &std::path::Path) -> (Self, Vec<String>) {
        let (raw, mut warnings) = match std::fs::read_to_string(config_path) {
            Ok(contents) => {
                tracing::info!("Loaded config from {}", config_path.display());
                match ::toml::from_str::<ConfigFile>(&contents) {
                    Ok(cf) => (cf, vec![]),
                    Err(e) => {
                        let msg = format!("config error: {e}");
                        tracing::error!("{msg}");
                        (ConfigFile::default(), vec![msg])
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("No config file found, using defaults");
                (ConfigFile::default(), vec![])
            }
            Err(e) => {
                let msg = format!("config: failed to read {}: {e}", config_path.display());
                tracing::error!("{msg}");
                (ConfigFile::default(), vec![msg])
            }
        };
        let (cfg, extra) = Self::from_raw_collect(raw);
        warnings.extend(extra);
        (cfg, warnings)
    }

    /// Build a Config from a parsed (but unvalidated) ConfigFile.
    /// Never touches process env — child env is built into `child_env` and
    /// applied via `Command::envs()` per spawn.
    fn from_raw(raw: ConfigFile) -> Self {
        Self::from_raw_collect(raw).0
    }

    /// Like `from_raw` but returns config file warnings (out-of-bounds values,
    /// deprecated fields, etc.) alongside the config so callers can surface
    /// them in the on-screen error bar.
    fn from_raw_collect(raw: ConfigFile) -> (Self, Vec<String>) {
        let mut errors: Warnings = Vec::new();

        /// Collect a validation warning: log it and also push to the errors vec.
        macro_rules! warn_and_collect {
            ($fmt:literal $(, $arg:expr)* $(,)?) => {
                collect_warn(&mut errors, format!($fmt $(, $arg)*))
            };
        }

        let mod_key = match raw.mod_key.as_deref() {
            Some("alt") => ModKey::Alt,
            Some("super") | None => ModKey::Super,
            Some(other) => {
                warn_and_collect!("config: unknown mod_key '{other}', using super");
                ModKey::Super
            }
        };

        let window_placement = match raw.window_placement.as_deref() {
            Some("cursor") => WindowPlacement::Cursor,
            Some("auto") => WindowPlacement::Auto,
            Some("center") | None => WindowPlacement::Center,
            Some(other) => {
                warn_and_collect!("config: unknown window_placement '{other}', using center");
                WindowPlacement::Center
            }
        };

        let mut bindings: HashMap<KeyCombo, Action> = default_bindings(mod_key)
            .into_iter()
            .map(|(mut k, v)| {
                k.normalize();
                (k, v)
            })
            .collect();

        let mut tap_bindings: HashMap<Modifiers, Action> = HashMap::new();
        if let Some(user_bindings) = raw.keybindings {
            for (key_str, action_str) in &user_bindings {
                // A bare modifier chord (no keysym) is a tap-modifier binding.
                if let Some(tap_result) = parse::parse_tap_combo(key_str, mod_key) {
                    match tap_result {
                        Ok(mods) => {
                            if action_str == "none" {
                                tap_bindings.remove(&mods);
                            } else {
                                match parse_action(action_str) {
                                    Ok(action) => {
                                        tap_bindings.insert(mods, action);
                                    }
                                    Err(e) => warn_and_collect!(
                                        "config: invalid action '{action_str}': {e}"
                                    ),
                                }
                            }
                        }
                        Err(e) => warn_and_collect!("config: invalid tap binding '{key_str}': {e}"),
                    }
                    continue;
                }
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
                                    warn_and_collect!("config: invalid action '{action_str}': {e}");
                                }
                            }
                        }
                    }
                    Err(e) => warn_and_collect!("config: invalid key combo '{key_str}': {e}"),
                }
            }
        }

        // The cycle "hold" modifier — released to commit an Alt-Tab cycle — is
        // whatever `cycle-windows forward` is bound to, so rebinding the cycle
        // key moves the hold modifier with it. Reading the forward binding (not
        // backward) keeps the backward binding's extra shift out of the set.
        let cycle_hold = bindings
            .iter()
            .find(|(_, action)| matches!(action, Action::CycleWindows { backward: false }))
            .map(|(combo, _)| combo.modifiers.clone())
            .filter(|m| !m.is_empty())
            .unwrap_or(Modifiers {
                alt: true,
                ..Modifiers::EMPTY
            });

        let resize_on_border = raw.mouse.resize_on_border.unwrap_or(true);
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
                                        warn_and_collect!(
                                            "config: invalid mouse action '{action_str}': {e}"
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn_and_collect!("config: invalid mouse binding '{key_str}': {e}")
                        }
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
                                        warn_and_collect!(
                                            "config: invalid gesture binding '{key_str}' = '{action_str}': {e}"
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn_and_collect!("config: invalid gesture binding '{key_str}': {e}")
                        }
                    }
                }
            }
        }

        let background = BackgroundConfig {
            mirror_tile: raw.background.mirror_tile.unwrap_or(false),
            cache_shader: raw.background.cache_shader.unwrap_or(false),
            transparent_shader: raw.background.transparent_shader.unwrap_or(false),
            cache_budget_mb: raw.background.cache_budget_mb.unwrap_or(128),
            kind: resolve_background_kind(raw.background, &mut errors),
        };

        let trackpad = {
            let t = &raw.input.trackpad;
            let accel_profile = match t.accel_profile.as_deref() {
                Some("flat") => AccelProfile::Flat,
                Some("adaptive") | None => AccelProfile::Adaptive,
                Some(other) => {
                    warn_and_collect!(
                        "config: unknown trackpad accel_profile '{other}', using adaptive"
                    );
                    AccelProfile::Adaptive
                }
            };
            TrackpadSettings {
                tap_to_click: t.tap_to_click.unwrap_or(true),
                natural_scroll: t.natural_scroll.unwrap_or(true),
                tap_and_drag: t.tap_and_drag.unwrap_or(true),
                accel_speed: clamp_warn(
                    t.accel_speed.unwrap_or(0.0),
                    -1.0,
                    1.0,
                    "trackpad.accel_speed",
                    &mut errors,
                ),
                accel_profile,
                // `"none"` means "use the libinput device default", same as unset.
                click_method: t.click_method.clone().filter(|m| m.as_str() != "none"),
                disable_while_typing: t.disable_while_typing.unwrap_or(true),
            }
        };

        let mouse_device = {
            let m = &raw.input.mouse;
            let accel_profile = match m.accel_profile.as_deref() {
                Some("adaptive") => AccelProfile::Adaptive,
                Some("flat") | None => AccelProfile::Flat,
                Some(other) => {
                    warn_and_collect!("config: unknown mouse accel_profile '{other}', using flat");
                    AccelProfile::Flat
                }
            };
            MouseDeviceSettings {
                accel_speed: clamp_warn(
                    m.accel_speed.unwrap_or(0.0),
                    -1.0,
                    1.0,
                    "mouse.accel_speed",
                    &mut errors,
                ),
                accel_profile,
                natural_scroll: m.natural_scroll.unwrap_or(false),
            }
        };

        let touch = {
            let t = &raw.input.touch;
            TouchSettings {
                enable: t.enable.unwrap_or(true),
                enable_canvas_gestures: t.enable_canvas_gestures.unwrap_or(true),
                zoom_speed: t.zoom_speed.unwrap_or(1.0),
                pan_speed: t.pan_speed.unwrap_or(1.0),
                touch_to_focus: t.touch_to_focus.unwrap_or(true),
                swipe_threshold: t.swipe_threshold.unwrap_or(100.0),
            }
        };

        let gesture_thresholds = GestureThresholds {
            swipe_distance: non_negative(
                raw.gestures.swipe_threshold.unwrap_or(12.0),
                "gestures.swipe_threshold",
                &mut errors,
            ),
            pinch_in_scale: non_negative(
                raw.gestures.pinch_in_threshold.unwrap_or(0.85),
                "gestures.pinch_in_threshold",
                &mut errors,
            ),
            pinch_out_scale: non_negative(
                raw.gestures.pinch_out_threshold.unwrap_or(1.15),
                "gestures.pinch_out_threshold",
                &mut errors,
            ),
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

        // A group-switch xkb option bound to a modifier pair (alt+shift,
        // ctrl+shift) rewrites the second key into a layout switch, so that
        // exact chord never registers — a tap binding on it would silently
        // never fire. Flag it instead of letting the user debug the silence.
        for mods in tap_bindings.keys() {
            if let Some(opt) = tap_option_conflict(mods, &keyboard_layout.options) {
                warn_and_collect!(
                    "config: tap binding '{}' will never fire while [input.keyboard] options has \
                     {} (it consumes those modifiers for layout switching) — remove the option \
                     and use this tap binding for layout switching instead",
                    tap_combo_label(mods),
                    opt,
                );
            }
        }

        let decorations = parse_decoration_config(raw.decorations, &mut errors);

        let window_rules: Vec<WindowRule> = raw
            .window_rules
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| parse_window_rule(r, mod_key, &mut errors))
            .collect();

        let output_configs = {
            let mut configs: Vec<OutputConfig> = Vec::new();
            for rule in raw.outputs.unwrap_or_default() {
                match parse_output_rule(rule) {
                    Ok(config) => {
                        if configs.iter().any(|c| c.name == config.name) {
                            warn_and_collect!(
                                "config: duplicate [[outputs]] name '{}', keeping first",
                                config.name
                            );
                        } else {
                            configs.push(config);
                        }
                    }
                    Err(e) => warn_and_collect!("config: bad [[outputs]] entry: {e}"),
                }
            }
            configs
        };

        let effects = parse_effects_config(raw.effects, &mut errors);
        let backend = parse_backend_config(raw.backend);

        let trackpad_speed = non_negative(
            raw.navigation.trackpad_speed.unwrap_or(1.5),
            "navigation.trackpad_speed",
            &mut errors,
        );
        let mouse_speed = non_negative(
            raw.navigation.mouse_speed.unwrap_or(1.0),
            "navigation.mouse_speed",
            &mut errors,
        );
        let drift = clamp_warn(
            raw.navigation.drift.unwrap_or(0.5),
            0.0,
            1.0,
            "navigation.drift",
            &mut errors,
        );
        // Valid range is (0, 1]: at 0 the lerp factor stays 0 and the camera
        // never reaches its target, so reject it (and negatives/NaN) back to the
        // default rather than freezing. Above 1 just clamps to instant.
        let animation_speed = match raw.navigation.animation_speed {
            Some(v) if v <= 0.0 || v.is_nan() => {
                warn_and_collect!(
                    "config: navigation.animation_speed {v} must be in (0, 1] (0 freezes the camera), using 0.3"
                );
                0.3
            }
            other => clamp_warn(
                other.unwrap_or(0.3),
                0.0,
                1.0,
                "navigation.animation_speed",
                &mut errors,
            ),
        };
        if raw.navigation.friction.is_some() {
            warn_and_collect!(
                "config: [navigation] friction was renamed to drift — use 0 (off) to 1 (floatiest), default 0.5"
            );
        }
        if raw.snap.same_edge.is_some() {
            warn_and_collect!("config: [snap] same_edge was renamed to corners");
        }
        if raw.snap.edge_center.is_some() {
            warn_and_collect!("config: [snap] edge_center was renamed to centers");
        }

        // `"none"` (theme) and `0` (size) are explicit "inherit from the
        // environment" sentinels — normalize them to unset so a config that
        // spells out the default still round-trips to omitting the field.
        let cursor_theme = raw.cursor.theme.filter(|t| t.as_str() != "none");
        let cursor_size = raw.cursor.size.filter(|s| *s != 0);

        let mut child_env: HashMap<String, String> = TOOLKIT_DEFAULTS
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if let Some(theme) = &cursor_theme {
            child_env.insert("XCURSOR_THEME".into(), theme.clone());
        }
        if let Some(size) = cursor_size {
            child_env.insert("XCURSOR_SIZE".into(), size.to_string());
        }
        for (k, v) in &raw.env {
            child_env.insert(k.clone(), v.clone());
        }

        let config = Self {
            mod_key,
            focus_follows_mouse: raw.focus_follows_mouse.unwrap_or(false),
            trackpad_speed,
            mouse_speed,
            drift,
            nudge_step: non_negative(
                raw.navigation.nudge_step.unwrap_or(20),
                "navigation.nudge_step",
                &mut errors,
            ),
            pan_step: non_negative(
                raw.navigation.pan_step.unwrap_or(100.0),
                "navigation.pan_step",
                &mut errors,
            ),
            repeat_delay: non_negative(
                raw.input.keyboard.repeat_delay.unwrap_or(200),
                "input.keyboard.repeat_delay",
                &mut errors,
            ),
            repeat_rate: non_negative(
                raw.input.keyboard.repeat_rate.unwrap_or(25),
                "input.keyboard.repeat_rate",
                &mut errors,
            ),
            edge_zone: non_negative(
                raw.navigation.edge_pan.zone.unwrap_or(100.0),
                "navigation.edge_pan.zone",
                &mut errors,
            ),
            edge_pan_min: non_negative(
                raw.navigation.edge_pan.speed_min.unwrap_or(4.0),
                "navigation.edge_pan.speed_min",
                &mut errors,
            ),
            edge_pan_max: non_negative(
                raw.navigation.edge_pan.speed_max.unwrap_or(10.0),
                "navigation.edge_pan.speed_max",
                &mut errors,
            ),
            edge_pan_cursor: raw.navigation.edge_pan.cursor_pan.unwrap_or(false),
            edge_pan_cursor_zone: non_negative(
                raw.navigation.edge_pan.cursor_zone.unwrap_or(20.0),
                "navigation.edge_pan.cursor_zone",
                &mut errors,
            ),
            animation_speed,
            auto_navigate_on_close: raw.navigation.auto_navigate_on_close.unwrap_or(true),
            cycle_hold,
            zoom_step: non_negative(raw.zoom.step.unwrap_or(1.1), "zoom.step", &mut errors),
            zoom_fit_padding: non_negative(
                raw.zoom.fit_padding.unwrap_or(80.0),
                "zoom.fit_padding",
                &mut errors,
            ),
            zoom_reset_on_new_window: raw.zoom.reset_on_new_window.unwrap_or(true),
            zoom_reset_on_activation: raw.zoom.reset_on_activation.unwrap_or(true),
            snap_enabled: raw.snap.enabled.unwrap_or(true),
            snap_gap: non_negative(raw.snap.gap.unwrap_or(12.0), "snap.gap", &mut errors),
            snap_distance: non_negative(
                raw.snap.distance.unwrap_or(24.0),
                "snap.distance",
                &mut errors,
            ),
            snap_break_force: non_negative(
                raw.snap.break_force.unwrap_or(32.0),
                "snap.break_force",
                &mut errors,
            ),
            snap_corners: raw.snap.corners.unwrap_or(false),
            snap_centers: raw.snap.centers.unwrap_or(false),
            background,
            decorations,
            effects,
            backend,
            trackpad,
            mouse_device,
            touch,
            gesture_thresholds,
            layout_independent: raw.input.keyboard.layout_independent.unwrap_or(true),
            keyboard_layout,
            remember_layout_per_window: raw
                .input
                .keyboard
                .remember_layout_per_window
                .unwrap_or(false),
            cursor_theme,
            cursor_size,
            inactive_cursor_opacity: clamp_warn(
                raw.cursor.inactive_opacity.unwrap_or(0.5),
                0.0,
                1.0,
                "cursor.inactive_opacity",
                &mut errors,
            ),
            output_outline: parse_output_outline(
                raw.output.outline.unwrap_or_default(),
                &mut errors,
            ),
            nav_anchors: raw
                .navigation
                .anchors
                .unwrap_or_else(|| vec![[0.0, 0.0]])
                .into_iter()
                .map(|[x, y]| Point::from((x, -y)))
                .collect(),
            autostart: raw.autostart.unwrap_or_default(),
            env: raw.env,
            child_env,
            window_rules,
            xwayland_enabled: raw.xwayland.enabled,
            xwayland_path: expand_tilde(&raw.xwayland.path),
            window_placement,
            output_configs,
            bindings,
            tap_bindings,
            mouse: mouse_bindings,
            resize_on_border,
            decoration_resize_snapped,
            decoration_fit_snapped,
            gestures: gesture_bindings,
            num_lock: raw.input.keyboard.num_lock.unwrap_or(true),
            caps_lock: raw.input.keyboard.caps_lock.unwrap_or(false),
        };
        // Stable sort by severity so the error bar's single slot shows the most
        // actionable warning first (rejected values before auto-clamped ones),
        // preserving parse order within each severity.
        errors.sort_by_key(|(severity, _)| *severity);
        let errors = errors.into_iter().map(|(_, msg)| msg).collect();
        (config, errors)
    }

    /// Find the first matching window rule for `app_id` and `title`.
    /// Kept for backward-compat callers that need a `&WindowRule` reference
    /// (layer shell position, render blur check).
    /// For building `AppliedWindowRule` use `resolve_window_rules` instead.
    pub fn match_window_rule(&self, app_id: &str, title: &str) -> Option<&WindowRule> {
        self.window_rules
            .iter()
            .find(|rule| rule.matches(app_id, title))
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
            .filter(|rule| rule.position.is_some() && rule.matches(app_id, title))
            .nth(n)
    }

    /// Merge ALL matching window rules and return the combined `AppliedWindowRule`.
    ///
    /// Rules are applied in config order; later rules override earlier ones for
    /// scalar fields (decoration, opacity, position, size). Boolean flags
    /// (widget, blur, pass_keys) are sticky-on.
    pub fn resolve_window_rules(&self, app_id: &str, title: &str) -> Option<AppliedWindowRule> {
        let mut result: Option<AppliedWindowRule> = None;
        for rule in &self.window_rules {
            if rule.matches(app_id, title) {
                match &mut result {
                    None => result = Some(AppliedWindowRule::from(rule)),
                    Some(r) => r.merge_from(rule),
                }
            }
        }
        result
    }

    /// Variant of `resolve_window_rules` for canvas-layer instances. Pairs with
    /// `match_window_rule_nth`: the Nth positioned rule (matching `app_id`/`title`)
    /// is treated as *this* instance's positioned rule; other positioned rules
    /// matching the same app are ignored, so multi-instance layer-shells like
    /// `waybar` get per-instance chrome. Non-positioned matching rules (e.g. an
    /// `app_id = "*"` wildcard) are still merged in, so shared chrome still
    /// applies across instances.
    pub fn resolve_window_rules_for_layer_instance(
        &self,
        app_id: &str,
        title: &str,
        instance_idx: usize,
    ) -> Option<AppliedWindowRule> {
        let nth_positioned_index: Option<usize> = self
            .window_rules
            .iter()
            .enumerate()
            .filter(|(_, r)| r.position.is_some() && r.matches(app_id, title))
            .map(|(i, _)| i)
            .nth(instance_idx);

        let mut result: Option<AppliedWindowRule> = None;
        for (i, rule) in self.window_rules.iter().enumerate() {
            if !rule.matches(app_id, title) {
                continue;
            }
            if rule.position.is_some() && Some(i) != nth_positioned_index {
                continue;
            }
            match &mut result {
                None => result = Some(AppliedWindowRule::from(rule)),
                Some(r) => r.merge_from(rule),
            }
        }
        result
    }
}

/// Render a modifier set as a binding string (e.g. `alt+shift`) for messages.
fn tap_combo_label(m: &Modifiers) -> String {
    let mut parts = Vec::new();
    if m.ctrl {
        parts.push("ctrl");
    }
    if m.alt {
        parts.push("alt");
    }
    if m.shift {
        parts.push("shift");
    }
    if m.logo {
        parts.push("super");
    }
    parts.join("+")
}

/// If a tap chord would be made unreachable by an xkb group-switch option in
/// `options` (comma-separated), return the offending option token. Only
/// modifier-*pair* toggles conflict: they turn the second modifier into a group
/// switch, so the pair never forms a held chord. Single-key toggles
/// (`grp:caps_toggle`, etc.) consume no modifier and so never conflict.
fn tap_option_conflict<'a>(mods: &Modifiers, options: &'a str) -> Option<&'a str> {
    options.split(',').map(str::trim).find(|opt| {
        (mods.alt && mods.shift && opt.contains("alt") && opt.contains("shift"))
            || (mods.ctrl
                && mods.shift
                && (opt.contains("ctrl") || opt.contains("control"))
                && opt.contains("shift"))
    })
}

fn resolve_background_kind(
    raw: toml::BackgroundFileConfig,
    errors: &mut Warnings,
) -> BackgroundKind {
    let toml::BackgroundFileConfig {
        kind,
        path,
        texture,
        mirror_tile: _,
        cache_shader: _,
        transparent_shader: _,
        cache_budget_mb: _,
    } = raw;
    let texture = texture.as_deref().map(expand_tilde);
    if let Some(t) = kind.as_deref() {
        return match (t, path) {
            ("shader", Some(p)) => BackgroundKind::Shader {
                path: expand_tilde(&p),
                texture,
            },
            ("tile", Some(p)) => BackgroundKind::Tile(expand_tilde(&p)),
            ("wallpaper", Some(p)) => BackgroundKind::Wallpaper(expand_tilde(&p)),
            ("default", _) => BackgroundKind::Default,
            // `path` is inapplicable here and silently ignored — like `texture`
            // on the tile/wallpaper types. Not worth a persistent error-bar
            // warning, since the background renders exactly as asked (nothing).
            ("none", _) => BackgroundKind::None,
            (_, None) => {
                collect_warn(
                    errors,
                    format!("config: [background] type=\"{t}\" requires `path`, using default"),
                );
                BackgroundKind::Default
            }
            (other, _) => {
                collect_warn(
                    errors,
                    format!("config: [background] unknown type \"{other}\", using default"),
                );
                BackgroundKind::Default
            }
        };
    }
    BackgroundKind::Default
}

impl Default for Config {
    fn default() -> Self {
        Self::from_raw(ConfigFile::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1 of the reference-config contract (#2/#3 live in
    /// `tests/config_reference_test.rs`): every config field the code defines is
    /// documented in `config.reference.toml`. Serializing a defaulted
    /// `ConfigFile` to JSON honors serde renames (`type`, `on-window`), and each
    /// leaf's full dotted path (`cursor.theme`, `input.keyboard.repeat_rate`) is
    /// matched under its section, so a field documented under the wrong section
    /// is still caught.
    ///
    /// Not covered: the inner fields of `[[window_rules]]` / `[[outputs]]` —
    /// those are documented in prose (`field — description`), not `key =` lines,
    /// so there is nothing to match a dotted path against.
    #[test]
    fn every_config_field_is_documented() {
        use std::collections::BTreeSet;

        const REFERENCE: &str = include_str!("../../config.reference.toml");
        // Deprecated, migration-only — intentionally undocumented.
        const ALLOWLIST: &[&str] = &["navigation.friction", "snap.same_edge", "snap.edge_center"];

        // Each `[a.b]` header is itself a documented path and sets the section
        // for the `key = …` lines under it (→ `a.b.key`).
        fn documented_paths(reference: &str) -> BTreeSet<String> {
            let mut paths = BTreeSet::new();
            let mut section = String::new();
            for raw in reference.lines() {
                let line = raw.trim_start();
                let line = line.strip_prefix("# ").unwrap_or(line);
                let line = line.strip_prefix("# ").unwrap_or(line).trim();
                // A real header is a bracketed token with nothing trailing, so
                // prose that merely opens with `[` isn't mistaken for a section
                // (mirrors the single-token key guard below).
                if line.starts_with('[') && line.ends_with(']') {
                    section = line
                        .trim_matches(|c: char| c == '[' || c == ']')
                        .trim()
                        .to_string();
                    paths.insert(section.clone());
                } else if let Some((key, _)) = line.split_once('=') {
                    // A real config key is a single token; skip illustrative prose
                    // with `=` (shell `export FOO=1`, GLSL `vec2 c = ...`).
                    let key = key.trim().trim_matches('"');
                    if !key.is_empty() && !key.contains(char::is_whitespace) {
                        paths.insert(if section.is_empty() {
                            key.to_string()
                        } else {
                            format!("{section}.{key}")
                        });
                    }
                }
            }
            paths
        }

        // Dotted path of every leaf; an empty object counts as a leaf, not a
        // section to recurse into.
        fn collect_paths(value: &serde_json::Value, prefix: &str, out: &mut BTreeSet<String>) {
            if let serde_json::Value::Object(map) = value {
                for (key, child) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    match child {
                        serde_json::Value::Object(inner) if !inner.is_empty() => {
                            collect_paths(child, &path, out)
                        }
                        _ => {
                            out.insert(path);
                        }
                    }
                }
            }
        }

        // The one Option<table> that defaults to None — populate it so its
        // fields are walked individually, not hidden behind a null.
        let mut raw = ConfigFile::default();
        raw.output.outline = Some(Default::default());
        let default_json = serde_json::to_value(&raw).expect("ConfigFile should serialize");
        let mut code_paths = BTreeSet::new();
        collect_paths(&default_json, "", &mut code_paths);

        let documented = documented_paths(REFERENCE);
        let undocumented: Vec<&str> = code_paths
            .iter()
            .map(String::as_str)
            .filter(|p| !documented.contains(*p) && !ALLOWLIST.contains(p))
            .collect();

        assert!(
            undocumented.is_empty(),
            "config fields defined in code but undocumented in config.reference.toml: {undocumented:?}"
        );
    }

    #[test]
    fn rejections_sort_before_clamps_for_error_bar() {
        // accel_speed is parsed before [decorations], so without severity
        // ordering the auto-clamped value would occupy the bar's first slot.
        let toml_str = r#"
            [input.trackpad]
            accel_speed = 5.0

            [decorations]
            font_weight = "bogus"
        "#;
        let (_config, warnings) = Config::from_toml_collect(toml_str).unwrap();
        assert!(warnings[0].contains("font_weight"), "got: {warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("accel_speed")));
    }

    #[test]
    fn out_of_range_navigation_values_warn_and_clamp() {
        let toml_str = r#"
            [navigation]
            drift = 5.0
            trackpad_speed = -2.0
        "#;
        let (config, warnings) = Config::from_toml_collect(toml_str).unwrap();
        assert_eq!(config.drift, 1.0);
        assert_eq!(config.trackpad_speed, 0.0);
        assert!(warnings.iter().any(|w| w.contains("navigation.drift")));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("navigation.trackpad_speed"))
        );
    }

    #[test]
    fn animation_speed_zero_falls_back_to_default() {
        let toml_str = r#"
            [navigation]
            animation_speed = 0.0
        "#;
        let (config, warnings) = Config::from_toml_collect(toml_str).unwrap();
        assert_eq!(config.animation_speed, 0.3);
        assert!(warnings.iter().any(|w| w.contains("animation_speed")));
    }

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

    #[test]
    fn remember_layout_per_window_parses_from_toml() {
        let toml_str = r#"
            [input.keyboard]
            remember_layout_per_window = true
        "#;
        let config = Config::from_toml(toml_str).unwrap();
        assert!(config.remember_layout_per_window);
    }

    #[test]
    fn remember_layout_per_window_omitted_in_toml_defaults_false() {
        let config = Config::from_toml("").unwrap();
        assert!(!config.remember_layout_per_window);
    }

    #[test]
    fn modifier_only_keybinding_parses_as_tap() {
        let toml_str = r#"
            [keybindings]
            "alt+shift" = "switch-layout next"
        "#;
        let config = Config::from_toml(toml_str).unwrap();
        let mods = Modifiers {
            alt: true,
            shift: true,
            ..Modifiers::EMPTY
        };
        assert!(matches!(
            config.tap_lookup(&mods),
            Some(Action::SwitchLayout(LayoutSwitch::Next))
        ));
    }

    #[test]
    fn normal_keybinding_is_not_a_tap() {
        let toml_str = r#"
            [keybindings]
            "mod+q" = "close-window"
        "#;
        let config = Config::from_toml(toml_str).unwrap();
        assert!(config.tap_bindings.is_empty());
    }

    #[test]
    fn tap_binding_conflicting_with_xkb_option_warns() {
        let toml_str = r#"
            [keybindings]
            "alt+shift" = "switch-layout next"

            [input.keyboard]
            options = "grp:alt_shift_toggle"
        "#;
        let (_config, warnings) = Config::from_toml_collect(toml_str).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("grp:alt_shift_toggle") && w.contains("tap binding")),
            "got: {warnings:?}"
        );
    }

    #[test]
    fn tap_binding_conflict_warns_for_side_specific_toggle() {
        // grp:lalt_lshift_toggle still consumes alt+shift — the warning names the
        // actual option token, not a hard-coded one.
        let toml_str = r#"
            [keybindings]
            "alt+shift" = "switch-layout next"

            [input.keyboard]
            options = "grp:lalt_lshift_toggle,compose:ralt"
        "#;
        let (_config, warnings) = Config::from_toml_collect(toml_str).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("grp:lalt_lshift_toggle")),
            "got: {warnings:?}"
        );
    }

    #[test]
    fn tap_binding_with_nonconflicting_xkb_option_does_not_warn() {
        let toml_str = r#"
            [keybindings]
            "alt+shift" = "switch-layout next"

            [input.keyboard]
            options = "grp:caps_toggle"
        "#;
        let (_config, warnings) = Config::from_toml_collect(toml_str).unwrap();
        assert!(
            !warnings.iter().any(|w| w.contains("tap binding")),
            "got: {warnings:?}"
        );
    }
}
