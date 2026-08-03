use smithay::{
    desktop::Window,
    reexports::wayland_server::Resource,
    utils::{Logical, Point, Size},
    wayland::seat::WaylandFocus,
};

use super::{DriftWm, PendingView, StageWindow, ZoomAnimationAnchor, output_state};
use driftwm::config;
use driftwm::stage::ElementId;
use driftwm::window_ext::WindowExt;

/// Build a `SnapRect` from a hypothetical canvas position, size, SSD
/// bar, and border width — used by `fit_window_snapped` /
/// `unfit_window_snapped` to describe the primary's pre-op and post-op
/// geometry, both for the exact per-edge deltas and for the primary's
/// own entry in the rect list those paths cluster over
/// (`snap_rects_with_primary`), and by the adopt's owed-rect payoff for a
/// size the client has been configured with but not yet committed.
/// Every term is widened before it is added: a non-interactive resize pairs a
/// size the client asked for — unbounded, since `resize_step` and `msg resize`
/// both take any `i32` — with a canvas coordinate that can already sit anywhere
/// in the range, and the sum of the two does not fit in an `i32`.
pub(super) fn snap_rect_at(
    loc: Point<i32, Logical>,
    size: Size<i32, Logical>,
    bar: i32,
    border_width: i32,
) -> driftwm::layout::snap::SnapRect {
    let bw = border_width as f64;
    driftwm::layout::snap::SnapRect {
        x_low: loc.x as f64 - bw,
        x_high: loc.x as f64 + size.w as f64 + bw,
        y_low: loc.y as f64 - bar as f64 - bw,
        y_high: loc.y as f64 + size.h as f64 + bw,
    }
}

/// Fit geometry for a primary window: the canvas position, size, camera
/// target, and visual center the primary would have if fitted to the
/// viewport right now. Shared between `fit_window` (which applies it) and
/// `fit_window_snapped` (which feeds the exact post-fit rect into the
/// cluster-shift helper, so per-edge deltas account for the half-pixel
/// truncation in `target_camera.* as i32`).
struct FitGeometry {
    new_loc: Point<i32, Logical>,
    target_size: Size<i32, Logical>,
    target_camera: Point<f64, Logical>,
    visual_center: Point<f64, Logical>,
}

impl DriftWm {
    fn compute_fit_geometry(&self, window: &Window) -> FitGeometry {
        let usable = self.get_usable_area();
        let gap = self.config.snap_gap;
        let chrome = self.element_chrome(window);
        // The gap bounds the *visual frame*, so the content inside it gives up the
        // whole chrome; deflating by the bar alone overflows the usable area by a
        // border per side.
        let target_size = chrome.content_size(Size::from((
            usable.size.w - (2.0 * gap) as i32,
            usable.size.h - (2.0 * gap) as i32,
        )));
        let usable_center_x = usable.loc.x as f64 + usable.size.w as f64 / 2.0;
        let usable_center_y = usable.loc.y as f64 + usable.size.h as f64 / 2.0;
        // `window_visual_center` sizes from the last configure, so a fit pressed
        // while fullscreen centers on the restored window instead of the viewport
        // the client is still reporting (see `configured_window_size`).
        let visual_center = self.window_visual_center(window).unwrap_or_default();
        let target_camera = Point::from((
            visual_center.x - usable_center_x,
            visual_center.y - usable_center_y,
        ));
        let new_loc = chrome.content_loc(Point::from((
            target_camera.x as i32 + usable.loc.x + gap as i32,
            target_camera.y as i32 + usable.loc.y + gap as i32,
        )));
        FitGeometry {
            new_loc,
            target_size,
            target_camera,
            visual_center,
        }
    }

