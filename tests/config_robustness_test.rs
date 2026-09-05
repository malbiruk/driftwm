//! `Config::from_toml_collect` must never panic: malformed TOML and type
//! mismatches become `Err`, and semantically-bad values (bad regex, unknown
//! decoration mode, out-of-range numbers) degrade to collected warnings. This
//! mechanically pins the hot-reload promise — a bad config edit keeps the old
//! config and never crashes the compositor (see `src/state/reload.rs`). proptest
//! turns any panic in the parser into a test failure with a minimized input.

use driftwm::config::Config;
use proptest::prelude::*;

/// Wrap a raw body as a TOML basic string, escaping `\` and `"` so whatever the
/// generators emit stays syntactically valid — the point is to drive the value
/// parsers, not to fail at the TOML layer.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Printable-but-hostile string bodies: glob/regex metacharacters, unicode,
/// empty, and deliberately malformed forms (unterminated regex group).
fn hostile_text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        prop::string::string_regex(r#"[A-Za-z0-9_*+/.^$?\\ -]{0,16}"#).unwrap(),
        Just("日本語".to_string()),
        Just("mod+shift+q".to_string()),
        Just("(unterminated".to_string()),
    ]
}

proptest! {
    #[test]
    fn arbitrary_input_never_panics(s in any::<String>()) {
        let _ = Config::from_toml_collect(&s);
    }
}

/// Real section headers (names verified against `ConfigFile` in
/// `src/config/toml.rs`) so generated blocks land in real handlers.
const SECTIONS: &[&str] = &[
    "[keybindings]",
    "[mouse]",
    "[gestures]",
    "[touch]",
    "[decorations]",
    "[navigation]",
    "[snap]",
    "[zoom]",
    "[backend]",
    "[background]",
    "[[window_rules]]",
    "[[outputs]]",
];

/// A mix of real field names and free identifiers, so both known and unknown
/// keys are exercised (unknown fields must be a clean `Err`, not a panic).
const FIELDS: &[&str] = &[
    "app_id",
    "title",
    "position",
    "size",
    "widget",
    "decoration",
    "blur",
    "opacity",
    "border_color",
    "pass_keys",
    "pass_mouse",
    "name",
    "scale",
    "mode",
    "transform",
    "gap",
    "distance",
    "step",
    "enabled",
    "shadow",
    "font_size",
];

fn key() -> impl Strategy<Value = String> {
    prop_oneof![
        prop::sample::select(FIELDS).prop_map(str::to_string),
        prop::string::string_regex("[A-Za-z_][A-Za-z0-9_]{0,7}").unwrap(),
    ]
}

/// A random TOML value across all scalar and array shapes, including extremes
/// (`i64::MIN`/`MAX`, overflowing/NaN floats) and mixed-arity arrays.
fn value_frag() -> impl Strategy<Value = String> {
    prop_oneof![
        hostile_text().prop_map(|s| quote(&s)),
        any::<i64>().prop_map(|n| n.to_string()),
        Just(i64::MIN.to_string()),
        Just(i64::MAX.to_string()),
        Just("1e999".to_string()),
        Just("-nan".to_string()),
        any::<bool>().prop_map(|b| b.to_string()),
        prop::collection::vec(
            prop_oneof![
                any::<i64>().prop_map(|n| n.to_string()),
                Just("\"s\"".to_string())
            ],
            0..4,
        )
        .prop_map(|xs| format!("[{}]", xs.join(", "))),
    ]
}

fn section_block() -> impl Strategy<Value = String> {
    (
        prop::sample::select(SECTIONS),
        prop::collection::vec((key(), value_frag()), 0..5),
    )
        .prop_map(|(header, lines)| {
            let body: String = lines.iter().map(|(k, v)| format!("{k} = {v}\n")).collect();
            format!("{header}\n{body}")
        })
}

proptest! {
    #[test]
    fn toml_shaped_fuzz_never_panics(blocks in prop::collection::vec(section_block(), 0..6)) {
        let _ = Config::from_toml_collect(&blocks.concat());
    }
}

