use driftwm::canvas::{camera_to_center_window, find_nearest};
use driftwm::config::Direction;
use smithay::utils::{Logical, Point, Size};
use std::f64::consts::FRAC_1_SQRT_2;

// --- camera_to_center_window tests ---

#[test]
fn camera_to_center_window_standard_window() {
    // Window at (100, 100) size 200x200, viewport 1920x1080
    // window center = (200, 200), viewport center = (960, 540)
    // expected camera = (200 - 960, 200 - 540) = (-760, -340)
    let loc = Point::<i32, Logical>::from((100, 100));
    let win_size = Size::<i32, Logical>::from((200, 200));
    let vp_size = Size::<i32, Logical>::from((1920, 1080));
    let camera = camera_to_center_window(loc, win_size, vp_size, 1.0);
    assert!((camera.x - (-760.0)).abs() < 1e-10, "camera.x should be -760, got {}", camera.x);
    assert!((camera.y - (-340.0)).abs() < 1e-10, "camera.y should be -340, got {}", camera.y);
}

#[test]
fn camera_to_center_window_small_viewport() {
    // Window at (0, 0) size 100x100, viewport 800x600
    // window center = (50, 50), viewport center = (400, 300)
    // expected camera = (50 - 400, 50 - 300) = (-350, -250)
    let loc = Point::<i32, Logical>::from((0, 0));
    let win_size = Size::<i32, Logical>::from((100, 100));
    let vp_size = Size::<i32, Logical>::from((800, 600));
    let camera = camera_to_center_window(loc, win_size, vp_size, 1.0);
    assert!((camera.x - (-350.0)).abs() < 1e-10, "camera.x should be -350, got {}", camera.x);
    assert!((camera.y - (-250.0)).abs() < 1e-10, "camera.y should be -250, got {}", camera.y);
}

#[test]
fn camera_to_center_window_far_offset_window() {
    // Window at (1000, 2000) size 400x300, viewport 1920x1080
    // window center = (1200, 2150), viewport center = (960, 540)
    // expected camera = (1200 - 960, 2150 - 540) = (240, 1610)
    let loc = Point::<i32, Logical>::from((1000, 2000));
    let win_size = Size::<i32, Logical>::from((400, 300));
    let vp_size = Size::<i32, Logical>::from((1920, 1080));
    let camera = camera_to_center_window(loc, win_size, vp_size, 1.0);
    assert!((camera.x - 240.0).abs() < 1e-10, "camera.x should be 240, got {}", camera.x);
    assert!((camera.y - 1610.0).abs() < 1e-10, "camera.y should be 1610, got {}", camera.y);
}

#[test]
fn camera_to_center_window_already_centered_returns_zero() {
    // Window at (860, 440) size 200x200, viewport 1920x1080
    // window center = (960, 540) = viewport center
    // expected camera = (0, 0)
    let loc = Point::<i32, Logical>::from((860, 440));
    let win_size = Size::<i32, Logical>::from((200, 200));
    let vp_size = Size::<i32, Logical>::from((1920, 1080));
    let camera = camera_to_center_window(loc, win_size, vp_size, 1.0);
    assert!((camera.x).abs() < 1e-10, "camera.x should be 0 for already-centered window, got {}", camera.x);
    assert!((camera.y).abs() < 1e-10, "camera.y should be 0 for already-centered window, got {}", camera.y);
}

#[test]
fn camera_to_center_window_with_zoom_half() {
    // At zoom=0.5, viewport center in canvas = viewport_size / (2 * 0.5) = viewport_size
    // Window at (0, 0) size 100x100, viewport 1920x1080, zoom 0.5
    // window center = (50, 50), viewport center canvas = (1920, 1080)
    // expected camera = (50 - 1920, 50 - 1080) = (-1870, -1030)
    let loc = Point::<i32, Logical>::from((0, 0));
    let win_size = Size::<i32, Logical>::from((100, 100));
    let vp_size = Size::<i32, Logical>::from((1920, 1080));
    let camera = camera_to_center_window(loc, win_size, vp_size, 0.5);
    assert!((camera.x - (-1870.0)).abs() < 1e-10, "camera.x should be -1870, got {}", camera.x);
    assert!((camera.y - (-1030.0)).abs() < 1e-10, "camera.y should be -1030, got {}", camera.y);
}

// --- Direction::to_unit_vec tests ---

#[test]
fn direction_up_unit_vec() {
    let (x, y) = Direction::Up.to_unit_vec();
    assert_eq!(x, 0.0, "Up x component should be 0");
    assert_eq!(y, -1.0, "Up y component should be -1");
}

