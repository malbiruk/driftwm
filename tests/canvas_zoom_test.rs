use driftwm::canvas::{
    MIN_ZOOM_FLOOR, all_windows_bbox, is_origin_visible, snap_zoom, visible_canvas_rect,
    zoom_anchor_camera, zoom_to_fit,
};
use smithay::utils::{Logical, Point, Rectangle, Size};

// --- is_origin_visible tests ---

fn vp(w: i32, h: i32) -> Size<i32, Logical> {
    Size::from((w, h))
}

fn cam(x: f64, y: f64) -> Point<f64, Logical> {
    Point::from((x, y))
}

#[test]
fn origin_visible_when_camera_centers_on_origin() {
    // camera = (-960, -540) → viewport spans [-960..960, -540..540] — origin is inside
    assert!(is_origin_visible(cam(-960.0, -540.0), vp(1920, 1080), 1.0));
}

#[test]
fn origin_visible_at_camera_zero() {
    // camera = (0, 0) → viewport spans [0..1920, 0..1080] — origin is at top-left corner
    assert!(is_origin_visible(cam(0.0, 0.0), vp(1920, 1080), 1.0));
}

#[test]
fn origin_not_visible_when_scrolled_far_right() {
    // camera = (500, 0) → viewport spans [500..2420, 0..1080] — origin is left of viewport
    assert!(!is_origin_visible(cam(500.0, 0.0), vp(1920, 1080), 1.0));
}

#[test]
fn origin_not_visible_when_scrolled_far_left() {
    // camera = (-2000, 0) → viewport spans [-2000..-80, 0..1080] — origin is right of viewport
    assert!(!is_origin_visible(cam(-2000.0, 0.0), vp(1920, 1080), 1.0));
}

#[test]
fn origin_not_visible_when_scrolled_far_down() {
    // camera = (0, 500) → viewport spans [0..1920, 500..1580] — origin is above viewport
    assert!(!is_origin_visible(cam(0.0, 500.0), vp(1920, 1080), 1.0));
}

#[test]
fn origin_visible_at_bottom_right_edge() {
    // camera = (-1920, -1080) → viewport spans [-1920..0, -1080..0] — origin at exact corner
    assert!(is_origin_visible(
        cam(-1920.0, -1080.0),
        vp(1920, 1080),
        1.0
    ));
}

#[test]
fn origin_not_visible_just_past_edge() {
    // camera = (-1920.1, -1080) → viewport x spans [-1920.1..-0.1] — origin at x=0 is outside
    assert!(!is_origin_visible(
        cam(-1920.1, -1080.0),
        vp(1920, 1080),
        1.0
    ));
}

#[test]
fn origin_visible_with_zoom_half_extends_visible_area() {
    // camera = (-2000, 0), viewport 1920x1080, zoom 0.5
    // visible_w = 1920/0.5 = 3840 → viewport spans [-2000..1840] — origin at 0 is inside
    assert!(is_origin_visible(cam(-2000.0, 0.0), vp(1920, 1080), 0.5));
}

#[test]
fn origin_not_visible_at_zoom_half_when_too_far() {
    // camera = (-4000, 0), viewport 1920x1080, zoom 0.5
    // visible_w = 3840 → viewport spans [-4000..-160] — origin at 0 is outside
    assert!(!is_origin_visible(cam(-4000.0, 0.0), vp(1920, 1080), 0.5));
}

// --- zoom_anchor_camera tests ---

#[test]
fn zoom_anchor_preserves_canvas_point() {
    // Canvas point (500, 300) is at screen (100, 50) at some zoom
    // After changing zoom, the canvas point should still map to the same screen pos
    let anchor_canvas = Point::<f64, Logical>::from((500.0, 300.0));
    let anchor_screen = Point::<f64, Logical>::from((100.0, 50.0));
    let new_zoom = 0.5;

    let new_camera = zoom_anchor_camera(anchor_canvas, anchor_screen, new_zoom);

    // Verify: screen = (canvas - camera) * zoom
    let verify_x = (anchor_canvas.x - new_camera.x) * new_zoom;
    let verify_y = (anchor_canvas.y - new_camera.y) * new_zoom;
    assert!((verify_x - anchor_screen.x).abs() < 1e-10);
    assert!((verify_y - anchor_screen.y).abs() < 1e-10);
}

#[test]
fn zoom_anchor_at_origin() {
    let anchor_canvas = Point::<f64, Logical>::from((0.0, 0.0));
    let anchor_screen = Point::<f64, Logical>::from((960.0, 540.0));
    let new_zoom = 0.5;

    let new_camera = zoom_anchor_camera(anchor_canvas, anchor_screen, new_zoom);
    // camera = 0 - 960/0.5 = -1920
    assert!((new_camera.x - (-1920.0)).abs() < 1e-10);
    assert!((new_camera.y - (-1080.0)).abs() < 1e-10);
}

#[test]
fn zoom_anchor_at_zoom_one() {
    let anchor_canvas = Point::<f64, Logical>::from((100.0, 200.0));
    let anchor_screen = Point::<f64, Logical>::from((50.0, 100.0));
    let camera = zoom_anchor_camera(anchor_canvas, anchor_screen, 1.0);
    // camera = canvas - screen/1.0 = canvas - screen
    assert!((camera.x - 50.0).abs() < 1e-10);
    assert!((camera.y - 100.0).abs() < 1e-10);
}

// --- snap_zoom tests ---

#[test]
fn snap_zoom_within_dead_zone() {
    assert_eq!(snap_zoom(0.96), 1.0);
    assert_eq!(snap_zoom(1.04), 1.0);
    assert_eq!(snap_zoom(0.951), 1.0);
    assert_eq!(snap_zoom(1.049), 1.0);
}

