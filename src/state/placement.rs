use std::collections::HashSet;

use smithay::desktop::Window;
use smithay::utils::{Logical, Size};
use smithay::wayland::seat::WaylandFocus;

use driftwm::canvas::Chrome;

use super::{AUTO_PLACE_CLUSTER_THRESHOLD, DriftWm, StageWindow};

impl DriftWm {
    /// Spawn pos for `placement = "cursor"`: center the visual frame
    /// (titlebar + content + border) on the cursor, clamped to the active
    /// output's usable rect. Returns a content top-left.
    pub fn cursor_placement_pos(
        &self,
        window_size: Size<i32, Logical>,
        chrome: Chrome,
    ) -> Option<(i32, i32)> {
        // A locked session keeps *screen* coords in `current_location` (see
        // `SessionLockHandler::lock`), which read as canvas would spawn the
        // window at a position nobody pointed at. Fall back to centering.
        if self.session_lock.is_locked() {
            return None;
        }
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
        // A border is symmetric and cancels out of the center; only the clamps
        // below have to know about it.
        let bar_f = chrome.bar as f64;
        let bw = chrome.border as f64;
        let raw_x = cursor.x - window_size.w as f64 / 2.0;
        let raw_y = cursor.y - window_size.h as f64 / 2.0 + bar_f / 2.0;

        // Clamp so the frame stays fully inside the usable canvas rect.
        // For oversized windows, .max() keeps the upper bound >= lower bound
        // (the top sticks at the usable edge; the bottom overflows).
        let max_x = (cx_max - window_size.w as f64 - bw).max(cx_min + bw);
        let max_y = (cy_max - window_size.h as f64 - bw).max(cy_min + bar_f + bw);
        let x = raw_x.clamp(cx_min + bw, max_x);
        let y = raw_y.clamp(cy_min + bar_f + bw, max_y);

        Some((x.round() as i32, y.round() as i32))
    }

    /// Spawn pos for `placement = "auto"`: snap-place adjacent to a cluster
    /// already in view. Returns content top-left (shifted down by `bar` so
    /// the visual frame snaps to the neighbor).
    ///
    /// The anchor is the focus snapshotted at `new_toplevel` time while it is
    /// *usable* — an eligible canvas element, visible enough that the user is
    /// plausibly working on its cluster. When it isn't (panned away, on another
    /// output, widget/pinned/fullscreen, or gone) the nearest element in view
    /// stands in for it, so a new window still joins what you are looking at.
    /// `None` — caller falls back to center — when the user deliberately cleared
    /// focus by clicking empty canvas, when nothing in view qualifies, or when
    /// no slot fits.
    ///
    /// `new_window` is excluded from anchor search and obstacle list. Without
    /// the skip we'd anchor the new window against itself, since by the time
    /// this runs `new_window` is already at the viewport center and front of
    /// `focus_history`.
    pub fn auto_placement_pos(
        &self,
        new_window: &Window,
        new_size: Size<i32, Logical>,
        chrome: Chrome,
    ) -> Option<(i32, i32)> {
        // Anchor = keyboard focus at `new_toplevel` time, snapshotted before
        // focus was reassigned to the new surface. A missing entry means the
        // anchor's surface died before placement — the same user situation as
        // having had no focus at all, so both take the fallback below.
        let new_surface = new_window.wl_surface()?.into_owned();
        let snapshot = self
            .auto_anchor_snapshot
            .get(&new_surface)
            .and_then(|a| a.as_ref())
            .filter(|a| {
                self.is_canvas_window(*a)
                    && self.window_visible_at_least(*a, AUTO_PLACE_CLUSTER_THRESHOLD)
            });
        if let Some(focused) = snapshot {
            return self.place_adjacent_to(focused, new_window, new_size, chrome);
        }

        // Clicking empty canvas is a deliberate blank slate: honor it and let
        // the caller center. Any other unusable anchor falls back to view.
        if self.suppress_auto_anchor {
            return None;
        }

        let anchor = self.nearest_auto_anchor(new_window)?;
        self.place_adjacent_to(&anchor, new_window, new_size, chrome)
    }

