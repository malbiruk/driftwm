# Contributing

Thanks for your interest! driftwm is experimental and primarily an AI-assisted learning project, but PRs and issues are welcome.

## Before you start

**Open an issue first for non-trivial changes.** Anything beyond a quick fix — features, refactors, multi-file changes — should start as an issue so we can align on the approach before you invest time. Small fixes can go straight to PR.

**Read the internal docs in [`dev/docs/`](dev/docs/) before touching code** — architectural rules ([`caveats.md`](dev/docs/caveats.md)) and testing conventions ([`testing.md`](dev/docs/testing.md)) especially.

**Using an AI coding agent?** [`AGENTS.md`](AGENTS.md) is the single source of agent instructions. It follows the [agents.md](https://agents.md) convention, so Codex, Copilot, Cursor, Gemini CLI, Antigravity, Zed and others read it with no setup. Claude Code looks for `CLAUDE.md`, which is a one-line import of the same file. If your tool wants a different path, add a stub that points at `AGENTS.md` — copies drift, pointers don't.

## Pull requests

**Keep PRs small and focused on one concern.** One PR = one logical change. If your description says "this does X and Y", that's two PRs.

When changes bundle multiple concerns, merging becomes all-or-nothing — if I like parts but not others, we lose a round trip asking you to split. Split up front and each piece lands (or doesn't) independently.

**CI must pass** — `cargo fmt --check`, `cargo clippy`, `cargo build`, and `cargo test` run automatically on PRs.

**Only state belongs in `src/state/mod.rs`** — the `DriftWm` fields, the types they're made of, and the routines that must enumerate every field. A new `impl DriftWm` method goes in the sibling module for the subsystem it serves.

## Blaming through reformats

Bulk reformats and pure file-motion commits are listed in [`.git-blame-ignore-revs`](.git-blame-ignore-revs), so `git blame` reaches the commit that actually wrote a line. GitHub applies it automatically; locally:

```bash
git config blame.ignoreRevsFile .git-blame-ignore-revs
```

## Reporting bugs

Include:

- What you expected vs what happened
- Steps to reproduce
- Distro, GPU, nested vs TTY, driftwm version
- `RUST_LOG=debug` logs if relevant

## Contributing without GitHub

If you'd rather not use GitHub, email patches to `2601074@gmail.com`. Generate them with `git format-patch` (one file per commit) and either attach them to a regular email or send via `git send-email`. They'll be applied with `git am`, preserving your authorship.
