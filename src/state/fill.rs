use smithay::{
    desktop::Window,
    reexports::wayland_server::Resource,
    utils::{Logical, Point, Size},
    wayland::seat::WaylandFocus,
};

use super::{DriftWm, StageWindow, output_state};
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

        // Grow from the last configured size, not the last committed one: a fill
        // dispatched while fullscreen runs right after the exit guard, and the
        // exit only *sends* the smaller configure, so committed geometry still
        // reports the viewport — a seed already spanning the whole usable area
        // with nothing to grow into.
        let cur_size = super::configured_window_size(window);
        let cur_loc = self.stage.position_of(window)?;
        let current = super::fit::snap_rect_at(cur_loc, cur_size, bar, bw);
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
        // committing so `fill_window` won't record a restore point. Compared
        // against the same restore-authoritative size the seed used.
        if new_size == cur_size && new_loc == cur_loc {
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
        // A widget or pinned window has no free canvas space to grow into.
        if self.is_pinned(window) || config::applied_rule(&wl_surface).is_some_and(|r| r.widget) {
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
        // Chromium the latter shrinks on each round-trip (see fit_window). On a
        // window this fill is taking out of fit, the pre-fit size is the only one
        // worth coming back to, and it has to be read before the clear below —
        // a fit window that never carried a restore size would otherwise record
        // the fit size as the rect an unfill restores.
        let pre_fit_size = self.stage.fit_saved_size(window);
        let saved_size = pre_fit_size
            .or_else(|| self.stage.restore_size(window))
            .unwrap_or_else(|| window.geometry().size);
        let Some(saved_pos) = self.stage.position_of(window) else {
            return;
        };
        // The pre-fit size against the fit rect's top-left describes no rect the
        // window ever had, and an unfill would drop it in the viewport's corner.
        // Re-derive the position around the fit rect's visual center, exactly as
        // `unfit_window` restores, so the two halves come from one epoch again.
        let saved_pos = if pre_fit_size.is_some() {
            let bar = self.window_ssd_bar(window);
            let center = super::visual_frame_center(
                saved_pos,
                super::configured_window_size(window),
                bar as f64,
            );
            super::frame_loc_for_center(center, saved_size, bar)
        } else {
            saved_pos
        };

        // A fill places the window absolutely.
        self.drop_owed_recenter(window);

        self.animate_window_geometry(window, new_size, None);
        self.send_size_configure(window, new_size);
        self.map_window(window.clone(), new_loc, false);
        self.stage.set_fill(window, saved_pos, saved_size);
        // The fill above inherited the pre-fit size as its own restore point and
        // the window no longer occupies the fit rect, so fit membership would
        // only claim a rect nothing holds — and hand the same size back twice.
        self.stage.clear_fit(window);
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
        if window.wl_surface().is_none() {
            return;
        }
        let Some((saved_pos, saved_size)) = self.stage.take_fill_saved(window) else {
            return;
        };

        // Visual center of the saved geometry; the settle completion re-derives
        // the loc from it via `frame_loc_for_center`.
        let bar = self.window_ssd_bar(window) as f64;
        let target_center = super::visual_frame_center(saved_pos, saved_size, bar);

        self.animate_window_geometry(window, saved_size, None);
        self.send_size_configure(window, saved_size);

        // The restore below maps before the client acks, so the animation has a
        // single target and travels the filled rect → restored rect as one leg.
        // Deferring it left the chase shrinking the window anchored at the filled
        // rect's top-left, then jumping when the settle fired. The filled rect
        // `fill_window` cached is stale, so refresh.
        self.establish_exit_placement(
            &StageWindow::Client(window.clone()),
            saved_pos,
            saved_size,
            target_center,
            true,
        );
    }

    pub fn toggle_fill_window(&mut self, window: &Window) {
        if self.stage.is_fill(window) {
            self.unfill_window(window);
        } else {
            self.fill_window(window);
        }
    }
}
