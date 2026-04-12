use std::collections::VecDeque;
use std::time::{Duration, Instant};

use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::config::Direction;

/// Hard floor for zoom — prevents division by zero / absurd values.
pub const MIN_ZOOM_FLOOR: f64 = 0.001;
/// Maximum zoom level (100% — native resolution, no magnification).
pub const MAX_ZOOM: f64 = 1.0;

/// A position in screen-local coordinates (0,0 = top-left of the output).
#[derive(Debug, Clone, Copy)]
pub struct ScreenPos(pub Point<f64, Logical>);

/// A position in infinite canvas coordinates (absolute world position).
#[derive(Debug, Clone, Copy)]
pub struct CanvasPos(pub Point<f64, Logical>);

/// screen_pos = (canvas_pos - camera) * zoom  ⟹  canvas = screen / zoom + camera
#[inline]
pub fn screen_to_canvas(screen: ScreenPos, camera: Point<f64, Logical>, zoom: f64) -> CanvasPos {
    CanvasPos(Point::from((
        screen.0.x / zoom + camera.x,
        screen.0.y / zoom + camera.y,
    )))
}

/// canvas_pos → screen_pos = (canvas - camera) * zoom
#[inline]
pub fn canvas_to_screen(canvas: CanvasPos, camera: Point<f64, Logical>, zoom: f64) -> ScreenPos {
    ScreenPos(Point::from((
        (canvas.0.x - camera.x) * zoom,
        (canvas.0.y - camera.y) * zoom,
    )))
}

/// Compute the camera position that centers a window at `screen_center` on screen.
/// `screen_center` is the screen-space point where the window center should appear
/// (typically the usable area center, accounting for panel exclusive zones).
pub fn camera_to_center_window(
    window_loc: Point<i32, Logical>,
    window_size: Size<i32, Logical>,
    screen_center: Point<f64, Logical>,
    zoom: f64,
    bar: i32,
) -> Point<f64, Logical> {
    let window_center_x = window_loc.x as f64 + window_size.w as f64 / 2.0;
    let bar_f = bar as f64;
    let window_center_y = window_loc.y as f64 - bar_f + (window_size.h as f64 + bar_f) / 2.0;
    Point::from((
        window_center_x - screen_center.x / zoom,
        window_center_y - screen_center.y / zoom,
    ))
}

/// Fraction of a rectangle's area visible in the current viewport (0.0–1.0).
/// Returns 0.0 for zero-area rectangles.
pub fn visible_fraction(
    rect_loc: Point<i32, Logical>,
    rect_size: Size<i32, Logical>,
    camera: Point<f64, Logical>,
    viewport_size: Size<i32, Logical>,
    zoom: f64,
) -> f64 {
    let area = rect_size.w as f64 * rect_size.h as f64;
    if area <= 0.0 {
        return 0.0;
    }

    let vw = viewport_size.w as f64 / zoom;
    let vh = viewport_size.h as f64 / zoom;

    let ix_min = (rect_loc.x as f64).max(camera.x);
    let ix_max = ((rect_loc.x + rect_size.w) as f64).min(camera.x + vw);
    let iy_min = (rect_loc.y as f64).max(camera.y);
    let iy_max = ((rect_loc.y + rect_size.h) as f64).min(camera.y + vh);

    let iw = (ix_max - ix_min).max(0.0);
    let ih = (iy_max - iy_min).max(0.0);

    (iw * ih) / area
}

/// Check whether the canvas origin (0, 0) is visible in the current viewport.
/// At zoom < 1.0, the visible area is larger: viewport_size / zoom.
pub fn is_origin_visible(
    camera: Point<f64, Logical>,
    viewport_size: Size<i32, Logical>,
    zoom: f64,
) -> bool {
    let visible_w = viewport_size.w as f64 / zoom;
    let visible_h = viewport_size.h as f64 / zoom;
    camera.x <= 0.0 && 0.0 <= camera.x + visible_w && camera.y <= 0.0 && 0.0 <= camera.y + visible_h
}

/// The canvas rectangle visible at the current camera + zoom.
/// Used to cull windows outside the viewport for `render_elements_for_region`.
///
/// `camera_i32` must be `camera.to_i32_round()` — the same rounding used by
/// `update_output_from_camera` — so that element position offsets match the
/// output mapping used for input hit-testing.
pub fn visible_canvas_rect(
    camera_i32: Point<i32, Logical>,
    viewport_size: Size<i32, Logical>,
    zoom: f64,
) -> Rectangle<i32, Logical> {
    let w = (viewport_size.w as f64 / zoom).ceil() as i32 + 2;
    let h = (viewport_size.h as f64 / zoom).ceil() as i32 + 2;
    Rectangle::new(camera_i32, (w, h).into())
}

