//! The recenter a window owes from a fullscreen, fit or fill exit it has not
//! acked: registering it, discarding it, and re-aiming it.
//!
//! The exit registers a `pending_recenter` so the client's next
//! differently-sized commit lands the window on its pre-exit visual center.
//! Anything that establishes a new placement in the meantime has to discard
//! that promise, or it fires later and undoes the placement — unless the new
//! placement is itself expressed as a center, which can be re-aimed instead.

use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point, Size};
use smithay::wayland::seat::WaylandFocus;

use super::{DriftWm, PendingRecenter, StageWindow};

impl DriftWm {
    /// Discard a recenter this window still owes from a fullscreen/fit/fill
    /// exit it has not acked.
    ///
    /// Call this from anywhere that establishes a new placement. The owed
    /// recenter fires on the client's next differently-sized commit and maps
    /// the window back to its pre-exit center — undoing the placement being
    /// established here, at an unpredictable later moment. Dropping beats
    /// merely skipping: an entry left owed also gates
    /// `reflow_grown_snapped_window` forever.
    pub(crate) fn drop_owed_recenter<W: WaylandFocus>(&mut self, window: &W) {
        if let Some(surface) = window.wl_surface() {
            self.pending_recenter.remove(&surface.id());
        }
    }

    /// Place `window` so its visual center lands on the window-rule point
    /// `(x, y)` — what `msg move` and the bookmark binding both ask for —
    /// re-aiming any recenter the window still owes rather than dropping it.
    ///
    /// A rule point *is* the visual center, and the map onto one is
    /// size-independent: `rule_to_content` subtracts half the frame per axis and
    /// `visual_frame_center` adds the same half straight back, leaving `(x, -y)`
    /// whatever the size. So a request that arrives mid-settle can re-aim the
    /// owed recenter without knowing the size the client is still resizing into,
    /// and the settle re-derives the location from the size it actually commits.
    /// Dropping the entry instead would strand the window half the size delta
    /// from the request with nothing left to correct it.
    ///
    /// Residual error against the location a same-size direct move would pick is
    /// up to a pixel per axis, and a whole one in half the cases that have any:
    /// an odd *frame* dimension (content plus the bar, on y) leaves a ½ in the
    /// settle's intermediate, and `as i32` truncates toward zero rather than
    /// down, so the half is lost where that intermediate is positive (a positive
    /// `x`, a negative rule `y`) and kept where it is negative.
    ///
    /// The re-aimed entry keeps gating `reflow_grown_snapped_window` until that
    /// commit lands, the cost [`Self::drop_owed_recenter`] describes; a client
    /// that never resizes holds it only until unmap clears it.
    pub(crate) fn map_window_to_rule_point(
        &mut self,
        window: &Window,
        x: i32,
        y: i32,
        activate: bool,
    ) {
        // Moving re-anchors the window, invalidating any fill restore point.
        self.stage.clear_fill(window);

        let owed = window
            .wl_surface()
            .filter(|s| self.pending_recenter.contains_key(&s.id()));
        // Mid-settle the client is still committing its pre-exit buffer, so the
        // size the exit configured is the authority for this provisional
        // placement. Outside a settle committed geometry is, since a
        // client-initiated resize never updates the size we last configured.
        let size = if owed.is_some() {
            super::configured_window_size(window)
        } else {
            window.geometry().size
        };
        let chrome = self.window_chrome(window);
        self.map_window(
            window.clone(),
            driftwm::canvas::rule_to_content(x, y, size, chrome),
            activate,
        );

        if let Some(surface) = owed {
            // The rule point is the visual center outright, so the settle needs
            // no size to re-derive it — only the Y-up flip.
            let center = Point::from((x as f64, -y as f64));
            if let Some(pending) = self.pending_recenter.get_mut(&surface.id()) {
                pending.target_center = center;
            }
        }
    }

    /// Place `window` where its fullscreen, fit or fill exit restores it, and
    /// settle the recenter that exit owes. The shared tail of all three exits;
    /// what runs before it (which configure, and whether the geometry animation
    /// is seeded before or after the map) is each exit's own business.
    ///
    /// `target_center` is the visual center the settle re-derives a location
    /// from once the client resizes, and is a parameter rather than something
    /// derived from `map_loc`: `unfit_window` maps to a location already
    /// truncated out of its center by `frame_loc_for_center`, and re-deriving
    /// would shift its center by up to a pixel per axis.
    ///
    /// `refresh_snap_rect` is true for the fit and fill exits, whose entries
    /// cached a fit/fill rect that this exit invalidates, and false for the
    /// fullscreen exit: `enter_fullscreen` caches nothing, so the cached rect
    /// is still the pre-fullscreen one this exit hands back.
    pub(crate) fn establish_exit_placement(
        &mut self,
        window: &StageWindow,
        map_loc: Point<i32, Logical>,
        saved_size: Size<i32, Logical>,
        target_center: Point<f64, Logical>,
        refresh_snap_rect: bool,
    ) {
        // The size the client is still committing. Neither the exit configure
        // nor the map below touches committed geometry, so every caller reads
        // the same value whether it captured it before or after those. An
        // enter->exit inside one frame can leave the client's first pre-exit
        // frame in flight; if its size differs from `saved_size` it completes
        // the settle early against a stale footprint.
        let pre_exit_size = window.geometry().size;

        self.map_window(window.clone(), map_loc, false);

        if saved_size == pre_exit_size {
            // The exit configure re-sends the size the client already has, so no
            // commit with a changed size will arrive to trigger the recenter —
            // the position mapped above is already final. A preceding exit can
            // have owed one already, so drop rather than merely skip.
            self.drop_owed_recenter(window);
            if refresh_snap_rect {
                // Refresh the cache the recenter completion would otherwise
                // have refreshed.
                self.refresh_stable_snap_rect(window);
            }
        } else if let Some(surface) = window.wl_surface() {
            // The client keeps committing pre-exit-sized frames until it acks
            // the restore configure; those stale-sized commits also read as
            // "grown past settled" in the reflow, which this entry gates.
            self.pending_recenter.insert(
                surface.id(),
                PendingRecenter {
                    target_center,
                    pre_exit_size,
                },
            );
        }
    }
}
