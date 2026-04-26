# Contributing

Thanks for your interest! driftwm is experimental and primarily an AI-assisted learning project, but PRs and issues are welcome.

## Before you start

**Open an issue first for non-trivial changes.** Anything beyond a quick fix — features, refactors, multi-file changes — should start as an issue so we can align on the approach before you invest time. Small fixes can go straight to PR.

## Pull requests

**Keep PRs small and focused on one concern.** One PR = one logical change. If your description says "this does X and Y", that's two PRs.

When changes bundle multiple concerns, merging becomes all-or-nothing — if I like parts but not others, we lose a round trip asking you to split. Split up front and each piece lands (or doesn't) independently.

**CI must pass** — `cargo build`, `cargo clippy`, and `cargo test` run automatically on PRs.

## Reporting bugs

Include:
- What you expected vs what happened
- Steps to reproduce
- Distro, GPU, nested vs TTY, driftwm version
- `RUST_LOG=debug` logs if relevant

## Contributing without GitHub

If you'd rather not use GitHub, email patches to `2601074@gmail.com`. Generate them with `git format-patch` (one file per commit) and either attach them to a regular email or send via `git send-email`. They'll be applied with `git am`, preserving your authorship.
