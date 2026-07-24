use std::cell::RefCell;

use smithay::{
    desktop::Window,
    input::{
        SeatHandler,
        pointer::{ButtonEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle},
        touch::{
            DownEvent, GrabStartData as TouchGrabStartData, MotionEvent as TouchMotionEvent,
            OrientationEvent, ShapeEvent, TouchGrab, TouchInnerHandle, UpEvent,
        },
    },
    output::Output,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Serial, Size},
    wayland::{compositor::with_states, seat::WaylandFocus, shell::xdg::SurfaceCachedState},
};

use smithay::input::pointer::CursorImageStatus;

use crate::state::{ClusterMember, ClusterResizeSnapshot, DriftWm, StageWindow, output_state};
use driftwm::canvas::{self, CanvasPos, canvas_to_screen};
use driftwm::layout::snap::{SnapState, snap_resize_edges};

/// Smallest a suspended window may be resized to — keeps the chrome usable.
/// Folded into the stand-in arm's `SizeConstraints` min so the shared apply
/// head floors it exactly like a client's declared minimum.
pub const MIN_SUSPENDED_SIZE: i32 = 120;

/// Client-declared size constraints captured once at grab start.
///
/// Both fields use smithay's convention: a value of `0` on any axis means
/// "unconstrained" on that axis. Read from `SurfaceCachedState::{min_size,
/// max_size}` on the xdg-toplevel.
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
        let Some(toplevel) = window.toplevel() else {
            return Self::default();
        };
        let cached = with_states(toplevel.wl_surface(), |states| {
            *states.cached_state.get::<SurfaceCachedState>().current()
        });
        Self {
            min: cached.min_size,
            max: cached.max_size,
        }
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

/// Bend a freshly computed resize target back onto a locked aspect `ratio`
/// (width / height). The driving axis follows the active edges: a pure
/// horizontal edge drives width, a pure vertical edge drives height, and a
/// corner drives whichever axis moved farther from `initial` relative to the
/// ratio. The other axis is derived from the ratio. Client min/max clamping
/// happens afterwards and may bend the ratio at those bounds.
pub fn constrain_to_ratio(
    target: Size<i32, Logical>,
    initial: Size<i32, Logical>,
    ratio: f64,
    edges: xdg_toplevel::ResizeEdge,
) -> Size<i32, Logical> {
    let horizontal = has_left(edges) || has_right(edges);
    let vertical = has_top(edges) || has_bottom(edges);
    let width_drives = match (horizontal, vertical) {
        (true, false) => true,
        (false, true) => false,
        // Corner: pick the axis whose ratio-normalized delta is larger so the
        // driver stays fixed as the cursor crosses the ratio ray. Comparing raw
        // |dw| vs |dh| flips the driver off the ray and jumps the derived axis.
        _ => {
            let dw = (target.w - initial.w).abs() as i64;
            let dh = (target.h - initial.h).abs() as i64;
            dw * initial.h as i64 >= dh * initial.w as i64
        }
    };
    if width_drives {
        let h = (target.w as f64 / ratio).round() as i32;
        Size::from((target.w, h.max(1)))
    } else {
        let w = (target.h as f64 * ratio).round() as i32;
        Size::from((w.max(1), target.h))
    }
}

/// Locked aspect ratio (width / height) for a resize of `window`, or `None`.
/// `Some` when the window carries the `preserve_aspect_ratio` rule; taken from
/// its size at grab start.
pub fn locked_ratio_for(window: &Window, initial_window_size: Size<i32, Logical>) -> Option<f64> {
    let surface = window.wl_surface()?;
    if initial_window_size.w > 0
        && initial_window_size.h > 0
        && driftwm::config::applied_rule(&surface).is_some_and(|r| r.preserve_aspect_ratio)
    {
        Some(initial_window_size.w as f64 / initial_window_size.h as f64)
    } else {
        None
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
        /// `Some` ⟹ pinned window: top/left-edge repositioning adjusts
        /// the pin site's `screen_pos` (output-relative) instead of the
        /// canvas loc.
        initial_screen_pos: Option<Point<i32, Logical>>,
        /// Size the last processed commit settled at. `handle_resize_commit`
        /// bumps the blur generation only when a commit changes this, so a
        /// continuously-repainting client under a held-still border doesn't
        /// re-blur every frosted window each repaint frame.
        last_committed_size: Size<i32, Logical>,
    },
    WaitingForLastCommit {
        edges: xdg_toplevel::ResizeEdge,
        initial_window_location: Point<i32, Logical>,
        initial_window_size: Size<i32, Logical>,
        initial_screen_pos: Option<Point<i32, Logical>>,
        last_committed_size: Size<i32, Logical>,
    },
}