/// Bounding box of all windows. Returns None if the iterator is empty.
pub fn all_windows_bbox(
    windows: impl Iterator<Item = (Point<i32, Logical>, Size<i32, Logical>)>,
) -> Option<Rectangle<i32, Logical>> {
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    let mut any = false;

    for (loc, size) in windows {
        any = true;
        min_x = min_x.min(loc.x);
        min_y = min_y.min(loc.y);
        max_x = max_x.max(loc.x + size.w);
        max_y = max_y.max(loc.y + size.h);
    }

    if any {
        Some(Rectangle::new(
            (min_x, min_y).into(),
            (max_x - min_x, max_y - min_y).into(),
        ))
    } else {
        None
    }
}

/// Zoom level that fits `bbox` inside `viewport` with `padding` pixels on each side.
/// Clamped to [MIN_ZOOM_FLOOR, MAX_ZOOM] — zooms out as far as needed to fit.
pub fn zoom_to_fit(
    bbox: Rectangle<i32, Logical>,
    viewport_size: Size<i32, Logical>,
    padding: f64,
) -> f64 {
    let padded_w = bbox.size.w as f64 + padding * 2.0;
    let padded_h = bbox.size.h as f64 + padding * 2.0;
    let zoom_x = viewport_size.w as f64 / padded_w;
    let zoom_y = viewport_size.h as f64 / padded_h;
    zoom_x.min(zoom_y).clamp(MIN_ZOOM_FLOOR, MAX_ZOOM)
}

/// Dynamic minimum zoom based on the current window layout.
/// Uses a virtual 5x5 window at the origin as baseline when no windows exist,
/// so the limit stays consistent as the first window appears.
pub fn dynamic_min_zoom(
    windows: impl Iterator<Item = (Point<i32, Logical>, Size<i32, Logical>)>,
    viewport_size: Size<i32, Logical>,
    padding: f64,
) -> f64 {
    let bbox =
        all_windows_bbox(windows).unwrap_or_else(|| Rectangle::new((-2, -2).into(), (5, 5).into()));
    // Allow zooming out to 50% beyond the fit zoom for breathing room
    let fit = zoom_to_fit(bbox, viewport_size, padding);
    (fit * 0.5).max(MIN_ZOOM_FLOOR)
}

/// Camera position that keeps `anchor_canvas` at `anchor_screen` after a zoom change.
/// Derived from: screen = (canvas - camera) * zoom  ⟹  camera = canvas - screen / zoom.
pub fn zoom_anchor_camera(
    anchor_canvas: Point<f64, Logical>,
    anchor_screen: Point<f64, Logical>,
    new_zoom: f64,
) -> Point<f64, Logical> {
    Point::from((
        anchor_canvas.x - anchor_screen.x / new_zoom,
        anchor_canvas.y - anchor_screen.y / new_zoom,
    ))
}

/// Snap zoom to 1.0 if within ±0.05 dead zone (avoids stuck-near-1.0 feel).
pub fn snap_zoom(z: f64) -> f64 {
    if (z - 1.0).abs() < 0.05 { 1.0 } else { z }
}

/// Closest point on an axis-aligned rect to `origin`.
/// If origin is inside the rect, returns origin itself (distance 0).
pub fn closest_point_on_rect(
    origin: Point<f64, Logical>,
    loc: Point<i32, Logical>,
    size: Size<i32, Logical>,
) -> Point<f64, Logical> {
    Point::from((
        origin.x.clamp(loc.x as f64, (loc.x + size.w) as f64),
        origin.y.clamp(loc.y as f64, (loc.y + size.h) as f64),
    ))
}

