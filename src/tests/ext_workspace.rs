//! `ext-workspace-v1`: driftwm exports its bookmark registry to bars as
//! workspaces — one group (the whole canvas), one workspace per bookmark, and a
//! single `active` bit tracking the focused viewport's nearest visible bookmark.
//! These scenarios drive a real client through the protocol and assert the
//! behavior a bar observes. The per-frame incumbent recompute + protocol diff
//! (`refresh_ext_workspaces`) runs from the render loops in production, which the
//! headless test server doesn't drive, so each scenario calls it explicitly
//! where a frame would. The pure active-bookmark math has its own unit tests in
//! `src/canvas.rs`; here it's exercised end to end through `OutputState`.

use smithay::utils::Point;
use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::{
    ExtWorkspaceHandleV1, State,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::ExtWorkspaceManagerV1;

use super::client::ClientId;
use super::{Fixture, config};

/// Run the per-frame ext-workspace refresh the render loops drive: recompute
/// every output's incumbent bookmark and diff the registry into protocol events.
fn refresh(f: &mut Fixture) {
    crate::render::refresh_ext_workspaces(f.state());
}

/// Replace the config's default bookmarks with a controlled set, then refresh so
/// the protocol sees them. Points are the user-facing Y-up `[x, y]` convention.
fn seed(f: &mut Fixture, entries: &[(&str, f64, f64)]) {
    f.state().bookmarks.clear();
    for &(name, x, y) in entries {
        f.state().bookmarks.insert(name.to_string(), [x, y]);
    }
    refresh(f);
}

/// Bind an ext-workspace client and settle its initial burst.
fn connect(f: &mut Fixture) -> ClientId {
    let id = f.add_client();
    f.double_roundtrip(id);
    id
}

fn manager(f: &mut Fixture, id: ClientId) -> ExtWorkspaceManagerV1 {
    f.client(id).state.ext_workspace.manager.clone().unwrap()
}

fn handle(f: &mut Fixture, id: ClientId, name: &str) -> ExtWorkspaceHandleV1 {
    f.client(id)
        .state
        .ext_workspace
        .workspace(name)
        .unwrap_or_else(|| panic!("client has no workspace named '{name}'"))
        .handle
        .clone()
}

/// Point a specific output's viewport at internal (Y-down) canvas coords.
fn aim_output(output: &smithay::output::Output, camera: (f64, f64)) {
    crate::state::output_state(output).camera = Point::from(camera);
}

#[test]
fn bind_hydrates_group_and_a_workspace_per_bookmark() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    seed(&mut f, &[("home", 0.0, 0.0), ("far", 5000.0, -5000.0)]);

    let id = connect(&mut f);
    let ws = &f.client(id).state.ext_workspace;

    // One group for the whole canvas, advertising create_workspace.
    assert!(ws.group.is_some(), "a workspace group is advertised");
    // A workspace per bookmark, named after it, id == name (BTreeMap order).
    assert_eq!(ws.names(), vec!["far".to_string(), "home".to_string()]);
    assert_eq!(ws.workspace("home").unwrap().id.as_deref(), Some("home"));
    assert_eq!(ws.workspace("far").unwrap().id.as_deref(), Some("far"));
    // Each workspace joined the group.
    assert!(ws.entered("home") && ws.entered("far"));
    // The hydration burst ends in exactly one atomic done.
    assert_eq!(ws.done_count, 1);
}

#[test]
fn registry_insert_and_remove_reach_the_client() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    seed(&mut f, &[("a", 0.0, 0.0)]);

    let id = connect(&mut f);
    assert_eq!(
        f.client(id).state.ext_workspace.names(),
        vec!["a".to_string()]
    );

    // A bookmark inserted into the registry surfaces as a new workspace.
    f.state().bookmarks.insert("b".into(), [100.0, 0.0]);
    refresh(&mut f);
    f.double_roundtrip(id);
    assert_eq!(
        f.client(id).state.ext_workspace.names(),
        vec!["a".to_string(), "b".to_string()]
    );

    // Removing it from the registry removes the workspace.
    f.state().bookmarks.remove("a");
    refresh(&mut f);
    f.double_roundtrip(id);
    assert_eq!(
        f.client(id).state.ext_workspace.names(),
        vec!["b".to_string()]
    );
}

