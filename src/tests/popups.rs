//! xdg-popup lifecycle and xdg-activation focus policy driven through a real
//! client: mapping/tracking, parent teardown, grab-serial handling, client
//! crash reaping, and the serial gate on activation.

use driftwm::config::BindingContext;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Logical, Point};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols::xdg::shell::client::xdg_positioner::{
    Anchor, ConstraintAdjustment, Gravity,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::decorations::DecorationHit;
use crate::state::output_state;

use super::client::{ClientId, PopupProps};
use super::{
    Fixture, config, first_popup_surface, keyboard_focus, map_layer_popup_with, map_popup,
    map_popup_with, map_window, popups_tracked_on, server_surface, window_by_app_id,
};

fn pt(x: f64, y: f64) -> Point<f64, Logical> {
    Point::from((x, y))
}

/// Scale a mapped popup's single-pixel buffer up to `size` through its
/// viewport. Without this the popup's input region is one logical pixel at
/// whatever corner the positioner placed it, however large the positioner
/// asked for — smithay sizes a surface from its viewport destination or its
/// buffer, never from the positioner.
fn grow_popup(f: &mut Fixture, id: ClientId, surface: &WlSurface, size: (u16, u16)) {
    let popup = f.client(id).popup(surface);
    popup.set_size(size.0, size.1);
    popup.commit();
    f.double_roundtrip(id);
}

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
    parent_only_bbox.loc += win_pos - window.geometry().loc;
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

/// The parent's own popup covers part of the parent's 8px CSD resize ring —
/// a menu item that overhangs the frame. Popups render above the compositor's
/// chrome, so that point belongs to the popup: reporting the ring there shows
/// a resize cursor over the menu and resizes the parent on click.
#[test]
fn popup_over_the_parents_resize_band_is_not_a_chrome_hit() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let parent = map_window(&mut f, id, "parent", (400, 300));
    let popup_surface = map_popup(&mut f, id, &parent);
    grow_popup(&mut f, id, &popup_surface, (200, 100));

    let window = window_by_app_id(&mut f, "parent").unwrap();
    let win_pos = f.state().stage.position_of(&window).unwrap();
    // The default positioner centers the 200x100 popup on the parent's
    // top-left corner, so its right half laps over the parent's left band.
    let probe = pt(f64::from(win_pos.x) - 4.0, f64::from(win_pos.y) + 10.0);

    let popup_server = first_popup_surface(&server_surface(&window)).unwrap();
    assert_eq!(
        f.state().surface_under(probe, None).map(|(t, _)| t.0),
        Some(popup_server),
        "test setup bug: the probe must land on the popup, not just near it"
    );

    let hit = f.state().decoration_under(probe).map(|(_, hit)| hit);
    assert!(
        hit.is_none(),
        "a band point covered by the window's own popup must not be chrome, got {hit:?}"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}

/// The pre-check takes a popup-shaped bite out of the chrome walk, not the
/// whole ring: a band point no popup reaches must keep answering.
#[test]
fn resize_band_uncovered_by_the_popup_still_reports_chrome() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let parent = map_window(&mut f, id, "parent", (400, 300));
    let popup_surface = map_popup(&mut f, id, &parent);

    let window = window_by_app_id(&mut f, "parent").unwrap();
    let win_pos = f.state().stage.position_of(&window).unwrap();
    // The right band, a full window width away from the popup sitting at the
    // parent's top-left corner.
    let probe = pt(f64::from(win_pos.x) + 404.0, f64::from(win_pos.y) + 150.0);

    let hit = f.state().decoration_under(probe).map(|(_, hit)| hit);
    assert!(
        matches!(
            hit,
            Some(DecorationHit::ResizeBorder(xdg_toplevel::ResizeEdge::Right))
        ),
        "an uncovered resize band must still report chrome, got {hit:?}"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}

/// A CSD client's shadow reaches past its geometry over the compositor's own
/// resize ring, and such clients usually declare no input region, so the
/// shadow reads as input across its whole buffer. The popup pre-check must
/// therefore skip the toplevel tree — occluding on it would kill
/// compositor-side border resize for every window that draws a shadow.
#[test]
fn a_windows_own_shadow_does_not_occlude_its_resize_band() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let shadow = Point::<i32, Logical>::from((26, 23));
    let surface = map_window(
        &mut f,
        id,
        "shadowed",
        ((400 + shadow.x * 2) as u16, (300 + shadow.y * 2) as u16),
    );
    f.client(id)
        .window(&surface)
        .set_geometry(shadow.x, shadow.y, 400, 300);
    f.client(id).window(&surface).commit();
    f.double_roundtrip(id);

    let window = window_by_app_id(&mut f, "shadowed").unwrap();
    assert_eq!(
        window.geometry().loc,
        shadow,
        "test setup bug: the window must carry the shadow offset"
    );
    let win_pos = f.state().stage.position_of(&window).unwrap();
    let probe = pt(f64::from(win_pos.x) - 4.0, f64::from(win_pos.y) + 10.0);

    assert!(
        f.state().element_under(probe).is_some(),
        "test setup bug: the shadow must really cover the probe"
    );

    let hit = f.state().decoration_under(probe).map(|(_, hit)| hit);
    assert!(
        matches!(
            hit,
            Some(DecorationHit::ResizeBorder(xdg_toplevel::ResizeEdge::Left))
        ),
        "the window's own shadow must not occlude its resize band, got {hit:?}"
    );
}