    pub fn fit_window(&mut self, window: &Window) {
        let Some(wl_surface) = window.wl_surface() else {
            return;
        };
        if self.is_pinned(window) || config::applied_rule(&wl_surface).is_some_and(|r| r.widget) {
            return;
        }

        // Use the tracked restore size rather than window.geometry().size —
        // for Chromium the latter shrinks on each unfit round-trip.
        let current_size = self
            .stage
            .restore_size(window)
            .unwrap_or_else(|| window.geometry().size);

        let FitGeometry {
            new_loc,
            target_size,
            target_camera,
            visual_center: center,
        } = self.compute_fit_geometry(window);

        // A fit establishes its own placement.
        self.drop_owed_recenter(window);

        // A freeze this fit arms of its own accord is the only one allowed to
        // hold its pan back, and a request-carrying (re)start is what bumps the
        // generation. A freeze already running belongs to some other action,
        // which never promised to release this camera.
        let generation_before = self
            .stage
            .id_of(window)
            .and_then(|id| self.window_animations.generation_of(id));

        // The fit position is passed in because the stage still holds the
        // pre-fit one — the map below is what writes it.
        self.animate_window_geometry(window, target_size, Some(new_loc));
        window.enter_fit_configure(target_size);
        self.map_window(window.clone(), new_loc, false);
        // After the map — set_fit needs the window's stage entry, which the
        // map guarantees even for a window that wasn't staged before.
        self.stage.set_fit(window, current_size);
        // Fit translates the window and unfit restores by visual center, so a
        // pre-fit fill's saved position would be permanently stale — drop it.
        self.stage.clear_fill(window);
        // Don't refresh `stable_snap_rects` here — the fit canvas position
        // snap-touches nothing, so close-time `cluster_of` would degrade to
        // `{self}`. The pre-fit rect is the window's cluster identity.
        // (Snapped fit follows a different rule — see `fit_window_snapped`.)

        // Raise, focus, animate camera + zoom to 1.0
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        self.raise_and_focus(window, serial);
        self.set_overview_return(None);
        let anchor = ZoomAnimationAnchor {
            canvas: center,
            screen: self.usable_center_screen(),
        };
        let frozen = self.stage.id_of(window).filter(|id| {
            self.window_animations.generation_of(*id) != generation_before
                && self.window_animations.start_held(*id)
        });
        let Some(output) = self.active_output() else {
            return;
        };
        // This fit owns the output's view from here on, whether it parks its pan
        // or applies it below. A pan parked on some other window's freeze was
        // promised to an action this one has just superseded — including one
        // already released and waiting out a grab.
        self.window_animations.drop_pending_views_on(&output.name());
        self.deferred_views.remove(&output.name());
        let Some(id) = frozen else {
            let mut os = output_state(&output);
            os.momentum.stop();
            os.zoom_animation_anchor = Some(anchor);
            os.camera_target = Some(target_camera);
            os.zoom_target = Some(1.0);
            return;
        };
        // Whatever navigation was in flight does not wait for the freeze too:
        // left running it would fly for the whole freeze and then be jumped out
        // of by the fit's own pan, and clearing it also makes the stamp below
        // exact — nothing legitimately moves this camera between here and the
        // release.
        self.cancel_animations_on(&output);
        let (staged_camera, staged_zoom) = {
            let os = output_state(&output);
            (os.camera, os.zoom)
        };
        self.window_animations.stage_pending_view(
            id,
            PendingView {
                output: output.name(),
                camera: target_camera,
                zoom: 1.0,
                anchor,
                staged_camera,
                staged_zoom,
            },
        );
    }

    pub fn unfit_window(&mut self, window: &Window) {
        if window.wl_surface().is_none() {
            return;
        }

        let Some(saved_size) = self.stage.take_fit_saved_size(window) else {
            return;
        };

        // Resize in-place around the preserved visual center. Sized from the last
        // configure, so an unfit dispatched out of a fullscreen exit centers on
        // the restored (fit-sized) window, not the viewport still being reported.
        let center = self.window_visual_center(window).unwrap_or_default();
        let bar = self.window_ssd_bar(window);
        let new_loc = super::frame_loc_for_center(center, saved_size, bar);

        self.animate_window_geometry(window, saved_size, None);
        window.exit_fit_configure(saved_size);
        // The fit rect `fit_window_snapped` cached is stale, so refresh.
        self.establish_exit_placement(
            &StageWindow::Client(window.clone()),
            new_loc,
            saved_size,
            center,
            true,
        );
    }

    pub fn toggle_fit_window(&mut self, window: &Window) {
        if self.stage.is_fit(window) {
            self.unfit_window(window);
        } else {
            self.fit_window(window);
        }
    }

    /// The live snap rects with `primary`'s own entry swapped for `rect` — the
    /// rect it is becoming, rather than the one it still commits. Only the
    /// primary can be mid-configure on these paths (a fit toggled straight out of
    /// a fullscreen exit runs before the client acks the restore); every other
    /// window's committed geometry is the authority, since a client-initiated
    /// resize never updates the size we last configured.
    #[allow(clippy::mutable_key_type)]
    fn snap_rects_with_primary(
        &self,
        primary: &StageWindow,
        rect: driftwm::layout::snap::SnapRect,
    ) -> Vec<(StageWindow, driftwm::layout::snap::SnapRect)> {
        let mut rects = self.all_windows_with_snap_rects();
        // A primary with no rect of its own (widget, pinned, unmapped) stays
        // absent, which is what degrades its cluster to `{self}`.
        if let Some((_, r)) = rects.iter_mut().find(|(w, _)| w == primary) {
            *r = rect;
        }
        rects
    }

