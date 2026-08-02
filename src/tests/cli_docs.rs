//! Generates `docs/cli.md` from the clap command tree and asserts the committed
//! page is in sync — the CLI analogue of `tests/config_docs_test.rs`. The
//! `driftwm` root command and its `msg` subcommands are the single source of
//! truth; this walks `Cli::command()` and renders deterministic markdown.
//!
//! Lives bin-side because `Cli` and `Msg` are private to the binary crate and a
//! `tests/` integration test only sees the library.
//!
//! Regenerate after changing the CLI:
//!
//! ```sh
//! UPDATE_CLI_DOCS=1 cargo test docs_cli_md_is_up_to_date
//! ```

use clap::{Arg, Command, CommandFactory};

const DOCS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/cli.md");

const INTRO: &str = "\
# CLI reference

<!-- Generated from the clap command tree — do not edit by hand.
     Regenerate with: UPDATE_CLI_DOCS=1 cargo test docs_cli_md_is_up_to_date -->

driftwm's command-line interface: the root command that starts the compositor,
and every `driftwm msg` subcommand for controlling a running one. For the raw
JSON wire protocol behind `msg`, see [ipc.md](ipc.md).

";

/// One runnable invocation per command section, keyed by its full path. They
/// live here rather than in the doc comments so `--help` stays terminal-shaped.
const EXAMPLES: &[(&str, &[&str])] = &[
    ("driftwm", &["driftwm --backend winit --config ~/dev.toml"]),
    (
        "driftwm msg",
        &["driftwm msg state", "driftwm msg --json focus --id 5"],
    ),
    (
        "driftwm msg state",
        &["driftwm msg --json state | jq '.Ok.State.windows'"],
    ),
    (
        "driftwm msg subscribe",
        &["driftwm msg --json subscribe | jq --unbuffered -r '.State.zoom'"],
    ),
    (
        "driftwm msg focus",
        &["driftwm msg focus firefox", "driftwm msg focus --id 5"],
    ),
    (
        "driftwm msg move",
        &["driftwm msg move", "driftwm msg move -400 200 --id 5"],
    ),
    (
        "driftwm msg resize",
        &["driftwm msg resize", "driftwm msg resize 800 600 --id 5"],
    ),
    ("driftwm msg close", &["driftwm msg close firefox"]),
    ("driftwm msg opacity", &["driftwm msg opacity 0.85 --id 5"]),
    ("driftwm msg suspend", &["driftwm msg suspend firefox"]),
    ("driftwm msg relaunch", &["driftwm msg relaunch firefox"]),
    (
        "driftwm msg camera",
        &["driftwm msg camera", "driftwm msg camera 500 300"],
    ),
    ("driftwm msg zoom", &["driftwm msg zoom 0.5"]),
    (
        "driftwm msg bookmark",
        &[
            "driftwm msg bookmark",
            "driftwm msg bookmark inbox 500 300",
            "driftwm msg bookmark inbox --delete",
        ],
    ),
    ("driftwm msg layout", &["driftwm msg layout --short"]),
    (
        "driftwm msg action",
        &[
            "driftwm msg action switch-layout next",
            "driftwm msg action toggle-fullscreen",
        ],
    ),
    (
        "driftwm msg screenshot",
        &["driftwm msg screenshot --scale 2 -o ~/canvas.png"],
    ),
    (
        "driftwm msg screenshot window",
        &["driftwm msg screenshot window -o - | wl-copy"],
    ),
    (
        "driftwm msg screenshot all",
        &["driftwm msg screenshot all --scale 0.5"],
    ),
    (
        "driftwm msg screenshot region",
        &[
            "driftwm msg screenshot region -1000 -500 2000 1000",
            "driftwm msg screenshot region \"$(slurp)\" --from-screen",
        ],
    ),
    (
        "driftwm msg debug-counters",
        &["driftwm msg debug-counters"],
    ),
];

