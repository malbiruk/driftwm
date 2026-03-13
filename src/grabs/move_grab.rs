use smithay::{
    desktop::Window,
    input::{
        pointer::{
            ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
        },
        SeatHandler,
    },
    output::Output,
    reexports::wayland_server::Resource,
    utils::{Logical, Point},
    wayland::seat::WaylandFocus,
};

use driftwm::canvas::{CanvasPos, canvas_to_screen};
use driftwm::config;
use driftwm::snap::{SnapRect, SnapParams, SnapState, update_axis};
use crate::state::{DriftWm, output_state, output_logical_size};

/// Which output edge is inhibited after a cross-output teleport.
#[derive(Clone, Copy)]
enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

pub struct MoveSurfaceGrab {
    pub start_data: GrabStartData<DriftWm>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
    pub snap: SnapState,
    /// Output this grab is pinned to (uses its camera/zoom throughout).
    pub output: Output,
    /// After teleport, suppress edge-pan on the entry edge until cursor moves inward.
    inhibited_edge: Option<Edge>,
}

impl MoveSurfaceGrab {
    pub fn new(
        start_data: GrabStartData<DriftWm>,
        window: Window,
        initial_window_location: Point<i32, Logical>,
        output: Output,
    ) -> Self {
        Self {
            start_data,
            window,
            initial_window_location,
            snap: SnapState::default(),
            output,
            inhibited_edge: None,
        }
    }

    /// Compute edge-pan velocity based on how deep the cursor is into the edge zone.
    /// Deeper = faster (like a joystick). Returns None when cursor is outside the zone.
    pub(crate) fn edge_pan_velocity(
        screen_pos: Point<f64, Logical>,
        output_w: f64,
        output_h: f64,
        edge_zone: f64,
        pan_min: f64,
        pan_max: f64,
    ) -> Option<Point<f64, Logical>> {
        let dist_left = screen_pos.x;
        let dist_right = output_w - screen_pos.x;
        let dist_top = screen_pos.y;
        let dist_bottom = output_h - screen_pos.y;
        let min_dist = dist_left.min(dist_right).min(dist_top).min(dist_bottom);

        if min_dist >= edge_zone {
            return None;
        }

        // Depth into the zone: 0.0 at boundary, 1.0 at viewport edge
        let t = ((edge_zone - min_dist) / edge_zone).clamp(0.0, 1.0);
        // Quadratic ramp — gentle start, fast finish
        let speed = pan_min + (pan_max - pan_min) * t * t;

        // Direction: push away from the nearest edge(s)
        let mut vx = 0.0;
        let mut vy = 0.0;
        if dist_left < edge_zone { vx -= speed * ((edge_zone - dist_left) / edge_zone); }
        if dist_right < edge_zone { vx += speed * ((edge_zone - dist_right) / edge_zone); }
        if dist_top < edge_zone { vy -= speed * ((edge_zone - dist_top) / edge_zone); }
        if dist_bottom < edge_zone { vy += speed * ((edge_zone - dist_bottom) / edge_zone); }

        // Normalize diagonal so it doesn't go √2 faster
        let len = (vx * vx + vy * vy).sqrt();
        if len > speed {
            vx = vx / len * speed;
            vy = vy / len * speed;
        }

        Some(Point::from((vx, vy)))
    }

    /// Determine the entry edge: the old output's layout center relative to the
    /// new output tells us which side the cursor entered from.
    fn entry_edge(old_output: &Output, new_output: &Output) -> Edge {
        let old_os = output_state(old_output);
        let old_lp = old_os.layout_position;
        drop(old_os);
        let old_size = output_logical_size(old_output);
        let old_cx = old_lp.x as f64 + old_size.w as f64 / 2.0;
        let old_cy = old_lp.y as f64 + old_size.h as f64 / 2.0;

        let new_os = output_state(new_output);
        let new_lp = new_os.layout_position;
        drop(new_os);
        let new_size = output_logical_size(new_output);
        let new_cx = new_lp.x as f64 + new_size.w as f64 / 2.0;
        let new_cy = new_lp.y as f64 + new_size.h as f64 / 2.0;

        let dx = old_cx - new_cx;
        let dy = old_cy - new_cy;

        // The entry edge is the side of the new output facing the old output.
        if dx.abs() >= dy.abs() {
            if dx > 0.0 { Edge::Right } else { Edge::Left }
        } else if dy > 0.0 {
            Edge::Bottom
        } else {
            Edge::Top
        }
    }

