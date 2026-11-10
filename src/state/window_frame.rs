//! Window frame geometry: resolving the [`Chrome`] an element wears, and the
//! conversions between a content top-left and the visual center of the frame
//! that chrome surrounds.
//!
//! [`visual_frame_center`] and [`frame_loc_for_center`] are inverses, shared by
//! navigation, fit, fill, and the fullscreen-exit settle so the formula cannot
//! drift between them.

use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Size};
use smithay::wayland::seat::WaylandFocus;

use driftwm::canvas::Chrome;
use driftwm::window_ext::WindowExt;

use super::{DriftWm, StageWindow};

impl DriftWm {
    pub fn window_ssd_bar<W: WaylandFocus + WindowExt>(&self, window: &W) -> i32 {
        // Every stand-in draws the same textless bar (a CSD-origin one shrinks
        // its body under it), so a suspended element always carries the bar
        // height regardless of origin.
        if window.is_suspended() {
            return self.config.decorations.title_bar_height;
        }
        window
            .wl_surface()
            .filter(|s| {
                self.decorations
                    .contains_key(&crate::decorations::DecorationKey::Surface(s.id()))
            })
            .map_or(0, |_| self.config.decorations.title_bar_height)
    }

    /// Hide or show `window`'s driftwm title bar by flipping its decoration
    /// between `server` and `minimal`: the bar goes, the border, corners and
    /// shadow stay. Both are `ServerSide` on the wire, so the client is not told.
    /// The content keeps its place; the bar is drawn above it.
    ///
    /// Left alone: a window that negotiated client-side decoration or draws no
    /// chrome (`client` / `none`), so an app with its own header bar never gets a
    /// second one. The override is written into the window's stored rule, so it is
    /// not saved with the session and is gone when the window closes.
    ///
    /// The frame changes by the bar height, so the cached snap rect is refreshed;
    /// neighbours in a snap cluster are not re-snapped.
    pub fn toggle_window_titlebar(&mut self, window: &Window) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;

        let Some(surface) = window.wl_surface().map(|s| s.into_owned()) else {
            return;
        };
        if window
            .toplevel()
            .is_some_and(|t| t.with_pending_state(|s| s.decoration_mode) == Some(Mode::ClientSide))
        {
            return;
        }
        let key = crate::decorations::DecorationKey::Surface(surface.id());
        let applied = driftwm::config::applied_rule(&surface);
        let mode = driftwm::config::effective_decoration_mode(
            applied.as_ref().and_then(|r| r.decoration.as_ref()),
            &self.config.decorations.default_mode,
        );
        let has_bar = self.decorations.contains_key(&key);

        let new_mode = match (has_bar, mode) {
            (true, _) => driftwm::config::DecorationMode::Minimal,
            (false, driftwm::config::DecorationMode::Minimal) => {
                driftwm::config::DecorationMode::Server
            }
            _ => return,
        };

        smithay::wayland::compositor::with_states(&surface, |states| {
            states.data_map.insert_if_missing_threadsafe(|| {
                std::sync::Mutex::new(driftwm::config::AppliedWindowRule::default())
            });
            states
                .data_map
                .get::<std::sync::Mutex<driftwm::config::AppliedWindowRule>>()
                .unwrap()
                .lock()
                .unwrap()
                .decoration = Some(new_mode);
        });