/// A pinned window's chrome answers from `pinned_decoration_under`, a separate
/// screen-space walk from `decoration_under`, so the popup carve-out has to
/// exist there too — a menu overhanging a pinned frame must not resize it.
#[test]
fn popup_over_a_pinned_parents_resize_band_is_not_a_chrome_hit() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [200, 150]
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let parent = map_window(&mut f, id, "pin", (200, 150));
    let popup_surface = map_popup(&mut f, id, &parent);
    grow_popup(&mut f, id, &popup_surface, (200, 100));

    let window = window_by_app_id(&mut f, "pin").unwrap();
    let site = f.state().stage.pin_of(&window).cloned().unwrap();
    let pin = site.screen_pos;

    // The far side of the same ring, where the popup does not reach. Without
    // it a pin the walk skips outright — wrong output, or never pinned at all —
    // would read as the popup silencing the chrome.
    let uncovered = pt(f64::from(pin.x) + 204.0, f64::from(pin.y) + 75.0);
    let chrome = f.state().pinned_decoration_under(uncovered).map(|(_, h)| h);
    assert!(
        matches!(
            chrome,
            Some(DecorationHit::ResizeBorder(xdg_toplevel::ResizeEdge::Right))
        ),
        "test setup bug: the pinned window's uncovered band must report chrome, got {chrome:?}"
    );

    // The default positioner centers the 200x100 popup on the parent's
    // top-left corner, so its right half laps over the parent's left band.
    let covered = pt(f64::from(pin.x) - 4.0, f64::from(pin.y) + 10.0);
    let hit = f.state().pinned_decoration_under(covered).map(|(_, h)| h);
    assert!(
        hit.is_none(),
        "a pinned band point covered by the window's own popup must not be chrome, got {hit:?}"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}

/// `pointer_context` counts chrome as on-window through `decoration_under`.
/// With that disjunct now silent over a popup-covered band, the context has to
/// rest on the popup-aware `element_under` arm — otherwise every on-window
/// mouse binding silently swaps for its on-canvas counterpart over a menu.
#[test]
fn binding_context_over_a_popup_covered_band_stays_on_window() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let parent = map_window(&mut f, id, "parent", (400, 300));
    let popup_surface = map_popup(&mut f, id, &parent);
    grow_popup(&mut f, id, &popup_surface, (200, 100));

    let window = window_by_app_id(&mut f, "parent").unwrap();
    let win_pos = f.state().stage.position_of(&window).unwrap();
    let probe = pt(f64::from(win_pos.x) - 4.0, f64::from(win_pos.y) + 10.0);

    // Without this the context could ride the `decoration_under` disjunct — the
    // very one the popup silences — and pass for the wrong reason.
    let chrome = f.state().decoration_under(probe).map(|(_, hit)| hit);
    assert!(
        chrome.is_none(),
        "test setup bug: the probe must sit where the chrome walk falls silent, got {chrome:?}"
    );
    assert_eq!(
        f.state().pointer_context(probe),
        BindingContext::OnWindow,
        "a point over a popup is on-window whether or not the chrome walk answers"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}

