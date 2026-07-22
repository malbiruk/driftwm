//! Guards `config.reference.toml` against drifting from the compiled-in
//! defaults — it doubles as documentation, so a stale documented default would
//! mislead every user who reads it.
//!
//! Reconstructing the file per its grammar (see
//! `dev/docs/reference-config-format.md`) must yield TOML that parses to exactly
//! `Config::from_toml("")`.

use driftwm::config::{ACTION_NAMES, Config};
use std::collections::{BTreeMap, BTreeSet};

const REFERENCE: &str = include_str!("../config.reference.toml");

/// Rebuild a plain TOML config from the reference by uncommenting every
/// documented default and keeping the uncommented `[section]` headers.
fn reconstruct(reference: &str) -> String {
    let mut out = String::new();
    for line in reference.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            // `# #` introduces prose / an example body — never active config.
            if rest.starts_with('#') {
                continue;
            }
            out.push_str(rest);
            out.push('\n');
        } else if line.starts_with('[') {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Diff two configs' pretty-Debug output as a line multiset, so HashMap
/// (binding map) ordering doesn't produce spurious differences.
fn debug_diff(reference: &Config, code: &Config) -> String {
    fn line_counts(c: &Config) -> BTreeMap<String, i32> {
        let mut counts = BTreeMap::new();
        for line in format!("{c:#?}").lines() {
            *counts.entry(line.trim().to_string()).or_insert(0) += 1;
        }
        counts
    }
    let (ref_counts, code_counts) = (line_counts(reference), line_counts(code));
    let mut diff = String::new();
    for (line, n) in &ref_counts {
        for _ in 0..(n - code_counts.get(line).copied().unwrap_or(0)) {
            diff.push_str(&format!("  reference-only: {line}\n"));
        }
    }
    for (line, n) in &code_counts {
        for _ in 0..(n - ref_counts.get(line).copied().unwrap_or(0)) {
            diff.push_str(&format!("  code-only:      {line}\n"));
        }
    }
    diff
}

/// `deny_unknown_fields` catches a documented field the code dropped; a warning
/// catches a documented default that violates a clamp or is deprecated.
#[test]
fn reference_reconstruction_parses_without_warnings() {
    let reconstructed = reconstruct(REFERENCE);
    let (_, warnings) = Config::from_toml_collect(&reconstructed).unwrap_or_else(|e| {
        panic!("reconstructed config.reference.toml failed to parse: {e}\n\n{reconstructed}")
    });
    assert!(
        warnings.is_empty(),
        "config.reference.toml documents defaults that warn on parse \
         (out-of-range or deprecated):\n{warnings:#?}"
    );
}

#[test]
fn reference_defaults_match_code_defaults() {
    let reconstructed = reconstruct(REFERENCE);
    let from_reference =
        Config::from_toml(&reconstructed).expect("reconstructed config.reference.toml must parse");
    let from_code = Config::from_toml("").expect("empty config must parse");
    assert!(
        from_reference == from_code,
        "config.reference.toml documents defaults that differ from the code defaults:\n{}",
        debug_diff(&from_reference, &from_code)
    );
}

/// Drop the one-per-load `go-to` deprecation notice: it's the expected warning
/// for a binding of the deprecated-but-still-valid `go-to` action, so tests that
/// exercise every action sample must ignore it while catching all others.
fn without_go_to_deprecation(warnings: &[String]) -> Vec<&String> {
    warnings
        .iter()
        .filter(|w| !w.contains("go-to is deprecated"))
        .collect()
}

/// True for a `"combo" = "action"` line, distinguishing real bindings from
/// prose that merely opens with a quoted word.
fn is_binding_line(body: &str) -> bool {
    let Some(rest) = body.strip_prefix('"') else {
        return false;
    };
    let Some(close) = rest.find('"') else {
        return false;
    };
    rest[close + 1..].trim_start().starts_with("= \"")
}

/// Every documented binding — active default or `# #` example — must parse
/// without warnings, so a renamed or removed action lingering in an example
/// surfaces here (a bad action is collected as a warning, not a hard error).
#[test]
fn reference_documented_bindings_parse() {
    let mut by_section: BTreeMap<&str, String> = BTreeMap::new();
    let mut section: Option<&str> = None;
    for line in REFERENCE.lines() {
        if line.starts_with('[') {
            section = Some(line);
            continue;
        }
        // Filters out prose that opens with a quoted word (`"wallpaper", "none"...`),
        // which lacks the `= "` of a real binding's quoted LHS.
        let body = line.trim_start_matches(['#', ' ']);
        if is_binding_line(body)
            && let Some(sec) = section
        {
            let buf = by_section.entry(sec).or_default();
            buf.push_str(body);
            buf.push('\n');
        }
    }

    for (sec, body) in &by_section {
        let toml = format!("{sec}\n{body}");
        let (_, warnings) = Config::from_toml_collect(&toml).unwrap_or_else(|e| {
            panic!("documented bindings under {sec} failed to parse: {e}\n\n{toml}")
        });
        assert!(
            warnings.is_empty(),
            "documented bindings under {sec} produced warnings:\n{warnings:#?}\n\n{toml}"
        );
    }
}

/// The TOML body of each `# # Example[: label]` block: `# #`-prefixed lines
/// running until the next marker, a real blank line, an active default, or a
/// section header.
fn example_blocks(reference: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in reference.lines() {
        let is_comment = line.starts_with("# #");
        let is_marker = is_comment && line.trim_start_matches(['#', ' ']).starts_with("Example");
        if is_marker {
            blocks.extend(current.take());
            current = Some(String::new());
        } else if is_comment {
            if let Some(b) = current.as_mut() {
                let toml = line.strip_prefix("# #").unwrap();
                b.push_str(toml.strip_prefix(' ').unwrap_or(toml));
                b.push('\n');
            }
        } else {
            blocks.extend(current.take());
        }
    }
    blocks.extend(current.take());
    blocks
}

/// Every `# # Example:` block that is a complete config fragment (declares
/// `[[window_rules]]` or `[[outputs]]`) must parse without warnings, so the
/// gnarliest snippets (globs, regex, pass_keys, output modes) can't silently
/// drift into invalid config.
#[test]
fn reference_example_blocks_parse() {
    for block in example_blocks(REFERENCE) {
        if !block.contains("[[window_rules]]") && !block.contains("[[outputs]]") {
            continue;
        }
        let (_, warnings) = Config::from_toml_collect(&block)
            .unwrap_or_else(|e| panic!("example block failed to parse: {e}\n\n{block}"));
        assert!(
            warnings.is_empty(),
            "example block produced warnings:\n{warnings:#?}\n\n{block}"
        );
    }
}

/// The reference text from a `# ## <heading>` doc heading up to the next
/// same-level heading (or EOF). The heading is matched only at the start of a
/// line, so a heading string can't accidentally hit a substring inside prose.
fn reference_section(heading: &str) -> &'static str {
    let start = REFERENCE
        .match_indices(heading)
        .map(|(i, _)| i)
        .find(|&i| i == 0 || REFERENCE.as_bytes()[i - 1] == b'\n')
        .unwrap_or_else(|| panic!("config.reference.toml is missing heading {heading:?}"));
    let body = &REFERENCE[start..];
    let end = body[heading.len()..]
        .find("\n# ## ")
        .map_or(body.len(), |i| heading.len() + i);
    &body[..end]
}

