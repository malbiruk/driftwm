//! The state every client-side resize starts from, shared by the four entry
//! points that can start one: the pointer, the client's own
//! `xdg_toplevel.resize`, touch, and the trackpad gesture — plus the
//! non-interactive resizes behind `driftwm msg resize` and the `grow-window` /
//! `shrink-window` steps, which own the whole operation because no grab follows
//! to finish it.

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
use driftwm::canvas::Chrome;
use driftwm::config::Direction;

/// A size a non-interactive resize asked a client for, and the committed sizes
/// that still read as an answer to it.
///
/// One record answers "what did I last ask for, and is it still live?" for both
/// halves of a non-interactive resize: the size the next request measures itself
/// against, and — for a step — the placement that request promised. They cannot
/// disagree about whether the client has spoken, because it is the same entry
/// they both read. It is overwritten by the next request on the same surface and
/// dropped with the surface; nothing else retires it, so a step that turns out to
/// have nothing to do leaves an absolute request's protection standing.
pub(crate) struct PendingResize {
    requested: Size<i32, Logical>,
    /// Committed geometry when the request went out — the other end of the band
    /// a commit has to land in to still count as an answer to it.
    at_request: Size<i32, Logical>,
    /// Set by a `grow-window` / `shrink-window` step, which places optimistically
    /// and has no absolute rect to re-derive from. The IPC verb leaves it `None`.
    step: Option<StepPlacement>,
}

/// The placement a step promised, kept until the next step can check it against
/// what the client actually committed.
struct StepPlacement {
    placed: Point<i32, Logical>,
    /// Per axis, whether the step moved the low edge — the only case where the
    /// position has to absorb what the client did with the size.
    moved_low: (bool, bool),
}

impl PendingResize {
    /// Whether this is still the request the window is answering.
    ///
    /// Two things retire it. Another path configuring a size of its own — a
    /// resize grab's motion tick, a fit, a fullscreen — makes *its* configure the
    /// live one, and this request history. And a commit outside the band between
    /// the geometry the request went out against and the size it asked for is the
    /// client sizing itself (a font change, a GTK `resize()`) rather than
    /// answering.
    ///
    /// Inside that band sit the client that has not repainted yet, the one that
    /// took the size exactly, and the terminal that rounded to a whole cell — and
    /// also, indistinguishably, a client that resized *itself* to a size in the
    /// same range. That one is misread as an answer, so an identical request
    /// after it is a no-op and the window keeps the size it chose (a request for
    /// any other size still reaches it). The trade is deliberate: reading a
    /// rounded answer as the client sizing itself instead would re-derive every
    /// repeated request from the rounded size and walk every cell-snapping
    /// terminal by half its rounding per call.
    fn is_live(&self, window: &Window) -> bool {
        let committed = window.geometry().size;
        let between = |c: i32, a: i32, b: i32| c >= a.min(b) && c <= a.max(b);
        super::configured_window_size(window) == self.requested
            && between(committed.w, self.at_request.w, self.requested.w)
            && between(committed.h, self.at_request.h, self.requested.h)
    }
}

/// The rect a `grow-window` / `shrink-window` step starts from: where the window
/// sits once the part of the last step's placement the client refused is taken
/// back, and the size that placement assumes.
struct StepStart {
    loc: Point<i32, Logical>,
    size: Size<i32, Logical>,
}