pub struct ResizeGrab {
    pub start_data: GrabStartData<DriftWm>,
    /// The resized element as a `Send`-safe handle (a grab must be `Send`, so it
    /// can't hold a `StageWindow`/`Rc`). Re-resolved to a live `StageWindow`
    /// each motion tick; a failed resolve (client died, or a stand-in dismissed
    /// / adopted mid-resize) degrades the grab to a pass-through. `pub` because
    /// the client construction sites build the grab by struct literal.
    pub target: ClusterMember,
    pub edges: xdg_toplevel::ResizeEdge,
    pub initial_window_location: Point<i32, Logical>,
    pub initial_window_size: Size<i32, Logical>,
    pub last_window_size: Size<i32, Logical>,
    pub output: Output,
    pub last_clamped_location: Point<f64, Logical>,
    pub snap: SnapState,
    /// Declared min/max size, read once at grab start. Used to clamp
    /// `new_w`/`new_h` before snap + propagation — otherwise the primary
    /// visually freezes at its real minimum while cluster members keep sliding
    /// in response to `width_delta` that doesn't match reality. The stand-in
    /// arm folds its `MIN_SUSPENDED_SIZE` floor in here.
    pub constraints: SizeConstraints,
    /// Snapshot of the primary's cluster captured at grab start. Empty
    /// `members` + empty `exclude` for single-window resize (every cluster
    /// loop becomes a no-op, `snap_targets` behaves as pre-slice-2).
    pub cluster_resize: ClusterResizeSnapshot,
    /// `Some` ⟹ resizing a screen-pinned window: the size delta is taken in
    /// output-relative screen space (× zoom), there's no snap or cluster reflow,
    /// and top/left-edge repositioning targets `screen_pos`. Holds the
    /// window's `screen_pos` at grab start. Pinned resize is client-only.
    pub pinned_initial_screen_pos: Option<Point<i32, Logical>>,
    /// Touch grab start data, present only for touch-initiated resizes. Mirrors
    /// `MoveGrab`; `apply_resize` reads `start_data.location` so the
    /// pointer and touch paths share one resize core.
    pub touch_start: Option<TouchGrabStartData<DriftWm>>,
    /// Fingers down for a touch resize; the grab unsets when this reaches zero,
    /// so a stray finger doesn't leak out of grab routing.
    pub touch_slots: usize,
    /// `Some(w/h)` ⟹ the window carries `preserve_aspect_ratio`: the resize
    /// derives its non-driving axis from this ratio and skips edge snapping.
    /// Snapshotted from the window's size at grab start. Always `None` for a
    /// stand-in (the rule lookup needs a surface).
    pub locked_ratio: Option<f64>,
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

impl PointerGrab<DriftWm> for ResizeGrab {
    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut PointerInnerHandle<'_, DriftWm>,
        _focus: Option<(<DriftWm as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // Resolved before the pinned branch so a dead pinned client stops
        // receiving configures. Never self-unsets on a failed resolve
        // (pointer-mutex reentrancy hazard).
        let Some(element) = self.target.resolve(&data.stage) else {
            handle.motion(data, None, event);
            return;
        };

        // Force pointer back if Phase 3 input routing crossed to another output.
        // event.location is in the wrong canvas space — use last valid position.
        if data
            .focused_output
            .as_ref()
            .is_some_and(|fo| *fo != self.output)
        {
            data.focused_output = Some(self.output.clone());
            let clamped_event = MotionEvent {
                location: self.last_clamped_location,
                serial: event.serial,
                time: event.time,
            };
            handle.motion(data, None, &clamped_event);
            return;
        }

        if self.pinned_initial_screen_pos.is_some() {
            if let StageWindow::Client(window) = &element {
                let clamped = self.apply_pinned_resize(window, event.location);
                let clamped_event = MotionEvent {
                    location: clamped,
                    serial: event.serial,
                    time: event.time,
                };
                handle.motion(data, None, &clamped_event);
            } else {
                // Pinned targets are client-only at construction; keep the
                // dead arm a pass-through rather than swallowing motion.
                handle.motion(data, None, event);
            }
            return;
        }

        // Clamp pointer to the grab's output bounds.
        let (camera, zoom) = {
            let os = crate::state::output_state(&self.output);
            (os.camera, os.zoom)
        };
        let output_size = crate::state::output_logical_size(&self.output);
        let screen = canvas_to_screen(CanvasPos(event.location), camera, zoom).0;
        let clamped_screen: Point<f64, Logical> = (
            screen.x.clamp(0.0, output_size.w as f64 - 1.0),
            screen.y.clamp(0.0, output_size.h as f64 - 1.0),
        )
            .into();
        let clamped = canvas::screen_to_canvas(canvas::ScreenPos(clamped_screen), camera, zoom).0;
        self.last_clamped_location = clamped;

        self.apply_resize(data, &element, clamped);

        // Warp pointer to clamped position so it visually stops at output edge.
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
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn unset(&mut self, data: &mut DriftWm) {
        match &self.target {
            // A client resize arms the commit-time reposition via
            // `WaitingForLastCommit`; a stand-in has no client to configure, so
            // persist its settled size on the session-store debounce instead.
            ClusterMember::Client(_) => self.finalize(),
            ClusterMember::Suspended(_) => data.session_store_mark_dirty(),
        }
        // Common to both arms: refresh every resolved member's stable snap rect
        // so a later close can reconstruct the cluster. The client primary's own
        // refresh stays in `handle_resize_commit`.
        for member in &self.cluster_resize.members {
            if let Some(element) = member.window.resolve(&data.stage) {
                data.refresh_stable_snap_rect(&element);
            }
        }
        // Reset the resize-edge cursor. Plain field writes only — calling into
        // `PointerHandle` from `unset` is the pointer-mutex reentrancy hazard.
        // Direct default write (not `MoveGrab`'s deferred resync): resize always
        // sets `grab_cursor`, so the resync flush gate wouldn't fire and the
        // resize icon would stick.
        data.cursor.grab_cursor = false;
        data.cursor.cursor_status = CursorImageStatus::default_named();
    }

    crate::grabs::forward_pointer_grab_methods!();
}

impl ResizeGrab {
    /// Wind down a *client* resize: drop the Wayland `Resizing` state and arm
    /// the commit-time reposition (`WaitingForLastCommit`) so a top/left-edge
    /// resize keeps its opposite edge fixed (see `handle_resize_commit`). Runs
    /// from `unset`, so the mouse button-release and the gesture-end paths
    /// finalize identically — gestures deliver no button release. No-op for a
    /// stand-in (no client surface to configure).
    fn finalize(&self) {
        let ClusterMember::Client(window) = &self.target else {
            return;
        };
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Resizing);
            });
            toplevel.send_pending_configure();
        }

        if let Some(surface) = window.wl_surface().map(|s| s.into_owned()) {
            with_states(&surface, |states| {
                let cell = states
                    .data_map
                    .get_or_insert(|| RefCell::new(ResizeState::Idle));
                // Carry the size the resize actually last committed at into the
                // waiting state, so the settle commit still bumps blur even for
                // a drag that ended back at its exact initial size (re-seeding
                // from `initial_window_size` would miss that bump).
                let last_committed_size = match *cell.borrow() {
                    ResizeState::Resizing {
                        last_committed_size,
                        ..
                    }
                    | ResizeState::WaitingForLastCommit {
                        last_committed_size,
                        ..
                    } => last_committed_size,
                    ResizeState::Idle => self.initial_window_size,
                };
                cell.replace(ResizeState::WaitingForLastCommit {
                    edges: self.edges,
                    initial_window_location: self.initial_window_location,
                    initial_window_size: self.initial_window_size,
                    initial_screen_pos: self.pinned_initial_screen_pos,
                    last_committed_size,
                });
            });
        }
    }

    /// Touch-initiated resize. The edge is fixed at grab start (chosen by where
    /// the fingers landed); the drag drives the size from `touch_start.location`.
    /// Touch resize is client-only.
    #[allow(clippy::too_many_arguments)]
    pub fn new_touch(
        touch_start: TouchGrabStartData<DriftWm>,
        window: Window,
        edges: xdg_toplevel::ResizeEdge,
        initial_window_location: Point<i32, Logical>,
        initial_window_size: Size<i32, Logical>,
        output: Output,
        constraints: SizeConstraints,
        slots: usize,
        cluster_resize: ClusterResizeSnapshot,
        pinned_initial_screen_pos: Option<Point<i32, Logical>>,
    ) -> Self {
        let locked_ratio = locked_ratio_for(&window, initial_window_size);
        Self {
            start_data: GrabStartData {
                focus: None,
                button: 0,
                location: touch_start.location,
            },
            target: ClusterMember::Client(window),
            edges,
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
            output,
            last_clamped_location: touch_start.location,
            snap: SnapState::default(),
            constraints,
            cluster_resize,
            pinned_initial_screen_pos,
            touch_start: Some(touch_start),
            touch_slots: slots,
            locked_ratio,
        }
    }

    /// Screen-pinned resize step: size delta in output-relative screen space,
    /// no snap / cluster. Top/left-edge repositioning of `screen_pos` happens
    /// at commit (handle_resize_commit), mirroring the canvas path. Returns the
    /// clamped canvas-space location the caller forwards. Shared by the pointer
    /// and touch resize paths; pinned resize is client-only, so `window` is
    /// always the resolved client.
    fn apply_pinned_resize(
        &mut self,
        window: &Window,
        location: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        let (camera, zoom) = {
            let os = crate::state::output_state(&self.output);
            (os.camera, os.zoom)
        };
        let output_size = crate::state::output_logical_size(&self.output);
        let screen = canvas_to_screen(CanvasPos(location), camera, zoom).0;
        let clamped_screen: Point<f64, Logical> = (
            screen.x.clamp(0.0, output_size.w as f64 - 1.0),
            screen.y.clamp(0.0, output_size.h as f64 - 1.0),
        )
            .into();
        self.last_clamped_location =
            canvas::screen_to_canvas(canvas::ScreenPos(clamped_screen), camera, zoom).0;

        let start_screen = canvas_to_screen(CanvasPos(self.start_data.location), camera, zoom).0;
        let delta = clamped_screen - start_screen;

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
        (new_w, new_h) = self.bend_to_locked_ratio(new_w, new_h);
        let (new_w, new_h) = self.constraints.clamp(new_w, new_h);
        let new_size = Size::from((new_w, new_h));
        if new_size != self.last_window_size {
            self.last_window_size = new_size;
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.size = Some(new_size);
                    state.states.set(xdg_toplevel::State::Resizing);
                });
                toplevel.send_pending_configure();
            }
        }
        self.last_clamped_location
    }

    /// Bend `(w, h)` onto the locked aspect ratio if the window carries one,
    /// returning the adjusted size (a no-op otherwise). Applied before clamp and
    /// snap so the ratio-derived axis stays consistent with the clamped size.
    fn bend_to_locked_ratio(&self, w: i32, h: i32) -> (i32, i32) {
        let Some(ratio) = self.locked_ratio else {
            return (w, h);
        };
        let s = constrain_to_ratio(
            Size::from((w, h)),
            self.initial_window_size,
            ratio,
            self.edges,
        );
        (s.w, s.h)
    }

    /// Apply a resize for canvas (non-pinned) windows from a canvas-space
    /// pointer/finger `location`, cascading to cluster members. Shared by the
    /// pointer and touch resize paths and by both target arms; the caller
    /// passes the element resolved for this tick.
    fn apply_resize(
        &mut self,
        data: &mut DriftWm,
        element: &StageWindow,
        location: Point<f64, Logical>,
    ) {
        let delta = location - self.start_data.location;

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

        (new_w, new_h) = self.bend_to_locked_ratio(new_w, new_h);

        // Clamp to declared min/max (also enforces the per-axis floor; the
        // stand-in arm's constraints fold `MIN_SUSPENDED_SIZE` in here).
        // Applied before snap and cluster propagation so both see the same
        // clamped new_w/new_h — otherwise width_delta keeps growing past the
        // real minimum while the primary visually freezes and cluster members
        // slide off into empty space.
        let (nw, nh) = self.constraints.clamp(new_w, new_h);
        new_w = nw;
        new_h = nh;

        // Snap active resize edges to nearby windows. Skipped under a locked
        // ratio: snapping one axis would fight the ratio-derived axis.
        if data.config.snap_enabled && self.locked_ratio.is_none() {
            let zoom = output_state(&self.output).zoom;
            #[allow(clippy::mutable_key_type)]
            let excludes = self.cluster_resize.exclude_set(&data.stage);
            let (others, self_bar, self_bw) = data.snap_targets(element, &excludes);

            snap_resize_edges(
                &mut self.snap,
                self.edges as u32,
                (
                    self.initial_window_location.x,
                    self.initial_window_location.y,
                ),
                (self.initial_window_size.w, self.initial_window_size.h),
                self_bar,
                self_bw,
                &mut new_w,
                &mut new_h,
                &others,
                zoom,
                data.config.snap_gap,
                data.config.snap_distance,
                data.config.snap_break_force,
                data.config.snap_corners,
            );
        }

        // A locked ratio drives the non-dragged axis geometrically; the cluster
        // snapshot only classified members against the dragged edges, so
        // propagating shifts would grow that derived axis into unclassified
        // neighbors. Treat a ratio-locked resize as single-window. Shifts run
        // every tick (not gated on size change): a member dying mid-tick can
        // reflow the cascade while the primary's size holds constant.
        let members_moved = if self.locked_ratio.is_none() {
            self.cluster_resize.apply_member_shifts(
                &mut data.stage,
                element,
                self.initial_window_size,
                new_w,
                new_h,
                data.config.snap_gap,
            )
        } else {
            false
        };

        let new_size = Size::from((new_w, new_h));
        // Computed before the per-arm tail, which updates `last_window_size`:
        // reading the check afterwards would always be false and the blur bump
        // below would never fire.
        let size_progressed = new_size != self.last_window_size;
        if size_progressed {
            self.last_window_size = new_size;
            match element {
                StageWindow::Client(window) => {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.with_pending_state(|state| {
                            state.size = Some(new_size);
                            state.states.set(xdg_toplevel::State::Resizing);
                        });
                        toplevel.send_pending_configure();
                    }
                }
                // A stand-in has no client to configure: write the size `Cell`
                // and reposition so a left/top drag keeps the opposite edge
                // fixed; the render pass rebuilds its chrome from the new size.
                StageWindow::Suspended(s) => {
                    let mut loc = self.initial_window_location;
                    if has_left(self.edges) {
                        loc.x =
                            self.initial_window_location.x + (self.initial_window_size.w - new_w);
                    }
                    if has_top(self.edges) {
                        loc.y =
                            self.initial_window_location.y + (self.initial_window_size.h - new_h);
                    }
                    s.size.set(new_size);
                    data.map_window(element.clone(), loc, false);
                }
            }
        }

        // Frost above a resized primary or a cascade-shifted neighbor would go
        // stale otherwise: the elements-below hash is identity-based, so
        // `blur_geometry_generation` is the only below-geometry signal for a
        // static background. The `members_moved` term covers a member reflowing
        // on a size-constant tick.
        if size_progressed || members_moved {
            data.render.blur_geometry_generation += 1;
        }
    }
}

