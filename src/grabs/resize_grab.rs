use std::cell::RefCell;

use smithay::{
    desktop::Window,
    input::{
        pointer::{
            ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
        },
        SeatHandler,
    },
    output::Output,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Size},
    reexports::wayland_server::Resource,
    wayland::{compositor::with_states, seat::WaylandFocus},
};

use smithay::input::pointer::CursorImageStatus;

use crate::state::{DriftWm, output_state};
use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::config;
use driftwm::snap::{EdgeSnapParams, SnapRect, SnapState, update_edge};

/// Tracks the resize lifecycle for a window. Stored in the surface data map
/// (wrapped in `RefCell`) so that `compositor::commit()` can reposition
/// top/left-edge resizes.
#[derive(Default, Clone, Copy)]
pub enum ResizeState {
    #[default]
    Idle,
    Resizing {
        edges: xdg_toplevel::ResizeEdge,
        initial_window_location: Point<i32, Logical>,
        initial_window_size: Size<i32, Logical>,
    },
    WaitingForLastCommit {
        edges: xdg_toplevel::ResizeEdge,
        initial_window_location: Point<i32, Logical>,
        initial_window_size: Size<i32, Logical>,
    },
}

pub struct ResizeSurfaceGrab {
    pub start_data: GrabStartData<DriftWm>,
    pub window: Window,
    pub edges: xdg_toplevel::ResizeEdge,
    pub initial_window_location: Point<i32, Logical>,
    pub initial_window_size: Size<i32, Logical>,
    pub last_window_size: Size<i32, Logical>,
    pub output: Output,
    pub last_clamped_location: Point<f64, Logical>,
    /// Throttle X11 configures to avoid overwhelming the client (X11 redraws synchronously).
    pub last_x11_configure: Option<std::time::Instant>,
    pub snap: SnapState,
}

/// Check if `edges` includes a horizontal/vertical component via raw bit values.
/// ResizeEdge values: Top=1, Bottom=2, Left=4, Right=8, combinations are ORed.
pub fn has_top(edges: xdg_toplevel::ResizeEdge) -> bool {
    edges as u32 & 1 != 0
}
pub fn has_bottom(edges: xdg_toplevel::ResizeEdge) -> bool {
    edges as u32 & 2 != 0
}
pub fn has_left(edges: xdg_toplevel::ResizeEdge) -> bool {
    edges as u32 & 4 != 0
}
pub fn has_right(edges: xdg_toplevel::ResizeEdge) -> bool {
    edges as u32 & 8 != 0
}

