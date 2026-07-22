//! Config hot-reload behavior: a bad edit must never take down the session,
//! a good edit applies live.

use super::{Fixture, config, map_window};
use crate::state::{ErrorSource, ModeIntent};

#[test]
fn bad_toml_keeps_old_config_and_raises_error() {
    let mut f = Fixture::with_config(config("[navigation]\ndrift = 0.25\n"));
    assert_eq!(f.state().config.drift, 0.25);

    f.state().reload_config_from_contents("this is [not toml");

    assert_eq!(f.state().config.drift, 0.25);
    assert!(f.state().errors.contains_key(&ErrorSource::Config));
}

#[test]
fn unknown_field_is_a_hard_error() {
    let mut f = Fixture::with_config(config("[navigation]\ndrift = 0.25\n"));

    // Valid TOML, but `deny_unknown_fields` rejects the misspelled key.
    f.state()
        .reload_config_from_contents("[navigation]\nanimation_speeed = 0.5\n");

    assert_eq!(f.state().config.drift, 0.25);
    assert!(f.state().errors.contains_key(&ErrorSource::Config));
}

#[test]
fn good_reload_applies_and_clears_error() {
    let mut f = Fixture::with_config(config("[navigation]\ndrift = 0.25\n"));

    f.state().reload_config_from_contents("not [valid toml");
    assert!(f.state().errors.contains_key(&ErrorSource::Config));

    f.state()
        .reload_config_from_contents("[navigation]\ndrift = 0.75\n");
    assert_eq!(f.state().config.drift, 0.75);
    assert!(!f.state().errors.contains_key(&ErrorSource::Config));
}

#[test]
fn soft_warnings_surface_without_rejecting() {
    let mut f = Fixture::with_config(config(""));

    // `drift` above its range clamps to 1.0 with a warning: the value still
    // applies, but the warning surfaces in the error bar.
    f.state()
        .reload_config_from_contents("[navigation]\ndrift = 5.0\n");

    assert_eq!(f.state().config.drift, 1.0);
    assert!(f.state().errors.contains_key(&ErrorSource::Config));
}

#[test]
fn reload_invalidates_suspended_label_cache() {
    use smithay::utils::{Point, Size};
    let mut f = Fixture::with_config(config("[decorations]\ndefault_mode = \"server\"\n"));
    f.add_output(1, (1920, 1080));
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((100, 100)),
        Size::from((300, 200)),
        "s",
        "S",
    );
    // Simulate a prior render having cached a label raster for this size/scale.
    f.state()
        .find_suspended(sid)
        .unwrap()
        .chrome
        .borrow_mut()
        .label_key = Some((300, 200, 1, false, true));

    // A decorations-affecting reload resets the cached key so the centered label
    // re-rasters, like every other decoration.
    f.state()
        .reload_config_from_contents("[decorations]\ndefault_mode = \"server\"\nfont_size = 16\n");
    assert!(
        f.state()
            .find_suspended(sid)
            .unwrap()
            .chrome
            .borrow()
            .label_key
            .is_none(),
        "reload reset the suspended label cache"
    );

    // The headless fixture has no backend to drain a queued mode intent.
    f.state().pending_mode_changes.clear();
    f.state().dismiss_suspended(sid);
}

#[test]
fn reload_to_preferred_mode_queues_intent() {
    let mut f = Fixture::with_config(config(""));
    f.add_output(1, (1920, 1080));

    // The state layer can't tell whether the current mode already is the
    // preferred one (no EDID knowledge), so a "preferred" rule always queues;
    // the backend resolves and skips the modeset when it's a no-op.
    f.state().reload_config_from_contents(
        r#"
[[outputs]]
name = "HEADLESS-1"
mode = "preferred"
"#,
    );

    assert_eq!(
        f.state().pending_mode_changes.get("HEADLESS-1"),
        Some(&ModeIntent::Preferred)
    );

    // Only the udev render loop drains this queue; the headless fixture has no
    // backend, so drain it by hand to leave teardown at the leak baseline.
    f.state().pending_mode_changes.clear();
}

#[test]
fn reload_to_max_mode_via_wildcard_queues_intent() {
    let mut f = Fixture::with_config(config(""));
    f.add_output(1, (1920, 1080));

    // A wildcard entry applies to any output without an exact-name entry; like
    // "preferred", "max" needs the connector's mode list to resolve, so the
    // state layer queues it unconditionally for the backend to resolve.
    f.state().reload_config_from_contents(
        r#"
[[outputs]]
name = "*"
mode = "max"
"#,
    );

    assert_eq!(
        f.state().pending_mode_changes.get("HEADLESS-1"),
        Some(&ModeIntent::Max)
    );

    // Only the udev render loop drains this queue; the headless fixture has no
    // backend, so drain it by hand to leave teardown at the leak baseline.
    f.state().pending_mode_changes.clear();
}

#[test]
fn reload_to_matching_explicit_mode_queues_nothing() {
    let mut f = Fixture::with_config(config(""));
    f.add_output(1, (1920, 1080));

    // An explicit rule matching the current mode is change-detected in the
    // state layer, so no intent is queued.
    f.state().reload_config_from_contents(
        r#"
[[outputs]]
name = "HEADLESS-1"
mode = "1920x1080"
"#,
    );

    assert!(f.state().pending_mode_changes.is_empty());
}

#[test]
fn reload_rules_affect_new_windows_not_existing() {
    let mut f = Fixture::with_config(config(""));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let existing = map_window(&mut f, id, "later", (400, 300));
    // Drain the mapping configures so we only observe post-reload traffic.
    let _ = f.client(id).window(&existing).format_recent_configures();

    // A rule matches only on a window's first commit, so adding one on reload
    // must not reconfigure the already-mapped window to the rule size.
    f.state().reload_config_from_contents(
        r#"
[[window_rules]]
app_id = "later"
size = [640, 480]
"#,
    );
    // Reload queues a preferred-mode intent for the connected output; only the
    // udev render loop drains that queue, so clear it to keep teardown at
    // the leak baseline.
    f.state().pending_mode_changes.clear();
    f.double_roundtrip(id);

    // Re-commit the existing surface so even a rule application deferred to
    // the next commit would surface before the absence assertion.
    f.client(id).window(&existing).commit();
    f.roundtrip(id);

    let existing_configures = f.client(id).window(&existing).format_recent_configures();
    assert!(
        !existing_configures.contains("size: 640 × 480"),
        "reload must not re-force the rule size on an existing window, got:\n{existing_configures}"
    );

    // A window mapped after the reload with the same app_id does get the rule.
    let fresh = map_window(&mut f, id, "later", (400, 300));
    let fresh_configures = f.client(id).window(&fresh).format_recent_configures();
    assert!(
        fresh_configures.contains("size: 640 × 480"),
        "a window mapped after the reload must receive the rule size, got:\n{fresh_configures}"
    );
}