#[test]
fn direction_down_unit_vec() {
    let (x, y) = Direction::Down.to_unit_vec();
    assert_eq!(x, 0.0, "Down x component should be 0");
    assert_eq!(y, 1.0, "Down y component should be 1");
}

#[test]
fn direction_left_unit_vec() {
    let (x, y) = Direction::Left.to_unit_vec();
    assert_eq!(x, -1.0, "Left x component should be -1");
    assert_eq!(y, 0.0, "Left y component should be 0");
}

#[test]
fn direction_right_unit_vec() {
    let (x, y) = Direction::Right.to_unit_vec();
    assert_eq!(x, 1.0, "Right x component should be 1");
    assert_eq!(y, 0.0, "Right y component should be 0");
}

#[test]
fn direction_upleft_unit_vec() {
    let (x, y) = Direction::UpLeft.to_unit_vec();
    assert!((x - (-FRAC_1_SQRT_2)).abs() < 1e-15, "UpLeft x should be -FRAC_1_SQRT_2, got {x}");
    assert!((y - (-FRAC_1_SQRT_2)).abs() < 1e-15, "UpLeft y should be -FRAC_1_SQRT_2, got {y}");
}

#[test]
fn direction_upright_unit_vec() {
    let (x, y) = Direction::UpRight.to_unit_vec();
    assert!((x - FRAC_1_SQRT_2).abs() < 1e-15, "UpRight x should be FRAC_1_SQRT_2, got {x}");
    assert!((y - (-FRAC_1_SQRT_2)).abs() < 1e-15, "UpRight y should be -FRAC_1_SQRT_2, got {y}");
}

#[test]
fn direction_downleft_unit_vec() {
    let (x, y) = Direction::DownLeft.to_unit_vec();
    assert!((x - (-FRAC_1_SQRT_2)).abs() < 1e-15, "DownLeft x should be -FRAC_1_SQRT_2, got {x}");
    assert!((y - FRAC_1_SQRT_2).abs() < 1e-15, "DownLeft y should be FRAC_1_SQRT_2, got {y}");
}

#[test]
fn direction_downright_unit_vec() {
    let (x, y) = Direction::DownRight.to_unit_vec();
    assert!((x - FRAC_1_SQRT_2).abs() < 1e-15, "DownRight x should be FRAC_1_SQRT_2, got {x}");
    assert!((y - FRAC_1_SQRT_2).abs() < 1e-15, "DownRight y should be FRAC_1_SQRT_2, got {y}");
}

#[test]
fn cardinal_directions_have_one_zero_component() {
    for dir in [Direction::Up, Direction::Down, Direction::Left, Direction::Right] {
        let (x, y) = dir.to_unit_vec();
        assert!(
            x == 0.0 || y == 0.0,
            "cardinal direction {dir:?} should have one zero component, got ({x}, {y})"
        );
    }
}

#[test]
fn diagonal_directions_have_equal_magnitude_components() {
    for dir in [Direction::UpLeft, Direction::UpRight, Direction::DownLeft, Direction::DownRight] {
        let (x, y) = dir.to_unit_vec();
        assert!(
            (x.abs() - y.abs()).abs() < 1e-15,
            "diagonal direction {dir:?} should have equal-magnitude components, got ({x}, {y})"
        );
    }
}

#[test]
fn all_directions_have_unit_magnitude() {
    let directions = [
        Direction::Up,
        Direction::Down,
        Direction::Left,
        Direction::Right,
        Direction::UpLeft,
        Direction::UpRight,
        Direction::DownLeft,
        Direction::DownRight,
    ];
    for dir in &directions {
        let (x, y) = dir.to_unit_vec();
        let magnitude = (x * x + y * y).sqrt();
        assert!(
            (magnitude - 1.0).abs() < 1e-15,
            "direction {dir:?} unit vec magnitude should be 1.0, got {magnitude}"
        );
    }
}

// --- find_nearest cone search tests ---