impl PointerGrab<DriftWm> for ResizeSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        _focus: Option<(<DriftWm as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // Force pointer back if Phase 3 input routing crossed to another output.
        // event.location is in the wrong canvas space — use last valid position.
        if data.focused_output.as_ref().is_some_and(|fo| *fo != self.output) {
            data.focused_output = Some(self.output.clone());
            let clamped_event = MotionEvent {
                location: self.last_clamped_location,
                serial: event.serial,
                time: event.time,
            };
            handle.motion(data, None, &clamped_event);
            return;
        }

        // Clamp pointer to the grab's output bounds
        let (camera, zoom) = {
            let os = crate::state::output_state(&self.output);
            (os.camera, os.zoom)
        };
        let output_size = crate::state::output_logical_size(&self.output);
        let screen = canvas_to_screen(CanvasPos(event.location), camera, zoom).0;
        let clamped_screen: Point<f64, Logical> = (
            screen.x.clamp(0.0, output_size.w as f64 - 1.0),
            screen.y.clamp(0.0, output_size.h as f64 - 1.0),
        ).into();
        let clamped = canvas::screen_to_canvas(
            canvas::ScreenPos(clamped_screen), camera, zoom,
        ).0;
        self.last_clamped_location = clamped;

        let delta = clamped - self.start_data.location;

        let mut new_w = self.initial_window_size.w;
        let mut new_h = self.initial_window_size.h;

        if has_left(self.edges) {
            new_w -= delta.x as i32;
        } else if has_right(self.edges) {
            new_w += delta.x as i32;
        }
        if has_top(self.edges) {
            new_h -= delta.y as i32;
        } else if has_bottom(self.edges) {
            new_h += delta.y as i32;
        }

        // Clamp to minimum 1×1
        new_w = new_w.max(1);
        new_h = new_h.max(1);

        // Snap active resize edges to nearby windows
        if data.config.snap_enabled {
            let zoom = output_state(&self.output).zoom;
            let effective_distance = data.config.snap_distance / zoom;
            let effective_break = data.config.snap_break_force / zoom;
            let gap = data.config.snap_gap;
            let same_edge = data.config.snap_same_edge;

            if let Some(self_surface) = self.window.wl_surface().map(|s| s.into_owned()) {
            let self_bar = if data.decorations.contains_key(&self_surface.id()) {
                config::DecorationConfig::TITLE_BAR_HEIGHT
            } else {
                0
            };

            let mut others: Vec<SnapRect> = Vec::new();
            for w in data.space.elements() {
                let Some(surface) = w.wl_surface() else { continue };
                if *surface == self_surface { continue }
                if config::applied_rule(&surface).is_some_and(|r| r.widget) { continue }
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

            let loc = self.initial_window_location;
            let init_w = self.initial_window_size.w;
            let init_h = self.initial_window_size.h;

            // Compute the window's visual bounds at current resize state
            let visual_top = if has_top(self.edges) {
                loc.y as f64 + init_h as f64 - new_h as f64 - self_bar as f64
            } else {
                loc.y as f64 - self_bar as f64
            };
            let visual_bottom = if has_bottom(self.edges) {
                loc.y as f64 + new_h as f64
            } else {
                loc.y as f64 + init_h as f64
            };

            // Horizontal edge snapping
            if has_right(self.edges) {
                let natural_right = loc.x as f64 + new_w as f64;
                let hp = EdgeSnapParams {
                    perp_low: visual_top, perp_high: visual_bottom,
                    horizontal: true, same_edge, others: &others,
                    gap, threshold: effective_distance, break_force: effective_break,
                    high_edge: true,
                };
                let snapped = update_edge(
                    &mut self.snap.x, &mut self.snap.cooldown_x, natural_right, &hp,
                );
                new_w = (snapped - loc.x as f64) as i32;
            } else if has_left(self.edges) {
                let fixed_right = loc.x as f64 + init_w as f64;
                let natural_left = fixed_right - new_w as f64;
                let hp = EdgeSnapParams {
                    perp_low: visual_top, perp_high: visual_bottom,
                    horizontal: true, same_edge, others: &others,
                    gap, threshold: effective_distance, break_force: effective_break,
                    high_edge: false,
                };
                let snapped = update_edge(
                    &mut self.snap.x, &mut self.snap.cooldown_x, natural_left, &hp,
                );
                new_w = (fixed_right - snapped) as i32;
            }

            // Vertical edge snapping
            if has_bottom(self.edges) {
                let natural_bottom = loc.y as f64 + new_h as f64;
                let vp = EdgeSnapParams {
                    perp_low: loc.x as f64, perp_high: loc.x as f64 + new_w as f64,
                    horizontal: false, same_edge, others: &others,
                    gap, threshold: effective_distance, break_force: effective_break,
                    high_edge: true,
                };
                let snapped = update_edge(
                    &mut self.snap.y, &mut self.snap.cooldown_y, natural_bottom, &vp,
                );
                new_h = (snapped - loc.y as f64) as i32;
            } else if has_top(self.edges) {
                let fixed_bottom = loc.y as f64 + init_h as f64;
                let natural_top = fixed_bottom - new_h as f64 - self_bar as f64;
                let vp = EdgeSnapParams {
                    perp_low: loc.x as f64, perp_high: loc.x as f64 + new_w as f64,
                    horizontal: false, same_edge, others: &others,
                    gap, threshold: effective_distance, break_force: effective_break,
                    high_edge: false,
                };
                let snapped = update_edge(
                    &mut self.snap.y, &mut self.snap.cooldown_y, natural_top, &vp,
                );
                new_h = (fixed_bottom - snapped - self_bar as f64) as i32;
            }

            }  // if let Some(self_surface)

            new_w = new_w.max(1);
            new_h = new_h.max(1);
        }

        let new_size = Size::from((new_w, new_h));

        // Only send configure when size actually changed
        if new_size != self.last_window_size {
            self.last_window_size = new_size;

            if let Some(toplevel) = self.window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.size = Some(new_size);
                    state.states.set(xdg_toplevel::State::Resizing);
                });
                toplevel.send_pending_configure();
            } else if let Some(x11) = self.window.x11_surface() {
                // Throttle X11 configures to ~60fps — X11 apps redraw synchronously
                let now = std::time::Instant::now();
                let throttle_ok = self.last_x11_configure.is_none_or(|t| {
                    now.duration_since(t) >= std::time::Duration::from_millis(16)
                });
                if throttle_ok {
                    self.last_x11_configure = Some(now);
                    let mut geo = x11.geometry();
                    geo.size = new_size;
                    x11.configure(geo).ok();
                }
            }
        }

        // Warp pointer to clamped position so it visually stops at output edge
        let clamped_event = MotionEvent {
            location: clamped,
            serial: event.serial,
            time: event.time,
        };
        handle.motion(data, None, &clamped_event);
    }

    fn button(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            // Grab released — unset Resizing state (Wayland only) and
            // transition to WaitingForLastCommit for position adjustment
            if let Some(toplevel) = self.window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Resizing);
                });
                toplevel.send_pending_configure();
            } else if let Some(x11) = self.window.x11_surface() {
                let mut geo = x11.geometry();
                geo.size = self.last_window_size;
                x11.configure(geo).ok();
            }

            let Some(surface) = self.window.wl_surface().map(|s| s.into_owned()) else {
                handle.unset_grab(self, data, event.serial, event.time, true);
                return;
            };
            let edges = self.edges;
            let initial_window_location = self.initial_window_location;
            let initial_window_size = self.initial_window_size;
            with_states(&surface, |states| {
                states
                    .data_map
                    .get_or_insert(|| RefCell::new(ResizeState::Idle))
                    .replace(ResizeState::WaitingForLastCommit {
                        edges,
                        initial_window_location,
                        initial_window_size,
                    });
            });

            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn unset(&mut self, data: &mut DriftWm) {
        data.grab_cursor = false;
        data.cursor_status = CursorImageStatus::default_named();
    }

    crate::grabs::forward_pointer_grab_methods!();
}