    /// Translate snapped cluster members to follow the primary's resize
    /// from `old_rect` to `new_rect`. Works for arbitrary asymmetric edge
    /// movements — per-side deltas are derived directly from the rects, so
    /// off-by-one from integer truncation in fit/unfit position math can't
    /// desync a neighbor from the primary's actual edge.
    ///
    /// Two passes over the existing resize-grab cluster infrastructure:
    /// `BottomRight` shifts right+bottom neighbors by `(dx_right, dy_bottom)`,
    /// `TopLeft` shifts left+top by `(dx_left, dy_top)`.
    fn shift_cluster_around_primary(
        &mut self,
        primary: &Window,
        old_rect: driftwm::layout::snap::SnapRect,
        new_rect: driftwm::layout::snap::SnapRect,
    ) {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

        let dx_right = (new_rect.x_high - old_rect.x_high) as i32;
        let dx_left = (new_rect.x_low - old_rect.x_low) as i32;
        let dy_bottom = (new_rect.y_high - old_rect.y_high) as i32;
        let dy_top = (new_rect.y_low - old_rect.y_low) as i32;

        if dx_right == 0 && dx_left == 0 && dy_bottom == 0 && dy_top == 0 {
            return;
        }

        let primary = StageWindow::Client(primary.clone());

        // `initial_size` here is a delta carrier for `apply_member_shifts`,
        // which uses it only to compute `width_delta = new_w - initial_size.w`.
        // Rect width/height (which include the SSD bar) are fine — bars cancel.
        let old_size = Size::from((
            (old_rect.x_high - old_rect.x_low) as i32,
            (old_rect.y_high - old_rect.y_low) as i32,
        ));
        let gap = self.config.snap_gap;

        // Both passes classify the cluster against `old_rect`, the very rect the
        // deltas above are measured from, so membership can never disagree with
        // the shift it feeds. Rebuilt per pass, since the BR pass moves members.

        // BR pass: right members shift by +dx_right (= width_delta),
        // bottom members shift by +dy_bottom (= height_delta).
        let mut br = self.cluster_snapshot_for_resize_with_rects(
            &primary,
            xdg_toplevel::ResizeEdge::BottomRight,
            &self.snap_rects_with_primary(&primary, old_rect),
        );
        br.apply_member_shifts(
            &mut self.stage,
            &primary,
            old_size,
            old_size.w + dx_right,
            old_size.h + dy_bottom,
            gap,
        );

        // TL pass: left members shift by `-width_delta`, so width_delta = -dx_left
        // (left edge moves left → dx_left is negative → width_delta positive →
        // left members shift by dx_left). Same reasoning for top.
        let mut tl = self.cluster_snapshot_for_resize_with_rects(
            &primary,
            xdg_toplevel::ResizeEdge::TopLeft,
            &self.snap_rects_with_primary(&primary, old_rect),
        );
        tl.apply_member_shifts(
            &mut self.stage,
            &primary,
            old_size,
            old_size.w - dx_left,
            old_size.h - dy_top,
            gap,
        );
    }

    pub fn fit_window_snapped(&mut self, window: &Window) {
        let Some(wl_surface) = window.wl_surface() else {
            return;
        };
        if self.is_pinned(window) || config::applied_rule(&wl_surface).is_some_and(|r| r.widget) {
            return;
        }
        let Some(old_loc) = self.stage.position_of(window) else {
            return;
        };
        // Last configured, not last committed, for the same reason as
        // `unfit_window_snapped`. The restore size can't stand in for it: a
        // fullscreen entry that rewrote an undersized `saved_size` (see
        // `MIN_RESTORE_FLOOR`) leaves the two describing different rects.
        let old_size = super::configured_window_size(window);
        let bar = self.window_ssd_bar(window);
        let bw = self.window_border_width(&wl_surface);
        let fit = self.compute_fit_geometry(window);
        let old_rect = snap_rect_at(old_loc, old_size, bar, bw);
        let new_rect = snap_rect_at(fit.new_loc, fit.target_size, bar, bw);
        let primary = StageWindow::Client(window.clone());
        // Capture the cluster before shifting — members' caches must follow
        // their new live positions, or close-time `cluster_of` sees stale
        // cache vs shifted live and can't reconstruct the cluster.
        #[allow(clippy::mutable_key_type)]
        let cluster_members: Vec<StageWindow> = {
            let rects = self.snap_rects_with_primary(&primary, old_rect);
            driftwm::layout::cluster::cluster_of(&primary, &rects, self.config.snap_gap)
                .into_iter()
                .filter(|w| w != window)
                .collect()
        };
        let primary_id = self.stage.id_of(window);
        let old_member_locs = self.cluster_member_positions(&cluster_members);
        self.shift_cluster_around_primary(window, old_rect, new_rect);
        self.animate_cluster_shift(old_member_locs, primary_id);
        self.fit_window(window);
        for member in &cluster_members {
            self.refresh_stable_snap_rect(member);
        }
        // Insert the primary's post-fit rect explicitly: `geometry().size`
        // is still pre-fit until the client acks the configure, so
        // `refresh_stable_snap_rect` would cache wrong dimensions.
        self.stable_snap_rects.insert(wl_surface.id(), new_rect);
    }