/// Find the nearest item in a 90° cone from `origin` in the given direction.
///
/// Uses dot/cross product against the direction unit vector: a candidate is
/// in the cone when `dot > 0 && |cross| <= dot` (i.e. within ±45° of the
/// direction). Scores by `distance / cos(angle)` — targets aligned with the
/// exact direction are preferred even if further away.
///
/// Generic over the item type so it works with `Window` in production and
/// simple types (e.g. `&str`) in tests.
pub fn find_nearest<W: PartialEq>(
    origin: Point<f64, Logical>,
    dir: &Direction,
    items: impl Iterator<Item = (W, Point<f64, Logical>)>,
    skip: Option<&W>,
) -> Option<W> {
    let (ux, uy) = dir.to_unit_vec();
    let mut best: Option<(W, f64)> = None;

    for (item, center) in items {
        if skip.is_some_and(|s| s == &item) {
            continue;
        }
        let dx = center.x - origin.x;
        let dy = center.y - origin.y;
        let dot = dx * ux + dy * uy;
        let cross = (dx * uy - dy * ux).abs();
        if dot > 0.0 && cross <= dot {
            // score = dist² / dot ∝ dist / cos(angle), avoids sqrt
            let dist_sq = dx * dx + dy * dy;
            let score = dist_sq / dot;
            if best.as_ref().is_none_or(|(_, d)| score < *d) {
                best = Some((item, score));
            }
        }
    }

    best.map(|(w, _)| w)
}

/// Sliding-window velocity tracker for scroll/gesture input.
/// Computes launch velocity from recent displacement over a fixed time window,
/// avoiding the EMA bias where the last 1-2 events dominate.
#[derive(Clone, Default)]
pub struct VelocityTracker {
    samples: VecDeque<(Instant, Point<f64, Logical>)>,
}

const VELOCITY_WINDOW: Duration = Duration::from_millis(80);

