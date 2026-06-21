//! Generates `docs/config.md` from `config.reference.toml` and asserts the
//! committed page is in sync. The reference is the single source of truth (see
//! `dev/docs/reference-config-format.md`); this renders its grammar into a
//! definition-list docs page.
//!
//! Regenerate after editing the reference:
//!
//! ```sh
//! UPDATE_CONFIG_DOCS=1 cargo test docs_config_md_is_up_to_date
//! ```

const REFERENCE: &str = include_str!("../config.reference.toml");
const DOCS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/config.md");

const INTRO: &str = "\
# Configuration

<!-- Generated from config.reference.toml — do not edit by hand.
     Regenerate with: UPDATE_CONFIG_DOCS=1 cargo test docs_config_md_is_up_to_date -->

driftwm reads its configuration from `~/.config/driftwm/config.toml` (respecting
`XDG_CONFIG_HOME`). Every field is optional — anything you omit uses the built-in
default shown below. Copy [`config.reference.toml`](../config.reference.toml) to
get started, then uncomment and edit only the lines you want to change. Validate a
config with `driftwm --check-config`.
";

#[test]
fn docs_config_md_is_up_to_date() {
    let rendered = render(REFERENCE);
    if std::env::var_os("UPDATE_CONFIG_DOCS").is_some() {
        std::fs::write(DOCS_PATH, &rendered).expect("write docs/config.md");
        return;
    }
    let current = std::fs::read_to_string(DOCS_PATH).unwrap_or_default();
    assert!(
        rendered == current,
        "docs/config.md is out of date with config.reference.toml.\n\
         Regenerate with:\n    UPDATE_CONFIG_DOCS=1 cargo test docs_config_md_is_up_to_date"
    );
}

/// One unit of accumulated section prose.
enum Prose {
    /// Flowing text — wraps into a paragraph with its neighbours.
    Text(String),
    /// Indented (preformatted) text — rendered verbatim in a code fence.
    Pre(String),
    /// A `# #` soft separator — a paragraph break inside a prose block.
    Break,
}

#[derive(Default)]
struct ProseBuf(Vec<Prose>);

impl ProseBuf {
    fn push_text(&mut self, s: &str) {
        self.0.push(Prose::Text(s.to_string()));
    }
    fn push_pre(&mut self, s: &str) {
        self.0.push(Prose::Pre(s.to_string()));
    }
    fn push_break(&mut self) {
        self.0.push(Prose::Break);
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Render the buffered prose and clear it.
    fn take(&mut self) -> String {
        let out = render_prose(&self.0);
        self.0.clear();
        out
    }
}

fn render_prose(items: &[Prose]) -> String {
    let mut out = String::new();
    let mut para: Vec<&str> = Vec::new();
    let mut pre: Vec<&str> = Vec::new();
    for item in items {
        match item {
            Prose::Text(s) => {
                flush_pre(&mut out, &mut pre);
                para.push(s);
            }
            Prose::Pre(s) => {
                flush_para(&mut out, &mut para);
                pre.push(s);
            }
            Prose::Break => {
                flush_para(&mut out, &mut para);
                flush_pre(&mut out, &mut pre);
            }
        }
    }
    flush_para(&mut out, &mut para);
    flush_pre(&mut out, &mut pre);
    out
}

fn flush_para(out: &mut String, para: &mut Vec<&str>) {
    if !para.is_empty() {
        out.push_str(&para.join(" "));
        out.push_str("\n\n");
        para.clear();
    }
}

fn flush_pre(out: &mut String, pre: &mut Vec<&str>) {
    if !pre.is_empty() {
        out.push_str(&render_pre_block(pre));
        pre.clear();
    }
}

/// An indented block in the reference is either actual code (commands, GLSL) or
/// a hand-aligned `name — description` list. Code stays monospace in a fence;
/// the lists become markdown bullets. The em-dash separator tells them apart.
fn render_pre_block(lines: &[&str]) -> String {
    let is_list = lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.contains(" — "));
    if is_list {
        render_definition_list(lines)
    } else {
        render_code_block(lines)
    }
}

fn render_code_block(lines: &[&str]) -> String {
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(leading_spaces)
        .min()
        .unwrap_or(0);
    let mut out = String::from("```text\n");
    for l in lines {
        out.push_str(l.get(indent..).unwrap_or(""));
        out.push('\n');
    }
    out.push_str("```\n\n");
    out
}