// The blocks below build syntactically valid configs that reach
// `from_raw_collect` (stage 2), where any hidden panic in the deep string
// parsers would live: key combos, glob/regex patterns, colors, output
// modes/transforms/positions.

/// A match pattern: bare (glob/exact) or wrapped in `/…/` (regex — arbitrary
/// inner text, so compile errors must degrade to warnings, not panic).
fn pattern_value() -> impl Strategy<Value = String> {
    prop_oneof![
        hostile_text(),
        hostile_text().prop_map(|s| format!("/{s}/")),
    ]
}

/// `pass_keys` / `pass_mouse`: a bool or a list of arbitrary combo strings.
fn pass_list_value() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("true".to_string()),
        Just("false".to_string()),
        prop::collection::vec(hostile_text(), 0..3).prop_map(|xs| {
            let items: Vec<String> = xs.iter().map(|s| quote(s)).collect();
            format!("[{}]", items.join(", "))
        }),
    ]
}

/// `[[outputs]]` `position`: the one slot typed as a raw `toml::Value`, so wrong
/// arity, wrong element type, and nested tables all reach `parse_output_position`.
fn position_value() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("\"auto\"".to_string()),
        Just("\"nonsense\"".to_string()),
        prop::collection::vec(any::<i64>(), 0..5).prop_map(|xs| {
            let items: Vec<String> = xs.iter().map(|n| n.to_string()).collect();
            format!("[{}]", items.join(", "))
        }),
        Just("[\"x\", \"y\"]".to_string()),
        Just("{ x = 1, y = 2 }".to_string()),
    ]
}

fn keybindings_block() -> impl Strategy<Value = String> {
    prop::collection::vec((hostile_text(), hostile_text()), 0..4).prop_map(|pairs| {
        let body: String = pairs
            .iter()
            .map(|(combo, action)| format!("{} = {}\n", quote(combo), quote(action)))
            .collect();
        format!("[keybindings]\n{body}")
    })
}

fn window_rule_block() -> impl Strategy<Value = String> {
    (
        pattern_value(),
        prop::option::of(hostile_text()),
        prop::option::of(hostile_text()),
        prop::option::of(pass_list_value()),
        prop::option::of(pass_list_value()),
    )
        .prop_map(
            |(app_id, border_color, decoration, pass_keys, pass_mouse)| {
                let mut s = format!("[[window_rules]]\napp_id = {}\n", quote(&app_id));
                if let Some(c) = border_color {
                    s.push_str(&format!("border_color = {}\n", quote(&c)));
                }
                if let Some(d) = decoration {
                    s.push_str(&format!("decoration = {}\n", quote(&d)));
                }
                if let Some(p) = pass_keys {
                    s.push_str(&format!("pass_keys = {p}\n"));
                }
                if let Some(p) = pass_mouse {
                    s.push_str(&format!("pass_mouse = {p}\n"));
                }
                s
            },
        )
}

fn output_block() -> impl Strategy<Value = String> {
    (
        prop::option::of(position_value()),
        prop::option::of(hostile_text()),
        prop::option::of(hostile_text()),
        prop::option::of(any::<f64>().prop_filter("finite", |f| f.is_finite())),
    )
        .prop_map(|(position, mode, transform, scale)| {
            let mut s = String::from("[[outputs]]\nname = \"OUT\"\n");
            if let Some(p) = position {
                s.push_str(&format!("position = {p}\n"));
            }
            if let Some(m) = mode {
                s.push_str(&format!("mode = {}\n", quote(&m)));
            }
            if let Some(t) = transform {
                s.push_str(&format!("transform = {}\n", quote(&t)));
            }
            if let Some(sc) = scale {
                s.push_str(&format!("scale = {sc:?}\n"));
            }
            s
        })
}

proptest! {
    #[test]
    fn hostile_values_in_valid_slots_never_panic(
        kb in keybindings_block(),
        wr in window_rule_block(),
        out in output_block(),
    ) {
        let _ = Config::from_toml_collect(&format!("{kb}{wr}{out}"));
    }
}
