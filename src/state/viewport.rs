use std::time::Instant;

use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use driftwm::config::FocusPlacement;
use driftwm::stage::StageElement;

use super::{DriftWm, OutputState, StageWindow, output_logical_size, output_state};

impl DriftWm {
    // Per-output field accessors. Getters fall back to a sensible default
    // when no output exists; setters silently no-op. Hotplug/lid-close races
    // briefly leave the compositor with zero outputs — must not panic then.
    pub fn camera(&self) -> Point<f64, Logical> {
        self.active_output()
            .map(|o| output_state(&o).camera)
            .unwrap_or_default()
    }
    pub fn set_camera(&mut self, val: Point<f64, Logical>) {
        if let Some(o) = self.active_output() {
            self.set_camera_on(&o, val);
        }
    }

    /// Move `output`'s camera, honoring the fullscreen lock. Every per-output
    /// animation tick (momentum, edge-pan, zoom, camera) routes through here, so
    /// none can move a fullscreen output's camera.
    ///
    /// Interactive pan/zoom writers do not rely on this lock. `drift_pan_on`,
    /// the touch grab's zoom and the pinch both guard themselves, up front and
    /// per gesture — the pinch bails for its whole arm before either of its
    /// branches runs, so the `set_camera` / `set_zoom` its no-pinned-output
    /// fallback would reach are a dead backstop rather than the check doing the
    /// work. A gesture that *begins* while the output is already fullscreen exits
    /// it first, so the common case reaches no guard at all — but fullscreen
    /// landing mid-gesture inverts that ordering, which is what those local
    /// guards exist for. Do not read this lock as covering them.
    ///
    /// Invariant: a fullscreen window is parked at its output's camera-origin at
    /// zoom 1, so the camera must not move or it slides off (0,0) and re-exposes
    /// black to that output (and other cameras). (`enter_fullscreen` seeds the
    /// origin by writing output_state directly, bypassing this guard.)
    pub fn set_camera_on(&mut self, output: &Output, val: Point<f64, Logical>) {
        if self.is_output_fullscreen(output) {
            return;
        }
        output_state(output).camera = val;
    }
    pub fn zoom(&self) -> f64 {
        // 1.0 default (not 0.0) avoids divide-by-zero in `step / zoom` callers.
        self.active_output()
            .map(|o| output_state(&o).zoom)
            .unwrap_or(1.0)
    }
    /// Zoom the active output, honoring the same fullscreen lock as
    /// [`Self::set_camera_on`]: the parked window fills its output at zoom 1,
    /// so any other zoom shrinks or crops it while the cull still hides
    /// everything behind. `enter_fullscreen` and the exit write output_state
    /// directly to seed and restore the park.
    pub fn set_zoom(&mut self, val: f64) {
        if let Some(o) = self.active_output()
            && !self.is_output_fullscreen(&o)
        {
            output_state(&o).zoom = val;
        }
    }
    pub fn zoom_target(&self) -> Option<f64> {
        self.active_output()
            .and_then(|o| output_state(&o).zoom_target)
    }
    pub fn set_zoom_target(&mut self, val: Option<f64>) {
        if let Some(o) = self.active_output() {
            output_state(&o).zoom_target = val;
        }
    }
    pub fn zoom_animation_anchor(&self) -> Option<super::ZoomAnimationAnchor> {
        self.active_output()
            .and_then(|o| output_state(&o).zoom_animation_anchor)
    }
    pub fn set_zoom_animation_anchor(
        &mut self,
        canvas: Point<f64, Logical>,
        screen: Point<f64, Logical>,
    ) {
        if let Some(o) = self.active_output() {
            output_state(&o).zoom_animation_anchor =
                Some(super::ZoomAnimationAnchor { canvas, screen });
        }
    }
    pub fn clear_zoom_animation_anchor(&mut self) {
        if let Some(o) = self.active_output() {
            output_state(&o).zoom_animation_anchor = None;
        }
    }
    pub fn overview_return(&self) -> Option<(Point<f64, Logical>, f64)> {
        self.active_output()
            .and_then(|o| output_state(&o).overview_return)
    }
    pub fn set_overview_return(&mut self, val: Option<(Point<f64, Logical>, f64)>) {
        if let Some(o) = self.active_output() {
            output_state(&o).overview_return = val;
        }
    }
    pub fn camera_target(&self) -> Option<Point<f64, Logical>> {
        self.active_output()
            .and_then(|o| output_state(&o).camera_target)
    }
    pub fn set_camera_target(&mut self, val: Option<Point<f64, Logical>>) {
        if let Some(o) = self.active_output() {
            output_state(&o).camera_target = val;
        }
    }
    pub fn last_scroll_pan(&self) -> Option<Instant> {
        self.active_output()
            .and_then(|o| output_state(&o).last_scroll_pan)
    }
    pub fn set_last_scroll_pan(&mut self, val: Option<Instant>) {
        if let Some(o) = self.active_output() {
            output_state(&o).last_scroll_pan = val;
        }
    }
    pub fn panning(&self) -> bool {
        self.active_output()
            .is_some_and(|o| output_state(&o).panning)
    }
    pub fn set_panning(&mut self, val: bool) {
        if let Some(o) = self.active_output() {
            output_state(&o).panning = val;
        }
    }
    pub fn last_frame_instant(&self) -> Instant {
        self.active_output()
            .map(|o| output_state(&o).last_frame_instant)
            .unwrap_or_else(Instant::now)
    }
    pub fn set_last_frame_instant(&mut self, val: Instant) {
        if let Some(o) = self.active_output() {
            output_state(&o).last_frame_instant = val;
        }
    }