/// Every example is checked against the real command tree. Renaming a command
/// or adding one already fails at `examples()`, but retyping or renaming a
/// *flag* would otherwise leave a stale invocation rendered into the docs.
#[test]
fn cli_doc_examples_still_parse() {
    for (section, lines) in EXAMPLES {
        for line in *lines {
            // A shell expansion is what clap would really see, not the token
            // standing in for it, so there is nothing to check statically.
            if line.contains("$(") {
                continue;
            }
            // Examples may pipe into other tools; only our side is ours to parse.
            let invocation = line.split('|').next().unwrap_or(line);
            if let Err(err) =
                crate::Cli::command().try_get_matches_from(invocation.split_whitespace())
            {
                panic!("`{line}` under `{section}` no longer parses: {err}");
            }
        }
    }
}

#[test]
fn docs_cli_md_is_up_to_date() {
    let rendered = render();
    if std::env::var_os("UPDATE_CLI_DOCS").is_some() {
        std::fs::write(DOCS_PATH, &rendered).expect("write docs/cli.md");
        return;
    }
    let current = std::fs::read_to_string(DOCS_PATH).unwrap_or_default();
    assert!(
        rendered == current,
        "docs/cli.md is out of date with the clap CLI definition.\n\
         Regenerate with:\n    UPDATE_CLI_DOCS=1 cargo test docs_cli_md_is_up_to_date"
    );
}

fn render() -> String {
    // clap only adds its auto-generated args when the command is built, and
    // `--version` is one of them.
    let mut cmd = crate::Cli::command();
    cmd.build();
    let mut out = String::from(INTRO);
    render_command(&mut out, &cmd, &[], &[]);
    format!("{}\n", out.trim_end())
}

/// Render one command, then recurse into its subcommands. `path` is the chain of
/// ancestor names; `shown_globals` are ids of global args already listed by an
/// ancestor (clap propagates globals to children — list each only once).
fn render_command(out: &mut String, cmd: &Command, path: &[String], shown_globals: &[String]) {
    let full: Vec<String> = path
        .iter()
        .cloned()
        .chain([cmd.get_name().to_string()])
        .collect();
    let title = full.join(" ");
    let level = (full.len() + 1).min(6);
    out.push_str(&format!("{} `{}`\n\n", "#".repeat(level), title));

    out.push_str("```\n");
    out.push_str(&usage_line(cmd, &title, shown_globals));
    out.push_str("\n```\n\n");

    if let Some(about) = cmd.get_long_about().or_else(|| cmd.get_about()) {
        out.push_str(about.to_string().trim_end());
        out.push_str("\n\n");
    }

    let mut child_globals = shown_globals.to_vec();
    let mut opts = String::new();
    for arg in visible_args(cmd) {
        if arg.is_global_set() {
            let id = arg.get_id().as_str().to_string();
            if shown_globals.contains(&id) {
                continue;
            }
            child_globals.push(id);
        }
        // Positionals appear in the usage line; only list them here when they
        // carry help worth repeating.
        if arg.is_positional() && arg.get_help().is_none() {
            continue;
        }
        opts.push_str(&format!("- `{}`", arg_signature(arg)));
        if let Some(help) = arg.get_help() {
            opts.push_str(&format!(" — {}", help.to_string().trim()));
        }
        if let Some(default) = default_value(arg) {
            opts.push_str(&format!(" (default: `{default}`)"));
        }
        opts.push('\n');
    }
    if !opts.is_empty() {
        out.push_str(&opts);
        out.push('\n');
    }

    out.push_str("```bash\n");
    for line in examples(&title) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("```\n\n");

    // Only a long subcommand list needs an index; a handful reads fine inline.
    if subcommands(cmd).count() > 3 {
        out.push_str(&subcommand_table(cmd, &full));
    }

    for sub in subcommands(cmd) {
        render_command(out, sub, &full, &child_globals);
    }
}

/// The example block for one section. Missing entries are a hard error so a new
/// command can't ship without a runnable invocation.
fn examples(title: &str) -> &'static [&'static str] {
    EXAMPLES
        .iter()
        .find(|(name, _)| *name == title)
        .map(|(_, lines)| *lines)
        .unwrap_or_else(|| panic!("add a `{title}` entry to EXAMPLES in src/tests/cli_docs.rs"))
}

