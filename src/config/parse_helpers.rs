//! TOML → config-type conversion helpers.
//!
//! Bridges raw serde structs in `super::toml` to the processed types in
//! `super::types`, applying defaults, clamping, and validation. No
//! compositor state is touched — these are pure functions.

use smithay::utils::Transform;

use super::parse::parse_key_combo;
use super::toml::{
    BackendFileConfig, DecorationFileConfig, EffectsFileConfig, OutputOutlineConfig,
    OutputRuleFile, PassKeysFile, WindowRuleFile,
};
use super::types::{
    BackendConfig, DecorationConfig, DecorationMode, EffectsConfig, KeyCombo, ModKey, OutputConfig,
    OutputMode, OutputOutlineSettings, OutputPosition, PassKeys, Pattern, WindowRule,
};

pub(super) fn parse_color(s: &str) -> Option<[u8; 4]> {
    let hex = s.strip_prefix('#')?;
    match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some([r, g, b, 0xFF])
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some([r, g, b, a])
        }
        _ => None,
    }
}

pub(super) fn parse_output_outline(raw: OutputOutlineConfig) -> OutputOutlineSettings {
    let defaults = OutputOutlineSettings::default();
    let color = match raw.color {
        Some(s) => parse_color(&s).unwrap_or_else(|| {
            tracing::warn!("Invalid output outline color '{s}', using default");
            defaults.color
        }),
        None => defaults.color,
    };
    OutputOutlineSettings {
        color,
        thickness: raw.thickness.unwrap_or(defaults.thickness).max(0),
        opacity: raw.opacity.unwrap_or(defaults.opacity).clamp(0.0, 1.0),
    }
}

pub(super) fn parse_decoration_config(raw: DecorationFileConfig) -> DecorationConfig {
    let defaults = DecorationConfig::default();

    let resolve = |opt: Option<String>, default: [u8; 4], name: &str| -> [u8; 4] {
        match opt {
            Some(s) => parse_color(&s).unwrap_or_else(|| {
                tracing::warn!("Invalid {name} color '{s}', using default");
                default
            }),
            None => default,
        }
    };

    let default_mode = match raw.default_mode.as_deref() {
        Some("client") | None => DecorationMode::Client,
        Some("borderless") => DecorationMode::Borderless,
        Some("none") => DecorationMode::None,
        Some("server") => {
            // Reserved for per-window rules. As a global default it's a footgun:
            // GTK/Electron toolkits ignore xdg-decoration and keep drawing CSD,
            // producing a double title bar.
            tracing::warn!(
                "default_mode = \"server\" is not supported globally (many toolkits \
                 ignore xdg-decoration and draw double titlebars). Use it in \
                 [[window_rules]] for specific apps instead. Falling back to \"client\"."
            );
            DecorationMode::Client
        }
        Some(other) => {
            tracing::warn!("Unknown default_mode '{other}', using client");
            DecorationMode::Client
        }
    };

    DecorationConfig {
        bg_color: resolve(raw.bg_color, defaults.bg_color, "bg_color"),
        fg_color: resolve(raw.fg_color, defaults.fg_color, "fg_color"),
        corner_radius: raw.corner_radius.unwrap_or(defaults.corner_radius).max(0),
        default_mode,
    }
}

fn parse_pattern(s: String) -> Pattern {
    // Strings wrapped in `/…/` are treated as regular expressions.
    // Everything else is a glob pattern (`*` = any sequence of chars).
    if s.len() >= 2 && s.starts_with('/') && s.ends_with('/') {
        let inner = &s[1..s.len() - 1];
        match regex::Regex::new(inner) {
            Ok(re) => return Pattern::Regex(re),
            Err(e) => tracing::warn!("Invalid regex '/{inner}/': {e}, treating as literal glob"),
        }
    }
    Pattern::Glob(s)
}

