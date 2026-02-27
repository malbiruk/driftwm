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

/// Compute the camera position that centers a window in the viewport.
/// At zoom < 1.0, the viewport covers more canvas area, so the "center"
/// shifts outward: viewport_center_canvas = viewport_size / (2 * zoom).
pub fn camera_to_center_window(
    window_loc: Point<i32, Logical>,
    window_size: Size<i32, Logical>,
    viewport_size: Size<i32, Logical>,
    zoom: f64,
) -> Point<f64, Logical> {
    let window_center_x = window_loc.x as f64 + window_size.w as f64 / 2.0;
    let window_center_y = window_loc.y as f64 + window_size.h as f64 / 2.0;
    let viewport_center_x = viewport_size.w as f64 / (2.0 * zoom);
    let viewport_center_y = viewport_size.h as f64 / (2.0 * zoom);
    Point::from((
        window_center_x - viewport_center_x,
        window_center_y - viewport_center_y,
    ))
}

/// Is any part of a rectangle visible in the current viewport?
pub fn is_rect_visible(
    rect_loc: Point<i32, Logical>,
    rect_size: Size<i32, Logical>,
    camera: Point<f64, Logical>,
    viewport_size: Size<i32, Logical>,
    zoom: f64,
) -> bool {
    let vw = viewport_size.w as f64 / zoom;
    let vh = viewport_size.h as f64 / zoom;
    (rect_loc.x as f64) < camera.x + vw
        && (rect_loc.x + rect_size.w) as f64 > camera.x
        && (rect_loc.y as f64) < camera.y + vh
        && (rect_loc.y + rect_size.h) as f64 > camera.y
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
    camera.x <= 0.0
        && 0.0 <= camera.x + visible_w
        && camera.y <= 0.0
        && 0.0 <= camera.y + visible_h
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
    let bbox = all_windows_bbox(windows).unwrap_or_else(|| {
        Rectangle::new((-2, -2).into(), (5, 5).into())
    });
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
    if (z - 1.0).abs() < 0.05 {
        1.0
    } else {
        z
    }
}

/// Find the nearest item in a 90° cone from `origin` in the given direction.
///
/// Uses dot/cross product against the direction unit vector: a candidate is
/// in the cone when `dot > 0 && |cross| <= dot` (i.e. within ±45° of the
/// direction). Among candidates, picks the nearest by Euclidean distance.
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
            let dist_sq = dx * dx + dy * dy;
            if best.as_ref().is_none_or(|(_, d)| dist_sq < *d) {
                best = Some((item, dist_sq));
            }
        }
    }

    best.map(|(w, _)| w)
}

/// Scroll momentum physics: velocity decays by friction each frame.
/// Uses EMA (exponential moving average) for accumulation to smooth
/// out jittery trackpad deltas.
pub struct MomentumState {
    pub velocity: Point<f64, Logical>,
    pub friction: f64,
    /// Stop when |velocity|^2 < threshold_sq (default 0.25 = 0.5 px/frame)
    pub threshold_sq: f64,
    /// Frame number of the last scroll event. Prevents double-counting
    /// camera movement on frames where a scroll event fired.
    pub last_scroll_frame: u64,
}

impl MomentumState {
    pub fn new(friction: f64) -> Self {
        Self {
            velocity: Point::from((0.0, 0.0)),
            friction,
            threshold_sq: 0.25,
            last_scroll_frame: 0,
        }
    }

    /// EMA accumulate: velocity = velocity * 0.3 + delta * 0.7
    pub fn accumulate(&mut self, delta: Point<f64, Logical>, frame: u64) {
        self.velocity = Point::from((
            self.velocity.x * 0.3 + delta.x * 0.7,
            self.velocity.y * 0.3 + delta.y * 0.7,
        ));
        self.last_scroll_frame = frame;
    }

    /// Returns Some(delta) to apply, or None if skipped/finished.
    pub fn tick(&mut self, current_frame: u64) -> Option<Point<f64, Logical>> {
        // Skip when a scroll/drag event recently moved the camera.
        // 1-frame grace window: on udev, input fires between renders with the
        // old frame_counter, so an exact match misses by one frame.
        if current_frame.saturating_sub(self.last_scroll_frame) <= 1 {
            return None;
        }
        if self.velocity.x.powi(2) + self.velocity.y.powi(2) < self.threshold_sq {
            self.velocity = Point::from((0.0, 0.0));
            return None;
        }
        let delta = self.velocity;
        self.velocity = Point::from((delta.x * self.friction, delta.y * self.friction));
        Some(delta)
    }

    pub fn stop(&mut self) {
        self.velocity = Point::from((0.0, 0.0));
    }
}
