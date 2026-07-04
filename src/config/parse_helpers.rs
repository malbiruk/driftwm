//! TOML → config-type conversion helpers.
//!
//! Bridges raw serde structs in `super::toml` to the processed types in
//! `super::types`, applying defaults, clamping, and validation. No
//! compositor state is touched — these are pure functions.

use std::collections::HashMap;

use smithay::utils::Transform;

use super::parse::parse_key_combo;
use super::toml::{
    BackendFileConfig, DecorationFileConfig, EffectsFileConfig, HotCornersFile, OutputOutlineConfig,
    OutputRuleFile, PassKeysFile, WindowRuleFile,
};
use super::types::{
    Action, BackendConfig, DecorationConfig, DecorationMode, EffectsConfig, FontWeight, KeyCombo,
    ModKey, OutputConfig, OutputMode, OutputOutlineSettings, OutputPosition, PassKeys, Pattern,
    TitleAlign, WindowRule, HotCorners,
};

/// How actionable a config warning is. The error bar has room for one message,
/// so `Rejected` (a value the compositor couldn't use) sorts ahead of
/// `Corrected` (a value it auto-clamped to something usable). Declaration order
/// is the sort order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Severity {
    Rejected,
    Corrected,
}

/// Config warnings tagged with severity; sorted by it before display.
pub(super) type Warnings = Vec<(Severity, String)>;

fn push_warn(errors: &mut Warnings, severity: Severity, msg: String) {
    tracing::warn!("{msg}");
    errors.push((severity, msg));
}

/// Warn about a value the compositor could not use (ignored or defaulted) — as
/// opposed to one it auto-clamped; see [`Severity`].
pub(super) fn collect_warn(errors: &mut Warnings, msg: String) {
    push_warn(errors, Severity::Rejected, msg);
}

/// Clamp into `[min, max]`, warning when out of range so invalid numeric
/// config is surfaced rather than silently corrected.
pub(super) fn clamp_warn<T>(value: T, min: T, max: T, field: &str, errors: &mut Warnings) -> T
where
    T: PartialOrd + std::fmt::Display + Copy,
{
    use std::cmp::Ordering;
    // `partial_cmp` returns None for NaN; group it with the below-min case so an
    // invalid float is clamped and surfaced instead of passing every comparison.
    if matches!(value.partial_cmp(&min), None | Some(Ordering::Less)) {
        push_warn(
            errors,
            Severity::Corrected,
            format!("config: {field} {value} below minimum {min}, using {min}"),
        );
        min
    } else if value > max {
        push_warn(
            errors,
            Severity::Corrected,
            format!("config: {field} {value} above maximum {max}, using {max}"),
        );
        max
    } else {
        value
    }
}

/// Floor a value at zero, warning when it was negative (or NaN). For knobs with
/// a natural lower bound but no upper limit (speeds, steps, distances, sizes).
pub(super) fn non_negative<T>(value: T, field: &str, errors: &mut Warnings) -> T
where
    T: PartialOrd + std::fmt::Display + Copy + Default,
{
    let zero = T::default();
    if matches!(
        value.partial_cmp(&zero),
        None | Some(std::cmp::Ordering::Less)
    ) {
        push_warn(
            errors,
            Severity::Corrected,
            format!("config: {field} {value} is negative, using 0"),
        );
        zero
    } else {
        value
    }
}

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

pub(super) fn parse_output_outline(
    raw: OutputOutlineConfig,
    errors: &mut Warnings,
) -> OutputOutlineSettings {
    let defaults = OutputOutlineSettings::default();
    let color = match raw.color {
        Some(s) => parse_color(&s).unwrap_or_else(|| {
            collect_warn(
                errors,
                format!("config: invalid output outline color '{s}', using default"),
            );
            defaults.color
        }),
        None => defaults.color,
    };
    OutputOutlineSettings {
        color,
        thickness: clamp_warn(
            raw.thickness.unwrap_or(defaults.thickness),
            0,
            i32::MAX,
            "output.outline.thickness",
            errors,
        ),
        opacity: clamp_warn(
            raw.opacity.unwrap_or(defaults.opacity),
            0.0,
            1.0,
            "output.outline.opacity",
            errors,
        ),
    }
}

