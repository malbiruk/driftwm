//! The state every client-side resize starts from, shared by the four entry
//! points that can start one: the pointer, the client's own
//! `xdg_toplevel.resize`, touch, and the trackpad gesture — plus the
//! non-interactive resize behind `driftwm msg resize`, which owns the whole
//! operation because no grab follows to finish it.

use std::cell::RefCell;

use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Size};
use smithay::wayland::compositor::with_states;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

use super::{DriftWm, StageWindow};
use crate::grabs::{ResizeState, SizeConstraints};
use driftwm::config::Direction;

/// The size a non-interactive resize measures its delta and its anchor against.
///
/// While the client still owes an ack this is the size we configured, so two
/// requests arriving before the first commit land on the same absolute rect
/// instead of walking the window by half the outstanding delta each time. Once
/// nothing is owed it is committed geometry: [`super::configured_window_size`]
/// keeps reporting the last configure forever, and a client that has since
/// resized itself (GTK `resize()`, a terminal font change) would otherwise be
/// anchored against a size it never had — and a request for the size we last
/// configured would silently report success without sending anything.
pub(crate) fn configured_element_size(element: &StageWindow) -> Size<i32, Logical> {
    match element {
        StageWindow::Client(window) if owes_a_configured_size(window) => {
            super::configured_window_size(window)
        }
        StageWindow::Client(window) => window.geometry().size,
        StageWindow::Suspended(s) => s.size.get(),
    }
}

/// Whether the client still owes an ack for a size we sent: a pending configure
/// carrying a real size that differs from committed geometry. The compositor's
/// benign zero-size configures ("client picks its own size") never count.
fn owes_a_configured_size(window: &Window) -> bool {
    let Some(surface) = window.wl_surface() else {
        return false;
    };
    let committed = window.geometry().size;
    with_states(&surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .is_some_and(|data| {
                data.lock().unwrap().pending_configures().iter().any(
                    |c| matches!(c.state.size, Some(s) if s.w > 0 && s.h > 0 && s != committed),
                )
            })
    })
}

/// The placement a `grow-window` / `shrink-window` step promised, kept until the
/// next step can check it against what the client actually committed. Carries
/// the pre-step size too, so a rect that came from anywhere else — a drag, a
/// fit, the IPC verb — fails the range check instead of being "corrected".
pub(crate) struct StepAnchor {
    surface: smithay::reexports::wayland_server::backend::ObjectId,
    placed: Point<i32, Logical>,
    previous: Size<i32, Logical>,
    requested: Size<i32, Logical>,
    /// Per axis, whether the step moved the low edge — the only case where the
    /// position has to absorb what the client did with the size.
    moved_low: (bool, bool),
}

/// What a non-interactive resize clamps a request to: the client's declared
/// min/max, or the usable-chrome floor for a stand-in that has no client to
/// declare any.
pub(crate) fn resize_constraints(element: &StageWindow) -> SizeConstraints {
    match element {
        StageWindow::Client(window) => SizeConstraints::for_window(window),
        StageWindow::Suspended(_) => SizeConstraints::for_suspended(),
    }
}

