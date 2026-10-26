//! Config hot-reload behavior: a bad edit must never take down the session,
//! a good edit applies live.

use wayland_protocols_misc::server_decoration::client::org_kde_kwin_server_decoration_manager::Mode as KdeMode;

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
    // Simulate a prior render having cached a label and body raster for this
    // size/scale — both bake decorations config (colors/radius) into pixels.
    {
        let s = f.state().find_suspended(sid).unwrap();
        let mut chrome = s.chrome.borrow_mut();
        chrome.label_key = Some((300, 200, 1, false, true));
        chrome.body_key = Some((300, 200, 1));
    }

    // A decorations-affecting reload resets both cached keys so the centered
    // label and the rounded body fill re-raster, like every other decoration.
    f.state()
        .reload_config_from_contents("[decorations]\ndefault_mode = \"server\"\nfont_size = 16\n");
    {
        let s = f.state().find_suspended(sid).unwrap();
        let chrome = s.chrome.borrow();
        assert!(
            chrome.label_key.is_none(),
            "reload reset the suspended label cache"
        );
        assert!(
            chrome.body_key.is_none(),
            "reload reset the suspended body cache"
        );
    }

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

/// Carried on an exact-name entry rather than a wildcard because a wildcard
/// can't hold a fixed position (it falls back to "auto"), and a fixed position
/// is what makes the position leg observable.
const OUTPUT_RULE_ALL_FIELDS: &str = r#"
[[outputs]]
name = "HEADLESS-1"
transform = "180"
scale = 2.0
mode = "1280x720"
position = [500, 300]
"#;

#[test]
fn reload_applies_mode_scale_and_transform_when_the_config_owns_them() {
    use smithay::utils::{Point, Transform};
    let mut f = Fixture::with_config(config(""));
    let output = f.add_output(1, (1920, 1080));

    f.state()
        .reload_config_from_contents(OUTPUT_RULE_ALL_FIELDS);

    assert_eq!(output.current_transform(), Transform::_180);
    assert_eq!(output.current_scale().fractional_scale(), 2.0);
    assert_eq!(
        f.state().pending_mode_changes.get("HEADLESS-1"),
        Some(&ModeIntent::Custom {
            w: 1280,
            h: 720,
            refresh_mhz: 60_000,
        })
    );
    assert_eq!(
        crate::state::output_state(&output).layout_position,
        Point::from((500, 300))
    );

    // Only the udev render loop drains this queue; the headless fixture has no
    // backend, so drain it by hand to leave teardown at the leak baseline.
    f.state().pending_mode_changes.clear();
}

#[test]
fn reload_leaves_mode_scale_and_transform_alone_when_the_backend_owns_them() {
    use smithay::utils::{Point, Transform};
    let mut f = Fixture::with_config(config(""));
    let output = f.add_output(1, (1920, 1080));
    // What the nested output carries from init: mode and scale from the host
    // window, transform from the renderer's Y-flip compensation.
    crate::state::output_state(&output).backend_owned_mode = true;

    f.state()
        .reload_config_from_contents(OUTPUT_RULE_ALL_FIELDS);

    assert_eq!(
        output.current_transform(),
        Transform::Normal,
        "a config transform must not override the backend's"
    );
    assert_eq!(
        output.current_scale().fractional_scale(),
        1.0,
        "a config scale must not override the host window's"
    );
    assert!(
        f.state().pending_mode_changes.is_empty(),
        "a mode intent nothing drains must not be queued"
    );
    assert_eq!(
        crate::state::output_state(&output).layout_position,
        Point::from((500, 300)),
        "position still applies on a backend-owned output"
    );
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

/// The KDE manager announces `default_mode` only on bind, so a reload must reach
/// the clients that bind after it and can't reach the ones already bound.
#[test]
fn reload_kde_default_mode_reaches_new_binds_not_existing() {
    let mut f = Fixture::with_config(config("[decorations]\ndefault_mode = \"client\"\n"));
    let early = f.add_client();
    f.roundtrip(early);

    f.state()
        .reload_config_from_contents("[decorations]\ndefault_mode = \"minimal\"\n");
    let late = f.add_client();
    f.roundtrip(late);
    f.roundtrip(early);

    assert_eq!(f.client(late).state.kde_default_modes, [KdeMode::Server]);
    assert_eq!(f.client(early).state.kde_default_modes, [KdeMode::Client]);
}