#[test]
fn snap_zoom_outside_dead_zone() {
    assert_eq!(snap_zoom(0.94), 0.94);
    assert_eq!(snap_zoom(1.06), 1.06);
    assert_eq!(snap_zoom(0.5), 0.5);
    assert_eq!(snap_zoom(0.05), 0.05);
}

#[test]
fn snap_zoom_exactly_at_boundary() {
    // 0.95 is within ±0.05
    assert_eq!(snap_zoom(0.95), 0.95); // |0.95 - 1.0| = 0.05, NOT < 0.05
    assert_eq!(snap_zoom(1.05), 1.05); // |1.05 - 1.0| = 0.05, NOT < 0.05
}

// --- zoom_to_fit tests ---

#[test]
fn zoom_to_fit_single_small_window() {
    // 200x200 window in 1920x1080 viewport, padding 100
    // padded = 400x400 → zoom_x = 1920/400 = 4.8, zoom_y = 1080/400 = 2.7
    // min = 2.7, clamped to MAX_ZOOM (1.0)
    let bbox = Rectangle::new((0, 0).into(), (200, 200).into());
    let viewport = Size::<i32, Logical>::from((1920, 1080));
    assert_eq!(zoom_to_fit(bbox, viewport, 100.0), 1.0);
}

#[test]
fn zoom_to_fit_windows_wider_than_viewport() {
    // 4000x200 bbox in 1920x1080, padding 100
    // padded = 4200x400 → zoom_x = 1920/4200 ≈ 0.457, zoom_y = 1080/400 = 2.7
    // min = 0.457
    let bbox = Rectangle::new((0, 0).into(), (4000, 200).into());
    let viewport = Size::<i32, Logical>::from((1920, 1080));
    let z = zoom_to_fit(bbox, viewport, 100.0);
    assert!((z - 1920.0 / 4200.0).abs() < 1e-10);
}

#[test]
fn zoom_to_fit_clamps_to_min_zoom() {
    // Enormous bbox — zoom_to_fit now goes as low as needed (only floor clamp)
    let bbox = Rectangle::new((0, 0).into(), (100000, 100000).into());
    let viewport = Size::<i32, Logical>::from((1920, 1080));
    let z = zoom_to_fit(bbox, viewport, 100.0);
    // 1080 / 100200 ≈ 0.01078
    assert!(z > MIN_ZOOM_FLOOR);
    assert!(z < 0.02);
}

#[test]
fn zoom_to_fit_spread_windows() {
    // 3000x2000 bbox in 1920x1080, padding 100
    // padded = 3200x2200 → zoom_x = 0.6, zoom_y ≈ 0.49
    let bbox = Rectangle::new((-500, -500).into(), (3000, 2000).into());
    let viewport = Size::<i32, Logical>::from((1920, 1080));
    let z = zoom_to_fit(bbox, viewport, 100.0);
    let expected = (1080.0 / 2200.0_f64).min(1920.0 / 3200.0);
    assert!((z - expected).abs() < 1e-10);
}

// --- visible_canvas_rect tests ---

#[test]
fn visible_canvas_rect_at_zoom_one() {
    let camera = Point::<i32, Logical>::from((100, 200));
    let viewport = Size::<i32, Logical>::from((1920, 1080));
    let rect = visible_canvas_rect(camera, viewport, 1.0);
    // loc = camera, size = ceil(viewport/zoom) + 2
    assert_eq!(rect.loc.x, 100);
    assert_eq!(rect.loc.y, 200);
    assert_eq!(rect.size.w, 1922);
    assert_eq!(rect.size.h, 1082);
}

#[test]
fn visible_canvas_rect_at_zoom_half() {
    let camera = Point::<i32, Logical>::from((0, 0));
    let viewport = Size::<i32, Logical>::from((1920, 1080));
    let rect = visible_canvas_rect(camera, viewport, 0.5);
    // visible_w = 1920/0.5 = 3840, +2 = 3842
    assert_eq!(rect.loc.x, 0);
    assert_eq!(rect.loc.y, 0);
    assert_eq!(rect.size.w, 3842);
    assert_eq!(rect.size.h, 2162);
}

// --- all_windows_bbox tests ---

#[test]
fn all_windows_bbox_empty() {
    let result = all_windows_bbox(std::iter::empty());
    assert!(result.is_none());
}

#[test]
fn all_windows_bbox_single_window() {
    let windows = vec![(
        Point::<i32, Logical>::from((100, 200)),
        Size::<i32, Logical>::from((300, 400)),
    )];
    let bbox = all_windows_bbox(windows.into_iter()).unwrap();
    assert_eq!(bbox.loc.x, 100);
    assert_eq!(bbox.loc.y, 200);
    assert_eq!(bbox.size.w, 300);
    assert_eq!(bbox.size.h, 400);
}

#[test]
fn all_windows_bbox_multiple_windows() {
    let windows = vec![
        (
            Point::<i32, Logical>::from((-100, -200)),
            Size::<i32, Logical>::from((200, 200)),
        ),
        (
            Point::<i32, Logical>::from((500, 300)),
            Size::<i32, Logical>::from((100, 100)),
        ),
    ];
    let bbox = all_windows_bbox(windows.into_iter()).unwrap();
    // min_x=-100, min_y=-200, max_x=600, max_y=400
    assert_eq!(bbox.loc.x, -100);
    assert_eq!(bbox.loc.y, -200);
    assert_eq!(bbox.size.w, 700);
    assert_eq!(bbox.size.h, 600);
}
