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
    wayland::{
        compositor::with_states,
        seat::WaylandFocus,
        shell::xdg::SurfaceCachedState,
    },
};

use smithay::input::pointer::CursorImageStatus;

use crate::state::{ClusterResizeSnapshot, DriftWm, output_state};
use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::snap::{SnapState, snap_resize_edges};

/// Client-declared size constraints captured once at grab start.
///
/// Both fields use smithay's convention: a value of `0` on any axis means
/// "unconstrained" on that axis. For XDG toplevels, this is read out of
/// `SurfaceCachedState::{min_size, max_size}`. For X11 we read
/// `X11Surface::min_size` / `max_size`, which return `Option`; `None` maps
/// to `(0, 0)` — unconstrained. Non-XDG/non-X11 windows (unreachable in
/// practice for a resize grab) also fall through to unconstrained.
#[derive(Clone, Copy, Debug, Default)]
pub struct SizeConstraints {
    pub min: Size<i32, Logical>,
    pub max: Size<i32, Logical>,
}

impl SizeConstraints {
    /// Snapshot constraints from the window's client at grab start. Cheap
    /// to clone; consumers should store this and clamp per motion tick
    /// instead of calling this in the inner loop.
    pub fn for_window(window: &Window) -> Self {
        if let Some(toplevel) = window.toplevel() {
            let surface = toplevel.wl_surface();
            let cached = with_states(surface, |states| {
                *states.cached_state.get::<SurfaceCachedState>().current()
            });
            return Self {
                min: cached.min_size,
                max: cached.max_size,
            };
        }
        if let Some(x11) = window.x11_surface() {
            return Self {
                min: x11.min_size().unwrap_or_default(),
                max: x11.max_size().unwrap_or_default(),
            };
        }
        Self::default()
    }

    /// Clamp a requested size to `[min, max]` along each axis. Zero values
    /// on either bound are ignored (unconstrained). Also enforces a 1×1
    /// floor so clients never see nonsense geometry from a fast drag.
    pub fn clamp(&self, w: i32, h: i32) -> (i32, i32) {
        let mut cw = w.max(1);
        let mut ch = h.max(1);
        if self.min.w > 0 {
            cw = cw.max(self.min.w);
        }
        if self.min.h > 0 {
            ch = ch.max(self.min.h);
        }
        if self.max.w > 0 {
            cw = cw.min(self.max.w);
        }
        if self.max.h > 0 {
            ch = ch.min(self.max.h);
        }
        (cw, ch)
    }
}

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
    /// Client-declared min/max size, read once at grab start. Used to
    /// clamp `new_w`/`new_h` before snap + propagation — otherwise the
    /// primary visually freezes at its real minimum while cluster members
    /// keep sliding in response to `width_delta` that doesn't match
    /// reality.
    pub constraints: SizeConstraints,
    /// Snapshot of the primary's cluster captured at grab start. Empty
    /// `members` + empty `exclude` for single-window resize (every cluster
    /// loop becomes a no-op, `snap_targets` behaves as pre-slice-2).
    pub cluster_resize: ClusterResizeSnapshot,
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

        // Clamp to client-declared min/max (also enforces a 1×1 floor).
        // Applied before snap and cluster propagation so both see the
        // same clamped new_w/new_h — otherwise width_delta would keep
        // growing past the client's real minimum and cluster members
        // would slide off into empty space while the primary visually
        // freezes.
        let (nw, nh) = self.constraints.clamp(new_w, new_h);
        new_w = nw;
        new_h = nh;

        // Snap active resize edges to nearby windows
        if data.config.snap_enabled
            && let Some(self_surface) = self.window.wl_surface().map(|s| s.into_owned())
        {
            let zoom = output_state(&self.output).zoom;
            let (others, self_bar) =
                data.snap_targets(&self_surface, &self.cluster_resize.exclude);

            snap_resize_edges(
                &mut self.snap,
                self.edges as u32,
                (self.initial_window_location.x, self.initial_window_location.y),
                (self.initial_window_size.w, self.initial_window_size.h),
                self_bar,
                &mut new_w, &mut new_h,
                &others, zoom,
                data.config.snap_gap,
                data.config.snap_distance,
                data.config.snap_break_force,
                data.config.snap_same_edge,
            );
        }

        self.cluster_resize.apply_member_shifts(
            &mut data.space,
            &self.window,
            self.initial_window_size,
            new_w,
            new_h,
            data.config.snap_gap,
        );

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
        data.cursor.grab_cursor = false;
        data.cursor.cursor_status = CursorImageStatus::default_named();
    }

    crate::grabs::forward_pointer_grab_methods!();
}