#[test]
fn activate_then_commit_jumps_the_camera_to_the_bookmark() {
    let mut f = Fixture::new();
    let out = f.add_output(1, (1920, 1080));
    seed(&mut f, &[("dest", 500.0, -300.0)]);
    let id = connect(&mut f);

    assert!(
        f.state().camera_target().is_none(),
        "no camera target before any activation"
    );

    handle(&mut f, id, "dest").activate();
    manager(&mut f, id).commit();
    f.double_roundtrip(id);

    let target = f
        .state()
        .camera_target()
        .expect("activate + commit sets a camera target");
    // go-to-bookmark centers the usable area on the bookmark — the same math
    // set-bookmark inverts (no panels here, so usable center == viewport center).
    let viewport = crate::state::output_logical_size(&out);
    let expected = driftwm::canvas::camera_for_center(500.0, -300.0, f.state().zoom(), viewport);
    assert!((target.x - expected.x).abs() < 1e-6 && (target.y - expected.y).abs() < 1e-6);
}

#[test]
fn requests_have_no_effect_until_commit() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    seed(&mut f, &[("dest", 500.0, -300.0)]);
    let id = connect(&mut f);

    // The protocol double-buffers: activate accumulates but must not apply
    // until commit.
    handle(&mut f, id, "dest").activate();
    f.double_roundtrip(id);
    assert!(
        f.state().camera_target().is_none(),
        "activate must not move the camera before commit"
    );

    manager(&mut f, id).commit();
    f.double_roundtrip(id);
    assert!(
        f.state().camera_target().is_some(),
        "commit applies the buffered activate"
    );
}

#[test]
fn create_workspace_then_commit_bookmarks_the_viewport_center() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    seed(&mut f, &[]);
    let id = connect(&mut f);

    let group = f.client(id).state.ext_workspace.group.clone().unwrap();
    group.create_workspace("new".into());
    manager(&mut f, id).commit();
    f.double_roundtrip(id);

    // set-bookmark captures the focused viewport's usable center; the default
    // camera centers the canvas origin, so the new bookmark lands there.
    assert_eq!(f.state().bookmarks["new"], [0.0, 0.0]);

    // …and the created bookmark round-trips back out as a workspace.
    refresh(&mut f);
    f.double_roundtrip(id);
    assert_eq!(
        f.client(id).state.ext_workspace.names(),
        vec!["new".to_string()]
    );
}

#[test]
fn create_workspace_with_empty_name_is_ignored() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    seed(&mut f, &[]);
    let id = connect(&mut f);

    let group = f.client(id).state.ext_workspace.group.clone().unwrap();
    group.create_workspace(String::new());
    manager(&mut f, id).commit();
    f.double_roundtrip(id);

    assert!(
        f.state().bookmarks.is_empty(),
        "an empty workspace name creates no bookmark"
    );
    refresh(&mut f);
    f.double_roundtrip(id);
    assert!(f.client(id).state.ext_workspace.names().is_empty());
}

#[test]
fn remove_then_commit_deletes_the_bookmark() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    seed(&mut f, &[("gone", 0.0, 0.0)]);
    let id = connect(&mut f);
    assert!(f.state().bookmarks.contains_key("gone"));

    handle(&mut f, id, "gone").remove();
    manager(&mut f, id).commit();
    f.double_roundtrip(id);

    assert!(
        !f.state().bookmarks.contains_key("gone"),
        "remove + commit deletes the bookmark from the registry"
    );
    refresh(&mut f);
    f.double_roundtrip(id);
    assert!(f.client(id).state.ext_workspace.names().is_empty());
}

#[test]
fn active_state_marks_the_bookmark_under_the_viewport() {
    let mut f = Fixture::new();
    let out = f.add_output(1, (1920, 1080));
    seed(&mut f, &[("spot", 0.0, 0.0)]);
    let id = connect(&mut f);

    // The default camera centers the canvas origin, where "spot" sits.
    assert_eq!(f.client(id).state.ext_workspace.active(), Some("spot"));

    // Pan far away so no bookmark is in view: the active bit clears.
    aim_output(&out, (100_000.0, 100_000.0));
    refresh(&mut f);
    f.double_roundtrip(id);
    assert_eq!(
        f.client(id).state.ext_workspace.active(),
        None,
        "no visible bookmark means no active workspace"
    );
}

