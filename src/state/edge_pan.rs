//! Edge-pan arming: which output edges the pointer is pushing against and how
//! fast the camera should follow. Only the request is recorded here; the
//! animation tick in `viewport_animation.rs` applies it.
//!
//! An edge that physically touches another monitor is held back by
//! `edge_pan_latency_ms`, so a pointer heading for the next screen crosses to
//! it instead of dragging the canvas along. Components not facing a monitor
//! stay immediate, including at a mixed inner/outer corner.

use std::time::{Duration, Instant};

use smithay::output::Output;
use smithay::utils::{Logical, Point, Size};

use super::{DriftWm, output_logical_size, output_state};

#[derive(Clone, Copy)]
pub(crate) struct EdgePanDelay {
    pub(crate) edges: u8,
    pub(crate) entered_at: Instant,
}

impl DriftWm {
    const EDGE_LEFT: u8 = 1 << 0;
    const EDGE_RIGHT: u8 = 1 << 1;
    const EDGE_TOP: u8 = 1 << 2;
    const EDGE_BOTTOM: u8 = 1 << 3;

    /// Arm edge-pan from a screen-local pointer position. The animation tick
    /// applies monitor-boundary latency; callers only describe pointer intent.
    pub(crate) fn update_edge_pan_request(
        &self,
        output: &Output,
        velocity: Option<Point<f64, Logical>>,
        screen_pos: Point<f64, Logical>,
    ) {
        let mut os = output_state(output);
        os.edge_pan_velocity = velocity;
        os.edge_pan_screen_pos = velocity.map(|_| screen_pos);
        if velocity.is_none() {
            os.edge_pan_delay = None;
        }
    }

    pub(crate) fn clear_edge_pan(&self, output: &Output) {
        let mut os = output_state(output);
        os.edge_pan_velocity = None;
        os.edge_pan_screen_pos = None;
        os.edge_pan_delay = None;
    }

    /// Edges of `output` that physically touch another output in layout space.
    /// A positive overlap is required, so monitors meeting only at a corner do
    /// not delay either edge.
    fn monitor_facing_edges_at(&self, output: &Output, screen_pos: Point<f64, Logical>) -> u8 {
        let os = output_state(output);
        let loc = os.layout_position;
        drop(os);
        let size = output_logical_size(output);
        let mut edges = 0;

        for other in self.space.outputs().filter(|other| *other != output) {
            let other_os = output_state(other);
            let other_loc = other_os.layout_position;
            drop(other_os);
            let other_size = output_logical_size(other);
            edges |= Self::shared_edges_at(loc, size, screen_pos, other_loc, other_size);
        }
        edges
    }

    /// Shared sides at one point on `loc`/`size`. Half-open spans match the
    /// layout's rectangle containment rule and make a point where two monitor
    /// segments meet belong to exactly one of them.
    fn shared_edges_at(
        loc: Point<i32, Logical>,
        size: Size<i32, Logical>,
        screen_pos: Point<f64, Logical>,
        other_loc: Point<i32, Logical>,
        other_size: Size<i32, Logical>,
    ) -> u8 {
        // Promote before adding sizes: fixed output positions are user input,
        // and a valid i32 position near either extreme must not overflow.
        let left = i64::from(loc.x);
        let right = left + i64::from(size.w);
        let top = i64::from(loc.y);
        let bottom = top + i64::from(size.h);
        let other_left = i64::from(other_loc.x);
        let other_right = other_left + i64::from(other_size.w);
        let other_top = i64::from(other_loc.y);
        let other_bottom = other_top + i64::from(other_size.h);
        let layout_x = loc.x as f64 + screen_pos.x;
        let layout_y = loc.y as f64 + screen_pos.y;
        let on_shared_vertical_span =
            top.max(other_top) as f64 <= layout_y && layout_y < bottom.min(other_bottom) as f64;
        let on_shared_horizontal_span =
            left.max(other_left) as f64 <= layout_x && layout_x < right.min(other_right) as f64;
        let mut edges = 0;

        if on_shared_vertical_span && left == other_right {
            edges |= Self::EDGE_LEFT;
        }
        if on_shared_vertical_span && right == other_left {
            edges |= Self::EDGE_RIGHT;
        }
        if on_shared_horizontal_span && top == other_bottom {
            edges |= Self::EDGE_TOP;
        }
        if on_shared_horizontal_span && bottom == other_top {
            edges |= Self::EDGE_BOTTOM;
        }
        edges
    }

