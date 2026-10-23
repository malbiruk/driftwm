//! Zone navigation: `Super+1..4` frames a connected snap cluster when
//! `navigation.workspace_source = "zones"`, and keeps the historical bookmark
//! fallback otherwise.

use driftwm::config::{Action, WorkspaceSource};
use smithay::utils::{Logical, Point};

use crate::state::StageWindow;

use super::{Fixture, config, map_window, settle, window_by_app_id};

/// Two adjacent windows (one zone) and one far-away window (a second zone).
fn zone_fixture() -> Fixture {
    let mut f = Fixture::with_config(config("[navigation]\nworkspace_source = \"zones\"\n"));
    f.add_output(1, (1920, 1080));
    // GoToWorkspace moves the camera, which seeds a blur generation that only
    // clears on output disconnect, so it cannot return to the pre-output
    // baseline.
    f.skip_baseline_check();

    let id = f.add_client();
    let gap = f.state().config.snap_gap as i32;
    map_window(&mut f, id, "left", (400, 300));
    map_window(&mut f, id, "right", (400, 300));
    map_window(&mut f, id, "far", (400, 300));

    let left = window_by_app_id(&mut f, "left").expect("left window");
    let right = window_by_app_id(&mut f, "right").expect("right window");
    let far = window_by_app_id(&mut f, "far").expect("far window");
    f.state()
        .map_window(StageWindow::Client(left), Point::from((0, 0)), false);
    f.state().map_window(
        StageWindow::Client(right),
        Point::from((400 + gap, 0)),
        false,
    );
    f.state()
        .map_window(StageWindow::Client(far), Point::from((4000, 0)), false);
    settle(&mut f);
    f
}

fn zone_center(zone: &crate::state::zones::Zone) -> Point<f64, Logical> {
    Point::from((
        zone.bbox.loc.x as f64 + zone.bbox.size.w as f64 / 2.0,
        zone.bbox.loc.y as f64 + zone.bbox.size.h as f64 / 2.0,
    ))
}

#[test]
fn go_to_workspace_frames_the_second_zone() {
    let mut f = zone_fixture();
    assert_eq!(f.state().config.workspace_source, WorkspaceSource::Zones);

    let zones = f.state().zones();
    assert_eq!(zones.len(), 2, "two disconnected snap clusters");
    assert_eq!(zones[0].name, "1");
    assert_eq!(zones[1].name, "2");
    let expected = zone_center(&zones[1]);

    f.state().execute_action(&Action::GoToWorkspace(2));
    settle(&mut f);

    let center = f.state().viewport_center_canvas();
    assert!(
        (center.x - expected.x).abs() < 0.5 && (center.y - expected.y).abs() < 0.5,
        "Super+2 should frame zone 2 at {expected:?}, got {center:?}"
    );
}

#[test]
fn go_to_workspace_out_of_range_is_a_noop() {
    let mut f = zone_fixture();
    let before = f.state().camera_target();

    f.state().execute_action(&Action::GoToWorkspace(9));

    assert_eq!(f.state().camera_target(), before);
}

#[test]
fn go_to_workspace_falls_back_to_a_bookmark_in_bookmarks_mode() {
    let mut f = Fixture::new();
    let out = f.add_output(1, (1920, 1080));
    assert_eq!(
        f.state().config.workspace_source,
        WorkspaceSource::Bookmarks
    );
    // The default Super+2 bookmark. A distinct point makes a no-op obvious.
    f.state().bookmarks.insert("2".into(), [500.0, -300.0]);

    f.state().execute_action(&Action::GoToWorkspace(2));

    let target = f
        .state()
        .camera_target()
        .expect("bookmarks mode still jumps to the numeric bookmark");
    let viewport = crate::state::output_logical_size(&out);
    let expected = driftwm::canvas::camera_for_center(500.0, -300.0, f.state().zoom(), viewport);
    assert!((target.x - expected.x).abs() < 1e-6 && (target.y - expected.y).abs() < 1e-6);
}