#[test]
fn active_bit_holds_incumbent_through_hysteresis_then_yields() {
    let mut f = Fixture::new();
    let out = f.add_output(1, (1920, 1080));
    // Y-up y = 0 → internal y = 0; usable center y = 540, so camera.y = -540
    // keeps the target on the bookmark row. "a" and "b" straddle it 1000 apart.
    seed(&mut f, &[("a", 500.0, 0.0), ("b", 1500.0, 0.0)]);
    let id = connect(&mut f);

    // Target internal (500, 0): only "a" is visible → it wins.
    aim_output(&out, (-460.0, -540.0));
    refresh(&mut f);
    f.double_roundtrip(id);
    assert_eq!(f.client(id).state.ext_workspace.active(), Some("a"));

    // Target internal (1000, 0): both visible, equidistant — incumbent holds.
    aim_output(&out, (40.0, -540.0));
    refresh(&mut f);
    f.double_roundtrip(id);
    assert_eq!(
        f.client(id).state.ext_workspace.active(),
        Some("a"),
        "a near-tie must not steal the active bit from the incumbent"
    );

    // Target internal (1050, 0): "b" is >10% closer (450 vs 550) — it takes over.
    aim_output(&out, (90.0, -540.0));
    refresh(&mut f);
    f.double_roundtrip(id);
    assert_eq!(
        f.client(id).state.ext_workspace.active(),
        Some("b"),
        "a decisively closer rival takes the active bit"
    );
}

#[test]
fn each_output_keeps_its_own_incumbent_and_active_follows_focus() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1920, 1080));
    // "left" at internal (0,0), "right" at internal (2000,0).
    seed(&mut f, &[("left", 0.0, 0.0), ("right", 2000.0, 0.0)]);

    // out1 sees only "left"; out2 sees only "right".
    aim_output(&out1, (-960.0, -540.0));
    aim_output(&out2, (1040.0, -540.0));
    let id = connect(&mut f);
    refresh(&mut f);
    f.double_roundtrip(id);

    // Per-output incumbents are independent, visible over IPC.
    let info = crate::ipc::state_info(f.state());
    let out_active = |name: &str| {
        info.outputs
            .iter()
            .find(|o| o.name == name)
            .unwrap()
            .active_bookmark
            .clone()
    };
    assert_eq!(out_active("HEADLESS-1"), Some("left".to_string()));
    assert_eq!(out_active("HEADLESS-2"), Some("right".to_string()));

    // The protocol projects only the focused output's incumbent.
    f.state().focused_output = Some(out1.clone());
    refresh(&mut f);
    f.double_roundtrip(id);
    assert_eq!(f.client(id).state.ext_workspace.active(), Some("left"));

    f.state().focused_output = Some(out2.clone());
    refresh(&mut f);
    f.double_roundtrip(id);
    assert_eq!(f.client(id).state.ext_workspace.active(), Some("right"));
}

#[test]
fn ipc_reports_active_bookmark_top_level_and_per_output() {
    let mut f = Fixture::with_config(config("[navigation.bookmarks]\n\"spot\" = [0, 0]\n"));
    let out = f.add_output(1, (1920, 1080));
    seed(&mut f, &[("spot", 0.0, 0.0)]);

    let info = crate::ipc::state_info(f.state());
    assert_eq!(info.active_bookmark, Some("spot".to_string()));
    assert_eq!(
        info.outputs[0].active_bookmark,
        Some("spot".to_string()),
        "the sole output reports its own incumbent too"
    );

    // Away from every bookmark, both the top-level and per-output field clear.
    aim_output(&out, (100_000.0, 100_000.0));
    refresh(&mut f);
    let info = crate::ipc::state_info(f.state());
    assert_eq!(info.active_bookmark, None);
    assert_eq!(info.outputs[0].active_bookmark, None);
}

#[test]
fn group_output_enter_and_leave_track_the_live_outputs() {
    let mut f = Fixture::new();
    let _out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    seed(&mut f, &[]);
    let id = connect(&mut f);

    // The manager global predates every wl_output, and the client binds outputs
    // only after the manager, so the bind burst can't carry the enters — the
    // per-frame refresh reconciles them once the client has bound the outputs.
    refresh(&mut f);
    f.double_roundtrip(id);
    assert_eq!(
        f.client(id).state.ext_workspace.output_enters.len(),
        2,
        "the group advertises both live outputs via output_enter"
    );

    // Disconnecting one output retracts it. The leave is sent before the
    // wl_output global is torn down, so the client's proxy is still valid.
    f.remove_output(&out2);
    f.double_roundtrip(id);
    assert_eq!(
        f.client(id).state.ext_workspace.output_leaves.len(),
        1,
        "the group retracts the disconnected output"
    );
}