        if has_bar {
            self.decorations.remove(&key);
            // With the bar gone the corners are clipped by the compositor, so the
            // surface's opaque region has to give them up too.
            self.trim_opaque_corners(&surface);
        } else if window.geometry().size.w > 0 {
            self.decorations.insert(
                key,
                crate::decorations::WindowDecoration::new(
                    window.geometry().size.w,
                    true,
                    &self.config.decorations,
                ),
            );
        }
        self.refresh_stable_snap_rect(&StageWindow::Client(window.clone()));
        self.mark_all_dirty();
    }

    /// Border width for an element with no surface to resolve a per-rule
    /// override against — a suspended window. Uses the global default mode's
    /// width, matching what a relaunched client would get before its rule
    /// re-applies.
    pub fn default_border_width(&self) -> i32 {
        let mode =
            driftwm::config::effective_decoration_mode(None, &self.config.decorations.default_mode);
        driftwm::config::effective_border_width(None, mode, &self.config.decorations)
    }

    /// The chrome an element wears. Every user-facing size and position is
    /// expressed against this frame, not the content rect the compositor stores —
    /// see [`driftwm::canvas::Chrome`].
    ///
    /// The bar's authority is the decorations map rather than the decoration mode
    /// a rule or a negotiation *implies*, because a client that calls `set_mode`
    /// after its first sized commit moves the map without moving that. The border
    /// has no such split — rule and config resolve it, as they do for the render
    /// path.
    ///
    /// Fullscreen is deliberately **not** suppressed here even though the render
    /// path suppresses it: most callers want the chrome the window wears on the
    /// canvas, and the fit path has to size against the frame it comes back to.
    /// The reads that can be asked about a fullscreen window use
    /// [`Self::reported_chrome`].
    pub fn element_chrome<W: WaylandFocus + WindowExt>(&self, w: &W) -> Chrome {
        if w.is_suspended() {
            return self.suspended_chrome();
        }
        Chrome {
            bar: self.window_ssd_bar(w),
            border: w.wl_surface().map_or(0, |s| self.window_border_width(&s)),
        }
    }

    /// [`Self::element_chrome`] as a user-facing surface reports it: nothing at
    /// all while fullscreen, where the compositor suppresses bar, border and
    /// shadow alike, so the window's frame is exactly its content.
    ///
    /// The `move` and `resize` read arms need this: they answer before their
    /// `is_canvas_window` guards, so a fullscreen window reaches them. Every
    /// other boundary either rejects one or never sees one.
    pub fn reported_chrome<W: WaylandFocus + WindowExt>(&self, w: &W) -> Chrome
    where
        StageWindow: PartialEq<W>,
    {
        if self.is_window_fullscreen(w) {
            return Chrome::NONE;
        }
        self.element_chrome(w)
    }

    /// The chrome a window is about to wear, for the commit that maps it: its
    /// decoration entry does not exist yet, so the bar is predicted from the
    /// decoration mode this same commit resolved. `effective == Server` is the
    /// exact condition that creates the entry further down, so the prediction
    /// and the entry agree by construction.
    ///
    /// A client that changes its decoration mode in a *later* request moves the
    /// entry without moving this, and reflows visibly when it does. The border
    /// needs no prediction: it resolves from the applied rule and config alone,
    /// which is what every later frame reads.
    pub(crate) fn mapping_chrome(
        &self,
        surface: &WlSurface,
        effective: &driftwm::config::DecorationMode,
    ) -> Chrome {
        Chrome {
            bar: if matches!(effective, driftwm::config::DecorationMode::Server) {
                self.config.decorations.title_bar_height
            } else {
                0
            },
            border: self.window_border_width(surface),
        }
    }

    /// [`Self::element_chrome`]'s stand-in arm, for the callers that hold no
    /// element to resolve against: every stand-in draws the same bar and has no
    /// surface to carry a per-rule border override, so the answer is uniform.
    pub fn suspended_chrome(&self) -> Chrome {
        Chrome {
            bar: self.config.decorations.title_bar_height,
            border: self.default_border_width(),
        }
    }

    /// Border width for any stage element: the per-rule width for a client, the
    /// global default for a surfaceless stand-in.
    pub fn element_border_width(&self, w: &StageWindow) -> i32 {
        match w {
            StageWindow::Client(c) => c.wl_surface().map_or(0, |s| self.window_border_width(&s)),
            StageWindow::Suspended(_) => self.default_border_width(),
        }
    }

    /// Per-window border width, resolving rule override against
    /// `[decorations] border_width`. Returns 0 when the effective decoration
    /// mode is `None` (hard veto — per-window overrides ignored).
    pub fn window_border_width(&self, surface: &WlSurface) -> i32 {
        let applied = driftwm::config::applied_rule(surface);
        let mode = driftwm::config::effective_decoration_mode(
            applied.as_ref().and_then(|r| r.decoration.as_ref()),
            &self.config.decorations.default_mode,
        );
        driftwm::config::effective_border_width(applied.as_ref(), mode, &self.config.decorations)
    }

    /// Visual center accounting for SSD title bar above content. Sized from
    /// [`configured_window_size`], so a center taken right after a fullscreen
    /// exit describes the restored window rather than the viewport the client is
    /// still reporting.
    pub fn window_visual_center(&self, window: &Window) -> Option<Point<f64, Logical>> {
        let loc = self.stage.position_of(window)?;
        let size = configured_window_size(window);
        let bar = self.window_ssd_bar(window) as f64;
        Some(visual_frame_center(loc, size, bar))
    }
}

/// Center of the visual frame (content plus the SSD title-bar strip above it)
/// from a content top-left, content size, and bar height. Inverse of
/// [`frame_loc_for_center`]. Shared by `window_visual_center`, `nav_center`, and
/// the fit/fill/fullscreen exit settles so the formula can't drift.
pub(crate) fn visual_frame_center(
    loc: Point<i32, Logical>,
    size: Size<i32, Logical>,
    bar: f64,
) -> Point<f64, Logical> {
    Point::from((
        loc.x as f64 + size.w as f64 / 2.0,
        loc.y as f64 - bar + (size.h as f64 + bar) / 2.0,
    ))
}

/// The size a window will have once it acks everything already configured: the
/// last size we sent, else its committed geometry.
///
/// `Window::geometry()` reports the last *committed* buffer, which lags a
/// configure round-trip. That lag is invisible most of the time but not after a
/// fullscreen exit: the exit only sends the smaller configure, so a geometry
/// action dispatched right behind it (the `execute_action` guard exits first)
/// would size and center against the still-reported viewport. The server's
/// pending state is what the window is becoming, so prefer it.
pub(crate) fn configured_window_size(window: &Window) -> Size<i32, Logical> {
    window
        .toplevel()
        .and_then(|toplevel| toplevel.with_pending_state(|state| state.size))
        .filter(|size| size.w > 0 && size.h > 0)
        .unwrap_or_else(|| window.geometry().size)
}

/// Content top-left that places a frame of `size` (plus its `bar` strip) so its
/// visual center lands on `center`. Inverse of [`visual_frame_center`]; used by
/// the fit exit and the pending-recenter completion to re-place a window around
/// a preserved center.
pub(crate) fn frame_loc_for_center(
    center: Point<f64, Logical>,
    size: Size<i32, Logical>,
    bar: i32,
) -> Point<i32, Logical> {
    let total_h = size.h + bar;
    Point::from((
        (center.x - size.w as f64 / 2.0) as i32,
        (center.y - total_h as f64 / 2.0) as i32 + bar,
    ))
}
