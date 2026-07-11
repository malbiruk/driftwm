use insta::assert_snapshot;

use driftwm::config::Config;

use super::Fixture;

/// One output, a plain toplevel. Captures the initial configure, then the
/// post-map configure once a buffer is attached (window becomes Activated).
#[test]
fn simple() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 0 × 0, bounds: 0 × 0, states: [Activated, TiledLeft, TiledRight, TiledTop, TiledBottom]"
    );

    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 0 × 0, bounds: 0 × 0, states: [Activated, TiledLeft, TiledRight, TiledTop, TiledBottom]"
    );
}

/// No outputs: opening a window must not panic. Whatever driftwm configures
/// with zero outputs is snapshotted as-is.
#[test]
fn no_output() {
    let mut f = Fixture::new();

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 0 × 0, bounds: 0 × 0, states: [Activated, TiledLeft, TiledRight, TiledTop, TiledBottom]"
    );

    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 0 × 0, bounds: 0 × 0, states: [Activated, TiledLeft, TiledRight, TiledTop, TiledBottom]"
    );
}

/// A window rule keyed on `app_id` forces size and canvas position. The initial
/// configure carries the rule size; after map, the stage records the window at
/// the rule position (converted from center/Y-up rule coords to internal
/// top-left via the production transform).
#[test]
fn window_rule_size_and_position() {
    let config = Config::from_toml(
        r#"
[[window_rules]]
app_id = "test-rule"
size = [640, 480]
position = [100, 200]
"#,
    )
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id("test-rule");
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @r"
        size: 640 × 480, bounds: 0 × 0, states: [Activated]
        size: 640 × 480, bounds: 0 × 0, states: [Activated, TiledLeft, TiledRight, TiledTop, TiledBottom]
        "
    );

    // Match the configured size so the mapped geometry is the rule size.
    window.set_size(640, 480);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let mapped = f.state().stage.windows().next().cloned().unwrap();
    let pos = f.state().stage.position_of(&mapped).unwrap();
    // Rule (100, 200) names the window CENTER in Y-up canvas coords; internal
    // top-left = (100 - 640/2, -200 - 480/2). Literal on purpose: computing it
    // via rule_to_internal would track a semantics regression instead of
    // catching it.
    assert_eq!(pos, smithay::utils::Point::from((-220, -440)));
}

/// Fullscreen round-trip: map a window, request fullscreen (expect an
/// output-sized Fullscreen configure), then unset it (expect the restored
/// pre-fullscreen size).
#[test]
fn fullscreen_configure_sequence() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.set_size(800, 600);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    // Advance the cursor past the map-time configures.
    f.client(id).window(&surface).format_recent_configures();

    let window = f.client(id).window(&surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 1920 × 1080, bounds: 0 × 0, states: [Activated, TiledLeft, TiledRight, TiledTop, TiledBottom, Fullscreen]"
    );
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    window.unset_fullscreen();
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 800 × 600, bounds: 0 × 0, states: [Activated, TiledLeft, TiledRight, TiledTop, TiledBottom]"
    );
}