    /// The element auto placement anchors to when the focus snapshot can't
    /// serve: the one nearest the viewport center among sufficiently visible
    /// canvas elements, suspended stand-ins included.
    fn nearest_auto_anchor(&self, placing: &Window) -> Option<StageWindow> {
        let origin = self.viewport_center_canvas();
        self.stage
            .windows()
            // Bottom→top reversed: `min_by` keeps the first minimum, so the
            // top-most of overlapping elements wins ties — the one the user
            // is looking at.
            .rev()
            .filter(|w| {
                // `placing` is already on the stage at the viewport center, so
                // without this skip it scores distance 0 and anchors itself;
                // `place_adjacent_to` then skips it while building rects, finds
                // no anchor, and the fallback silently no-ops.
                *w != placing
                    && self.is_canvas_window(*w)
                    && !self.awaiting_placement(w)
                    && self.window_visible_at_least(*w, AUTO_PLACE_CLUSTER_THRESHOLD)
            })
            .map(|w| {
                // Deliberately without `CenterNearest`'s closest-== origin
                // substitution (it exists there for a direction vector): using
                // it here would score an element spanning the center by its
                // distant center point, losing to a small one at the edge.
                let closest = self.element_closest_point(origin, w);
                let dist_sq = (closest.x - origin.x).powi(2) + (closest.y - origin.y).powi(2);
                (w, dist_sq)
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(w, _)| w.clone())
    }

    /// True while `w` is still parked on the viewport-center seed `new_toplevel`
    /// gave it (`pending_center`): a `size` window rule holds it there through a
    /// client roundtrip with real, fully-visible geometry, so it would otherwise
    /// score distance 0 and win the anchor with a rect it's about to leave.
    /// Stand-ins have no surface, so they're always eligible.
    fn awaiting_placement(&self, w: &StageWindow) -> bool {
        w.wl_surface()
            .is_some_and(|s| self.pending_center.contains(&*s))
    }

    /// Geometry-only placement of `placing` (content sized `new_size`, wearing
    /// `chrome`) adjacent to `anchor`'s snap cluster — a live window or a
    /// focused suspended stand-in — treating every other mapped window as an
    /// obstacle. Returns the content top-left in canvas coords, or `None` when
    /// `anchor` is ineligible or no slot fits.
    pub fn place_adjacent_to(
        &self,
        anchor: &StageWindow,
        placing: &Window,
        new_size: Size<i32, Logical>,
        chrome: Chrome,
    ) -> Option<(i32, i32)> {
        placing.wl_surface()?;

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
            // A window awaiting a deferred adopt occupies no visible ground: it
            // is not drawn where it sits and the flush moves it, so reserving
            // that ground would push the window being placed off into free
            // canvas for nothing.
            if widget || is_fs || self.is_pinned(w) || self.hidden_by_deferred_adopt(w) {
                continue;
            }
            let Some(loc) = self.stage.position_of(w) else {
                continue;
            };
            let size = w.geometry().size;
            let b = self.window_ssd_bar(w);
            let bw = self.element_border_width(w) as f64;
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

        let frame = chrome.frame_size(new_size);
        let (new_w_f, new_h_f) = (frame.w as f64, frame.h as f64);

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
        let content = chrome.content_loc((pos.0.round() as i32, pos.1.round() as i32).into());
        Some((content.x, content.y))
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
        chrome: Chrome,
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
            let bw = self.element_border_width(w) as f64;
            let idx = rects.len();
            rects.push(driftwm::layout::auto_placement::Rect {
                x: loc.x as f64 - bw,
                y: (loc.y - b) as f64 - bw,
                w: size.w as f64 + 2.0 * bw,
                h: (size.h + b) as f64 + 2.0 * bw,
            });
            eligible.insert(idx);
        }

        let frame = chrome.frame_size(new_size);
        let (new_w_f, new_h_f) = (frame.w as f64, frame.h as f64);

        // Bias toward the fullscreen window's home center; its live location is
        // the fullscreen viewport, which is irrelevant to canvas placement.
        let vc = (
            anchor_rect.x + anchor_rect.w / 2.0,
            anchor_rect.y + anchor_rect.h / 2.0,
        );

        if let Some(pos) = driftwm::layout::auto_placement::place_auto(
            &rects,
            0,
            &eligible,
            new_w_f,
            new_h_f,
            vc,
            self.config.snap_gap,
        ) {
            let content = chrome.content_loc((pos.0.round() as i32, pos.1.round() as i32).into());
            return Some((content.x, content.y));
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
                        .stage
                        .position_of(w)
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