/// A verb | summary | link table over `cmd`'s subcommands, for jumping into a
/// long list of sections.
fn subcommand_table(cmd: &Command, path: &[String]) -> String {
    let mut table = String::from("| Command | Description |\n| --- | --- |\n");
    for sub in subcommands(cmd) {
        let name = sub.get_name();
        let anchor = path
            .iter()
            .map(String::as_str)
            .chain([name])
            .collect::<Vec<_>>()
            .join("-")
            .to_lowercase();
        let about = sub
            .get_about()
            .map(|a| a.to_string().replace('\n', " "))
            .unwrap_or_default();
        table.push_str(&format!("| [`{name}`](#{anchor}) | {} |\n", about.trim()));
    }
    table.push('\n');
    table
}

/// A command's arguments, minus clap's auto-generated `--help`.
fn visible_args(cmd: &Command) -> impl Iterator<Item = &Arg> {
    cmd.get_arguments().filter(|a| a.get_id() != "help")
}

/// A command's real subcommands, minus clap's auto-generated `help`.
fn subcommands(cmd: &Command) -> impl Iterator<Item = &Command> {
    cmd.get_subcommands().filter(|s| s.get_name() != "help")
}

/// `driftwm msg move [OPTIONS] [X] [Y]` — the full path, an options marker, each
/// positional's value token, and a subcommand slot. Globals an ancestor already
/// listed don't earn the marker, matching where they're documented.
fn usage_line(cmd: &Command, title: &str, shown_globals: &[String]) -> String {
    let mut s = title.to_string();
    let own_option =
        |a: &Arg| !a.is_positional() && !shown_globals.iter().any(|g| g == a.get_id().as_str());
    if visible_args(cmd).any(own_option) {
        s.push_str(" [OPTIONS]");
    }
    for arg in visible_args(cmd).filter(|a| a.is_positional()) {
        s.push(' ');
        s.push_str(&positional_token(arg));
    }
    if subcommands(cmd).next().is_some() {
        s.push(' ');
        s.push_str(if cmd.is_subcommand_required_set() {
            "<COMMAND>"
        } else {
            "[COMMAND]"
        });
    }
    s
}

fn value_name(arg: &Arg) -> String {
    arg.get_value_names()
        .and_then(|v| v.first())
        .map(|s| s.to_string())
        .unwrap_or_else(|| arg.get_id().as_str().to_uppercase())
}

/// `[X]` / `<SPEC>...` for a positional, honouring required and multi-value.
fn positional_token(arg: &Arg) -> String {
    let name = value_name(arg);
    let repeated = arg.get_num_args().is_some_and(|r| r.max_values() > 1);
    let dots = if repeated { "..." } else { "" };
    if arg.is_required_set() {
        format!("<{name}>{dots}")
    } else {
        format!("[{name}]{dots}")
    }
}

/// `--json`, `-o, --output <OUTPUT>`, or a positional's value token.
fn arg_signature(arg: &Arg) -> String {
    if arg.is_positional() {
        return positional_token(arg);
    }
    let mut s = String::new();
    if let Some(short) = arg.get_short() {
        s.push_str(&format!("-{short}, "));
    }
    if let Some(long) = arg.get_long() {
        s.push_str(&format!("--{long}"));
    }
    if takes_value(arg) {
        s.push_str(&format!(" <{}>", value_name(arg)));
    }
    s
}

fn takes_value(arg: &Arg) -> bool {
    matches!(
        arg.get_action(),
        clap::ArgAction::Set | clap::ArgAction::Append
    )
}

fn default_value(arg: &Arg) -> Option<String> {
    // Building the command gives every flag an implicit `false`; only an arg
    // that takes a value has a default worth printing.
    if !takes_value(arg) {
        return None;
    }
    let defaults = arg.get_default_values();
    (!defaults.is_empty()).then(|| {
        defaults
            .iter()
            .map(|v| v.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(",")
    })
}
