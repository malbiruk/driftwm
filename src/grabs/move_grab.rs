use std::collections::HashSet;

use smithay::{
    desktop::Window,
    input::{
        SeatHandler,
        pointer::{ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle},
    },
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Logical, Point},
    wayland::seat::WaylandFocus,
};

use crate::state::{DriftWm, output_logical_size, output_state};
use driftwm::canvas::{CanvasPos, canvas_to_screen};
use driftwm::snap::{SnapParams, SnapState, update_axis};

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
    /// Other windows in the primary's cluster, with offsets from the primary
    /// captured at drag start. Offsets are canvas-global and invariant over
    /// motion, snap, and cross-output teleport. Strong `Window` refs; dropped
    /// at grab end. `!alive()` guards any `map_element` we'd otherwise make
    /// on a member that got unmapped mid-drag.
    cluster_members: Vec<(Window, Point<i32, Logical>)>,
    /// Exclude set for `snap_targets`, frozen at drag start. Cluster membership
    /// doesn't change mid-drag, so we pay the `HashSet` build cost once
    /// instead of rebuilding it every motion tick.
    cluster_member_surfaces: HashSet<WlSurface>,
}

impl MoveSurfaceGrab {
    pub fn new(
        start_data: GrabStartData<DriftWm>,
        window: Window,
        initial_window_location: Point<i32, Logical>,
        output: Output,
        cluster_members: Vec<(Window, Point<i32, Logical>)>,
        cluster_member_surfaces: HashSet<WlSurface>,
    ) -> Self {
        Self {
            start_data,
            window,
            initial_window_location,
            snap: SnapState::default(),
            output,
            inhibited_edge: None,
            cluster_members,
            cluster_member_surfaces,
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
        if dist_left < edge_zone {
            vx -= speed * ((edge_zone - dist_left) / edge_zone);
        }
        if dist_right < edge_zone {
            vx += speed * ((edge_zone - dist_right) / edge_zone);
        }
        if dist_top < edge_zone {
            vy -= speed * ((edge_zone - dist_top) / edge_zone);
        }
        if dist_bottom < edge_zone {
            vy += speed * ((edge_zone - dist_bottom) / edge_zone);
        }

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
            Edge::Left => {
                if v.x < 0.0 {
                    v.x = 0.0;
                }
            }
            Edge::Right => {
                if v.x > 0.0 {
                    v.x = 0.0;
                }
            }
            Edge::Top => {
                if v.y < 0.0 {
                    v.y = 0.0;
                }
            }
            Edge::Bottom => {
                if v.y > 0.0 {
                    v.y = 0.0;
                }
            }
        }
        if v.x == 0.0 && v.y == 0.0 {
            None
        } else {
            Some(v)
        }
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
        data.render.blur_geometry_generation += 1;

        // Phase 3 input routing already converted event.location to the focused
        // output's canvas space and updated data.focused_output. If that differs
        // from self.output, the pointer crossed an output boundary.
        if data
            .focused_output
            .as_ref()
            .is_some_and(|fo| *fo != self.output)
        {
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

            // Same ordering invariant as the normal-motion branch: map
            // members first so the primary's `map_element` below lands last
            // in `Space::elements` and stays on top of its own cluster.
            // Offsets are canvas-global, so no recomputation — each member
            // simply re-applies at new_primary_pos + offset.
            for (member, offset) in &self.cluster_members {
                if !member.alive() {
                    continue;
                }
                let member_pos = self.initial_window_location + *offset;
                data.space.map_element(member.clone(), member_pos, false);
                data.sync_x11_position(member);
            }
            data.space
                .map_element(self.window.clone(), self.initial_window_location, false);
            data.sync_x11_position(&self.window);

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

            let Some(self_surface) = self.window.wl_surface().map(|s| s.into_owned()) else {
                return;
            };
            let (others, self_bar) =
                data.snap_targets(&self_surface, &self.cluster_member_surfaces);
            let window_size = self.window.geometry().size;
            let extent_x = window_size.w as f64;
            let extent_y = window_size.h as f64 + self_bar as f64;

            let visual_y = natural_y - self_bar as f64;

            // Perpendicular ranges must reflect the *visual* window position,
            // not the raw cursor. When an axis is held-snapped, the cursor may
            // drift by up to break_force while the window stays pinned, so the
            // natural cursor position can wander into another window's perp
            // range without any visual overlap. Using that for the other axis's
            // candidate search would produce spurious corner snaps.
            let visual_y_for_perp = self.snap.y.as_ref().map_or(visual_y, |s| s.snapped_pos);

            let params_x = SnapParams {
                extent: extent_x,
                perp_low: visual_y_for_perp,
                perp_high: visual_y_for_perp + extent_y,
                horizontal: true,
                others: &others,
                gap,
                threshold: effective_distance,
                break_force: effective_break,
                same_edge: data.config.snap_same_edge,
            };
            let final_x = update_axis(
                &mut self.snap.x,
                &mut self.snap.cooldown_x,
                natural_x,
                &params_x,
            );

            // X was just updated above — self.snap.x now reflects this frame's
            // state (engaged, broken, or untouched).
            let visual_x_for_perp = self.snap.x.as_ref().map_or(natural_x, |s| s.snapped_pos);

            let params_y = SnapParams {
                extent: extent_y,
                perp_low: visual_x_for_perp,
                perp_high: visual_x_for_perp + extent_x,
                horizontal: false,
                others: &others,
                gap,
                threshold: effective_distance,
                break_force: effective_break,
                same_edge: data.config.snap_same_edge,
            };
            let final_visual_y = update_axis(
                &mut self.snap.y,
                &mut self.snap.cooldown_y,
                visual_y,
                &params_y,
            );
            let final_y = final_visual_y + self_bar as f64;

            (final_x, final_y)
        };

        let new_loc = Point::from((final_x as i32, final_y as i32));

        // smithay's `Space::map_element` re-inserts the element at the end
        // of the element list (within its z-index bucket) even with
        // `activate: false`. Map members FIRST so the primary's subsequent
        // `map_element` lands last and stays on top of its own cluster.
        // TODO(cluster): raise members above *non-cluster* windows too —
        // today they keep their original z relative to everything else,
        // which may surprise users whose members get hidden by outsiders.
        for (member, offset) in &self.cluster_members {
            if !member.alive() {
                continue;
            }
            let member_pos = new_loc + *offset;
            data.space.map_element(member.clone(), member_pos, false);
            data.sync_x11_position(member);
        }
        data.space.map_element(self.window.clone(), new_loc, false);
        data.sync_x11_position(&self.window);
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
                    edge,
                    screen_pos,
                    size.w as f64,
                    size.h as f64,
                    cfg.edge_zone,
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