/// `window_for_surface` matches a toplevel's own surface only, so the button
/// dispatch's "is anything on top of this chrome?" guard read every popup and
/// subsurface hit as *nothing on top* — a silent no-op. The root-resolving
/// lookup answers with the window that actually owns the surface, however deep
/// the popup chain.
#[test]
fn window_for_surface_root_resolves_popups_to_their_toplevel() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let parent = map_window(&mut f, id, "parent", (400, 300));
    let menu = map_popup(&mut f, id, &parent);
    let submenu = map_popup(&mut f, id, &menu);

    let window = window_by_app_id(&mut f, "parent").unwrap();
    let root = server_surface(&window);

    let popups: Vec<_> = smithay::desktop::PopupManager::popups_for_surface(&root)
        .map(|(kind, _)| kind.wl_surface().clone())
        .collect();
    assert_eq!(
        popups.len(),
        2,
        "test setup bug: both the menu and its submenu must be tracked"
    );

    assert_eq!(
        f.state().window_for_surface(&popups[0]),
        None,
        "a popup surface is not a stage window's own surface — the gap this resolves"
    );
    for popup in &popups {
        assert_eq!(
            f.state().window_for_surface_root(popup),
            Some(window.clone()),
            "every popup in the chain must resolve to the toplevel it hangs off"
        );
    }
    assert_eq!(
        f.state().window_for_surface_root(&root),
        Some(window.clone()),
        "a toplevel's own surface still resolves to itself"
    );

    f.client(id).popup(&submenu).destroy();
    f.client(id).popup(&menu).destroy();
    f.double_roundtrip(id);
}