pub(super) fn parse_window_rule(r: WindowRuleFile, mod_key: ModKey) -> Option<WindowRule> {
    if r.app_id.is_none() && r.title.is_none() && r.xclass.is_none() && r.xinstance.is_none() {
        tracing::warn!("Window rule has no match criteria (app_id/title/xclass/xinstance), skipping");
        return None;
    }
    // None = "field not set" → window inherits [decorations] default_mode.
    // Some(_) = explicit user choice that overrides the default.
    let decoration = match r.decoration.as_deref() {
        None => None,
        Some("none") => Some(DecorationMode::None),
        Some("borderless") => Some(DecorationMode::Borderless),
        Some("server") => Some(DecorationMode::Server),
        Some("client") => Some(DecorationMode::Client),
        Some(other) => {
            tracing::warn!("Unknown decoration mode '{other}', falling through to default_mode");
            None
        }
    };
    let pass_keys = match r.pass_keys {
        None | Some(PassKeysFile::Bool(false)) => PassKeys::None,
        Some(PassKeysFile::Bool(true)) => PassKeys::All,
        Some(PassKeysFile::Keys(strs)) => {
            let combos: Vec<KeyCombo> = strs
                .iter()
                .filter_map(|s| match parse_key_combo(s, mod_key) {
                    Ok(mut c) => {
                        c.normalize();
                        Some(c)
                    }
                    Err(e) => {
                        tracing::warn!("pass_keys: invalid key combo '{s}': {e}");
                        None
                    }
                })
                .collect();
            if combos.is_empty() {
                PassKeys::None
            } else {
                PassKeys::Only(combos)
            }
        }
    };
    Some(WindowRule {
        app_id: r.app_id.map(parse_pattern),
        title: r.title.map(parse_pattern),
        xclass: r.xclass.map(parse_pattern),
        xinstance: r.xinstance.map(parse_pattern),
        position: r.position.map(|[x, y]| (x, y)),
        size: r.size.and_then(|[w, h]| {
            if w > 0 && h > 0 {
                Some((w, h))
            } else {
                tracing::warn!("Window rule size must be positive, got [{w}, {h}]");
                None
            }
        }),
        widget: r.widget,
        decoration,
        blur: r.blur.unwrap_or(false),
        blur_passes: r.blur_passes.map(|p| p.clamp(1, 8)),
        blur_strength: r.blur_strength.map(|s| {
            if s <= 0.0 {
                tracing::warn!("Window rule blur_strength {s} must be > 0, clamping to 0.1");
                0.1_f64
            } else {
                s
            }
        }),
        opacity: r.opacity.map(|v| {
            if !(0.0..=1.0).contains(&v) {
                tracing::warn!("Window rule opacity {v} out of range, clamping to 0.0–1.0");
                v.clamp(0.0, 1.0)
            } else {
                v
            }
        }),
        pass_keys,
    })
}

pub(super) fn parse_effects_config(raw: EffectsFileConfig) -> EffectsConfig {
    EffectsConfig {
        blur_radius: raw.blur_radius.unwrap_or(2),
        blur_strength: raw.blur_strength.unwrap_or(1.1),
        animated_blur: raw.animated_blur.unwrap_or(false),
    }
}

pub(super) fn parse_backend_config(raw: BackendFileConfig) -> BackendConfig {
    BackendConfig {
        wait_for_frame_completion: raw.wait_for_frame_completion.unwrap_or(false),
        disable_direct_scanout: raw.disable_direct_scanout.unwrap_or(false),
    }
}

pub(super) fn parse_transform(s: &str) -> Result<Transform, String> {
    match s {
        "normal" => Ok(Transform::Normal),
        "90" => Ok(Transform::_90),
        "180" => Ok(Transform::_180),
        "270" => Ok(Transform::_270),
        "flipped" => Ok(Transform::Flipped),
        "flipped-90" => Ok(Transform::Flipped90),
        "flipped-180" => Ok(Transform::Flipped180),
        "flipped-270" => Ok(Transform::Flipped270),
        _ => Err(format!("unknown transform '{s}'")),
    }
}

pub(super) fn parse_output_mode(s: &str) -> Result<OutputMode, String> {
    if s == "preferred" {
        return Ok(OutputMode::Preferred);
    }
    // "WxH" or "WxH@Hz"
    let (res_part, hz_part) = match s.split_once('@') {
        Some((res, hz)) => (res, Some(hz)),
        None => (s, None),
    };
    let (w_str, h_str) = res_part
        .split_once('x')
        .ok_or_else(|| format!("invalid mode '{s}', expected WxH or WxH@Hz"))?;
    let w: i32 = w_str
        .parse()
        .map_err(|_| format!("invalid width in mode '{s}'"))?;
    let h: i32 = h_str
        .parse()
        .map_err(|_| format!("invalid height in mode '{s}'"))?;
    match hz_part {
        Some(hz_str) => {
            let hz: u32 = hz_str
                .parse()
                .map_err(|_| format!("invalid refresh rate in mode '{s}'"))?;
            Ok(OutputMode::SizeRefresh(w, h, hz))
        }
        None => Ok(OutputMode::Size(w, h)),
    }
}