impl DriftWm {
    /// Enter a client-side resize: drop fit/fill membership, seed the
    /// `ResizeState` that `handle_resize_commit` repositions from, and put the
    /// toplevel into `Resizing` without `Maximized`.
    ///
    /// Every resize entry point runs exactly this, and the `Maximized` unset is
    /// the part that is easy to forget: after the fit clear above, a `Maximized`
    /// left set is one the client can never shed — its restore button would
    /// dispatch an unmaximize_request that `unfit_window` silently drops.
    ///
    /// Callers own everything around this: bail checks, edge inference, cursor,
    /// cluster snapshot, grab construction and installation. Nothing here sends
    /// a configure — the pending state rides out on the grab's first motion.
    pub(crate) fn begin_client_resize(
        &mut self,
        window: &Window,
        wl_surface: &WlSurface,
        edges: xdg_toplevel::ResizeEdge,
        initial_window_size: Size<i32, Logical>,
        pinned_initial_screen_pos: Option<Point<i32, Logical>>,
    ) {
        // A camera flight still running when this grab installs would read as
        // resize input once a tick warps the pointer into it (same trap
        // `arm_interactive_move` guards against for moves).
        self.cancel_animations_everywhere();
        self.stage.clear_fit(window);
        self.stage.clear_fill(window);

        with_states(wl_surface, |states| {
            states
                .data_map
                .get_or_insert(|| RefCell::new(ResizeState::Idle))
                .replace(ResizeState::Resizing {
                    edges,
                    initial_screen_pos: pinned_initial_screen_pos,
                    last_committed_size: initial_window_size,
                });
        });

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
                state.states.unset(xdg_toplevel::State::Maximized);
            });
        }
    }

    /// Resize `element` to `size` and place its content top-left at `loc`: the
    /// whole of a non-interactive resize, with no grab behind it to finish the
    /// job. The anchor is the caller's to choose, since it is `loc` — the IPC
    /// verb passes the position that preserves the visual center. `raise` maps
    /// through the z-order (what an action fired at the focused element wants)
    /// rather than writing the position in place (what IPC wants for a
    /// stand-in, which `msg move` also declines to re-raise).
    ///
    /// `size` is the caller's request already clamped through
    /// [`resize_constraints`], and both it and `loc` are written optimistically:
    /// the placement follows the size we are *asking* for, not the one the
    /// client eventually commits. A client that cell-snaps therefore lands up to
    /// half its snap error off the anchor — bounded, and re-derived from the
    /// settled size by the next call. Nothing here waits on a round-trip, so no
    /// commit-time seam can strand the operation half-applied.
    pub(crate) fn resize_element_to(
        &mut self,
        element: &StageWindow,
        size: Size<i32, Logical>,
        loc: Point<i32, Logical>,
        raise: bool,
    ) {
        if size == configured_element_size(element) && self.stage.position_of(element) == Some(loc)
        {
            return;
        }

        // This writes a placement directly, and an owed recenter fires on the
        // client's next differently-sized commit — the very commit the configure
        // below provokes — mapping the window back to a pre-exit center. The
        // grab path can lean on `placement_owns_size` instead; there is no grab
        // here to read it.
        self.drop_owed_recenter(element);

        match element {
            StageWindow::Client(window) => {
                // Seeded while the stage still holds the pre-resize rect, which
                // is what `animate_window_geometry` chases from.
                self.animate_window_geometry(window, size, None);
                self.stage.clear_fit(window);
                self.stage.clear_fill(window);
                self.send_size_configure(window, size);
            }
            // No client to configure: the size cell is the stand-in's authority,
            // and the render pass rebuilds its chrome from it.
            StageWindow::Suspended(s) => s.size.set(size),
        }

        if raise {
            self.map_window(element.clone(), loc, false);
        } else {
            self.stage.set_position(element, loc);
        }

        match element {
            StageWindow::Client(window) => {
                // Cache the requested rect directly rather than through
                // `refresh_stable_snap_rect`, which builds from `geometry()` —
                // still the pre-ack size, so it would cache the new position
                // against the old dimensions. Left stale on a grow, every later
                // commit reads as "grew past settled" and
                // `reflow_grown_snapped_window` relocates the window out of the
                // center this call just promised.
                if let Some(surface) = window.wl_surface() {
                    let bar = self.window_ssd_bar(window) as f64;
                    let bw = self.window_border_width(&surface) as f64;
                    self.stable_snap_rects.insert(
                        surface.id(),
                        driftwm::layout::snap::SnapRect {
                            x_low: loc.x as f64 - bw,
                            x_high: loc.x as f64 + size.w as f64 + bw,
                            y_low: loc.y as f64 - bar - bw,
                            y_high: loc.y as f64 + size.h as f64 + bw,
                        },
                    );
                }
                // Optimistic like the placement, and for the same reason: no
                // commit-time seam can confirm this size, since a client is free
                // to commit back the size it already had. Left stale, the next
                // fit or fullscreen round-trip silently hands back the
                // pre-resize rect. Client-only — a stand-in entry must carry no
                // restore size, or `Stage::replace` hands it to the window
                // adopted into its slot and the real adopt size is dropped.
                self.stage.set_restore_size(window, size);
            }
            // A stand-in's canvas rect is durable session state, and no commit
            // follows to arm the write for us.
            StageWindow::Suspended(_) => self.session_store_mark_dirty(),
        }

        // Frost above the resized element goes stale otherwise, and nothing else
        // bumps this: `handle_resize_commit` returns early outside a grab, and a
        // stand-in has no commit to reach it with at all.
        self.render.blur_geometry_generation += 1;
    }

    /// Walk the `dir` edge of the focused element outward by `step` px — inward
    /// for a negative `step` — holding the opposite edge where it is. A diagonal
    /// moves both of its named edges; an axis the direction does not name keeps
    /// its size untouched.
    ///
    /// The shift that keeps the opposite edge still comes off the delta the
    /// clamp actually granted, not the one asked for; what the client refuses
    /// of that is taken back by [`Self::reconcile_step_anchor`] before the
    /// next step measures anything.
    pub(crate) fn step_resize_focused(&mut self, dir: &Direction, step: i32) {
        let Some(element) = self.focused_element().filter(|e| self.is_canvas_window(e)) else {
            return;
        };
        // A grab recomputes the rect absolutely from its own start anchors on
        // every motion tick, so a step landing mid-drag is erased by the next
        // one — and the settling tail still owes a top/left compensation this
        // would strip the witnesses for.
        if self.element_under_interactive_grab(&element) {
            return;
        }
        let Some(loc) = self.reconcile_step_anchor(&element) else {
            return;
        };

        let current = configured_element_size(&element);
        // `to_unit_vec` is Y-down, the convention stage positions use, so a
        // negative component names the edge that sits lower in coordinate order:
        // left on x, top on y.
        let (ux, uy) = dir.to_unit_vec();
        let dw = (ux.abs() * step as f64).round() as i32;
        let dh = (uy.abs() * step as f64).round() as i32;
        let clamped = resize_constraints(&element)
            .clamp(current.w.saturating_add(dw), current.h.saturating_add(dh));
        // An axis the step leaves alone keeps its size, clamp included: a client
        // is free to declare a `max_size` its current size already violates
        // (after a fit, say), and `grow-window up` must not silently narrow the
        // window and jump its right edge.
        let size = Size::from((
            if dw == 0 { current.w } else { clamped.0 },
            if dh == 0 { current.h } else { clamped.1 },
        ));
        // Only an axis whose low edge is the named one moves the position, and it
        // absorbs the whole granted delta.
        let anchor_shift = Point::from((
            if ux < 0.0 { size.w - current.w } else { 0 },
            if uy < 0.0 { size.h - current.h } else { 0 },
        ));
        let loc = loc - anchor_shift;
        self.resize_element_to(&element, size, loc, true);
        self.resize_step_anchor = element.wl_surface().map(|surface| StepAnchor {
            surface: surface.id(),
            placed: loc,
            previous: current,
            requested: size,
            moved_low: (ux < 0.0, uy < 0.0),
        });
    }

    /// Take back the part of the previous step's placement the client refused,
    /// and answer with the position the next step should measure from.
    ///
    /// A step places optimistically, so the anchor edge only lands where it was
    /// promised if the client commits the size it was handed. One that keeps its
    /// own size, or snaps to a cell, leaves the anchor off by the difference —
    /// and unlike the absolute IPC verb, a relative step has no rect to re-derive
    /// from, so left alone the error compounds once per key repeat.
    ///
    /// The record only applies while the element still sits where the step left
    /// it and its size lies between what it had and what it was asked for.
    /// Anything else — a nudge, a drag, a fit — means the promise is void.
    fn reconcile_step_anchor(&mut self, element: &StageWindow) -> Option<Point<i32, Logical>> {
        let loc = self.stage.position_of(element)?;
        let Some(anchor) = self.resize_step_anchor.take() else {
            return Some(loc);
        };
        let committed = configured_element_size(element);
        let between = |c: i32, a: i32, b: i32| c >= a.min(b) && c <= a.max(b);
        if element.wl_surface().map(|s| s.id()) != Some(anchor.surface)
            || loc != anchor.placed
            || !between(committed.w, anchor.previous.w, anchor.requested.w)
            || !between(committed.h, anchor.previous.h, anchor.requested.h)
        {
            return Some(loc);
        }

        let corrected = Point::from((
            if anchor.moved_low.0 {
                loc.x + (anchor.requested.w - committed.w)
            } else {
                loc.x
            },
            if anchor.moved_low.1 {
                loc.y + (anchor.requested.h - committed.h)
            } else {
                loc.y
            },
        ));
        if corrected != loc {
            self.stage.set_position(element, corrected);
        }
        Some(corrected)
    }

    /// Send a plain sized configure — no Maximized/Fullscreen/Resizing state, so
    /// the window resizes in place. Tiled stays set from map time, so clients
    /// keep suppressing their own chrome, and the explicit size keeps SCTK from
    /// reading "Tiled + None" as "hold current size".
    pub(crate) fn send_size_configure(&self, window: &Window, size: Size<i32, Logical>) {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        toplevel.with_pending_state(|state| {
            state.size = Some(size);
            // Load-bearing for `fill_window` and `resize_element_to`, which
            // clear the fit membership they may have found: a Maximized
            // outliving that is one the client can never shed — its restore
            // button, or a panel's foreign-toplevel unset_maximized, dispatches
            // an unmaximize_request that `unfit_window` drops on the absent
            // saved size. Inert for `unfill_window`: `set_fit` clears fill and
            // `set_fill` clears fit, so a filled window is never also a fit one.
            state.states.unset(xdg_toplevel::State::Maximized);
        });
        toplevel.send_configure();
    }
}