/// A canvas-positioned layer widget (see the `widget`/`position` window
/// rule) can parent an xdg popup directly (`zwlr_layer_surface_v1.get_popup`).
/// `canvas_layer_under` must find that popup even where it overhangs past
/// the widget's own bbox.
#[test]
fn overhanging_popup_on_layer_widget_is_hit_testable() {
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
    assert_eq!(
        popups_tracked_on(&widget_root),
        1,
        "a layer-parented popup must be tracked exactly once — a duplicate tree entry renders it twice"
    );

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

/// A popup's constraint target is the *parent's* output, not the pointer's:
/// with the parent visible only on output 2 while output 1 is active, sliding
/// the popup back into bounds must use output 2's viewport. The anchor sits at
/// the parent's own right edge and is pushed left until it's flush with
/// output 2's right edge (window-relative: camera 5000 + 1280 + 2 padding -
/// window x 6000 = 282, so 391 + 200 - 282 = 309px of slide, landing at
/// x = 391 - 309 = 82). Constraining against output 1 instead would slide it
/// thousands of pixels in the same direction, since output 1's viewport sits
/// near the canvas origin, nowhere near the parent.
#[test]
fn popup_constrains_against_non_active_parent_output() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    // Move output 2 far from the canvas origin so its viewport can't overlap
    // output 1's default (origin-centered) one.
    output_state(&out2).camera = Point::from((5000.0, 5000.0));
    f.state().focused_output = Some(out1.clone());

    let id = f.add_client();
    let parent = map_window(&mut f, id, "parent", (400, 300));
    let window = window_by_app_id(&mut f, "parent").unwrap();
    // Place the parent inside output 2's viewport, near its right edge.
    f.state()
        .stage
        .set_position(&window, Point::from((6000, 5000)));

    let popup_surface = map_popup_with(
        &mut f,
        id,
        &parent,
        PopupProps {
            anchor_rect: (390, 140, 1, 1),
            anchor: Anchor::Right,
            gravity: Gravity::Right,
            constraint_adjustment: ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY,
            ..Default::default()
        },
    );

    assert_eq!(
        f.client(id).popup(&popup_surface).pending_configure.pos,
        (82, 90),
        "popup must slide against the parent's own output (2), not the active one (1)"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}

/// Same shape as above, but the parent's output is zoomed: output 2's visible
/// canvas area shrinks to 642×362 (1280/2 + 2, 720/2 + 2), so the correct
/// slide distance differs from the zoom-1 case, and still has nothing to do
/// with output 1's un-zoomed viewport.
#[test]
fn popup_constrains_against_non_active_parent_output_zoomed() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    output_state(&out2).camera = Point::from((5000.0, 5000.0));
    output_state(&out2).zoom = 2.0;
    f.state().focused_output = Some(out1.clone());

    let id = f.add_client();
    let parent = map_window(&mut f, id, "parent", (400, 300));
    let window = window_by_app_id(&mut f, "parent").unwrap();
    f.state()
        .stage
        .set_position(&window, Point::from((5400, 5100)));

    let popup_surface = map_popup_with(
        &mut f,
        id,
        &parent,
        PopupProps {
            anchor_rect: (390, 140, 1, 1),
            anchor: Anchor::Right,
            gravity: Gravity::Right,
            constraint_adjustment: ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY,
            ..Default::default()
        },
    );

    // Target right edge (window-relative): 5000 + 642 - 5400 = 242, so
    // 391 + 200 - 242 = 349px of slide, landing at x = 391 - 349 = 42.
    assert_eq!(
        f.client(id).popup(&popup_surface).pending_configure.pos,
        (42, 90),
        "popup must slide against the parent's zoomed output (2), not the un-zoomed active one (1)"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}

/// The tie-break that makes "the parent's output" well-defined: with default
/// cameras every viewport is centered on the canvas origin, so a parent sitting
/// there is shown by *both* outputs and the answer can't come from scanning the
/// output list. The active output wins — popups are pointer-triggered, and the
/// pointer is on the active output. Resolving by a plain first-match instead
/// would pick output 1 (registered first) and leave the popup unslid at x=600,
/// hanging off the smaller active output.
#[test]
fn popup_prefers_the_active_output_when_viewports_overlap() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    // Cameras stay at their defaults: both viewports straddle the origin.
    f.state().focused_output = Some(out2.clone());
    assert_eq!(
        f.state().space.outputs().next().map(|o| o.name()),
        Some(out1.name()),
        "precondition: output 1 comes first, so first-match and active-first disagree"
    );

    let id = f.add_client();
    let parent = map_window(&mut f, id, "parent", (400, 300));
    let window = window_by_app_id(&mut f, "parent").unwrap();
    f.state().stage.set_position(&window, Point::from((0, 0)));

    let popup_surface = map_popup_with(
        &mut f,
        id,
        &parent,
        PopupProps {
            offset: (700, 0),
            constraint_adjustment: ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY,
            ..Default::default()
        },
    );

    // Unconstrained the popup sits at (600, -50). Output 2's visible canvas
    // rect is (-640, -360, 1282, 722), window-relative unchanged (the window is
    // at the origin), so the popup's right edge overshoots by 800 - 642 = 158
    // and slides back to x = 442. Output 1's rect reaches x = 962 and would not
    // slide it at all.
    assert_eq!(
        f.client(id).popup(&popup_surface).pending_configure.pos,
        (442, -50),
        "overlapping viewports must be broken by the active output, not list order"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}

/// A screen-pinned parent constrains in screen space against its own pin
/// output — never the canvas camera/zoom of whichever output is active. The
/// pin output's zoom (2.0) is deliberately different from the active output's
/// (1.0): a pinned window always renders at scale 1.0, so the correct slide
/// distance doesn't depend on it at all, while constraining against the
/// active output's (camera, zoom) viewport would.
#[test]
fn popup_on_pinned_parent_constrains_against_pin_output() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [200, 150]
"#,
    ));
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    output_state(&out2).zoom = 2.0;
    // Pin binds to whichever output is active when the window maps.
    f.state().focused_output = Some(out2.clone());

    let id = f.add_client();
    let parent = map_window(&mut f, id, "pin", (200, 150));
    let window = window_by_app_id(&mut f, "pin").unwrap();
    let site = f.state().stage.pin_of(&window).cloned().unwrap();
    assert_eq!(site.output, out2.name(), "precondition: pinned to output 2");
    // Centered on output 2: (1280/2 - 200/2, 720/2 - 150/2) = (540, 285).
    assert_eq!(site.screen_pos, Point::from((540, 285)));

    f.state().focused_output = Some(out1.clone());

    let popup_surface = map_popup_with(
        &mut f,
        id,
        &parent,
        PopupProps {
            offset: (700, 400),
            constraint_adjustment: ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY,
            ..Default::default()
        },
    );

    // Unconstrained popup sits at (600, 350) (offset centered by the default
    // 200×100 size). Output 2's screen rect, window-relative, is
    // (-540, -285, 1280, 720): it clips the popup's right/bottom edges by
    // (60, 15), landing at (540, 335).
    assert_eq!(
        f.client(id).popup(&popup_surface).pending_configure.pos,
        (540, 335),
        "popup must slide against the pin's own output (2) in screen space, not the active output (1)"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}