/// Whether the client still owes an ack for a size we sent: a pending configure
/// carrying a real size that differs from committed geometry. The compositor's
/// benign zero-size configures ("client picks its own size") never count.
///
/// Only ever an "is a configure in flight" test. smithay prunes the entry at the
/// `ack_configure` request rather than at the commit that applies it, so a false
/// answer here does *not* mean the client's geometry is current — see
/// [`DriftWm::requested_element_size`].
pub(crate) fn owes_a_configured_size(window: &Window) -> bool {
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

/// What a non-interactive resize clamps a request to: the client's declared
/// min/max, or the usable-chrome floor for a stand-in that has no client to
/// declare any. Content-space either way — the stand-in's floor is stated
/// against its visible frame, so `chrome` deflates it back down here.
pub(crate) fn resize_constraints(element: &StageWindow, chrome: Chrome) -> SizeConstraints {
    match element {
        StageWindow::Client(window) => SizeConstraints::for_window(window),
        StageWindow::Suspended(_) => SizeConstraints::for_suspended(chrome),
    }
}

impl DriftWm {
    /// The size an *absolute* resize measures its delta and its anchor against:
    /// the request still outstanding on this element, else the size its
    /// placement assumes.
    ///
    /// Ack state cannot answer this. smithay prunes a pending configure at the
    /// `ack_configure` request, and real clients ack as soon as they process the
    /// event and then go on committing their old size until the next repaint —
    /// so "nothing pending" is not "the client's geometry is current". Measured
    /// that way, a re-run of the same request re-derives from the pre-resize
    /// size and shifts the window by another half-delta every call, forever
    /// against a client that never takes the size at all.
    ///
    /// The outstanding request is the answer instead, for exactly as long as it
    /// stays live: an identical request is then a no-op rather than a second
    /// shift, and the placement this one wrote keeps its meaning.
    pub(crate) fn requested_element_size(&self, element: &StageWindow) -> Size<i32, Logical> {
        element
            .client()
            .and_then(|window| {
                let record = self.pending_resizes.get(&window.wl_surface()?.id())?;
                record.is_live(window).then_some(record.requested)
            })
            .unwrap_or_else(|| self.placed_element_size(element))
    }

    /// The size the element's placement currently assumes — what a *relative*
    /// step measures its delta and its anchor against.
    ///
    /// While a configure is still unacked the placement was written for the size
    /// we asked for, and a step has to keep walking from there or a held key
    /// grows the window at the client's round-trip rate instead of the repeat
    /// rate. Once the client acks it has spoken for the placement: measure from
    /// what it committed, and [`Self::step_start_rect`] takes back the part of the
    /// offer it declined. The two halves have to answer alike — measuring from the
    /// offer while correcting toward the commit inflates the anchor edge once per
    /// repeat.
    fn placed_element_size(&self, element: &StageWindow) -> Size<i32, Logical> {
        match element {
            // A stand-in's size cell is written synchronously, so its placement
            // never assumes anything the compositor is not already drawing.
            StageWindow::Client(window) if owes_a_configured_size(window) => {
                super::configured_window_size(window)
            }
            _ => element.geometry().size,
        }
    }

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
    ///
    /// The request is recorded so the next one measures against it rather than
    /// against geometry the client has not repainted yet; see
    /// [`Self::requested_element_size`]. `step_axes` names the axes whose low edge
    /// a step moved, and is what turns that record into the promise
    /// [`Self::step_start_rect`] answers against — `None` for the absolute verb,
    /// which re-derives from its own rect instead. It rides in here rather than
    /// being patched on afterwards so a request that never went out (a step with
    /// nothing to grant returns below) cannot promise anything.
    pub(crate) fn resize_element_to(
        &mut self,
        element: &StageWindow,
        size: Size<i32, Logical>,
        loc: Point<i32, Logical>,
        raise: bool,
        step_axes: Option<(bool, bool)>,
    ) {
        // "Nothing to do" is the caller's own question: a step is done when the
        // size its placement assumes is already the target, an absolute request
        // when it repeats the one still outstanding. Reading the request here for
        // a step would swallow the re-offer a client that declined the last one
        // still has coming.
        let previous = match step_axes {
            Some(_) => self.placed_element_size(element),
            None => self.requested_element_size(element),
        };
        if size == previous && self.stage.position_of(element) == Some(loc) {
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
            // and the render pass rebuilds its chrome from it. Deliberately
            // un-animated, unlike the client arm — a stand-in's animation entry
            // is position-only by construction (the compose pass puts the whole
            // slide in the render offset and never stretches it), so a geometry
            // chase would land on the new size in one frame anyway.
            StageWindow::Suspended(s) => s.size.set(size),
        }

        if raise {
            self.map_window(element.clone(), loc, false);
        } else {
            self.stage.set_position(element, loc);
        }

        match element {
            StageWindow::Client(window) => {
                if let Some(surface) = window.wl_surface() {
                    self.pending_resizes.insert(
                        surface.id(),
                        PendingResize {
                            requested: size,
                            at_request: window.geometry().size,
                            step: step_axes.map(|moved_low| StepPlacement {
                                placed: loc,
                                moved_low,
                            }),
                        },
                    );
                }
                // Cache the rect directly rather than through
                // `refresh_stable_snap_rect`, which builds from `geometry()` —
                // still the pre-ack size, so it would cache the new position
                // against the old dimensions. Left stale on a grow, every later
                // commit reads as "grew past settled" and
                // `reflow_grown_snapped_window` relocates the window out of the
                // center this call just promised.
                //
                // Which is why the cache takes the *larger* of the two sizes per
                // axis: a client may decline a shrink, and a settled footprint
                // the live one exceeds is that same "grew past settled" read from
                // the other side. It bounds only what this resize itself put in
                // play — a window still committing a size outside both (mid-settle
                // out of a fullscreen, say) is left to the reflow's own gates. The
                // price on the other side is that a *granted* shrink leaves the
                // pre-shrink footprint cached: nothing refreshes it on a plain
                // commit, so the window keeps the wider cluster identity until
                // some settled event (a grab end, a fit) re-derives it.
                let settled = Size::from((size.w.max(previous.w), size.h.max(previous.h)));
                self.cache_stable_snap_rect(window, loc, settled);
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
    /// of that is taken back by [`Self::step_start_rect`] before the next step
    /// measures anything.
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
        let Some(StepStart { loc, size: current }) = self.step_start_rect(&element) else {
            return;
        };

        // `to_unit_vec` is Y-down, the convention stage positions use, so a
        // negative component names the edge that sits lower in coordinate order:
        // left on x, top on y.
        let (ux, uy) = dir.to_unit_vec();
        let dw = (ux.abs() * step as f64).round() as i32;
        let dh = (uy.abs() * step as f64).round() as i32;
        let clamped = resize_constraints(&element, self.element_chrome(&element))
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
        // absorbs the whole granted delta. Saturating, because an unconstrained
        // client granted a pathological `resize_step` shifts by nearly the whole
        // `i32` range from a canvas position that can already be anywhere in it.
        let loc = Point::from((
            if ux < 0.0 {
                loc.x.saturating_sub(size.w - current.w)
            } else {
                loc.x
            },
            if uy < 0.0 {
                loc.y.saturating_sub(size.h - current.h)
            } else {
                loc.y
            },
        ));
        self.resize_element_to(&element, size, loc, true, Some((ux < 0.0, uy < 0.0)));
    }

    /// Where the next step starts from: the size the element's placement assumes,
    /// and the position with the part of the last step's placement the client
    /// refused taken back.
    ///
    /// A step places optimistically, so the anchor edge only lands where it was
    /// promised if the client commits the size it was handed. One that keeps its
    /// own size, or snaps to a cell, leaves the anchor off by the difference —
    /// and unlike the absolute IPC verb, a relative step has no rect to re-derive
    /// from, so left alone the error compounds once per key repeat.
    ///
    /// The correction is only computed, never written: [`Self::resize_element_to`]
    /// writes it along with the step's own placement, so the geometry animation
    /// seeds from where the window is drawn rather than from a position it never
    /// occupied. It reaches the stage even when the step is fully clamped, since
    /// a correction still pending makes the position differ and the primitive's
    /// no-op check does not fire — and if the step is a no-op on both counts, the
    /// record stands and the same answer comes back next time.
    ///
    /// Seeding the compensation `handle_resize_commit` already performs, instead
    /// of correcting here, is not available: that path only runs off a non-`Idle`
    /// `ResizeState`, and `element_under_interactive_grab` reads any such state as
    /// a live grab — which would make the next step, and the IPC verb, bail for as
    /// long as the compensation stayed owed.
    ///
    /// The promise only applies while the element still sits where the step left
    /// it and its size still reads as an answer to what it was asked for.
    /// Anything else — a nudge, a drag, a self-resize — voids it.
    fn step_start_rect(&self, element: &StageWindow) -> Option<StepStart> {
        let loc = self.stage.position_of(element)?;
        let size = self.placed_element_size(element);
        // Nothing of ours outstanding: a stand-in, or a size some other path
        // configured, which owns its own placement.
        let Some(window) = element.client() else {
            return Some(StepStart { loc, size });
        };
        let Some(surface) = window.wl_surface() else {
            return Some(StepStart { loc, size });
        };
        let Some(record) = self.pending_resizes.get(&surface.id()) else {
            return Some(StepStart { loc, size });
        };
        let loc = match &record.step {
            Some(step) if record.is_live(window) && loc == step.placed => Point::from((
                if step.moved_low.0 {
                    loc.x.saturating_add(record.requested.w - size.w)
                } else {
                    loc.x
                },
                if step.moved_low.1 {
                    loc.y.saturating_add(record.requested.h - size.h)
                } else {
                    loc.y
                },
            )),
            _ => loc,
        };
        Some(StepStart { loc, size })
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