fn resolve_color(
    opt: Option<String>,
    default: [u8; 4],
    name: &str,
    errors: &mut Warnings,
) -> [u8; 4] {
    match opt {
        Some(s) => parse_color(&s).unwrap_or_else(|| {
            collect_warn(
                errors,
                format!("config: invalid {name} color '{s}', using default"),
            );
            default
        }),
        None => default,
    }
}

pub(super) fn parse_decoration_config(
    raw: DecorationFileConfig,
    errors: &mut Warnings,
) -> DecorationConfig {
    let defaults = DecorationConfig::default();

    let default_mode = match raw.default_mode.as_deref() {
        Some("client") | None => DecorationMode::Client,
        Some("minimal") => DecorationMode::Minimal,
        Some("none") => DecorationMode::None,
        Some("server") => {
            // Reserved for per-window rules. As a global default it's a footgun:
            // GTK/Electron toolkits ignore xdg-decoration and keep drawing CSD,
            // producing a double title bar.
            collect_warn(
                errors,
                "config: default_mode = \"server\" is not supported globally (many toolkits \
                 ignore xdg-decoration and draw double titlebars). Use it in [[window_rules]] \
                 for specific apps instead. Falling back to \"client\"."
                    .to_string(),
            );
            DecorationMode::Client
        }
        Some(other) => {
            collect_warn(
                errors,
                format!("config: unknown default_mode '{other}', using client"),
            );
            DecorationMode::Client
        }
    };

    let font_weight = match raw
        .font_weight
        .as_deref()
        .map(|s| s.trim().to_lowercase())
        .as_deref()
    {
        Some("thin" | "hairline") => FontWeight::Thin,
        Some("extralight" | "extra-light" | "ultralight") => FontWeight::ExtraLight,
        Some("light") => FontWeight::Light,
        Some("normal" | "regular") => FontWeight::Normal,
        Some("medium") | None => FontWeight::Medium,
        Some("semibold" | "semi-bold" | "demibold") => FontWeight::SemiBold,
        Some("bold") => FontWeight::Bold,
        Some("extrabold" | "extra-bold" | "ultrabold") => FontWeight::ExtraBold,
        Some("black" | "heavy") => FontWeight::Black,
        Some(other) => {
            collect_warn(
                errors,
                format!("config: unknown font_weight '{other}', using medium"),
            );
            FontWeight::Medium
        }
    };

    let title_align = match raw.title_align.as_deref() {
        Some("left") => TitleAlign::Left,
        Some("center") | None => TitleAlign::Center,
        Some(other) => {
            collect_warn(
                errors,
                format!("config: unknown title_align '{other}', using center"),
            );
            TitleAlign::Center
        }
    };

    DecorationConfig {
        bg_color: resolve_color(raw.bg_color, defaults.bg_color, "bg_color", errors),
        fg_color: resolve_color(raw.fg_color, defaults.fg_color, "fg_color", errors),
        corner_radius: clamp_warn(
            raw.corner_radius.unwrap_or(defaults.corner_radius),
            0,
            i32::MAX,
            "decorations.corner_radius",
            errors,
        ),
        default_mode,
        border_width: clamp_warn(
            raw.border_width.unwrap_or(defaults.border_width),
            0,
            i32::MAX,
            "decorations.border_width",
            errors,
        ),
        border_color: resolve_color(
            raw.border_color,
            defaults.border_color,
            "border_color",
            errors,
        ),
        border_color_focused: resolve_color(
            raw.border_color_focused,
            defaults.border_color_focused,
            "border_color_focused",
            errors,
        ),
        shadow: raw.shadow.unwrap_or(defaults.shadow),
        title_bar_height: clamp_warn(
            raw.title_bar_height.unwrap_or(defaults.title_bar_height),
            1,
            i32::MAX,
            "decorations.title_bar_height",
            errors,
        ),
        font: raw.font.unwrap_or(defaults.font),
        font_size: clamp_warn(
            raw.font_size.unwrap_or(defaults.font_size),
            1,
            u32::MAX,
            "decorations.font_size",
            errors,
        ),
        font_weight,
        title_align,
    }
}

