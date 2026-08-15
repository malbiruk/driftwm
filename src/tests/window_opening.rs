use insta::assert_snapshot;

use driftwm::config::Config;

use super::{Fixture, adopt_last_configure, map_window, window_by_app_id};

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
        @"size: 0 × 0, bounds: 0 × 0, states: [TiledLeft, TiledRight, TiledTop, TiledBottom]"
    );

    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 0 × 0, bounds: 0 × 0, states: [TiledLeft, TiledRight, TiledTop, TiledBottom, Activated]"
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
        @"size: 0 × 0, bounds: 0 × 0, states: [TiledLeft, TiledRight, TiledTop, TiledBottom]"
    );

    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 0 × 0, bounds: 0 × 0, states: [TiledLeft, TiledRight, TiledTop, TiledBottom, Activated]"
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
        size: 640 × 480, bounds: 0 × 0, states: []
        size: 640 × 480, bounds: 0 × 0, states: [TiledLeft, TiledRight, TiledTop, TiledBottom]
        "
    );

    // Match the configured size so the mapped geometry is the rule size.
    window.set_size(640, 480);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    // Activation was withheld from the size-only rule configures above; it
    // arrives on the placement configure once the buffer commit maps the window.
    assert_snapshot!(
        f.client(id).window(&surface).format_recent_configures(),
        @"size: 640 × 480, bounds: 0 × 0, states: [TiledLeft, TiledRight, TiledTop, TiledBottom, Activated]"
    );

    let mapped = f.state().stage.windows().next().cloned().unwrap();
    let pos = f.state().stage.position_of(&mapped).unwrap();
    // Rule (100, 200) names the window CENTER in Y-up canvas coords; internal
    // top-left = (100 - 640/2, -200 - 480/2). Literal on purpose: computing it
    // via rule_to_internal would track a semantics regression instead of
    // catching it.
    assert_eq!(pos, smithay::utils::Point::from((-220, -440)));
}

/// A window rule's `size` names the *visual frame*: under a per-window SSD
/// rule the client is configured with the content that frame encloses, not
/// the frame itself.
#[test]
fn window_rule_size_is_the_visual_frame_under_ssd() {
    let config = Config::from_toml(
        r#"
[decorations]
border_width = 4

[[window_rules]]
app_id = "ssd-rule"
size = [800, 600]
decoration = "server"
"#,
    )
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id("ssd-rule");
    window.commit();
    f.roundtrip(id);

    // The rule names an 800×600 frame; the initial configure carries the
    // content it encloses, deflated by the 25px bar and 4px border on every
    // side: 800 - 2×4 = 792 wide, 600 - 25 - 2×4 = 567 tall. Literal on
    // purpose.
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 792 × 567"),
        "expected the chrome-deflated content size, got:\n{configures}"
    );
}

/// A rule `size` smaller than its own chrome floors the configured content at
/// 1×1 rather than sending a zero or negative size — xdg-shell reads 0 as
/// "client picks its own", which would silently drop the rule instead of
/// shrinking the window.
#[test]
fn window_rule_size_smaller_than_its_own_chrome_floors_at_one_pixel() {
    let config = Config::from_toml(
        r#"
[decorations]
border_width = 4

[[window_rules]]
app_id = "tiny-rule"
size = [4, 4]
decoration = "server"
"#,
    )
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id("tiny-rule");
    window.commit();
    f.roundtrip(id);

    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 1 × 1"),
        "a frame smaller than its own chrome must floor at 1×1, not vanish to \
         0×0, got:\n{configures}"
    );
}

/// A window rule's `position` centers the *visual frame* — the same rule
/// convention `size` uses — so an SSD window's content lands stepped in from
/// the rule point by the bar and border.
#[test]
fn window_rule_position_centers_the_frame_under_ssd() {
    let config = Config::from_toml(
        r#"
[decorations]
border_width = 4

[[window_rules]]
app_id = "pos-rule"
size = [400, 300]
position = [100, -50]
decoration = "server"
"#,
    )
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id("pos-rule");
    window.commit();
    f.roundtrip(id);

    // Match the deflated content size (400 - 2×4 = 392 wide, 300 - 25 - 2×4 =
    // 267 tall) so the position configure lands.
    let window = f.client(id).window(&surface);
    window.set_size(392, 267);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let mapped = window_by_app_id(&mut f, "pos-rule").unwrap();
    let pos = f.state().stage.position_of(&mapped).unwrap();
    // Rule (100, -50) names the FRAME's center, Y-up. The frame recovers to
    // exactly 400×300 (392+2×4, 267+25+2×4); frame top-left =
    // (100 - 400/2, 50 - 300/2) = (-100, -100). Content top-left steps in by
    // the border and (bar+border): (-100+4, -100+25+4) = (-96, -71). Literal
    // on purpose.
    assert_eq!(pos, smithay::utils::Point::from((-96, -71)));
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
        @"size: 1920 × 1080, bounds: 0 × 0, states: [TiledLeft, TiledRight, TiledTop, TiledBottom, Activated, Fullscreen]"
    );
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    window.unset_fullscreen();
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 800 × 600, bounds: 0 × 0, states: [TiledLeft, TiledRight, TiledTop, TiledBottom, Activated]"
    );
}

