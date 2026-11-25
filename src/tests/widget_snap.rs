//! Snapping is scoped to a class. A widget — the element `widget_movable` hands
//! to the pointer — aligns to the widget layer, and a window never aligns to a
//! widget. Cluster membership follows the same split with one narrowing: an
//! immovable widget is a fine thing to snap *to* and a broken thing to be moved
//! by one, so it never joins a cluster.

use std::collections::HashSet;

use driftwm::layout::snap::SnapState;
use smithay::utils::Point;

use crate::state::StageWindow;

use super::{Fixture, config, map_window, window_by_app_id};

/// Camera at the canvas origin, zoom 1: canvas == screen.
fn origin_view(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });
}

/// Two movable widgets, one immovable widget and one ordinary window, each
/// parked where the test names it.
fn row(f: &mut Fixture) -> (StageWindow, StageWindow, StageWindow, StageWindow) {
    let id = f.add_client();
    map_window(f, id, "dock", (200, 100));
    let dock = StageWindow::Client(window_by_app_id(f, "dock").unwrap());
    map_window(f, id, "panel", (200, 100));
    let panel = StageWindow::Client(window_by_app_id(f, "panel").unwrap());
    map_window(f, id, "nailed", (200, 100));
    let nailed = StageWindow::Client(window_by_app_id(f, "nailed").unwrap());
    map_window(f, id, "terminal", (200, 100));
    let terminal = StageWindow::Client(window_by_app_id(f, "terminal").unwrap());
    (dock, panel, nailed, terminal)
}

const RULES: &str = r#"
[[window_rules]]
app_id = "dock"
widget = true
widget_movable = true

[[window_rules]]
app_id = "panel"
widget = true
widget_movable = true

[[window_rules]]
app_id = "nailed"
widget = true

[[window_rules]]
app_id = "terminal"
"#;

/// A movable widget dragged a hair past a widget's edge docks to it; the same
/// hair against an application window docks nowhere. Neither direction crosses
/// the layer: the widget's target list holds no windows, the window's holds no
/// widgets.
#[test]
#[allow(clippy::mutable_key_type)]
fn a_movable_widget_snaps_to_widgets_and_never_to_a_window() {
    let mut f = Fixture::with_config(config(RULES));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let (dock, panel, _nailed, terminal) = row(&mut f);

    // Dock and terminal are the two candidates, far enough apart that a snap to
    // one cannot be mistaken for a snap to the other.
    f.state()
        .map_window(dock.clone(), Point::from((0, 0)), false);
    f.state()
        .map_window(terminal.clone(), Point::from((2000, 0)), false);

    let gap = f.state().config.snap_gap;
    let excludes = HashSet::new();

    let targets = f.state().snap_rects_for_layer(&panel, false);
    let Some(dock_rect) = targets.iter().find(|(w, _)| w == &dock).map(|(_, r)| *r) else {
        panic!("a widget's snap layer holds the widget beside it");
    };
    assert!(
        !targets.iter().any(|(w, _)| w == &terminal),
        "…and no application window"
    );
    assert!(
        f.state()
            .snap_rects_for_layer(&terminal, false)
            .iter()
            .all(|(w, _)| w != &dock && w != &panel),
        "and a window's snap layer holds no widgets"
    );

    // Three pixels short of docking to the dock's right edge, one gap past it.
    let natural = Point::from((dock_rect.x_high + gap - 3.0, dock_rect.y_low));
    let snapped =
        f.state()
            .snap_move_location(&panel, 1.0, natural, &mut SnapState::default(), &excludes);
    assert!(
        (snapped.x - (dock_rect.x_high + gap)).abs() < 1.0,
        "the panel docked a gap off the dock's right edge ({snapped:?})"
    );

    // The same approach against the terminal leaves it where the pointer left it.
    let terminal_rect = f.state().snap_rect_for(&terminal).unwrap();
    let panel_w = panel.geometry().size.w as f64;
    let natural = Point::from((
        terminal_rect.x_low - gap - panel_w + 3.0,
        terminal_rect.y_low,
    ));
    let snapped =
        f.state()
            .snap_move_location(&panel, 1.0, natural, &mut SnapState::default(), &excludes);
    assert!(
        (snapped.x - natural.x).abs() < 0.5,
        "a widget never docks to an application window ({snapped:?} vs {natural:?})"
    );
}

/// A cluster drag carries the widgets flush against it and no others: an
/// immovable widget stays nailed where it is, and an ordinary window beside the
/// same row is not the dragged widget's layer either. A window's own cluster
/// keeps the mirror rule — it never claims a widget.
#[test]
fn a_widget_cluster_carries_movable_widgets_only() {
    let mut f = Fixture::with_config(config(RULES));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let (dock, panel, nailed, terminal) = row(&mut f);

    // One row at the snap gap — the distance `adjacent_side` calls clustered —
    // in the order: nailed | panel | dock | terminal.
    let gap = f.state().config.snap_gap as i32;
    let slot = |i| Point::from((-200 - gap + i * (200 + gap), 0));
    f.state().map_window(nailed.clone(), slot(0), false);
    f.state().map_window(panel.clone(), slot(1), false);
    f.state().map_window(dock.clone(), slot(2), false);
    f.state().map_window(terminal.clone(), slot(3), false);

    let cluster = f.state().cluster_snapshot_for_drag(&panel, slot(1));
    assert!(
        cluster.iter().any(|(w, _)| w == &dock),
        "the movable widget flush against the drag rides along with it"
    );
    assert!(
        !cluster.iter().any(|(w, _)| w == &nailed),
        "an immovable widget is never carried along by a drag"
    );
    assert!(
        !cluster.iter().any(|(w, _)| w == &terminal),
        "and neither is an application window"
    );

    // The ordinary window, flush against a widget it cannot cluster with, finds
    // nothing of its own layer to move.
    let window_cluster = f.state().cluster_snapshot_for_drag(&terminal, slot(3));
    assert!(
        window_cluster
            .iter()
            .all(|(w, _)| w != &panel && w != &nailed),
        "a window's cluster holds no widgets"
    );
}