    pub fn unfit_window_snapped(&mut self, window: &Window) {
        let Some(wl_surface) = window.wl_surface() else {
            return;
        };
        let Some(saved_size) = self.stage.fit_saved_size(window) else {
            return;
        };
        let Some(old_loc) = self.stage.position_of(window) else {
            return;
        };
        // Last configured, not last committed — the cluster deltas below must be
        // measured against the rect the window is actually becoming.
        let old_size = super::configured_window_size(window);
        let bar = self.window_ssd_bar(window);
        let bw = self.window_border_width(&wl_surface);
        // Mirror unfit_window's new_loc so per-edge deltas match.
        let center = self.window_visual_center(window).unwrap_or_default();
        let new_loc = super::frame_loc_for_center(center, saved_size, bar);
        let old_rect = snap_rect_at(old_loc, old_size, bar, bw);
        let new_rect = snap_rect_at(new_loc, saved_size, bar, bw);
        let primary = StageWindow::Client(window.clone());
        // See `fit_window_snapped` for why we refresh member caches here.
        #[allow(clippy::mutable_key_type)]
        let cluster_members: Vec<StageWindow> = {
            let rects = self.snap_rects_with_primary(&primary, old_rect);
            driftwm::layout::cluster::cluster_of(&primary, &rects, self.config.snap_gap)
                .into_iter()
                .filter(|w| w != window)
                .collect()
        };
        let primary_id = self.stage.id_of(window);
        let old_member_locs = self.cluster_member_positions(&cluster_members);
        self.shift_cluster_around_primary(window, old_rect, new_rect);
        self.animate_cluster_shift(old_member_locs, primary_id);
        self.unfit_window(window);
        for member in &cluster_members {
            self.refresh_stable_snap_rect(member);
        }
        // Primary's cache is refreshed by `establish_exit_placement` when the
        // exit size is unchanged, otherwise by the pending_recenter completion
        // in `handlers/compositor.rs` once the client acks the exit configure.
    }

    /// Snapshot each cluster member's pre-shift canvas position, so the shift
    /// can be animated after the stage moves them.
    fn cluster_member_positions(
        &self,
        members: &[StageWindow],
    ) -> Vec<(StageWindow, Point<i32, Logical>)> {
        members
            .iter()
            .filter_map(|m| Some((m.clone(), self.stage.position_of(m)?)))
            .collect()
    }

    /// Animate each cluster member from its captured pre-shift position to its
    /// new (already-applied) stage position, held back until `primary`'s own
    /// resize freeze releases so the push and the window pushing it start on the
    /// same tick. `primary`'s entry does not exist yet here — the wait is
    /// resolved at tick time, and one that never resolves is simply dropped.
    fn animate_cluster_shift(
        &mut self,
        old_locs: Vec<(StageWindow, Point<i32, Logical>)>,
        primary: Option<ElementId>,
    ) {
        for (member, old_loc) in old_locs {
            if self.stage.position_of(&member) != Some(old_loc) {
                self.animate_element_move_from(&member, old_loc, primary);
            }
        }
    }

    pub fn toggle_fit_window_snapped(&mut self, window: &Window) {
        if self.stage.is_fit(window) {
            self.unfit_window_snapped(window);
        } else {
            self.fit_window_snapped(window);
        }
    }

    pub fn decoration_toggle_fit(&mut self, window: &Window) {
        if self.config.decoration_fit_snapped {
            self.toggle_fit_window_snapped(window);
        } else {
            self.toggle_fit_window(window);
        }
    }

    pub fn decoration_fit(&mut self, window: &Window) {
        if self.config.decoration_fit_snapped {
            self.fit_window_snapped(window);
        } else {
            self.fit_window(window);
        }
    }

    pub fn decoration_unfit(&mut self, window: &Window) {
        if self.config.decoration_fit_snapped {
            self.unfit_window_snapped(window);
        } else {
            self.unfit_window(window);
        }
    }
}
