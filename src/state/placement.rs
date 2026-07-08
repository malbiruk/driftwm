use std::collections::HashSet;

use smithay::desktop::Window;
use smithay::utils::{Logical, Size};
use smithay::wayland::seat::WaylandFocus;

use super::{AUTO_PLACE_CLUSTER_THRESHOLD, DriftWm};

impl DriftWm {
    /// Spawn pos for `placement = "cursor"`: center the visual frame
    /// (titlebar + content) on the cursor, clamped to the active output's
    /// usable rect. `bar` is SSD title-bar height (0 for CSD/minimal).
    pub fn cursor_placement_pos(
        &self,
        window_size: Size<i32, Logical>,
        bar: i32,
    ) -> Option<(i32, i32)> {
        self.active_output()?;

        let pointer = self.seat.get_pointer()?;
        let cursor = pointer.current_location();

        // usable area is screen-local; convert to canvas coords.
        let usable = self.get_usable_area();
        let zoom = self.zoom();
        let camera = self.camera();
        let cx_min = camera.x + usable.loc.x as f64 / zoom;
        let cy_min = camera.y + usable.loc.y as f64 / zoom;
        let cx_max = camera.x + (usable.loc.x + usable.size.w) as f64 / zoom;
        let cy_max = camera.y + (usable.loc.y + usable.size.h) as f64 / zoom;

        // Target: visual frame center on cursor. Frame spans [loc.y - bar, loc.y + h],
        // so frame center = loc.y + (h - bar)/2  →  loc.y = cursor.y - h/2 + bar/2.
        let bar_f = bar as f64;
        let raw_x = cursor.x - window_size.w as f64 / 2.0;
        let raw_y = cursor.y - window_size.h as f64 / 2.0 + bar_f / 2.0;

        // Clamp so the frame stays fully inside the usable canvas rect.
        // For oversized windows, .max() keeps the upper bound >= lower bound
        // (the top sticks at the usable edge; the bottom overflows).
        let max_x = (cx_max - window_size.w as f64).max(cx_min);
        let max_y = (cy_max - window_size.h as f64).max(cy_min + bar_f);
        let x = raw_x.clamp(cx_min, max_x);
        let y = raw_y.clamp(cy_min + bar_f, max_y);

        Some((x.round() as i32, y.round() as i32))
    }

    /// Spawn pos for `placement = "auto"`: snap-place adjacent to the focused
    /// window's cluster. Returns content top-left (shifted down by `bar` so
    /// the visual frame snaps to the neighbor). `None` on no eligible focus
    /// or no valid placement; caller falls back to center.
    ///
    /// `new_window` is excluded from anchor search and obstacle list. Without
    /// the skip we'd anchor the new window against itself, since by the time
    /// this runs `new_window` is already at the viewport center and front of
    /// `focus_history`.
    pub fn auto_placement_pos(
        &self,
        new_window: &Window,
        new_size: Size<i32, Logical>,
        bar: i32,
    ) -> Option<(i32, i32)> {
        // Anchor = keyboard focus at `new_toplevel` time, snapshotted before
        // focus was reassigned to the new surface. `None` (or absent) means
        // no anchor and caller falls back to center.
        let new_surface = new_window.wl_surface()?.into_owned();
        let focused = self.auto_anchor_snapshot.get(&new_surface)?.as_ref()?;
        let widget = focused
            .wl_surface()
            .and_then(|s| driftwm::config::applied_rule(&s))
            .is_some_and(|r| r.widget);
        let is_fs = self.is_window_fullscreen(focused);
        if widget || is_fs || self.is_pinned(focused) {
            return None;
        }

        // Only anchor when enough of the focused window is visible that the
        // user is plausibly working on its cluster; otherwise they intend a
        // fresh cluster and caller falls back to center.
        if !self.window_visible_at_least(focused, AUTO_PLACE_CLUSTER_THRESHOLD) {
            return None;
        }

        self.place_adjacent_to(focused, new_window, new_size, bar)
    }