    /// Resolve monitor-boundary latency on the frame tick. Non-monitor-facing
    /// components remain immediate, including at mixed inner/outer corners.
    ///
    /// A fullscreen output has no effective velocity: its camera is locked
    /// (see `set_camera_on`), and the caller's compensating pointer warp
    /// would otherwise walk the cursor off the parked window. Answered here
    /// rather than at each tick so the two can't disagree. The request stays
    /// armed and resumes once fullscreen exits.
    pub(super) fn effective_edge_pan_velocity(
        &self,
        output: &Output,
        now: Instant,
    ) -> Option<Point<f64, Logical>> {
        let latency = Duration::from_millis(self.config.edge_pan_latency_ms);
        let os = output_state(output);
        let requested = os.edge_pan_velocity?;
        let screen_pos = os.edge_pan_screen_pos?;
        drop(os);
        // Below the arming check, which is the common case and free: the
        // fullscreen lookup allocates the output name, and every output would pay
        // it every frame if it ran first. `edge_pan_delay` is left as it stands —
        // an armed delay keeps its `entered_at` frozen across the whole
        // fullscreen session, so a monitor-facing edge held throughout is already
        // past its latency when the exit lets the pan resume.
        if self.is_output_fullscreen(output) {
            return None;
        }
        let monitor_edges = self.monitor_facing_edges_at(output, screen_pos);
        let mut os = output_state(output);
        let mut immediate = requested;
        let mut delayed_edges = 0;

        if requested.x < 0.0 && monitor_edges & Self::EDGE_LEFT != 0 {
            immediate.x = 0.0;
            delayed_edges |= Self::EDGE_LEFT;
        } else if requested.x > 0.0 && monitor_edges & Self::EDGE_RIGHT != 0 {
            immediate.x = 0.0;
            delayed_edges |= Self::EDGE_RIGHT;
        }
        if requested.y < 0.0 && monitor_edges & Self::EDGE_TOP != 0 {
            immediate.y = 0.0;
            delayed_edges |= Self::EDGE_TOP;
        } else if requested.y > 0.0 && monitor_edges & Self::EDGE_BOTTOM != 0 {
            immediate.y = 0.0;
            delayed_edges |= Self::EDGE_BOTTOM;
        }

        if delayed_edges == 0 || latency.is_zero() {
            os.edge_pan_delay = None;
            return Some(requested);
        }

        let entered_at = match os.edge_pan_delay {
            Some(delay) if delay.edges == delayed_edges => delay.entered_at,
            _ => {
                os.edge_pan_delay = Some(EdgePanDelay {
                    edges: delayed_edges,
                    entered_at: now,
                });
                now
            }
        };
        if now.saturating_duration_since(entered_at) >= latency {
            Some(requested)
        } else if immediate.x != 0.0 || immediate.y != 0.0 {
            Some(immediate)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_pan_delay_only_covers_shared_vertical_segment() {
        let loc = Point::from((0, 0));
        let size = Size::from((1920, 1080));
        let other_loc = Point::from((1920, 300));
        let other_size = Size::from((1280, 600));

        let edges = |y| {
            DriftWm::shared_edges_at(loc, size, Point::from((1919.0, y)), other_loc, other_size)
        };
        assert_eq!(edges(299.999), 0);
        assert_eq!(edges(300.0), DriftWm::EDGE_RIGHT);
        assert_eq!(edges(899.999), DriftWm::EDGE_RIGHT);
        assert_eq!(edges(900.0), 0);
    }

    #[test]
    fn edge_pan_delay_only_covers_shared_horizontal_segment() {
        let edges = DriftWm::shared_edges_at(
            Point::from((-1000, 200)),
            Size::from((1600, 900)),
            Point::from((750.0, 0.0)),
            Point::from((-500, -700)),
            Size::from((600, 900)),
        );
        assert_eq!(edges, DriftWm::EDGE_TOP);

        let outside = DriftWm::shared_edges_at(
            Point::from((-1000, 200)),
            Size::from((1600, 900)),
            Point::from((1100.0, 0.0)),
            Point::from((-500, -700)),
            Size::from((600, 900)),
        );
        assert_eq!(outside, 0);
    }

    #[test]
    fn edge_pan_delay_ignores_corner_only_contact() {
        let edges = DriftWm::shared_edges_at(
            Point::from((0, 0)),
            Size::from((1000, 1000)),
            Point::from((999.0, 999.0)),
            Point::from((1000, 1000)),
            Size::from((500, 500)),
        );
        assert_eq!(edges, 0);
    }

    #[test]
    fn edge_pan_delay_segment_endpoints_are_half_open() {
        let loc = Point::from((0, 0));
        let size = Size::from((1000, 1000));
        let screen_pos = Point::from((999.0, 500.0));
        let upper = DriftWm::shared_edges_at(
            loc,
            size,
            screen_pos,
            Point::from((1000, 0)),
            Size::from((500, 500)),
        );
        let lower = DriftWm::shared_edges_at(
            loc,
            size,
            screen_pos,
            Point::from((1000, 500)),
            Size::from((500, 500)),
        );
        assert_eq!(upper, 0);
        assert_eq!(lower, DriftWm::EDGE_RIGHT);
    }

    #[test]
    fn edge_pan_delay_geometry_does_not_overflow_extreme_positions() {
        let edges = DriftWm::shared_edges_at(
            Point::from((i32::MAX - 10, i32::MIN)),
            Size::from((100, 100)),
            Point::from((99.0, 50.0)),
            Point::from((i32::MIN, i32::MAX - 10)),
            Size::from((100, 100)),
        );
        assert_eq!(edges, 0);
    }
}
