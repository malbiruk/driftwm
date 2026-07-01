use std::time::Instant;

use smithay::output::Output;
use smithay::utils::{Logical, Point};

use super::{DriftWm, output_state};

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
    /// none can move a fullscreen output's camera. Interactive pan/zoom grabs
    /// still write output_state directly, but they exit fullscreen before
    /// panning, so they never race the lock.
    ///
    /// Invariant: a fullscreen window is parked at its output's camera-origin at
    /// zoom 1, so the camera must not move or it slides off (0,0) and re-exposes
    /// black to that output (and other cameras). (`enter_fullscreen` seeds the
    /// origin by writing output_state directly, before its state is inserted.)
    pub fn set_camera_on(&mut self, output: &Output, val: Point<f64, Logical>) {
        if self.fullscreen.contains_key(output) {
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
    pub fn set_zoom(&mut self, val: f64) {
        if let Some(o) = self.active_output() {
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
    pub fn zoom_animation_center(&self) -> Option<Point<f64, Logical>> {
        self.active_output()
            .and_then(|o| output_state(&o).zoom_animation_center)
    }
    pub fn set_zoom_animation_center(&mut self, val: Option<Point<f64, Logical>>) {
        if let Some(o) = self.active_output() {
            output_state(&o).zoom_animation_center = val;
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
    pub fn edge_pan_velocity(&self) -> Option<Point<f64, Logical>> {
        self.active_output()
            .and_then(|o| output_state(&o).edge_pan_velocity)
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
}