    /// Geometry-only placement of `placing` (content sized `new_size`, SSD
    /// bar `bar`) adjacent to `anchor`'s snap cluster, treating every other
    /// mapped window as an obstacle. Returns the content top-left in canvas
    /// coords, or `None` when `anchor` is ineligible or no slot fits.
    pub fn place_adjacent_to(
        &self,
        anchor: &Window,
        placing: &Window,
        new_size: Size<i32, Logical>,
        bar: i32,
    ) -> Option<(i32, i32)> {
        let placing_surface = placing.wl_surface()?.into_owned();

        // Widgets sit visually below windows (wallpaper-like) — neither
        // anchors nor obstacles for auto placement.
        let mut rects: Vec<driftwm::layout::auto_placement::Rect> = Vec::new();
        let mut eligible: HashSet<usize> = HashSet::new();
        let mut anchor_idx: Option<usize> = None;
        for w in self.stage.windows() {
            if w == placing {
                continue;
            }
            let widget = w
                .wl_surface()
                .and_then(|s| driftwm::config::applied_rule(&s))
                .is_some_and(|r| r.widget);
            let is_fs = self.is_window_fullscreen(w);
            if widget || is_fs || self.is_pinned(w) {
                continue;
            }
            let Some(loc) = self.stage.position_of(w) else {
                continue;
            };
            let size = w.geometry().size;
            let b = self.window_ssd_bar(w);
            let bw = w.wl_surface().map_or(0, |s| self.window_border_width(&s)) as f64;
            let idx = rects.len();
            rects.push(driftwm::layout::auto_placement::Rect {
                x: loc.x as f64 - bw,
                y: (loc.y - b) as f64 - bw,
                w: size.w as f64 + 2.0 * bw,
                h: (size.h + b) as f64 + 2.0 * bw,
            });
            eligible.insert(idx);
            if w == anchor {
                anchor_idx = Some(idx);
            }
        }
        let anchor_idx = anchor_idx?;

        let new_bw = self.window_border_width(&placing_surface) as f64;
        let new_w_f = new_size.w as f64 + 2.0 * new_bw;
        let new_h_f = (new_size.h + bar) as f64 + 2.0 * new_bw;

        let camera = self.camera();
        let zoom = self.zoom();
        let vc_screen = self.usable_center_screen();
        let vc = (camera.x + vc_screen.x / zoom, camera.y + vc_screen.y / zoom);

        let pos = driftwm::layout::auto_placement::place_auto(
            &rects,
            anchor_idx,
            &eligible,
            new_w_f,
            new_h_f,
            vc,
            self.config.snap_gap,
        )?;

        // place_auto returns frame top-left (outside border, above title bar);
        // shift inward to content top-left.
        let bw_i = new_bw as i32;
        Some((
            pos.0.round() as i32 + bw_i,
            pos.1.round() as i32 + bw_i + bar,
        ))
    }