/// Helper: build items list from (name, x, y) tuples.
fn items<'a>(positions: &'a [(&'a str, f64, f64)]) -> Vec<(&'a str, Point<f64, Logical>)> {
    positions
        .iter()
        .map(|(name, x, y)| (*name, Point::from((*x, *y))))
        .collect()
}

fn origin(x: f64, y: f64) -> Point<f64, Logical> {
    Point::from((x, y))
}

#[test]
fn find_nearest_directly_right() {
    let w = items(&[("a", 200.0, 0.0)]);
    let result = find_nearest(origin(0.0, 0.0), &Direction::Right, w.into_iter(), None);
    assert_eq!(result, Some("a"));
}

#[test]
fn find_nearest_44_degrees_inside_cone() {
    // tan(44°) ≈ 0.9657 — just inside the ±45° cone
    let w = items(&[("a", 100.0, 96.0)]);
    let result = find_nearest(origin(0.0, 0.0), &Direction::Right, w.into_iter(), None);
    assert_eq!(result, Some("a"), "44° should be inside the 90° cone");
}

#[test]
fn find_nearest_46_degrees_outside_cone() {
    // tan(46°) ≈ 1.0355 — just outside the ±45° cone
    let w = items(&[("a", 100.0, 104.0)]);
    let result = find_nearest(origin(0.0, 0.0), &Direction::Right, w.into_iter(), None);
    assert_eq!(result, None, "46° should be outside the 90° cone");
}

#[test]
fn find_nearest_exactly_45_degrees_is_on_boundary() {
    // At exactly 45°, cross == dot, so cross <= dot → included
    let w = items(&[("a", 100.0, 100.0)]);
    let result = find_nearest(origin(0.0, 0.0), &Direction::Right, w.into_iter(), None);
    assert_eq!(result, Some("a"), "exactly 45° (boundary) should be included");
}

#[test]
fn find_nearest_no_window_in_direction() {
    // Window is behind the origin (to the left), searching right
    let w = items(&[("a", -200.0, 0.0)]);
    let result = find_nearest(origin(0.0, 0.0), &Direction::Right, w.into_iter(), None);
    assert_eq!(result, None);
}

#[test]
fn find_nearest_empty_list() {
    let w: Vec<(&str, Point<f64, Logical>)> = vec![];
    let result = find_nearest(origin(0.0, 0.0), &Direction::Right, w.into_iter(), None);
    assert_eq!(result, None);
}

#[test]
fn find_nearest_closest_of_two_candidates_wins() {
    let w = items(&[("far", 500.0, 0.0), ("near", 100.0, 0.0)]);
    let result = find_nearest(origin(0.0, 0.0), &Direction::Right, w.into_iter(), None);
    assert_eq!(result, Some("near"));
}

#[test]
fn find_nearest_skipped_item_is_excluded() {
    let w = items(&[("skip_me", 100.0, 0.0), ("other", 200.0, 0.0)]);
    let result = find_nearest(
        origin(0.0, 0.0),
        &Direction::Right,
        w.into_iter(),
        Some(&"skip_me"),
    );
    assert_eq!(result, Some("other"));
}

#[test]
fn find_nearest_skip_only_candidate_returns_none() {
    let w = items(&[("only", 100.0, 0.0)]);
    let result = find_nearest(
        origin(0.0, 0.0),
        &Direction::Right,
        w.into_iter(),
        Some(&"only"),
    );
    assert_eq!(result, None);
}

#[test]
fn find_nearest_diagonal_direction() {
    // Searching DownRight from origin — window at (100, 100) is directly on that diagonal
    let w = items(&[("a", 100.0, 100.0)]);
    let result = find_nearest(origin(0.0, 0.0), &Direction::DownRight, w.into_iter(), None);
    assert_eq!(result, Some("a"));
}

#[test]
fn find_nearest_diagonal_rejects_perpendicular() {
    // Searching DownRight — window at (-100, 100) is in the DownLeft direction
    let w = items(&[("a", -100.0, 100.0)]);
    let result = find_nearest(origin(0.0, 0.0), &Direction::DownRight, w.into_iter(), None);
    assert_eq!(result, None);
}

#[test]
fn find_nearest_up_direction() {
    // y-axis is inverted (up = negative y)
    let w = items(&[("above", 0.0, -300.0), ("below", 0.0, 300.0)]);
    let result = find_nearest(origin(0.0, 0.0), &Direction::Up, w.into_iter(), None);
    assert_eq!(result, Some("above"));
}

#[test]
fn find_nearest_nonzero_origin() {
    let w = items(&[("a", 600.0, 400.0)]);
    let result = find_nearest(origin(500.0, 400.0), &Direction::Right, w.into_iter(), None);
    assert_eq!(result, Some("a"));
}

#[test]
fn find_nearest_prefers_aligned() {
    // "right_45" at 45° is closer by distance, but "far_right" at 0° wins
    // because it's perfectly aligned with the direction axis.
    let w = items(&[("right_45", 100.0, 100.0), ("far_right", 180.0, 0.0)]);
    let result = find_nearest(origin(0.0, 0.0), &Direction::Right, w.into_iter(), None);
    assert_eq!(result, Some("far_right"));
}
