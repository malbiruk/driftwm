use smithay::{
    desktop::Window,
    reexports::wayland_server::Resource,
    utils::{Logical, Point, Size},
    wayland::seat::WaylandFocus,
};

use super::{DriftWm, PendingRecenter, StageWindow, output_state};
use crate::grabs::SizeConstraints;
use driftwm::canvas::{ScreenPos, screen_to_canvas};
use driftwm::config;
use driftwm::layout::snap::SnapRect;

/// The window's target content size + map location after filling the free space
/// around it, or `None` when filling is a no-op (already fills the space, or the
/// window sits entirely outside the usable area).
struct FillGeometry {
    new_loc: Point<i32, Logical>,
    new_size: Size<i32, Logical>,
    frame: SnapRect,
}

impl DriftWm {
    fn compute_fill_geometry(&self, window: &Window) -> Option<FillGeometry> {
        let surface = window.wl_surface()?;
        let output = self.output_for_window(window)?;

        // Usable screen rect → canvas rect via the output's own camera/zoom.
        let usable = self.usable_area_on(&output);
        let (camera, zoom) = {
            let os = output_state(&output);
            (os.camera, os.zoom)
        };
        let top_left = screen_to_canvas(
            ScreenPos(Point::from((usable.loc.x as f64, usable.loc.y as f64))),
            camera,
            zoom,
        )
        .0;
        let bottom_right = screen_to_canvas(
            ScreenPos(Point::from((
                (usable.loc.x + usable.size.w) as f64,
                (usable.loc.y + usable.size.h) as f64,
            ))),
            camera,
            zoom,
        )
        .0;
        let bounds = SnapRect {
            x_low: top_left.x,
            x_high: bottom_right.x,
            y_low: top_left.y,
            y_high: bottom_right.y,
        };

        let current = self.snap_rect_for(&StageWindow::Client(window.clone()))?;
        #[allow(clippy::mutable_key_type)]
        let obstacles: Vec<SnapRect> = self
            .all_windows_with_snap_rects()
            .into_iter()
            .filter(|(w, _)| w != window)
            .map(|(_, r)| r)
            .collect();

        // `window_snap_rect` inflates content by the SSD bar (top) and border
        // (all sides); mirror that inflation onto the client's content-space
        // size hints so the constraints live in the same frame space as the
        // rects, preserving the 0 = unconstrained sentinel.
        let bar = self.window_ssd_bar(window);
        let bw = self.window_border_width(&surface);
        let inflate = |v: i32, extra: i32| -> f64 { if v > 0 { (v + extra) as f64 } else { 0.0 } };
        let constraints = SizeConstraints::for_window(window);
        let min_size = (
            inflate(constraints.min.w, 2 * bw),
            inflate(constraints.min.h, 2 * bw + bar),
        );
        let max_size = (
            inflate(constraints.max.w, 2 * bw),
            inflate(constraints.max.h, 2 * bw + bar),
        );

        let filled = driftwm::layout::fill::fill_rect(
            current,
            &obstacles,
            bounds,
            self.config.snap_gap,
            min_size,
            max_size,
        )?;

        // Invert `window_snap_rect` back to a content size + top-left location.
        let bw = bw as f64;
        let bar = bar as f64;
        // Deflating a sliver free-region by borders/bar can go non-positive; a
        // client size must stay at least 1px on each axis.
        let new_size = Size::from((
            ((filled.x_high - filled.x_low - 2.0 * bw).round() as i32).max(1),
            ((filled.y_high - filled.y_low - 2.0 * bw - bar).round() as i32).max(1),
        ));
        let new_loc = Point::from((
            (filled.x_low + bw).round() as i32,
            (filled.y_low + bar + bw).round() as i32,
        ));

        // No-op: the window already fills its free space. Return without
        // committing so `fill_window` won't record a restore point.
        let cur_loc = self.stage.position_of(window)?;
        if new_size == window.geometry().size && new_loc == cur_loc {
            return None;
        }
        Some(FillGeometry {
            new_loc,
            new_size,
            frame: filled,
        })
    }

    pub fn fill_window(&mut self, window: &Window) {
        let Some(wl_surface) = window.wl_surface() else {
            return;
        };
        // A fit (maximized) window is fit's business; a widget or pinned window
        // has no free canvas space to grow into.
        if self.is_pinned(window)
            || self.stage.is_fit(window)
            || config::applied_rule(&wl_surface).is_some_and(|r| r.widget)
        {
            return;
        }

        let Some(FillGeometry {
            new_loc,
            new_size,
            frame,
        }) = self.compute_fill_geometry(window)
        else {
            return;
        };

        // Use the tracked restore size rather than window.geometry().size — for
        // Chromium the latter shrinks on each round-trip (see fit_window).
        let saved_size = self
            .stage
            .restore_size(window)
            .unwrap_or_else(|| window.geometry().size);
        let Some(saved_pos) = self.stage.position_of(window) else {
            return;
        };

        self.send_size_configure(window, new_size);
        self.map_window(window.clone(), new_loc, false);
        self.stage.set_fill(window, saved_pos, saved_size);
        // Cache the filled rect directly: `geometry().size` is still pre-ack, so
        // `refresh_stable_snap_rect` would cache stale dimensions. Unlike plain
        // fit, the filled rect is the window's new in-place identity — leaving
        // the pre-fill rect cached makes every later commit read as "grew past
        // settled" (a perpetual reflow scan once something clears the fill
        // state, and a reflow translation if the fill kept an unresolvable
        // overlap), and skews the spatial-focus queries built on this cache.
        self.stable_snap_rects.insert(wl_surface.id(), frame);
    }

    pub fn unfill_window(&mut self, window: &Window) {
        let Some(wl_surface) = window.wl_surface() else {
            return;
        };
        let Some((saved_pos, saved_size)) = self.stage.take_fill_saved(window) else {
            return;
        };

        // Visual center of the saved geometry; the settle completion re-derives
        // the loc from it via `frame_loc_for_center`.
        let bar = self.window_ssd_bar(window) as f64;
        let target_center = super::visual_frame_center(saved_pos, saved_size, bar);

        let pre_exit_size = window.geometry().size;
        self.send_size_configure(window, saved_size);

        if saved_size == pre_exit_size {
            // The exit configure re-sends the size the client already has, so no
            // commit with a changed size will arrive to trigger the recenter —
            // restore the position immediately instead.
            self.map_window(window.clone(), saved_pos, false);
            self.refresh_stable_snap_rect(&StageWindow::Client(window.clone()));
        } else {
            self.pending_recenter.insert(
                wl_surface.id(),
                PendingRecenter {
                    target_center,
                    pre_exit_size,
                },
            );
        }
    }

    pub fn toggle_fill_window(&mut self, window: &Window) {
        if self.stage.is_fill(window) {
            self.unfill_window(window);
        } else {
            self.fill_window(window);
        }
    }

    /// Send a plain sized configure — no Maximized/Fullscreen/Resizing state, so
    /// the window resizes in place. Tiled stays set from map time, so clients
    /// keep suppressing their own chrome, and the explicit size keeps SCTK from
    /// reading "Tiled + None" as "hold current size".
    fn send_size_configure(&self, window: &Window, size: Size<i32, Logical>) {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        toplevel.with_pending_state(|state| {
            state.size = Some(size);
        });
        toplevel.send_configure();
    }
}