    /// Batch-access per-output state under a single mutex lock. Returns
    /// `None` (skipping `f`) when there's no active output — per-output state
    /// has no meaning then. Value-returning callers should provide a fallback
    /// (e.g. `unwrap_or(1.0)` for zoom).
    pub fn with_output_state<R>(&mut self, f: impl FnOnce(&mut OutputState) -> R) -> Option<R> {
        let output = self.active_output()?;
        let mut guard = output_state(&output);
        Some(f(&mut guard))
    }

    pub fn get_viewport_size(&self) -> Size<i32, Logical> {
        self.active_output()
            .map(|o| output_logical_size(&o))
            .unwrap_or((1, 1).into())
    }

    /// Viewport area minus layer-shell exclusive zones (panels, bars).
    pub fn get_usable_area(&self) -> Rectangle<i32, Logical> {
        self.active_output()
            .map(|o| self.usable_area_on(&o))
            .unwrap_or_else(|| Rectangle::new((0, 0).into(), (1, 1).into()))
    }

    /// `output`'s usable area (viewport minus layer-shell exclusive zones).
    pub fn usable_area_on(&self, output: &Output) -> Rectangle<i32, Logical> {
        smithay::desktop::layer_map_for_output(output).non_exclusive_zone()
    }

    /// Screen-space center of the usable area (= viewport center when no panels exist).
    pub fn usable_center_screen(&self) -> Point<f64, Logical> {
        self.active_output()
            .map(|o| self.usable_center_screen_on(&o))
            .unwrap_or_else(|| Point::from((0.5, 0.5)))
    }

    /// Screen-space center of `output`'s usable area (viewport minus panels).
    pub fn usable_center_screen_on(&self, output: &Output) -> Point<f64, Logical> {
        let usable = smithay::desktop::layer_map_for_output(output).non_exclusive_zone();
        Point::from((
            usable.loc.x as f64 + usable.size.w as f64 / 2.0,
            usable.loc.y as f64 + usable.size.h as f64 / 2.0,
        ))
    }

    /// The `focus_placement` in effect on `output`: its `[[outputs]]` override
    /// when it carries one, else the top-level key. An output with no entry at
    /// all inherits the global rather than forcing center.
    pub fn focus_placement_on(&self, output: &Output) -> FocusPlacement {
        self.config
            .output_config(&output.name())
            .and_then(|c| c.focus_placement)
            .unwrap_or(self.config.focus_placement)
    }