/// A window mapped under a fullscreen window is background-placed and must not
/// steal the fullscreen window's Activated hint. Focus later moving to that
/// window must flush an Activated configure on its own — activation isn't
/// riding any other pending send at that point.
#[test]
fn new_window_under_fullscreen_keeps_activation() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_window(&mut f, id, "a", (800, 600));
    let a = window_by_app_id(&mut f, "a").unwrap();

    // A enters fullscreen and adopts the fullscreen size.
    let cw = f.client(id).window(&a_surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);
    // Drain A's configures so only its exit configure is inspected later.
    f.client(id).window(&a_surface).format_recent_configures();

    // A second window maps under the fullscreen one.
    let c_surface = map_window(&mut f, id, "c", (400, 300));

    // Keyboard focus stays on the fullscreen window.
    assert_eq!(
        f.state().focused_window().as_ref(),
        Some(&a),
        "a new background window must not take focus from the fullscreen window"
    );

    // The newcomer is background-placed; its initial configure is not activated.
    let c_configures = f.client(id).window(&c_surface).format_recent_configures();
    assert!(
        !c_configures.contains("Activated"),
        "a background-placed window's configures must not be activated, got:\n{c_configures}"
    );

    // The fullscreen window still carries Activated when it exits.
    let cw = f.client(id).window(&a_surface);
    cw.unset_fullscreen();
    f.double_roundtrip(id);
    let a_configures = f.client(id).window(&a_surface).format_recent_configures();
    assert!(
        a_configures.contains("Activated"),
        "the fullscreen window must keep its Activated hint through exit, got:\n{a_configures}"
    );

    // Focus now moves to the background window; the flip must flush its own
    // Activated configure since nothing else is queued to carry it.
    f.client(id).window(&c_surface).format_recent_configures();
    f.client(id).window(&a_surface).format_recent_configures();
    let c = window_by_app_id(&mut f, "c").unwrap();
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&c, serial);
    f.double_roundtrip(id);

    let c_focused = f.client(id).window(&c_surface).format_recent_configures();
    assert!(
        c_focused.contains("Activated"),
        "focusing the background window must flush an Activated configure, got:\n{c_focused}"
    );
    let a_deactivated = f.client(id).window(&a_surface).format_recent_configures();
    assert!(
        !a_deactivated.is_empty() && !a_deactivated.contains("Activated"),
        "the de-focused window must receive a deactivate configure, got:\n{a_deactivated}"
    );
}

/// Raising a parent re-raises its whole subtree, but activation is exclusive to
/// the topmost of it — the child. When the child already holds Activated, the
/// re-raise must be a no-op on the wire: activating once for the final target
/// avoids ping-ponging the hint (and flushing a burst of configures) between a
/// parent and its modal child.
#[test]
fn raise_parent_with_activated_child_is_quiet() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // Parent, then a child parented to it — the child ends up topmost/activated.
    let p_surface = map_window(&mut f, id, "parent", (800, 600));
    let p = window_by_app_id(&mut f, "parent").unwrap();
    let p_toplevel = f.client(id).window(&p_surface).xdg_toplevel.clone();

    let child = f.client(id).create_window();
    let c_surface = child.surface.clone();
    child.set_app_id("child");
    child.set_parent(Some(&p_toplevel));
    child.commit();
    f.roundtrip(id);
    let child = f.client(id).window(&c_surface);
    child.set_size(400, 300);
    child.attach_new_buffer();
    child.ack_last_and_commit();
    f.double_roundtrip(id);

    // Drain both windows' configures to isolate the re-raise.
    f.client(id).window(&p_surface).format_recent_configures();
    f.client(id).window(&c_surface).format_recent_configures();

    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&p, serial);
    f.double_roundtrip(id);

    let p_after = f.client(id).window(&p_surface).format_recent_configures();
    let c_after = f.client(id).window(&c_surface).format_recent_configures();
    assert!(
        p_after.is_empty() && c_after.is_empty(),
        "re-raising a parent with an already-activated child must emit no configures, \
         got parent:\n{p_after}\nchild:\n{c_after}"
    );
}
