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
    let mut out = String::from(INTRO);
    render_command(&mut out, &crate::Cli::command(), &[], &[]);
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
    out.push_str(&usage_line(cmd, &title));
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

    for sub in subcommands(cmd) {
        render_command(out, sub, &full, &child_globals);
    }
}

/// A command's arguments, minus clap's auto-generated `--help` / `--version`.
fn visible_args(cmd: &Command) -> impl Iterator<Item = &Arg> {
    cmd.get_arguments()
        .filter(|a| a.get_id() != "help" && a.get_id() != "version")
}

/// A command's real subcommands, minus clap's auto-generated `help`.
fn subcommands(cmd: &Command) -> impl Iterator<Item = &Command> {
    cmd.get_subcommands().filter(|s| s.get_name() != "help")
}

/// `driftwm msg move [OPTIONS] [X] [Y]` — the full path, an options marker, each
/// positional's value token, and a subcommand slot.
fn usage_line(cmd: &Command, title: &str) -> String {
    let mut s = title.to_string();
    if visible_args(cmd).any(|a| !a.is_positional()) {
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
    let defaults = arg.get_default_values();
    (!defaults.is_empty()).then(|| {
        defaults
            .iter()
            .map(|v| v.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(",")
    })
}