pub(super) fn parse_output_position(val: &::toml::Value) -> Result<OutputPosition, String> {
    match val {
        ::toml::Value::String(s) if s == "auto" => Ok(OutputPosition::Auto),
        ::toml::Value::String(s) => Err(format!(
            "invalid position '{s}', expected \"auto\" or [x, y]"
        )),
        ::toml::Value::Array(arr) => {
            if arr.len() != 2 {
                return Err(format!(
                    "position array must have 2 elements, got {}",
                    arr.len()
                ));
            }
            let x = arr[0]
                .as_integer()
                .ok_or("position[0] must be an integer")? as i32;
            let y = arr[1]
                .as_integer()
                .ok_or("position[1] must be an integer")? as i32;
            Ok(OutputPosition::Fixed(x, y))
        }
        _ => Err("position must be \"auto\" or [x, y]".into()),
    }
}

pub(super) fn parse_output_rule(r: OutputRuleFile) -> Result<OutputConfig, String> {
    let scale = match r.scale {
        Some(s) if s <= 0.0 => return Err(format!("scale must be positive, got {s}")),
        other => other,
    };
    let transform = r.transform.map(|s| parse_transform(&s)).transpose()?;
    let position = r
        .position
        .map(|v| parse_output_position(&v))
        .transpose()?
        .unwrap_or_default();
    let mode = r
        .mode
        .map(|s| parse_output_mode(&s))
        .transpose()?
        .unwrap_or_default();
    Ok(OutputConfig {
        name: r.name,
        scale,
        transform,
        position,
        mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_transform_all_variants() {
        let cases = [
            ("normal", Transform::Normal),
            ("90", Transform::_90),
            ("180", Transform::_180),
            ("270", Transform::_270),
            ("flipped", Transform::Flipped),
            ("flipped-90", Transform::Flipped90),
            ("flipped-180", Transform::Flipped180),
            ("flipped-270", Transform::Flipped270),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_transform(input).unwrap(), expected, "input: {input}");
        }
    }

    #[test]
    fn parse_transform_invalid() {
        assert!(parse_transform("upside-down").is_err());
        assert!(parse_transform("").is_err());
    }

    #[test]
    fn parse_mode_preferred() {
        assert_eq!(
            parse_output_mode("preferred").unwrap(),
            OutputMode::Preferred
        );
    }

    #[test]
    fn parse_mode_size() {
        assert_eq!(
            parse_output_mode("1920x1080").unwrap(),
            OutputMode::Size(1920, 1080)
        );
    }

    #[test]
    fn parse_mode_size_refresh() {
        assert_eq!(
            parse_output_mode("2560x1440@144").unwrap(),
            OutputMode::SizeRefresh(2560, 1440, 144)
        );
    }

    #[test]
    fn parse_mode_invalid() {
        assert!(parse_output_mode("big").is_err());
        assert!(parse_output_mode("1920").is_err());
        assert!(parse_output_mode("1920x1080@fast").is_err());
    }

    #[test]
    fn parse_position_auto() {
        let val = ::toml::Value::String("auto".into());
        assert_eq!(parse_output_position(&val).unwrap(), OutputPosition::Auto);
    }

    #[test]
    fn parse_position_fixed() {
        let val = ::toml::Value::Array(vec![
            ::toml::Value::Integer(100),
            ::toml::Value::Integer(-200),
        ]);
        assert_eq!(
            parse_output_position(&val).unwrap(),
            OutputPosition::Fixed(100, -200)
        );
    }

    #[test]
    fn parse_position_invalid_string() {
        let val = ::toml::Value::String("left".into());
        assert!(parse_output_position(&val).is_err());
    }

    #[test]
    fn parse_position_wrong_array_length() {
        let val = ::toml::Value::Array(vec![::toml::Value::Integer(1)]);
        assert!(parse_output_position(&val).is_err());
    }
}