    /// Check if the cursor has moved far enough from the inhibited edge to clear it.
    fn should_clear_inhibition(
        edge: Edge,
        screen_pos: Point<f64, Logical>,
        output_w: f64,
        output_h: f64,
        edge_zone: f64,
    ) -> bool {
        match edge {
            Edge::Left => screen_pos.x >= edge_zone,
            Edge::Right => (output_w - screen_pos.x) >= edge_zone,
            Edge::Top => screen_pos.y >= edge_zone,
            Edge::Bottom => (output_h - screen_pos.y) >= edge_zone,
        }
    }

    /// Zero out the velocity component for the inhibited edge, keeping others.
    fn suppress_inhibited_edge(
        edge: Edge,
        velocity: Option<Point<f64, Logical>>,
    ) -> Option<Point<f64, Logical>> {
        let mut v = velocity?;
        match edge {
            Edge::Left => { if v.x < 0.0 { v.x = 0.0; } }
            Edge::Right => { if v.x > 0.0 { v.x = 0.0; } }
            Edge::Top => { if v.y < 0.0 { v.y = 0.0; } }
            Edge::Bottom => { if v.y > 0.0 { v.y = 0.0; } }
        }
        if v.x == 0.0 && v.y == 0.0 { None } else { Some(v) }
    }
}