    /// Placement for a new window that would otherwise land on top of a
    /// fullscreen window. Anchors to the fullscreen window's *saved*
    /// (pre-fullscreen) canvas rect so the new window tucks in beside it, off
    /// the fullscreen viewport — culled now, revealed cleanly on exit.
    ///
    /// Output-scoped: only fires when the new window's own output is the
    /// fullscreen one, so a window on a monitor you're actively using still
    /// places normally. `None` when there's no fullscreen window to tuck behind.
    pub fn fullscreen_background_pos(
        &self,
        new_window: &Window,
        new_size: Size<i32, Logical>,
        bar: i32,
    ) -> Option<(i32, i32)> {
        let new_surface = new_window.wl_surface()?.into_owned();

        // The fullscreen window to tuck behind: the map-time focus anchor when
        // it is itself fullscreen (auto/center/cursor all snapshot it), else the
        // active output's fullscreen window. The fallback covers a no-anchor map
        // and the case where focus sits on some other window while an output is
        // fullscreen — the pointer's output is the one the window lands on.
        let anchor = self
            .auto_anchor_snapshot
            .get(&new_surface)
            .and_then(|o| o.clone());
        let output = anchor
            .as_ref()
            .and_then(|a| {
                let name = self.stage.fullscreen_output_of(a).map(str::to_owned)?;
                self.output_by_name(&name)
            })
            .or_else(|| {
                let out = self.active_output()?;
                self.is_output_fullscreen(&out).then_some(out)
            })?;
        let fs = self.stage.fullscreen_on(&output.name())?;

        // Anchor rect = the fullscreen window's canvas home, reconstructed as a
        // frame rect (borders + SSD bar) exactly like `auto_placement_pos`.
        let fs_bw = fs
            .window
            .wl_surface()
            .map_or(0, |s| self.window_border_width(&s)) as f64;
        let fs_bar = self.window_ssd_bar(&fs.window);
        let anchor_rect = driftwm::layout::auto_placement::Rect {
            x: fs.saved_location.x as f64 - fs_bw,
            y: (fs.saved_location.y - fs_bar) as f64 - fs_bw,
            w: fs.saved_size.w as f64 + 2.0 * fs_bw,
            h: (fs.saved_size.h + fs_bar) as f64 + 2.0 * fs_bw,
        };

        let mut rects = vec![anchor_rect];
        let mut eligible: HashSet<usize> = HashSet::new();
        eligible.insert(0);

        for w in self.stage.windows() {
            if w == new_window || w == &fs.window {
                continue;
            }
            let widget = w
                .wl_surface()
                .and_then(|s| driftwm::config::applied_rule(&s))
                .is_some_and(|r| r.widget);
            if widget || self.is_window_fullscreen(w) || self.is_pinned(w) {
                continue;
            }
            let Some(loc) = self.stage.position_of(w) else {
                continue;
            };
            let size = w.geometry().size;
            let b = self.window_ssd_bar(w);
            let bw = w.wl_surface().map_or(0, |s| self.window_border_width(&s)) as f64;
            let idx = rects.len();
            rects.push(driftwm::layout::auto_placement::Rect {
                x: loc.x as f64 - bw,
                y: (loc.y - b) as f64 - bw,
                w: size.w as f64 + 2.0 * bw,
                h: (size.h + b) as f64 + 2.0 * bw,
            });
            eligible.insert(idx);
        }

        let new_bw = self.window_border_width(&new_surface) as f64;
        let new_w_f = new_size.w as f64 + 2.0 * new_bw;
        let new_h_f = (new_size.h + bar) as f64 + 2.0 * new_bw;

        // Bias toward the fullscreen window's home center; its live location is
        // the fullscreen viewport, which is irrelevant to canvas placement.
        let vc = (
            anchor_rect.x + anchor_rect.w / 2.0,
            anchor_rect.y + anchor_rect.h / 2.0,
        );

        let bw_i = new_bw as i32;
        if let Some(pos) = driftwm::layout::auto_placement::place_auto(
            &rects,
            0,
            &eligible,
            new_w_f,
            new_h_f,
            vc,
            self.config.snap_gap,
        ) {
            return Some((
                pos.0.round() as i32 + bw_i,
                pos.1.round() as i32 + bw_i + bar,
            ));
        }

        // No adjacent slot: park it just below the fullscreen window's saved
        // home so it doesn't overlap where that window restores to on exit.
        let gap = self.config.snap_gap.round() as i32;
        Some((
            fs.saved_location.x,
            fs.saved_location.y + fs.saved_size.h + gap,
        ))
    }

    /// Walk a spawn position in title-bar-sized diagonal steps until it
    /// doesn't sit on top of an existing window.
    pub fn cascade_position(&self, mut pos: (i32, i32), skip: &Window) -> (i32, i32) {
        let step = self.config.decorations.title_bar_height;
        loop {
            let dominated = self.stage.windows().any(|w| {
                w != skip
                    && self
                        .space
                        .element_location(w)
                        .is_some_and(|loc| (loc.x - pos.0).abs() <= 2 && (loc.y - pos.1).abs() <= 2)
            });
            if !dominated {
                break pos;
            }
            pos.0 += step;
            pos.1 += step;
        }
    }
}
