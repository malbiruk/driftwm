//! xdg-popup lifecycle and xdg-activation focus policy driven through a real
//! client: mapping/tracking, parent teardown, grab-serial handling, client
//! crash reaping, and the serial gate on activation.

use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1;

use super::{
    Fixture, config, first_popup_surface, keyboard_focus, map_popup, map_window, popups_tracked_on,
    server_surface, window_by_app_id,
};

#[test]
fn popup_maps_and_is_tracked() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let parent = map_window(&mut f, id, "parent", (400, 300));
    let popup = map_popup(&mut f, id, &parent);

    let window = window_by_app_id(&mut f, "parent").unwrap();
    let root = server_surface(&window);
    assert_eq!(
        popups_tracked_on(&root),
        1,
        "compositor should track the mapped popup on its parent"
    );

    let cfgs = f.client(id).popup(&popup).format_recent_configures();
    assert!(!cfgs.is_empty(), "popup should have received a configure");
    assert!(
        !f.client(id).popup(&popup).popup_done,
        "a freshly mapped popup must not be dismissed"
    );

    f.client(id).popup(&popup).destroy();
    f.double_roundtrip(id);
}

#[test]
fn popup_orphaned_when_parent_closes_reaps_cleanly() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let parent = map_window(&mut f, id, "parent", (400, 300));
    let popup = map_popup(&mut f, id, &parent);

    let window = window_by_app_id(&mut f, "parent").unwrap();
    let root = server_surface(&window);
    let popup_server = first_popup_surface(&root).unwrap();

    // Destroy the parent toplevel out from under the still-open popup.
    f.client(id).window(&parent).destroy();
    f.double_roundtrip(id);
    f.pump(5);
    f.roundtrip(id);

    // driftwm does not proactively dismiss an orphaned popup: no popup_done is
    // sent and it stays tracked. Reaping is deferred to the popup's own
    // teardown (below) or the client's death — never leaked, never a crash.
    assert!(
        !f.client(id).popup(&popup).popup_done,
        "driftwm sends no popup_done on parent close"
    );
    assert!(
        f.state().popups.find_popup(&popup_server).is_some(),
        "orphaned popup stays tracked until its own surface is destroyed"
    );

    // Destroying the popup surface reaps it on the next cleanup pass.
    f.client(id).popup(&popup).destroy();
    f.double_roundtrip(id);
    f.pump(3);
    assert!(
        f.state().popups.find_popup(&popup_server).is_none(),
        "popup must be reaped once its surface is destroyed"
    );
}

#[test]
fn popup_grab_with_unrecognized_serial_is_honored() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let parent = map_window(&mut f, id, "parent", (400, 300));

    let popup = f.client(id).create_popup(&parent);
    let popup_surface = popup.surface.clone();
    popup.commit();
    f.roundtrip(id);

    let popup = f.client(id).popup(&popup_surface);
    popup.grab(999_999);
    popup.attach_new_buffer();
    popup.ack_last_and_commit();
    f.double_roundtrip(id);

    // driftwm does not validate the grab serial: the grab is installed and the
    // popup stays mapped rather than being dismissed.
    assert!(
        f.state().popup_grab.is_some(),
        "grab should be installed despite the bogus serial"
    );
    assert!(
        !f.client(id).popup(&popup_surface).popup_done,
        "popup must not be dismissed for an unrecognized grab serial"
    );

    let window = window_by_app_id(&mut f, "parent").unwrap();
    let root = server_surface(&window);
    assert_eq!(popups_tracked_on(&root), 1);

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}

#[test]
fn client_crash_with_open_popup_reaps_everything() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let parent = map_window(&mut f, id, "parent", (400, 300));

    // A grabbed popup, so the crash also has to reap the popup grab.
    let popup = f.client(id).create_popup(&parent);
    let popup_surface = popup.surface.clone();
    popup.commit();
    f.roundtrip(id);
    let popup = f.client(id).popup(&popup_surface);
    popup.grab(1);
    popup.attach_new_buffer();
    popup.ack_last_and_commit();
    f.double_roundtrip(id);

    let window = window_by_app_id(&mut f, "parent").unwrap();
    let root = server_surface(&window);
    let popup_server = first_popup_surface(&root).unwrap();
    assert!(f.state().popup_grab.is_some());

    f.kill_client(id);
    f.pump(20);

    assert!(
        f.state().popups.find_popup(&popup_server).is_none(),
        "popup must be reaped when its client dies"
    );
    assert!(
        f.state().popup_grab.is_none(),
        "popup grab must be released when its client dies"
    );
}