    /// Screen-space point on `output` where a centering navigation parks a
    /// window's visual frame center, per [`Self::focus_placement_on`]. `frame` is
    /// the frame's canvas-space size and `zoom` the zoom the navigation lands at,
    /// so this is not a fixed screen point — it moves with both.
    pub fn align_point_on(
        &self,
        output: &Output,
        frame: Size<i32, Logical>,
        zoom: f64,
    ) -> Point<f64, Logical> {
        let usable = self.usable_area_on(output);
        let center = Point::from((
            usable.loc.x as f64 + usable.size.w as f64 / 2.0,
            usable.loc.y as f64 + usable.size.h as f64 / 2.0,
        ));
        let (pull_x, pull_y) = self.focus_placement_on(output).pull();
        if (pull_x, pull_y) == (0, 0) {
            return center;
        }
        // `snap_gap` is canvas px, matching the inset `fill_rect` applies, so a
        // placed window and a filled one share an edge at every zoom.
        let gap = self.config.snap_gap * zoom;
        // `frame` carries both borders while `Chrome::frame_loc` subtracts one,
        // so the border cancels: half the frame from its center lands exactly
        // on the outer edge, not half a border off.
        let placed = [
            (pull_x, usable.loc.x, usable.size.w, frame.w, center.x),
            (pull_y, usable.loc.y, usable.size.h, frame.h, center.y),
        ]
        .map(|(pull, low, span, frame_extent, axis_center)| {
            let (low, span) = (low as f64, span as f64);
            let extent = frame_extent as f64 * zoom;
            // Edge-placing a frame that can't fit its two gutters only shoves one
            // side off screen; centering at least shows the middle.
            if extent + 2.0 * gap > span {
                return axis_center;
            }
            match pull {
                -1 => low + gap + extent / 2.0,
                1 => low + span - gap - extent / 2.0,
                _ => axis_center,
            }
        });
        Point::from((placed[0], placed[1]))
    }

    /// Arm a camera + zoom animation that holds `anchor_canvas` at
    /// `anchor_screen` on screen.
    ///
    /// `camera_target` is *derived* from the anchor pair rather than passed in.
    /// The zoom tick nulls `camera_target` every frame and lands the camera at
    /// `anchor.canvas - anchor.screen / zoom` (`viewport_animation.rs`), so the
    /// anchor is what actually arrives — a separately computed target that
    /// disagreed would be discarded without failing anything. Deriving it states
    /// the destination once.
    pub(crate) fn set_camera_anchor(
        &mut self,
        output: &Output,
        anchor_canvas: Point<f64, Logical>,
        anchor_screen: Point<f64, Logical>,
        zoom: f64,
    ) {
        let mut os = crate::state::output_state(output);
        // Disarms a pending zoom-to-fit return, same as a pan: the fit view is
        // a camera position, not a mode that navigation exits.
        os.overview_return = None;
        os.momentum.stop();
        os.camera_target = Some(Point::from((
            anchor_canvas.x - anchor_screen.x / zoom,
            anchor_canvas.y - anchor_screen.y / zoom,
        )));
        os.zoom_animation_anchor = Some(crate::state::ZoomAnimationAnchor {
            canvas: anchor_canvas,
            screen: anchor_screen,
        });
        os.zoom_target = Some(zoom);
    }

    pub fn viewport_center_canvas(&self) -> Point<f64, Logical> {
        let vc = self.usable_center_screen();
        let camera = self.camera();
        let zoom = self.zoom();
        Point::from((camera.x + vc.x / zoom, camera.y + vc.y / zoom))
    }

    /// True if at least `threshold` of the window's area is inside the active
    /// output's viewport.
    pub fn window_visible_at_least<W>(&self, window: &W, threshold: f64) -> bool
    where
        W: StageElement,
        StageWindow: PartialEq<W>,
    {
        self.active_output()
            .is_some_and(|o| self.window_visible_at_least_on(window, &o, threshold))
    }

    /// As `window_visible_at_least`, but against `output`'s viewport instead
    /// of the active one.
    pub fn window_visible_at_least_on<W>(&self, window: &W, output: &Output, threshold: f64) -> bool
    where
        W: StageElement,
        StageWindow: PartialEq<W>,
    {
        let Some(loc) = self.stage.position_of(window) else {
            return false;
        };
        let os = output_state(output);
        driftwm::canvas::visible_fraction(
            loc,
            StageElement::size(window),
            os.camera,
            output_logical_size(output),
            os.zoom,
        ) >= threshold
    }
}