fn render_definition_list(lines: &[&str]) -> String {
    let base = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(leading_spaces)
        .min()
        .unwrap_or(0);
    // (name, description, sub-items). A `name — desc` line at the base indent
    // starts an item; a deeper `— ` line is a sub-bullet; anything else wraps
    // the line above it.
    let mut items: Vec<(String, String, Vec<String>)> = Vec::new();
    for l in lines {
        let text = l.trim();
        if text.is_empty() {
            continue;
        }
        if leading_spaces(l) <= base && text.contains(" — ") {
            let (name, desc) = text.split_once(" — ").unwrap();
            items.push((name.trim().to_string(), desc.trim().to_string(), Vec::new()));
        } else if text.contains(" — ") {
            match items.last_mut() {
                Some(item) => item.2.push(text.to_string()),
                None => items.push((String::new(), text.to_string(), Vec::new())),
            }
        } else {
            match items.last_mut() {
                Some(item) if !item.2.is_empty() => {
                    append_wrapped(item.2.last_mut().unwrap(), text)
                }
                Some(item) => append_wrapped(&mut item.1, text),
                None => items.push((String::new(), text.to_string(), Vec::new())),
            }
        }
    }
    let mut out = String::new();
    for (name, desc, subs) in &items {
        if name.is_empty() {
            out.push_str(&format!("- {desc}\n"));
        } else if name.contains('`') {
            out.push_str(&format!("- {name} — {desc}\n"));
        } else {
            out.push_str(&format!("- `{name}` — {desc}\n"));
        }
        for sub in subs {
            out.push_str(&format!("  - {sub}\n"));
        }
    }
    out.push('\n');
    out
}

fn append_wrapped(target: &mut String, text: &str) {
    if !target.is_empty() {
        target.push(' ');
    }
    target.push_str(text);
}

fn leading_spaces(l: &&str) -> usize {
    l.len() - l.trim_start().len()
}

fn render(reference: &str) -> String {
    let mut out = String::from(INTRO);
    let lines: Vec<&str> = reference.lines().collect();

    // Skip the file's preamble (TOML-template grammar notes) — the docs page has
    // its own intro. The preamble is `# #` prose; it ends at the first heading,
    // section, or default.
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if l.starts_with('[') {
            break;
        }
        if let Some(rest) = l.strip_prefix("# ")
            && (rest.starts_with("##") || !rest.starts_with('#'))
        {
            break;
        }
        i += 1;
    }

    let mut prose = ProseBuf::default();
    let mut bindings: Vec<[String; 3]> = Vec::new();

    while i < lines.len() {
        let line = lines[i];

        // Continuation lines are consumed by the default handler below; a stray
        // one here is harmless.
        if is_continuation(line) {
            i += 1;
            continue;
        }

        if line.trim().is_empty() {
            // A blank line leaves the comment block: pending prose is standalone.
            flush_prose(&mut out, &mut prose);
            i += 1;
            continue;
        }

        if line.starts_with('[') {
            flush_bindings(&mut out, &mut bindings);
            flush_prose(&mut out, &mut prose);
            out.push_str(&format!("\n## `{line}`\n\n"));
            i += 1;
            continue;
        }

        let Some(rest) = line.strip_prefix("# ") else {
            i += 1;
            continue;
        };

        if !rest.starts_with('#') {
            // `# key = value  # gloss` — a default, plus any continuation lines.
            let (decl, gloss) = gloss_split(rest);
            let (key, value) = split_kv(decl);
            let mut desc = gloss.to_string();
            while i + 1 < lines.len() && is_continuation(lines[i + 1]) {
                let cont = continuation_text(lines[i + 1]);
                if !desc.is_empty() {
                    desc.push(' ');
                }
                desc.push_str(&cont);
                i += 1;
            }
            if is_quoted(key) {
                // A binding row. Prose before a binding block is a lead-in.
                flush_prose(&mut out, &mut prose);
                bindings.push([key.to_string(), strip_quotes(value).to_string(), desc]);
            } else {
                // A scalar setting. Prose directly abutting it is its lead-in
                // description (a blank line would have flushed it already).
                flush_bindings(&mut out, &mut bindings);
                let lead = prose.take();
                render_setting(&mut out, key, value, &lead, &desc);
            }
            i += 1;
            continue;
        }

        // `# #` prose, `# ## heading`, or `# # Example:`.
        let hashes = rest.chars().take_while(|c| *c == '#').count();
        if hashes >= 2 {
            flush_bindings(&mut out, &mut bindings);
            flush_prose(&mut out, &mut prose);
            let title = rest[hashes..].trim();
            out.push_str(&format!("\n{} {}\n\n", "#".repeat(hashes), title));
            i += 1;
            continue;
        }

        let content_raw = &rest[1..];
        let content = content_raw.strip_prefix(' ').unwrap_or(content_raw);

        if let Some(label) = content.strip_prefix("Example:") {
            flush_bindings(&mut out, &mut bindings);
            flush_prose(&mut out, &mut prose);
            let label = label.trim().to_string();
            let mut body: Vec<String> = Vec::new();
            i += 1;
            while i < lines.len() {
                let bl = lines[i];
                let Some(brest) = bl.strip_prefix("# ") else {
                    break;
                };
                if !brest.starts_with('#') {
                    break;
                }
                let bcontent_raw = &brest[1..];
                let bcontent = bcontent_raw.strip_prefix(' ').unwrap_or(bcontent_raw);
                if bcontent.starts_with("Example:") {
                    break;
                }
                body.push(bcontent.to_string());
                i += 1;
            }
            while body.first().is_some_and(String::is_empty) {
                body.remove(0);
            }
            while body.last().is_some_and(String::is_empty) {
                body.pop();
            }
            render_example(&mut out, &label, &body);
            continue;
        }

        if content.is_empty() {
            prose.push_break();
        } else if content.starts_with(' ') {
            prose.push_pre(content);
        } else {
            prose.push_text(content);
        }
        i += 1;
    }

    flush_bindings(&mut out, &mut bindings);
    flush_prose(&mut out, &mut prose);

    // A heading prepends a blank line and the block before it already ended with
    // one, so section breaks accumulate a stray blank. Collapse them.
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    let trimmed = out.trim_end();
    format!("{trimmed}\n")
}