fn parse_pattern(s: String, errors: &mut Warnings) -> Pattern {
    // Strings wrapped in `/…/` are treated as regular expressions.
    // Everything else is a glob pattern (`*` = any sequence of chars).
    if s.len() >= 2 && s.starts_with('/') && s.ends_with('/') {
        let inner = &s[1..s.len() - 1];
        match regex::Regex::new(inner) {
            Ok(re) => return Pattern::Regex(re),
            Err(e) => collect_warn(
                errors,
                format!("config: invalid regex '/{inner}/': {e}, treating as literal glob"),
            ),
        }
    }
    Pattern::Glob(s)
}

pub(super) fn parse_window_rule(
    r: WindowRuleFile,
    mod_key: ModKey,
    errors: &mut Warnings,
) -> Option<WindowRule> {
    if r.app_id.is_none() && r.title.is_none() {
        collect_warn(
            errors,
            "config: window rule has no match criteria (app_id/title), skipping".to_string(),
        );
        return None;
    }
    // None = "field not set" → window inherits [decorations] default_mode.
    // Some(_) = explicit user choice that overrides the default.
    let decoration = match r.decoration.as_deref() {
        None => None,
        Some("none") => Some(DecorationMode::None),
        Some("minimal") => Some(DecorationMode::Minimal),
        Some("server") => Some(DecorationMode::Server),
        Some("client") => Some(DecorationMode::Client),
        Some(other) => {
            collect_warn(
                errors,
                format!(
                    "config: unknown decoration mode '{other}', falling through to default_mode"
                ),
            );
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
                        collect_warn(
                            errors,
                            format!("config: pass_keys invalid key combo '{s}': {e}"),
                        );
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
    let app_id = r.app_id.map(|s| parse_pattern(s, errors));
    let title = r.title.map(|s| parse_pattern(s, errors));
    let size = r.size.and_then(|[w, h]| {
        if w > 0 && h > 0 {
            Some((w, h))
        } else {
            collect_warn(
                errors,
                format!("config: window rule size must be positive, got [{w}, {h}]"),
            );
            None
        }
    });
    let opacity = r
        .opacity
        .map(|v| clamp_warn(v, 0.0, 1.0, "window rule opacity", errors));
    let border_width = r
        .border_width
        .map(|bw| clamp_warn(bw, 0, i32::MAX, "window rule border_width", errors));
    let corner_radius = r
        .corner_radius
        .map(|cr| clamp_warn(cr, 0, i32::MAX, "window rule corner_radius", errors));
    let border_color = r.border_color.and_then(|s| {
        let parsed = parse_color(&s);
        if parsed.is_none() {
            collect_warn(
                errors,
                format!("config: window rule border_color '{s}' invalid, ignoring"),
            );
        }
        parsed
    });
    let border_color_focused = r.border_color_focused.and_then(|s| {
        let parsed = parse_color(&s);
        if parsed.is_none() {
            collect_warn(
                errors,
                format!("config: window rule border_color_focused '{s}' invalid, ignoring"),
            );
        }
        parsed
    });
    Some(WindowRule {
        app_id,
        title,
        position: r.position.map(|[x, y]| (x, y)),
        size,
        widget: r.widget,
        pinned_to_screen: r.pinned_to_screen,
        decoration,
        blur: r.blur.unwrap_or(false),
        opacity,
        pass_keys,
        border_width,
        border_color,
        border_color_focused,
        corner_radius,
        shadow: r.shadow,
        output: r.output,
    })
}

pub(super) fn parse_effects_config(raw: EffectsFileConfig, errors: &mut Warnings) -> EffectsConfig {
    EffectsConfig {
        blur_radius: raw.blur_radius.unwrap_or(2),
        blur_strength: non_negative(
            raw.blur_strength.unwrap_or(1.1),
            "effects.blur_strength",
            errors,
        ),
        animate_blur: raw.animate_blur.unwrap_or(false),
    }
}

pub(super) fn parse_backend_config(raw: BackendFileConfig) -> BackendConfig {
    BackendConfig {
        wait_for_frame_completion: raw.wait_for_frame_completion.unwrap_or(false),
        disable_direct_scanout: raw.disable_direct_scanout.unwrap_or(false),
        disable_hardware_cursor: raw.disable_hardware_cursor.unwrap_or(false),
        max_capture_fps: raw.max_capture_fps.unwrap_or(0),
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

    let hot_corners = match r.hot_corners {
        Some(hcf) => parse_hot_corners(hcf)?,
        None => HotCorners::default(),
    };

    Ok(OutputConfig {
        name: r.name,
        scale,
        transform,
        position,
        mode,
        hot_corners,
    })
}

pub(super) fn parse_hot_corners(hcf: HotCornersFile) -> Result<HotCorners, String> {
    use super::parse::parse_action;
    use super::types::HotCorner;

    let threshold = hcf.threshold.unwrap_or(4.0);
    if threshold <= 0.0 {
        return Err(format!("hot_corners.threshold must be > 0, got {threshold}"));
    }

    let mut bindings = HashMap::new();
    let try_set = |corner: HotCorner, raw: &Option<String>, bindings: &mut HashMap<HotCorner, Action>|
        -> Result<(), String>
    {
        if let Some(s) = raw {
            if s == "none" {
                bindings.remove(&corner);
            } else {
                let action = parse_action(s)?;
                bindings.insert(corner, action);
            }
        }
        Ok(())
    };

    try_set(HotCorner::TopLeft,     &hcf.top_left,     &mut bindings)?;
    try_set(HotCorner::TopRight,    &hcf.top_right,    &mut bindings)?;
    try_set(HotCorner::BottomLeft,  &hcf.bottom_left,  &mut bindings)?;
    try_set(HotCorner::BottomRight, &hcf.bottom_right, &mut bindings)?;

    let disable_when_fullscreen = hcf.disable_when_fullscreen.unwrap_or(true);
    let disable_while_dragging  = hcf.disable_while_dragging.unwrap_or(true);

    Ok(HotCorners {
        bindings,
        threshold,
        disable_when_fullscreen,
        disable_while_dragging,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_warn_in_range_is_silent() {
        let mut errors = Vec::new();
        assert_eq!(clamp_warn(5, 0, 10, "x", &mut errors), 5);
        assert!(errors.is_empty());
    }

    #[test]
    fn clamp_warn_below_min_clamps_and_collects() {
        let mut errors = Vec::new();
        assert_eq!(clamp_warn(-3, 0, 10, "x", &mut errors), 0);
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn clamp_warn_above_max_clamps_and_collects() {
        let mut errors = Vec::new();
        assert_eq!(clamp_warn(99.0, 0.0, 1.0, "x", &mut errors), 1.0);
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn non_negative_passes_through_when_positive() {
        let mut errors = Vec::new();
        assert_eq!(non_negative(3.5, "x", &mut errors), 3.5);
        assert_eq!(non_negative(0, "y", &mut errors), 0);
        assert!(errors.is_empty());
    }

    #[test]
    fn non_negative_floors_and_collects_when_negative() {
        let mut errors = Vec::new();
        assert_eq!(non_negative(-2.0, "x", &mut errors), 0.0);
        assert_eq!(non_negative(-7, "y", &mut errors), 0);
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn nan_is_treated_as_invalid_not_silently_passed() {
        let mut errors = Vec::new();
        assert_eq!(clamp_warn(f64::NAN, 0.0, 1.0, "x", &mut errors), 0.0);
        assert_eq!(non_negative(f64::NAN, "y", &mut errors), 0.0);
        assert_eq!(errors.len(), 2);
    }

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
