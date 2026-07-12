//! Config hot-reload behavior: a bad edit must never take down the session,
//! a good edit applies live.

use super::{Fixture, config, map_window};
use crate::state::ErrorSource;

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