impl TouchGrab<DriftWm> for ResizeGrab {
    fn down(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        _focus: Option<(<DriftWm as SeatHandler>::TouchFocus, Point<f64, Logical>)>,
        event: &DownEvent,
        seq: Serial,
    ) {
        // Extra fingers during a touch resize are ignored — single-window only.
        self.touch_slots += 1;
        handle.down(data, None, event, seq);
    }

    fn up(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        event: &UpEvent,
        seq: Serial,
    ) {
        handle.up(data, event, seq);
        self.touch_slots = self.touch_slots.saturating_sub(1);
        // Keep the grab alive until every finger lifts so stray fingers don't
        // leak out of grab routing; `unset` finalizes the resize.
        if self.touch_slots == 0 {
            handle.unset_grab(self, data);
        }
    }

    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        _focus: Option<(<DriftWm as SeatHandler>::TouchFocus, Point<f64, Logical>)>,
        event: &TouchMotionEvent,
        seq: Serial,
    ) {
        if event.slot != self.touch_start.as_ref().expect("touch resize grab").slot {
            handle.motion(data, None, event, seq);
            return;
        }
        let Some(element) = self.target.resolve(&data.stage) else {
            handle.motion(data, None, event, seq);
            return;
        };
        if self.pinned_initial_screen_pos.is_some() {
            if let StageWindow::Client(window) = &element {
                let clamped = self.apply_pinned_resize(window, event.location);
                let clamped_event = TouchMotionEvent {
                    slot: event.slot,
                    location: clamped,
                    time: event.time,
                };
                handle.motion(data, None, &clamped_event, seq);
            } else {
                // Pinned targets are client-only at construction; keep the
                // dead arm a pass-through rather than swallowing motion.
                handle.motion(data, None, event, seq);
            }
            return;
        }
        self.apply_resize(data, &element, event.location);
        handle.motion(data, None, event, seq);
    }

    fn frame(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        seq: Serial,
    ) {
        handle.frame(data, seq);
    }

    fn cancel(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        seq: Serial,
    ) {
        handle.cancel(data, seq);
        handle.unset_grab(self, data);
    }

    fn shape(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        event: &ShapeEvent,
        seq: Serial,
    ) {
        handle.shape(data, event, seq);
    }

    fn orientation(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        event: &OrientationEvent,
        seq: Serial,
    ) {
        handle.orientation(data, event, seq);
    }

    fn start_data(&self) -> &TouchGrabStartData<DriftWm> {
        self.touch_start.as_ref().expect("touch resize grab")
    }

    fn unset(&mut self, data: &mut DriftWm) {
        // Touch never set the grab cursor (it's hidden during touch), so don't
        // reset `cursor_status` — that field is client-owned and clobbering it
        // would lose the app's shape when the pointer next reappears.
        match &self.target {
            ClusterMember::Client(_) => self.finalize(),
            ClusterMember::Suspended(_) => data.session_store_mark_dirty(),
        }
        for member in &self.cluster_resize.members {
            if let Some(element) = member.window.resolve(&data.stage) {
                data.refresh_stable_snap_rect(&element);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Initial 200×100 → locked ratio 2.0 (width / height).
    fn constrained(target: (i32, i32), edges: xdg_toplevel::ResizeEdge) -> (i32, i32) {
        let s = constrain_to_ratio(Size::from(target), Size::from((200, 100)), 2.0, edges);
        (s.w, s.h)
    }

    #[test]
    fn horizontal_edge_drives_width() {
        assert_eq!(
            constrained((300, 100), xdg_toplevel::ResizeEdge::Right),
            (300, 150)
        );
        assert_eq!(
            constrained((80, 100), xdg_toplevel::ResizeEdge::Left),
            (80, 40)
        );
    }

    #[test]
    fn vertical_edge_drives_height() {
        assert_eq!(
            constrained((200, 300), xdg_toplevel::ResizeEdge::Bottom),
            (600, 300)
        );
        assert_eq!(
            constrained((200, 40), xdg_toplevel::ResizeEdge::Top),
            (80, 40)
        );
    }

    #[test]
    fn corner_drives_axis_with_larger_delta() {
        // Height moved farther (200 vs 60) → height drives.
        assert_eq!(
            constrained((260, 300), xdg_toplevel::ResizeEdge::BottomRight),
            (600, 300)
        );
        // Width moved farther (200 vs 20) → width drives.
        assert_eq!(
            constrained((400, 120), xdg_toplevel::ResizeEdge::BottomRight),
            (400, 200)
        );
    }

    #[test]
    fn corner_drag_stays_continuous_across_the_ratio_ray() {
        // BottomRight drag on the 2:1 window: crossing the ratio ray flips the
        // driving axis, but the derived width must move a few px, not jump ~100px.
        let below = constrained((300, 199), xdg_toplevel::ResizeEdge::BottomRight);
        let above = constrained((300, 201), xdg_toplevel::ResizeEdge::BottomRight);
        assert_eq!(below, (398, 199));
        assert_eq!(above, (402, 201));
        assert!((above.0 - below.0).abs() <= 8);
    }
}
