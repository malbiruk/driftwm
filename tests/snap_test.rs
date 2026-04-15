use driftwm::snap::*;

fn rect_h(x_low: f64, x_high: f64) -> SnapRect {
    SnapRect { x_low, x_high, y_low: -10000.0, y_high: 10000.0 }
}

fn params_h<'a>(extent: f64, others: &'a [SnapRect], gap: f64, threshold: f64) -> SnapParams<'a> {
    SnapParams {
        extent, perp_low: -10000.0, perp_high: 10000.0, horizontal: true,
        others, gap, threshold, break_force: 32.0, same_edge: false,
    }
}

#[test]
fn snap_right_edge_to_left_edge() {
    let others = vec![rect_h(310.0, 510.0)];
    let p = params_h(200.0, &others, 8.0, 16.0);
    let result = find_snap_candidate(100.0, &p);
    assert!(result.is_some());
    let (origin, _dist) = result.unwrap();
    assert!((origin - 102.0).abs() < 0.001);
}

#[test]
fn snap_left_edge_to_right_edge() {
    let others = vec![rect_h(200.0, 492.0)];
    let p = params_h(200.0, &others, 8.0, 16.0);
    let result = find_snap_candidate(500.0, &p);
    assert!(result.is_some());
    let (origin, _dist) = result.unwrap();
    assert!((origin - 500.0).abs() < 0.001);
}

#[test]
fn no_snap_when_too_far() {
    let others = vec![rect_h(500.0, 700.0)];
    let p = params_h(200.0, &others, 8.0, 16.0);
    let result = find_snap_candidate(100.0, &p);
    assert!(result.is_none());
}

#[test]
fn picks_closest_candidate() {
    let others = vec![
        rect_h(310.0, 510.0),
        rect_h(305.0, 505.0),
    ];
    let p = params_h(200.0, &others, 8.0, 16.0);
    let result = find_snap_candidate(100.0, &p);
    assert!(result.is_some());
    let (origin, _) = result.unwrap();
    assert!((origin - 97.0).abs() < 0.001);
}

#[test]
fn snap_break_and_cooldown() {
    let mut snap: Option<AxisSnap> = None;
    let mut cooldown: Option<f64> = None;
    let others = vec![rect_h(308.0, 508.0)];
    let p = SnapParams {
        extent: 200.0,
        perp_low: 0.0,
        perp_high: 100.0,
        horizontal: true,
        others: &others,
        gap: 8.0,
        threshold: 16.0,
        break_force: 32.0,
        same_edge: false,
    };

    let pos = update_axis(&mut snap, &mut cooldown, 100.0, &p);
    assert!(snap.is_some());
    assert!((pos - 100.0).abs() < 0.001);

    let pos = update_axis(&mut snap, &mut cooldown, 110.0, &p);
    assert!(snap.is_some());
    assert!((pos - 100.0).abs() < 0.001);

    let pos = update_axis(&mut snap, &mut cooldown, 140.0, &p);
    assert!(snap.is_none());
    assert!(cooldown.is_some());
    assert!((pos - 140.0).abs() < 0.001);

    let pos = update_axis(&mut snap, &mut cooldown, 105.0, &p);
    assert!(snap.is_none());
    assert!(cooldown.is_some());
    assert!((pos - 105.0).abs() < 0.001);

    let _pos = update_axis(&mut snap, &mut cooldown, 200.0, &p);
    assert!(cooldown.is_none());

    let pos = update_axis(&mut snap, &mut cooldown, 100.0, &p);
    assert!(snap.is_some());
    assert!((pos - 100.0).abs() < 0.001);
}