impl VelocityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, now: Instant, delta: Point<f64, Logical>) {
        self.samples.push_back((now, delta));
        let cutoff = now - VELOCITY_WINDOW;
        while self.samples.front().is_some_and(|(t, _)| *t < cutoff) {
            self.samples.pop_front();
        }
    }

    /// Total displacement / elapsed time = px/sec. Zero if < 2 samples.
    pub fn launch_velocity(&self) -> Point<f64, Logical> {
        if self.samples.len() < 2 {
            return Point::from((0.0, 0.0));
        }
        let first_time = self.samples.front().unwrap().0;
        let last_time = self.samples.back().unwrap().0;
        let elapsed = (last_time - first_time).as_secs_f64();
        if elapsed < 1e-6 {
            return Point::from((0.0, 0.0));
        }
        let total: Point<f64, Logical> = self
            .samples
            .iter()
            .fold(Point::from((0.0, 0.0)), |acc, (_, d)| {
                Point::from((acc.x + d.x, acc.y + d.y))
            });
        Point::from((total.x / elapsed, total.y / elapsed))
    }

    pub fn last_sample_time(&self) -> Option<Instant> {
        self.samples.back().map(|(t, _)| *t)
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

/// Stop threshold in px/sec (15 px/sec ≈ 0.25 px/frame at 60Hz)
const MOMENTUM_STOP_THRESHOLD: f64 = 15.0;

/// Scroll momentum physics with time-based friction.
/// Velocity is in px/sec; friction is applied via `powf(dt * 60)` for
/// frame-rate independence.
#[derive(Clone)]
pub struct MomentumState {
    pub velocity: Point<f64, Logical>,
    pub tracker: VelocityTracker,
    pub friction: f64,
    pub coasting: bool,
}

impl MomentumState {
    pub fn new(friction: f64) -> Self {
        Self {
            velocity: Point::from((0.0, 0.0)),
            tracker: VelocityTracker::new(),
            friction,
            coasting: false,
        }
    }

    /// Record an input delta. Resets coasting — we're receiving live input.
    pub fn accumulate(&mut self, delta: Point<f64, Logical>, now: Instant) {
        self.tracker.push(now, delta);
        self.coasting = false;
    }

    /// Snapshot launch velocity from the tracker and begin coasting.
    pub fn launch(&mut self) {
        self.velocity = self.tracker.launch_velocity();
        self.coasting = true;
        self.tracker.clear();
    }

    /// Advance momentum by `dt`. Returns Some(canvas delta) to apply, or None.
    pub fn tick(&mut self, dt: Duration) -> Option<Point<f64, Logical>> {
        if !self.coasting {
            return None;
        }
        let speed = (self.velocity.x.powi(2) + self.velocity.y.powi(2)).sqrt();
        if speed < MOMENTUM_STOP_THRESHOLD {
            self.velocity = Point::from((0.0, 0.0));
            self.coasting = false;
            return None;
        }

        let dt_secs = dt.as_secs_f64();

        // Speed-dependent friction: gentle scrolls stop quickly, fast flings coast longer
        let effective_friction = speed_dependent_friction(self.friction, speed);
        let decay = effective_friction.powf(dt_secs * 60.0);
        let delta = Point::from((self.velocity.x * dt_secs, self.velocity.y * dt_secs));
        self.velocity = Point::from((self.velocity.x * decay, self.velocity.y * decay));
        Some(delta)
    }

    pub fn stop(&mut self) {
        self.velocity = Point::from((0.0, 0.0));
        self.tracker.clear();
        self.coasting = false;
    }
}

/// Derive effective per-frame friction from the config value and current speed.
/// Low speeds get more friction (quick stop), high speeds get less (long coast).
fn speed_dependent_friction(friction: f64, speed: f64) -> f64 {
    let low_friction = (friction - 0.06).clamp(0.80, 0.95);
    let high_friction = (friction + 0.025).clamp(0.95, 0.995);
    let reference_speed = 2500.0; // px/sec
    let t = (speed / reference_speed).min(1.0);
    low_friction + t * (high_friction - low_friction)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(x: f64, y: f64) -> Point<f64, Logical> {
        Point::from((x, y))
    }
    fn vp(w: i32, h: i32) -> Size<i32, Logical> {
        Size::from((w, h))
    }
    /// Screen center point for a viewport of given size (no panels).
    fn vp_center(w: i32, h: i32) -> Point<f64, Logical> {
        Point::from((w as f64 / 2.0, h as f64 / 2.0))
    }

    #[test]
    fn fully_visible() {
        // 100x100 window at (200, 200), camera at (0,0), viewport 1000x1000, zoom 1.0
        let f = visible_fraction(
            (200, 200).into(),
            (100, 100).into(),
            cam(0.0, 0.0),
            vp(1000, 1000),
            1.0,
        );
        assert!((f - 1.0).abs() < 1e-9);
    }

    #[test]
    fn fully_off_screen() {
        // Window completely to the right of viewport
        let f = visible_fraction(
            (2000, 0).into(),
            (100, 100).into(),
            cam(0.0, 0.0),
            vp(1000, 1000),
            1.0,
        );
        assert!((f - 0.0).abs() < 1e-9);
    }

    #[test]
    fn half_off_right_edge() {
        // 100x100 window, right half off-screen
        let f = visible_fraction(
            (950, 0).into(),
            (100, 100).into(),
            cam(0.0, 0.0),
            vp(1000, 1000),
            1.0,
        );
        assert!((f - 0.5).abs() < 1e-9);
    }

    #[test]
    fn zero_area_window() {
        let f = visible_fraction(
            (0, 0).into(),
            (0, 100).into(),
            cam(0.0, 0.0),
            vp(1000, 1000),
            1.0,
        );
        assert!((f - 0.0).abs() < 1e-9);
    }

    #[test]
    fn zoom_affects_viewport() {
        // At zoom 0.5, viewport covers 2000x2000 canvas units.
        // 100x100 window at (1500, 0) is fully visible.
        let f = visible_fraction(
            (1500, 0).into(),
            (100, 100).into(),
            cam(0.0, 0.0),
            vp(1000, 1000),
            0.5,
        );
        assert!((f - 1.0).abs() < 1e-9);

        // Same window at zoom 1.0 is fully off-screen.
        let f = visible_fraction(
            (1500, 0).into(),
            (100, 100).into(),
            cam(0.0, 0.0),
            vp(1000, 1000),
            1.0,
        );
        assert!((f - 0.0).abs() < 1e-9);
    }

    // -- Coordinate transform round-trip tests --

    #[test]
    fn screen_canvas_round_trip_zoom_1() {
        let camera = cam(100.0, 200.0);
        let original = ScreenPos(Point::from((400.0, 300.0)));
        let canvas = screen_to_canvas(original, camera, 1.0);
        let back = canvas_to_screen(canvas, camera, 1.0);
        assert!((back.0.x - original.0.x).abs() < 1e-9);
        assert!((back.0.y - original.0.y).abs() < 1e-9);
    }

    #[test]
    fn screen_canvas_round_trip_zoomed_out() {
        let camera = cam(-500.0, -300.0);
        let zoom = 0.25;
        let original = ScreenPos(Point::from((640.0, 480.0)));
        let canvas = screen_to_canvas(original, camera, zoom);
        let back = canvas_to_screen(canvas, camera, zoom);
        assert!((back.0.x - original.0.x).abs() < 1e-9);
        assert!((back.0.y - original.0.y).abs() < 1e-9);
    }

    #[test]
    fn screen_to_canvas_math() {
        // screen = (canvas - camera) * zoom  ⟹  canvas = screen / zoom + camera
        let canvas = screen_to_canvas(ScreenPos(Point::from((100.0, 50.0))), cam(10.0, 20.0), 0.5);
        // 100/0.5 + 10 = 210, 50/0.5 + 20 = 120
        assert!((canvas.0.x - 210.0).abs() < 1e-9);
        assert!((canvas.0.y - 120.0).abs() < 1e-9);
    }

    #[test]
    fn canvas_to_screen_math() {
        // screen = (canvas - camera) * zoom
        let screen = canvas_to_screen(CanvasPos(Point::from((210.0, 120.0))), cam(10.0, 20.0), 0.5);
        // (210 - 10) * 0.5 = 100, (120 - 20) * 0.5 = 50
        assert!((screen.0.x - 100.0).abs() < 1e-9);
        assert!((screen.0.y - 50.0).abs() < 1e-9);
    }

    // -- camera_to_center_window tests --

    #[test]
    fn center_window_zoom_1() {
        // 200x100 window at (300, 400), 1920x1080 viewport, zoom 1.0
        let cam = camera_to_center_window(
            (300, 400).into(),
            (200, 100).into(),
            vp_center(1920, 1080),
            1.0,
            0,
        );
        // window center: (400, 450), viewport center offset: (960, 540)
        assert!((cam.x - (400.0 - 960.0)).abs() < 1e-9);
        assert!((cam.y - (450.0 - 540.0)).abs() < 1e-9);
    }

    #[test]
    fn center_window_zoomed_out() {
        // At zoom 0.5, viewport center = viewport_size / (2 * 0.5) = viewport_size
        let cam = camera_to_center_window(
            (0, 0).into(),
            (100, 100).into(),
            vp_center(1000, 1000),
            0.5,
            0,
        );
        // window center: (50, 50), viewport center offset at 0.5: (1000, 1000)
        assert!((cam.x - (50.0 - 1000.0)).abs() < 1e-9);
        assert!((cam.y - (50.0 - 1000.0)).abs() < 1e-9);
    }

    // -- find_nearest tests --

    fn pt(x: f64, y: f64) -> Point<f64, Logical> {
        Point::from((x, y))
    }

    #[test]
    fn find_nearest_right() {
        let origin = pt(0.0, 0.0);
        let items = vec![
            ("a", pt(100.0, 0.0)),  // directly right
            ("b", pt(-100.0, 0.0)), // directly left
            ("c", pt(200.0, 0.0)),  // further right
        ];
        let result = find_nearest(origin, &Direction::Right, items.into_iter(), None::<&&str>);
        assert_eq!(result, Some("a"));
    }

    #[test]
    fn find_nearest_up() {
        let origin = pt(0.0, 0.0);
        let items = vec![("above", pt(0.0, -100.0)), ("below", pt(0.0, 100.0))];
        let result = find_nearest(origin, &Direction::Up, items.into_iter(), None::<&&str>);
        assert_eq!(result, Some("above"));
    }

    #[test]
    fn find_nearest_down() {
        let origin = pt(0.0, 0.0);
        let items = vec![("above", pt(0.0, -100.0)), ("below", pt(0.0, 100.0))];
        let result = find_nearest(origin, &Direction::Down, items.into_iter(), None::<&&str>);
        assert_eq!(result, Some("below"));
    }

    #[test]
    fn find_nearest_left() {
        let origin = pt(0.0, 0.0);
        let items = vec![("left", pt(-100.0, 0.0)), ("right", pt(100.0, 0.0))];
        let result = find_nearest(origin, &Direction::Left, items.into_iter(), None::<&&str>);
        assert_eq!(result, Some("left"));
    }

    #[test]
    fn find_nearest_outside_cone() {
        // Item at 60° from the right axis — outside the 45° cone
        let origin = pt(0.0, 0.0);
        let items = vec![("diagonal", pt(50.0, 100.0))];
        let result = find_nearest(origin, &Direction::Right, items.into_iter(), None::<&&str>);
        assert_eq!(result, None);
    }

    #[test]
    fn find_nearest_skips_self() {
        let origin = pt(0.0, 0.0);
        let items = vec![("self", pt(10.0, 0.0)), ("other", pt(20.0, 0.0))];
        let result = find_nearest(origin, &Direction::Right, items.into_iter(), Some(&"self"));
        assert_eq!(result, Some("other"));
    }

    #[test]
    fn find_nearest_empty() {
        let origin = pt(0.0, 0.0);
        let items: Vec<(&str, Point<f64, Logical>)> = vec![];
        let result = find_nearest(origin, &Direction::Right, items.into_iter(), None::<&&str>);
        assert_eq!(result, None);
    }
}
