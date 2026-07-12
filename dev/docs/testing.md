# Testing

The test-suite map and the few rules that keep it trustworthy. Everything runs
under plain `cargo test` — no display, GPU, or root — and CI runs exactly that.

## The map

- **Unit tests** — `#[cfg(test)]` modules across `src/` (canvas math, config
  parsing, membership, persistence change detection, …). Pure logic; smithay
  glue (handlers, delegates) is deliberately untested — see
  [caveats.md](caveats.md).
- **Integration tests** — `tests/*.rs`, linking the lib crate: config
  parse/docs-sync gates, canvas transforms, snap, window-rule matching.
- **Stage proptest harness** — `src/stage/tests.rs` (`mod harness`): random
  op-histories (map/close/crash/fullscreen/pin/fit/cluster/hotplug/auto-place)
  drive the real `Stage` alongside a model of `DriftWm` policy, with structural
  invariants checked after every op. This layer found the suspended-pin
  output-unplug bug.
- **Config robustness proptests** — `tests/config_robustness_test.rs`:
  arbitrary and hostile TOML through `Config::from_toml_collect` must never
  panic — the mechanical form of the hot-reload "a bad edit never crashes"
  promise.
- **In-process conformance fixture** — `src/tests/` (bin-crate unit tests): a
  real `DriftWm` runs headless (`backend = None`), real wayland-client test
  clients connect over socketpairs, and one `Fixture::dispatch()` pumps the
  whole graph deterministically. `verify_stage_invariants` runs on every
  dispatch, so a state leak aborts the test and an in-process deadlock becomes
  a hang you can't miss. Covers the layer nothing else reaches: protocol
  wiring, configure sequences as the client sees them, crash paths, focus
  timing. Start reading at `src/tests/window_opening.rs` — the idiom is
  map → roundtrip → snapshot/assert, and new scenarios should read as
  specification sentences. Every scenario is also leak-checked at teardown: the
  fixture's `Drop` tears down all clients and asserts `debug-counters` return to
  the construction-time baseline (opt out with `skip_baseline_check`).

## Rules

**Config options end in the reference, and the generated doc is committed.**
`config.reference.toml` is the single source of truth for every option
(grammar: [reference-config-format.md](reference-config-format.md)). Adding or
changing an option means documenting it there, regenerating with
`UPDATE_CONFIG_DOCS=1 cargo test docs_config_md_is_up_to_date`, and committing
both files. Never hand-edit `docs/config.md`. Tests gate the loop: reference
examples must parse warning-free, reference defaults must equal code defaults,
and the generated doc must be current.

**Never touch `Space` window APIs — go through the stage.** `clippy.toml`
rejects every `Space` element read and write via `disallowed-methods`; the
stage is the sole window store and `Space` is output-registry only. Applies to
tests too: drive `DriftWm` methods or the fixture. Details in
[caveats.md](caveats.md).

**Proptests run 256 cases in CI; go deeper before merging.** Plain
`cargo test` uses proptest's default 256 cases per property. Work that touches
the stage, layout, or config parsing deserves a deeper local pass:
`PROPTEST_CASES=4000 cargo test` (seconds, not minutes). When proptest finds a
counterexample it writes a seed under `proptest-regressions/` — commit it;
seeds replay first on every subsequent run, pinning the bug forever.

**Slow tests are opt-in.** The soak/leak scenario (`src/tests/soak.rs`) churns
hundreds of windows through map, state transitions, and both teardown paths,
asserting the counters, file descriptors, and RSS all plateau; it is gated
behind `RUN_SLOW_TESTS=1` so the default `cargo test` lane stays fast. Run it
alone with `RUN_SLOW_TESTS=1 cargo test --bin driftwm soak` — the fd/RSS
assertions are process-wide, so a concurrent full suite adds noise the slack has
to absorb. Gate any future slow test (wlcs, deeper soaks) the same way.

**Fixture tests never touch the real session.** Construct configs with
`Config::from_toml` + `Fixture::with_config` — never read
`~/.config/driftwm/config.toml` (that's what `DriftWm::new_with_config` is
for). Never read or write `$XDG_RUNTIME_DIR/driftwm`; the test pump
deliberately excludes the state-file writer. Don't set process env vars in
tests — the harness is multi-threaded.