/// Regression guard: a popup that already fits inside its single output's
/// viewport is left untouched even with slide adjustments enabled.
#[test]
fn popup_inside_single_output_viewport_is_not_slid() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let parent = map_window(&mut f, id, "parent", (400, 300));

    let popup_surface = map_popup_with(
        &mut f,
        id,
        &parent,
        PopupProps {
            constraint_adjustment: ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY,
            ..Default::default()
        },
    );

    // Same unconstrained position as the default (no constraint_adjustment)
    // positioner in `overhanging_popup_keeps_parent_hit_testable`.
    assert_eq!(
        f.client(id).popup(&popup_surface).pending_configure.pos,
        (-100, -50),
        "a popup that already fits must not be slid"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}

/// A popup parented to a screen-anchored layer surface constrains against the
/// output whose layer map holds that layer, not the active one. Both halves of
/// the target come from that output: its size, and the layer's arranged
/// position inside it. Resolving to the active output instead finds the layer
/// in no map at all, so its geometry silently defaults to the output origin and
/// the target degrades to the whole (wrong-sized) screen.
#[test]
fn popup_on_layer_parent_constrains_against_the_layer_output() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    // Output 1 keeps the focus it took as the first-connected output.
    assert_eq!(
        f.state().active_output().map(|o| o.name()),
        Some(out1.name()),
        "precondition: the layer's output is not the active one"
    );

    let id = f.add_client();
    f.double_roundtrip(id);
    let out2_wl = f.client(id).output(&out2.name());

    let layer = f
        .client(id)
        .create_layer(Some(&out2_wl), zwlr_layer_shell_v1::Layer::Top, "bar");
    let layer_surface = layer.surface.clone();
    // Anchored bottom-right so the arranged position is nowhere near the
    // origin a missing layer-map lookup would fall back to.
    layer.set_configure_props(super::client::LayerConfigureProps {
        size: Some((200, 150)),
        anchor: Some(zwlr_layer_surface_v1::Anchor::Bottom | zwlr_layer_surface_v1::Anchor::Right),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&layer_surface);
    layer.set_size(200, 150);
    layer.attach_new_buffer();
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    let popup_surface = map_layer_popup_with(
        &mut f,
        id,
        &layer_surface,
        PopupProps {
            offset: (300, 0),
            constraint_adjustment: ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY,
            ..Default::default()
        },
    );

    // The layer arranges to (1080, 570) on output 2, so the target — output 2's
    // 1280×720 screen in layer-relative coords — is (-1080, -570, 1280, 720).
    // The popup starts at (200, -50) and its right edge overshoots x = 200 by
    // 200, sliding to x = 0; its top edge is inside, so y stays. Against output
    // 1 the target would be (0, 0, 1920, 1080): x would not slide and y would,
    // giving (200, 0).
    assert_eq!(
        f.client(id).popup(&popup_surface).pending_configure.pos,
        (0, -50),
        "popup must constrain against the output holding its layer parent"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}

/// A popup parented to a canvas-layer widget constrains against the output
/// whose viewport shows that widget — the widget lives at a canvas position, so
/// its output comes from the camera, not from any layer map. Output 2 shows it
/// while output 1 is active; constraining against output 1, whose viewport
/// straddles the canvas origin thousands of pixels away, would drag the popup
/// to (-4738, -4783) instead.
#[test]
fn popup_on_canvas_layer_widget_constrains_against_its_output() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "widget"
position = [5600, -5300]
"#,
    ));
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    output_state(&out2).camera = Point::from((5000.0, 5000.0));
    f.state().focused_output = Some(out1.clone());

    let id = f.add_client();
    let layer = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Top, "widget");
    let layer_surface = layer.surface.clone();
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

    // The rule centers the 200×150 widget at (5600, 5300), well inside output
    // 2's (5000, 5000, 1282, 722) viewport and far outside output 1's.
    assert_eq!(
        f.state().canvas_layers[0].position,
        Some(Point::from((5500, 5225))),
        "precondition: the widget sits only inside output 2's viewport"
    );

    let popup_surface = map_layer_popup_with(
        &mut f,
        id,
        &layer_surface,
        PopupProps {
            offset: (800, 0),
            constraint_adjustment: ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY,
            ..Default::default()
        },
    );

    // Widget-relative, output 2's viewport is (-500, -225, 1282, 722). The
    // popup starts at (700, -50) and its right edge overshoots x = 782 by 118,
    // sliding to x = 582; vertically it already fits.
    assert_eq!(
        f.client(id).popup(&popup_surface).pending_configure.pos,
        (582, -50),
        "popup must constrain against the output showing its canvas-layer parent"
    );

    f.client(id).popup(&popup_surface).destroy();
    f.double_roundtrip(id);
}
