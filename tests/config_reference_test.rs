//! Guards `config.reference.toml` against drifting from the compiled-in
//! defaults — it doubles as documentation, so a stale documented default would
//! mislead every user who reads it.
//!
//! Reconstructing the file per its grammar (see
//! `dev/docs/reference-config-format.md`) must yield TOML that parses to exactly
//! `Config::from_toml("")`.

use driftwm::config::Config;
use std::collections::BTreeMap;

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