fn flush_prose(out: &mut String, prose: &mut ProseBuf) {
    if !prose.is_empty() {
        out.push_str(&prose.take());
    }
}

fn flush_bindings(out: &mut String, bindings: &mut Vec<[String; 3]>) {
    if bindings.is_empty() {
        return;
    }
    out.push_str("| Binding | Action | Notes |\n| --- | --- | --- |\n");
    for [binding, action, notes] in bindings.iter() {
        out.push_str(&format!(
            "| `{}` | `{}` | {} |\n",
            esc_cell(binding),
            esc_cell(action),
            esc_cell(notes),
        ));
    }
    out.push('\n');
    bindings.clear();
}

fn render_setting(out: &mut String, key: &str, value: &str, lead: &str, desc: &str) {
    out.push_str(&format!("### `{key}`\n\nDefault: `{value}`\n\n"));
    if !lead.trim().is_empty() {
        out.push_str(lead);
    }
    if !desc.is_empty() {
        out.push_str(desc);
        out.push_str("\n\n");
    }
}

fn render_example(out: &mut String, label: &str, body: &[String]) {
    if label.is_empty() {
        out.push_str("**Example:**\n\n");
    } else {
        out.push_str(&format!("**Example: {label}**\n\n"));
    }
    out.push_str("```toml\n");
    out.push_str(&body.join("\n"));
    out.push_str("\n```\n\n");
}

/// Split a `key = value  # gloss` declaration at the comment `#`, ignoring any
/// `#` inside a double-quoted string (e.g. a `"#303030"` hex colour).
fn gloss_split(decl: &str) -> (&str, &str) {
    let mut in_str = false;
    let mut escaped = false;
    for (idx, c) in decl.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_str => escaped = true,
            '"' => in_str = !in_str,
            '#' if !in_str => return (decl[..idx].trim_end(), decl[idx + 1..].trim()),
            _ => {}
        }
    }
    (decl.trim_end(), "")
}

fn split_kv(decl: &str) -> (&str, &str) {
    match decl.split_once('=') {
        Some((k, v)) => (k.trim(), v.trim()),
        None => (decl.trim(), ""),
    }
}

fn is_quoted(s: &str) -> bool {
    s.starts_with('"')
}

fn strip_quotes(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

fn is_continuation(line: &str) -> bool {
    line.starts_with(char::is_whitespace) && line.trim_start().starts_with('#')
}

fn continuation_text(line: &str) -> String {
    line.trim_start()
        .strip_prefix('#')
        .unwrap_or("")
        .trim()
        .to_string()
}

fn esc_cell(s: &str) -> String {
    s.replace('|', "\\|")
}
