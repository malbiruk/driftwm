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