impl PointerGrab<DriftWm> for MoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        _focus: Option<(<DriftWm as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        data.blur_scene_generation += 1;
        data.blur_geometry_generation += 1;

        // Phase 3 input routing already converted event.location to the focused
        // output's canvas space and updated data.focused_output. If that differs
        // from self.output, the pointer crossed an output boundary.
        if data.focused_output.as_ref().is_some_and(|fo| *fo != self.output) {
            let new_output = data.focused_output.clone().unwrap();

            // event.location is already in the new output's canvas space.
            // Canvas-space offset between cursor and window corner is
            // zoom-independent — canvas coords are the source of truth.
            let canvas_offset: Point<f64, Logical> = Point::from((
                self.initial_window_location.x as f64 - self.start_data.location.x,
                self.initial_window_location.y as f64 - self.start_data.location.y,
            ));

            let entry_edge = Self::entry_edge(&self.output, &new_output);

            // Clear edge-pan on the old output before switching.
            output_state(&self.output).edge_pan_velocity = None;

            self.start_data.location = event.location;
            self.initial_window_location = Point::from((
                (event.location.x + canvas_offset.x) as i32,
                (event.location.y + canvas_offset.y) as i32,
            ));
            self.output = new_output;
            self.snap = SnapState::default();
            self.inhibited_edge = Some(entry_edge);

            // Map window at new position immediately.
            data.space.map_element(
                self.window.clone(),
                self.initial_window_location,
                false,
            );
            handle.motion(data, None, event);
            return;
        }

        // Normal case — event.location is in self.output's canvas space.
        let delta = event.location - self.start_data.location;
        let natural_x = self.initial_window_location.x as f64 + delta.x;
        let natural_y = self.initial_window_location.y as f64 + delta.y;

        let (final_x, final_y) = if !data.config.snap_enabled {
            (natural_x, natural_y)
        } else {
            let zoom = output_state(&self.output).zoom;
            let effective_distance = data.config.snap_distance / zoom;
            let effective_break = data.config.snap_break_force / zoom;
            let gap = data.config.snap_gap;

            // Collect other windows' snap rects (exclude self and widgets)
            let Some(self_surface) = self.window.wl_surface().map(|s| s.into_owned()) else {
                return;
            };
            let window_size = self.window.geometry().size;
            let self_bar = if data.decorations.contains_key(&self_surface.id()) {
                config::DecorationConfig::TITLE_BAR_HEIGHT
            } else {
                0
            };
            let extent_x = window_size.w as f64;
            let extent_y = window_size.h as f64 + self_bar as f64;

            let mut others: Vec<SnapRect> = Vec::new();
            for w in data.space.elements() {
                let Some(surface) = w.wl_surface() else { continue; };
                if *surface == self_surface {
                    continue;
                }
                if config::applied_rule(&surface).is_some_and(|r| r.widget) {
                    continue;
                }
                let Some(loc) = data.space.element_location(w) else { continue };
                let size = w.geometry().size;
                let bar = if data.decorations.contains_key(&surface.id()) {
                    config::DecorationConfig::TITLE_BAR_HEIGHT
                } else {
                    0
                };
                others.push(SnapRect {
                    x_low: loc.x as f64,
                    x_high: loc.x as f64 + size.w as f64,
                    y_low: loc.y as f64 - bar as f64,
                    y_high: loc.y as f64 + size.h as f64,
                });
            }

            // Use natural (un-snapped) positions for perpendicular ranges
            let visual_y = natural_y - self_bar as f64;

            let params_x = SnapParams {
                extent: extent_x,
                perp_low: visual_y,
                perp_high: visual_y + extent_y,
                horizontal: true,
                others: &others,
                gap,
                threshold: effective_distance,
                break_force: effective_break,
            };
            let final_x = update_axis(
                &mut self.snap.x, &mut self.snap.cooldown_x, natural_x, &params_x,
            );

            // Shift y into visual space (title bar top) for snapping,
            // then convert back to geometry origin.
            let params_y = SnapParams {
                extent: extent_y,
                perp_low: natural_x,
                perp_high: natural_x + extent_x,
                horizontal: false,
                others: &others,
                gap,
                threshold: effective_distance,
                break_force: effective_break,
            };
            let final_visual_y = update_axis(
                &mut self.snap.y, &mut self.snap.cooldown_y, visual_y, &params_y,
            );
            let final_y = final_visual_y + self_bar as f64;

            (final_x, final_y)
        };

        let new_loc = Point::from((final_x as i32, final_y as i32));
        data.space.map_element(self.window.clone(), new_loc, false);
        handle.motion(data, None, event);

        // Edge auto-pan detection using pinned output.
        let (camera, zoom) = {
            let os = output_state(&self.output);
            (os.camera, os.zoom)
        };
        let screen_pos = canvas_to_screen(CanvasPos(event.location), camera, zoom).0;
        let output_size = Some(output_logical_size(&self.output));

        if let Some(size) = output_size {
            let cfg = &data.config;
            let velocity = Self::edge_pan_velocity(
                screen_pos,
                size.w as f64,
                size.h as f64,
                cfg.edge_zone,
                cfg.edge_pan_min,
                cfg.edge_pan_max,
            );

            let effective_velocity = if let Some(edge) = self.inhibited_edge {
                if Self::should_clear_inhibition(
                    edge, screen_pos, size.w as f64, size.h as f64, cfg.edge_zone,
                ) {
                    self.inhibited_edge = None;
                    velocity
                } else {
                    Self::suppress_inhibited_edge(edge, velocity)
                }
            } else {
                velocity
            };

            output_state(&self.output).edge_pan_velocity = effective_velocity;
        }
    }

    fn button(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            output_state(&self.output).edge_pan_velocity = None;
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn unset(&mut self, _data: &mut DriftWm) {
        output_state(&self.output).edge_pan_velocity = None;
    }

    crate::grabs::forward_pointer_grab_methods!();
}