#[test]
fn overhanging_popup_keeps_parent_hit_testable() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let parent = map_window(&mut f, id, "parent", (400, 300));
    let popup_surface = map_popup(&mut f, id, &parent);

    let window = window_by_app_id(&mut f, "parent").unwrap();
    let win_pos = f.state().stage.position_of(&window).unwrap();
    // The default positioner (1×1 anchor rect at the parent's top-left
    // corner, no anchor/gravity) centers the popup on that corner, so most
    // of it overhangs up and to the left of the parent's own bbox.
    let popup_pos = f.client(id).popup(&popup_surface).pending_configure.pos;

    let overhang: smithay::utils::Point<f64, smithay::utils::Logical> = (
        f64::from(win_pos.x + popup_pos.0),
        f64::from(win_pos.y + popup_pos.1),
    )
        .into();

    // Guard against a vacuous test: the overhang point really must fall
    // outside the parent's own (popup-less) bbox.
    #[allow(clippy::disallowed_methods)] // the popup-less box is the point here
    let mut parent_only_bbox = window.bbox();
    parent_only_bbox.loc += win_pos;
    assert!(
        !parent_only_bbox.to_f64().contains(overhang),
        "test setup bug: overhang point {overhang:?} is inside the parent's own bbox {parent_only_bbox:?}"
    );

    let hit = f.state().element_under(overhang).map(|(w, _)| w.clone());
    assert_eq!(
        hit,
        Some(window.clone()),
        "a point over the popup's overhang must still hit-test to the parent window"
    );

    // Sanity: a point clearly outside both the window and the popup finds nothing.
    let far_away: smithay::utils::Point<f64, smithay::utils::Logical> = (
        f64::from(win_pos.x) - 10_000.0,
        f64::from(win_pos.y) - 10_000.0,
    )
        .into();
    assert!(
        f.state().element_under(far_away).is_none(),
        "a point far from both the window and the popup must hit nothing"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}

/// A canvas-positioned layer widget (see the `widget`/`position` window
/// rule) can parent an xdg popup directly (`zwlr_layer_surface_v1.get_popup`).
/// `canvas_layer_under` must find that popup even where it overhangs past
/// the widget's own bbox.
#[test]
fn overhanging_popup_keeps_layer_widget_hit_testable() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "widget"
position = [0, 0]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let layer = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Top, "widget");
    let layer_surface = layer.surface.clone();
    // The layer's own requested size must be non-zero before any commit
    // (unanchored, so the compositor can't derive it from anchor edges).
    layer.set_configure_props(super::client::LayerConfigureProps {
        size: Some((200, 150)),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&layer_surface);
    layer.set_size(200, 150);
    layer.attach_new_buffer();
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let popup = f.client(id).create_layer_popup(&layer_surface);
    let popup_surface = popup.surface.clone();
    popup.commit();
    f.roundtrip(id);

    let popup = f.client(id).popup(&popup_surface);
    popup.attach_new_buffer();
    popup.ack_last_and_commit();
    f.double_roundtrip(id);

    let cl_pos = f.state().canvas_layers[0].position.unwrap();
    // Same default positioner as the xdg-toplevel case: 1×1 anchor rect at
    // the widget's top-left corner, no anchor/gravity, so the popup overhangs
    // up and to the left of the widget's own bbox.
    let popup_pos = f.client(id).popup(&popup_surface).pending_configure.pos;
    let overhang: smithay::utils::Point<f64, smithay::utils::Logical> = (
        f64::from(cl_pos.x + popup_pos.0),
        f64::from(cl_pos.y + popup_pos.1),
    )
        .into();

    // Guard against a vacuous test: the overhang point really must fall
    // outside the widget's own (popup-less) bbox.
    let mut widget_only_bbox = f.state().canvas_layers[0].surface.bbox();
    widget_only_bbox.loc += cl_pos;
    assert!(
        !widget_only_bbox.to_f64().contains(overhang),
        "test setup bug: overhang point {overhang:?} is inside the widget's own bbox {widget_only_bbox:?}"
    );

    let widget_root = f.state().canvas_layers[0].surface.wl_surface().clone();
    let popup_server_surface = first_popup_surface(&widget_root).unwrap();

    let hit = f.state().canvas_layer_under(overhang).map(|(t, _)| t.0);
    assert_eq!(
        hit,
        Some(popup_server_surface),
        "a point over the popup's overhang must hit-test to the popup surface"
    );

    // Sanity: a point clearly outside both the widget and the popup finds nothing.
    let far_away: smithay::utils::Point<f64, smithay::utils::Logical> = (
        f64::from(cl_pos.x) - 10_000.0,
        f64::from(cl_pos.y) - 10_000.0,
    )
        .into();
    assert!(
        f.state().canvas_layer_under(far_away).is_none(),
        "a point far from both the widget and the popup must hit nothing"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}

#[test]
fn activation_with_serial_moves_focus() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let target = map_window(&mut f, id, "target", (400, 300));
    let requester = map_window(&mut f, id, "requester", (400, 300));

    let requester_win = window_by_app_id(&mut f, "requester").unwrap();
    assert_eq!(keyboard_focus(&mut f), Some(server_surface(&requester_win)));

    // Token created from user input (carries a serial) → honored.
    f.client(id).request_activation_token(&requester, true);
    f.roundtrip(id);
    f.client(id).activate(&target);
    f.double_roundtrip(id);

    let target_win = window_by_app_id(&mut f, "target").unwrap();
    assert_eq!(
        keyboard_focus(&mut f),
        Some(server_surface(&target_win)),
        "activation with a valid serial must move focus to the target"
    );
}

#[test]
fn activation_without_serial_does_not_move_focus() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let target = map_window(&mut f, id, "target", (400, 300));
    let requester = map_window(&mut f, id, "requester", (400, 300));

    let requester_win = window_by_app_id(&mut f, "requester").unwrap();
    let requester_surface = server_surface(&requester_win);
    assert_eq!(keyboard_focus(&mut f), Some(requester_surface.clone()));

    // Token with no serial is a spontaneous attention request → ignored.
    f.client(id).request_activation_token(&requester, false);
    f.roundtrip(id);
    f.client(id).activate(&target);
    f.double_roundtrip(id);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(requester_surface),
        "activation without a serial must not steal focus"
    );
}