#[test]
fn snap_from_inside_does_not_immediately_break() {
    let mut snap: Option<AxisSnap> = None;
    let mut cooldown: Option<f64> = None;
    let others = vec![rect_h(0.0, 500.0)];
    let p = SnapParams {
        extent: 200.0,
        perp_low: 0.0,
        perp_high: 100.0,
        horizontal: true,
        others: &others,
        gap: 12.0,
        threshold: 24.0,
        break_force: 32.0,
        same_edge: false,
    };

    let pos = update_axis(&mut snap, &mut cooldown, 480.0, &p);
    assert!(snap.is_some(), "should engage");
    assert!((pos - 512.0).abs() < 0.001);

    let pos = update_axis(&mut snap, &mut cooldown, 500.0, &p);
    assert!(snap.is_some(), "should stay snapped moving toward snap");
    assert!((pos - 512.0).abs() < 0.001);

    let pos = update_axis(&mut snap, &mut cooldown, 440.0, &p);
    assert!(snap.is_none(), "should break on retreat past engage point");
    assert!((pos - 440.0).abs() < 0.001);
}

#[test]
fn no_snap_without_perpendicular_overlap() {
    let others = vec![SnapRect { x_low: 310.0, x_high: 510.0, y_low: 1000.0, y_high: 1200.0 }];
    let p = SnapParams {
        extent: 200.0, perp_low: 0.0, perp_high: 100.0, horizontal: true,
        others: &others, gap: 8.0, threshold: 16.0, break_force: 32.0, same_edge: false,
    };
    let result = find_snap_candidate(100.0, &p);
    assert!(result.is_none(), "should not snap to window with no Y overlap");
}

#[test]
fn no_snap_when_perp_edges_only_touch() {
    // perp_high (100) exactly meets other.y_low (100) — zero shared length.
    // Strict overlap (post-tightening) rejects this: edges meeting at a
    // point is not overlap, so the corresponding axis won't snap.
    let others = vec![SnapRect { x_low: 310.0, x_high: 510.0, y_low: 100.0, y_high: 300.0 }];
    let p = SnapParams {
        extent: 200.0, perp_low: 0.0, perp_high: 100.0, horizontal: true,
        others: &others, gap: 8.0, threshold: 16.0, break_force: 32.0, same_edge: false,
    };
    let result = find_snap_candidate(100.0, &p);
    assert!(
        result.is_none(),
        "exact perpendicular edge-touch should not count as overlap",
    );
}

#[test]
fn no_snap_perpendicular_gap_exceeds_tolerance() {
    let others = vec![SnapRect { x_low: 310.0, x_high: 510.0, y_low: 200.0, y_high: 400.0 }];
    let p = SnapParams {
        extent: 200.0, perp_low: 0.0, perp_high: 100.0, horizontal: true,
        others: &others, gap: 8.0, threshold: 16.0, break_force: 32.0, same_edge: false,
    };
    let result = find_snap_candidate(100.0, &p);
    assert!(result.is_none(), "should not snap when perp gap exceeds threshold");
}

#[test]
fn y_axis_snap_filters_by_x_overlap() {
    let others = vec![
        SnapRect { x_low: 0.0, x_high: 300.0, y_low: 310.0, y_high: 510.0 },
        SnapRect { x_low: 5000.0, x_high: 5300.0, y_low: 310.0, y_high: 510.0 },
    ];
    let p = SnapParams {
        extent: 200.0, perp_low: 0.0, perp_high: 300.0, horizontal: false,
        others: &others, gap: 8.0, threshold: 16.0, break_force: 32.0, same_edge: false,
    };
    let result = find_snap_candidate(100.0, &p);
    assert!(result.is_some(), "should snap to Y-nearby window with X overlap");
    let (origin, _) = result.unwrap();
    assert!((origin - 102.0).abs() < 0.001);

    let far_only = vec![
        SnapRect { x_low: 5000.0, x_high: 5300.0, y_low: 310.0, y_high: 510.0 },
    ];
    let p2 = SnapParams {
        extent: 200.0, perp_low: 0.0, perp_high: 300.0, horizontal: false,
        others: &far_only, gap: 8.0, threshold: 16.0, break_force: 32.0, same_edge: false,
    };
    let result = find_snap_candidate(100.0, &p2);
    assert!(result.is_none(), "should not snap when only far window exists");
}