/// The `# #` comment lines (prefix stripped) directly under each `# # <marker>`
/// line in `section`, one inner `Vec` per marker occurrence. A block runs from
/// its marker to the next blank comment (`# #`), nested `#`, or non-comment
/// line — the shared walk behind `documented_fields` and `documented_actions`.
fn marker_blocks<'a>(section: &'a str, marker: &str) -> Vec<Vec<&'a str>> {
    let lines: Vec<&str> = section.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let is_marker = lines[i]
            .strip_prefix("# #")
            .is_some_and(|rest| rest.trim() == marker);
        if !is_marker {
            i += 1;
            continue;
        }
        i += 1;
        let mut block: Vec<&str> = Vec::new();
        while i < lines.len() {
            let Some(body) = lines[i].strip_prefix("# #") else {
                break;
            };
            if body.trim().is_empty() || body.starts_with('#') {
                break;
            }
            block.push(body);
            i += 1;
        }
        blocks.push(block);
    }
    blocks
}

/// The field names documented under every `# # Supported fields:` block in
/// `section`. At the block's shallowest indent, a line reading `name — …`
/// contributes `name`; deeper continuation lines and sub-bullets are skipped.
/// Multiple markers in a section are unioned.
fn documented_fields(section: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for block in marker_blocks(section, "Supported fields:") {
        let base = block
            .iter()
            .map(|b| b.len() - b.trim_start().len())
            .min()
            .unwrap_or(0);
        for body in block {
            if body.len() - body.trim_start().len() != base {
                continue;
            }
            if let Some((name, _)) = body.trim_start().split_once('\u{2014}') {
                let name = name.trim();
                if !name.is_empty() && !name.contains(char::is_whitespace) {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names
}

/// The field names serde lists after "expected one of" in a `deny_unknown_fields`
/// rejection — the backtick-quoted tokens.
fn expected_fields(err: &str) -> Vec<String> {
    let (_, tail) = err
        .split_once("expected one of")
        .unwrap_or_else(|| panic!("not a deny_unknown_fields error:\n{err}"));
    tail.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// Assert the fields documented under `Supported fields:` and the fields serde
/// accepts are the same set, both ways — the only check keeping the two in
/// lockstep. A field added in code but missing an entry fails the forward
/// direction; an entry for a field that no longer exists fails the reverse.
fn assert_fields_match(bogus_toml: &str, heading: &str) {
    let err = Config::from_toml_collect(bogus_toml)
        .expect_err("an unknown field must be rejected by deny_unknown_fields")
        .to_string();
    let code: BTreeSet<String> = expected_fields(&err).into_iter().collect();
    assert!(!code.is_empty(), "no field names parsed from:\n{err}");
    let documented = documented_fields(reference_section(heading));

    let missing: Vec<&str> = code.difference(&documented).map(String::as_str).collect();
    assert!(
        missing.is_empty(),
        "fields exist in code but have no entry under `Supported fields:` in the \
         {heading:?} section of config.reference.toml: {missing:?}"
    );
    let extra: Vec<&str> = documented.difference(&code).map(String::as_str).collect();
    assert!(
        extra.is_empty(),
        "entries are documented under `Supported fields:` in the {heading:?} section \
         of config.reference.toml but do not exist in code: {extra:?}"
    );
}

#[test]
fn window_rule_fields_are_all_documented() {
    assert_fields_match(
        "[[window_rules]]\napp_id = \"x\"\nbogus_field_zz = 1\n",
        "# ## Window rules",
    );
}

#[test]
fn output_fields_are_all_documented() {
    assert_fields_match(
        "[[outputs]]\nname = \"eDP-1\"\nbogus_field_zz = 1\n",
        "# ## Outputs",
    );
}

/// The action names documented under the unique `# # Actions:` block in the
/// reference (same block walk as `marker_blocks`). At the block's shallowest
/// indent, the pre-em-dash text's first whitespace-separated token is the
/// action name; rows sharing a name collapse via the set.
fn documented_actions() -> BTreeSet<String> {
    let blocks = marker_blocks(REFERENCE, "Actions:");
    assert_eq!(
        blocks.len(),
        1,
        "expected exactly one `# # Actions:` marker in config.reference.toml, found {}",
        blocks.len()
    );
    let block = blocks.into_iter().next().unwrap();
    let base = block
        .iter()
        .map(|b| b.len() - b.trim_start().len())
        .min()
        .unwrap_or(0);

    let mut names = BTreeSet::new();
    for body in block {
        if body.len() - body.trim_start().len() != base {
            continue;
        }
        if let Some((pre, _)) = body.trim_start().split_once('\u{2014}')
            && let Some(name) = pre.split_whitespace().next()
        {
            names.insert(name.to_string());
        }
    }
    names
}

/// Keeps `ACTION_NAMES` and the documented `Actions:` catalog in lockstep: every
/// listed action must be a real, parseable binding, and the two name sets must
/// match exactly.
#[test]
fn keybinding_actions_are_all_documented() {
    // Sample-parse: each canonical sample must parse cleanly, proving every
    // const entry names a real action (a bad action surfaces as a warning).
    let mut toml = String::from("[keybindings]\n");
    for (i, (_, sample)) in ACTION_NAMES.iter().enumerate() {
        toml.push_str(&format!("\"ctrl+F{}\" = \"{sample}\"\n", i + 1));
    }
    let (_, warnings) = Config::from_toml_collect(&toml)
        .unwrap_or_else(|e| panic!("sample action bindings failed to parse: {e}\n\n{toml}"));
    // `go-to` is deprecated but still a documented, parseable action, so binding
    // its sample warns once; every other sample must parse clean.
    let warnings = without_go_to_deprecation(&warnings);
    assert!(
        warnings.is_empty(),
        "sample action bindings produced warnings:\n{warnings:#?}\n\n{toml}"
    );

    // `none` is the unbind sentinel handled outside `parse_action`, but it is a
    // documented, bindable value, so it belongs in the catalog.
    let code: BTreeSet<String> = ACTION_NAMES
        .iter()
        .map(|(n, _)| n.to_string())
        .chain(std::iter::once("none".to_string()))
        .collect();
    let documented = documented_actions();

    let missing: Vec<&str> = code.difference(&documented).map(String::as_str).collect();
    assert!(
        missing.is_empty(),
        "actions exist in code but have no entry under `Actions:` in \
         config.reference.toml: {missing:?}"
    );
    let extra: Vec<&str> = documented.difference(&code).map(String::as_str).collect();
    assert!(
        extra.is_empty(),
        "entries are documented under `Actions:` in config.reference.toml but \
         do not exist in code: {extra:?}"
    );
}

/// The docs claim threshold gestures accept "any action from the [keybindings]
/// Actions list" with no exceptions — this keeps that literally true: every
/// action parses clean on the threshold triggers. Bare `center-nearest` is the
/// one direction-from-swipe special case: valid on a swipe, rejected on triggers
/// that carry no direction.
#[test]
fn threshold_gestures_accept_every_action() {
    for (name, sample) in ACTION_NAMES {
        // Covers the base-swipe threshold branch, the per-direction/pinch-in/hold
        // triggers, and the touch tap arm.
        for (section, trigger) in [
            ("gestures.anywhere", "4-finger-swipe-up"),
            ("gestures.anywhere", "4-finger-pinch-in"),
            ("gestures.anywhere", "4-finger-hold"),
            ("gestures.anywhere", "4-finger-swipe"),
            ("touch.anywhere", "3-finger-tap"),
        ] {
            let toml = format!("[{section}]\n\"{trigger}\" = \"{sample}\"\n");
            let (_, warnings) = Config::from_toml_collect(&toml).unwrap_or_else(|e| {
                panic!("{name} on {trigger} in [{section}] failed to parse: {e}")
            });
            // `go-to` is deprecated but still a valid threshold action; its
            // sample warns. Every other action must parse clean.
            let warnings = without_go_to_deprecation(&warnings);
            assert!(
                warnings.is_empty(),
                "`{sample}` should be a valid threshold action on {trigger} [{section}]: {warnings:?}"
            );
        }
    }

    let (_, warnings) =
        Config::from_toml_collect("[gestures.anywhere]\n\"4-finger-swipe\" = \"center-nearest\"\n")
            .expect("must parse");
    assert!(
        warnings.is_empty(),
        "bare center-nearest on a swipe should parse clean: {warnings:?}"
    );
    for trigger in ["4-finger-pinch-in", "4-finger-hold"] {
        let toml = format!("[gestures.anywhere]\n\"{trigger}\" = \"center-nearest\"\n");
        let (_, warnings) = Config::from_toml_collect(&toml).expect("must parse");
        assert!(
            !warnings.is_empty(),
            "bare center-nearest should be rejected on {trigger} (no direction)"
        );
    }
}

/// Touch and trackpad share the same swipe/pinch/grab rules (touch only adds its
/// own tap / doubletap arms), so pin their agreement: across every shared trigger
/// kind, a candidate is accepted by both parsers or rejected by both — no
/// exceptions.
#[test]
fn gesture_and_touch_rules_match() {
    const CONTINUOUS: &[&str] = &[
        "pan-viewport",
        "zoom",
        "move-window",
        "move-snapped-windows",
        "resize-window",
        "resize-window-snapped",
    ];
    const KINDS: &[&str] = &["swipe", "swipe-up", "pinch", "pinch-in", "doubletap-swipe"];

    let candidates: Vec<String> = ACTION_NAMES
        .iter()
        .map(|(_, sample)| sample.to_string())
        .chain(CONTINUOUS.iter().map(|s| s.to_string()))
        .chain(std::iter::once("center-nearest".to_string()))
        .collect();

    let accepted = |section: &str, trigger: &str, action: &str| -> bool {
        let toml = format!("[{section}]\n\"{trigger}\" = \"{action}\"\n");
        let (_, warnings) = Config::from_toml_collect(&toml).unwrap_or_else(|e| {
            panic!("{action} on {trigger} in [{section}] failed to parse: {e}")
        });
        warnings.is_empty()
    };

    for candidate in &candidates {
        for kind in KINDS {
            let trigger = format!("3-finger-{kind}");
            let g = accepted("gestures.anywhere", &trigger, candidate);
            let t = accepted("touch.anywhere", &trigger, candidate);
            assert_eq!(
                g, t,
                "`{candidate}` on 3-finger-{kind}: gestures={g} touch={t} must agree"
            );
        }
    }
}

/// The body of every ```` ```toml ```` fence in a markdown document.
fn toml_fences(md: &str) -> Vec<String> {
    let mut fences = Vec::new();
    let mut current: Option<String> = None;
    for line in md.lines() {
        match &mut current {
            Some(buf) if line.trim_start().starts_with("```") => {
                fences.push(std::mem::take(buf));
                current = None;
            }
            Some(buf) => {
                buf.push_str(line);
                buf.push('\n');
            }
            None if line.trim() == "```toml" => current = Some(String::new()),
            None => {}
        }
    }
    fences
}

/// Every window-rule recipe in `docs/window-rules.md` must be valid, warning-free
/// config, so a hand-written recipe can't drift into broken TOML unnoticed.
#[test]
fn window_rules_doc_snippets_parse() {
    const DOC: &str = include_str!("../docs/window-rules.md");
    let mut checked = 0;
    for fence in toml_fences(DOC) {
        if !fence.contains("[[window_rules]]") {
            continue;
        }
        let (_, warnings) = Config::from_toml_collect(&fence)
            .unwrap_or_else(|e| panic!("window-rules.md snippet failed to parse: {e}\n\n{fence}"));
        assert!(
            warnings.is_empty(),
            "window-rules.md snippet produced warnings:\n{warnings:#?}\n\n{fence}"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "found no [[window_rules]] snippets in docs/window-rules.md"
    );
}