#[test]
fn removal_leaves_the_group_before_removing_the_workspace() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    seed(&mut f, &[("gone", 0.0, 0.0)]);
    let id = connect(&mut f);
    let h = handle(&mut f, id, "gone");
    assert!(
        f.client(id).state.ext_workspace.entered("gone"),
        "the workspace joined the group"
    );

    f.state().bookmarks.remove("gone");
    refresh(&mut f);
    f.double_roundtrip(id);

    let ws = &f.client(id).state.ext_workspace;
    // A workspace may only be removed once it belongs to no group, so the group
    // must emit workspace_leave before the workspace is removed.
    assert!(
        ws.workspace_leaves.contains(&h),
        "the workspace left the group"
    );
    let record = ws.workspaces.iter().find(|w| w.handle == h).unwrap();
    assert!(record.removed, "and was then removed");
}

#[test]
fn every_workspace_gets_an_initial_state_event() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // "here" sits at the origin (under the default camera → active); "far" is
    // far off-screen (inactive).
    seed(&mut f, &[("here", 0.0, 0.0), ("far", 100_000.0, 0.0)]);
    let id = connect(&mut f);

    let empty = State::empty().bits();
    let active = State::Active.bits();
    {
        // Bind hydration sends state for every workspace — empty for inactive
        // ones (omission would wrongly read as active-cleared, not "unset").
        let ws = &f.client(id).state.ext_workspace;
        assert_eq!(ws.workspace("far").unwrap().state, Some(empty));
        assert_eq!(ws.workspace("here").unwrap().state, Some(active));
    }

    // A bookmark created later gets its initial state through the refresh path.
    f.state().bookmarks.insert("late".into(), [100_000.0, 0.0]);
    refresh(&mut f);
    f.double_roundtrip(id);
    assert_eq!(
        f.client(id)
            .state
            .ext_workspace
            .workspace("late")
            .unwrap()
            .state,
        Some(empty),
    );
}

#[test]
fn destroying_the_group_keeps_workspaces_and_requests_flowing() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    seed(&mut f, &[("dest", 500.0, -300.0)]);
    let id = connect(&mut f);

    // The client drops the group handle but keeps the manager and workspaces.
    let group = f.client(id).state.ext_workspace.group.clone().unwrap();
    group.destroy();
    f.double_roundtrip(id);

    // A registry insert still surfaces as a workspace, under a done barrier —
    // only the group-scoped events are suppressed.
    let done_before = f.client(id).state.ext_workspace.done_count;
    f.state().bookmarks.insert("added".into(), [0.0, 0.0]);
    refresh(&mut f);
    f.double_roundtrip(id);
    let ws = &f.client(id).state.ext_workspace;
    assert!(ws.names().contains(&"added".to_string()));
    assert!(
        ws.done_count > done_before,
        "changes still arrive under a done barrier"
    );

    // And a buffered activate still applies on commit through the live manager.
    handle(&mut f, id, "dest").activate();
    manager(&mut f, id).commit();
    f.double_roundtrip(id);
    assert!(
        f.state().camera_target().is_some(),
        "activate + commit still jumps the camera after the group is gone"
    );
}

#[test]
fn manager_stop_sends_finished() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    seed(&mut f, &[]);
    let id = connect(&mut f);

    manager(&mut f, id).stop();
    f.double_roundtrip(id);
    assert!(
        f.client(id).state.ext_workspace.finished,
        "stop elicits a finished event"
    );
}

#[test]
fn two_clients_both_receive_registry_changes() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    seed(&mut f, &[("a", 0.0, 0.0)]);
    let id1 = connect(&mut f);
    let id2 = connect(&mut f);
    assert_eq!(
        f.client(id1).state.ext_workspace.names(),
        vec!["a".to_string()]
    );
    assert_eq!(
        f.client(id2).state.ext_workspace.names(),
        vec!["a".to_string()]
    );

    f.state().bookmarks.insert("b".into(), [100.0, 0.0]);
    refresh(&mut f);
    f.double_roundtrip(id1);
    f.double_roundtrip(id2);
    let both = vec!["a".to_string(), "b".to_string()];
    assert_eq!(f.client(id1).state.ext_workspace.names(), both);
    assert_eq!(f.client(id2).state.ext_workspace.names(), both);
}

#[test]
fn incremental_change_arrives_under_a_done_barrier() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    seed(&mut f, &[("a", 0.0, 0.0)]);
    let id = connect(&mut f);
    let done_before = f.client(id).state.ext_workspace.done_count;

    f.state().bookmarks.insert("b".into(), [100.0, 0.0]);
    refresh(&mut f);
    f.double_roundtrip(id);
    assert!(
        f.client(id).state.ext_workspace.done_count > done_before,
        "the insert arrives atomically under a new done"
    );
}
