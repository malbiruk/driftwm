//! What marks the compositor dirty and what lets it sleep: surface damage
//! routed to the outputs that actually show it, and the liveness checks the
//! idle path consults before skipping a frame.
//!
//! Damage resolves against each output's zoom-aware visible canvas rect rather
//! than `Space`'s cached mode-sized geometry, which undercounts at zoom < 1.

use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use super::{DriftWm, output_logical_size, output_state};

impl DriftWm {
    pub fn mark_all_dirty(&mut self) {
        self.redraws_needed.clone_from(&self.active_outputs);
    }

    /// Mark every output displaying `surface` (or its root toplevel / hosting
    /// layer / lock output) as needing a redraw. Falls back to
    /// [`Self::mark_all_dirty`] when the surface can't be resolved — covers
    /// DnD icons, orphan popups, and pre-mapping toplevels.
    pub fn mark_dirty_for_surface(&mut self, surface: &WlSurface) {
        use smithay::desktop::{WindowSurfaceType, layer_map_for_output};
        use smithay::wayland::compositor::get_parent;

        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        if let Some(window) = self.window_for_surface(&root)
            && let Some(win_bbox) = self.window_bbox_with_popups(&window)
        {
            // Use zoom-aware visible canvas rect rather than
            // `Space::outputs_for_element`: the latter is built on the cached
            // mode-sized output geometry, which undercounts at zoom < 1.
            // bbox (not geometry) ensures popups extending past the toplevel
            // still damage the right outputs — matches smithay's refresh semantics.
            // Inflate by SSD chrome (shadow + title bar) so a window whose body
            // is off-screen but whose shadow/title-bar sliver still shows marks
            // that output. A resolved window always returns: when visible on no
            // output it marks nothing, rather than falling through to the
            // `mark_all_dirty` path and redrawing every output for it.
            let margin = self.config.decorations.title_bar_height
                + driftwm::config::DecorationConfig::SHADOW_RADIUS.ceil() as i32;
            let mut chrome_bbox = win_bbox;
            chrome_bbox.loc.x -= margin;
            chrome_bbox.loc.y -= margin;
            chrome_bbox.size.w += 2 * margin;
            chrome_bbox.size.h += 2 * margin;
            for output in self.space.outputs() {
                let (cam, zoom) = {
                    let os = output_state(output);
                    (os.camera.to_i32_round(), os.zoom)
                };
                let viewport = output_logical_size(output);
                let visible = driftwm::canvas::visible_canvas_rect(cam, viewport, zoom);
                if visible.overlaps(chrome_bbox) {
                    self.redraws_needed.insert(output.clone());
                }
            }
            return;
        }

        // Canvas-positioned layer widgets aren't in any LayerMap; resolve them
        // against each output's visible canvas rect like windows, so a widget
        // commit redraws only the outputs showing it, not every output.
        let widget_bbox = self
            .canvas_layers
            .iter()
            .find(|cl| cl.surface.wl_surface() == &root)
            .and_then(|cl| {
                let pos = cl.position?;
                let mut bbox = cl.surface.bbox_with_popups();
                bbox.loc += pos;
                Some(bbox)
            });
        if let Some(widget_bbox) = widget_bbox {
            for output in self.space.outputs() {
                let (cam, zoom) = {
                    let os = output_state(output);
                    (os.camera.to_i32_round(), os.zoom)
                };
                let viewport = output_logical_size(output);
                let visible = driftwm::canvas::visible_canvas_rect(cam, viewport, zoom);
                if visible.overlaps(widget_bbox) {
                    self.redraws_needed.insert(output.clone());
                }
            }
            return;
        }

        for output in self.space.outputs() {
            let hit = layer_map_for_output(output)
                .layer_for_surface(&root, WindowSurfaceType::ALL)
                .is_some();
            if hit {
                self.redraws_needed.insert(output.clone());
                return;
            }
        }

        if let Some(output) = self
            .lock_surfaces
            .iter()
            .find(|(_, ls)| ls.wl_surface() == &root)
            .map(|(o, _)| o.clone())
        {
            self.redraws_needed.insert(output);
            return;
        }

        self.mark_all_dirty();
    }

    pub fn output_has_active_animations(&self, output: &Output) -> bool {
        // Read camera/zoom and drop the guard before any rect math: the window
        // animation scoping below re-reads no output_state, but the guard would
        // otherwise deadlock if it did (output_state panics on re-entrant lock).
        let (camera_active, camera, zoom) = {
            let os = output_state(output);
            (
                os.camera_target.is_some()
                    || os.zoom_target.is_some()
                    || os.edge_pan_velocity.is_some()
                    || os.momentum.velocity.x != 0.0
                    || os.momentum.velocity.y != 0.0,
                os.camera,
                os.zoom,
            )
        };
        // No cutoff: a frozen entry draws nothing new, but its deadline can only
        // fire from a tick, so it has to keep the loop awake.
        camera_active || self.output_shows_window_animations(output, camera, zoom, None)
    }

    /// True when `output_name`'s animated background is due for its next tick
    /// under `[background] animate_fps` (0 = every frame). The timestamp is
    /// stamped where the uniforms are actually pushed, in
    /// `update_background_element`. Keyed per output: outputs render on their
    /// own vblanks, and a global stamp would let whichever renders first
    /// satisfy the interval and starve the rest.
    pub fn background_animation_due(&self, output_name: &str) -> bool {
        if !self.render.background_is_animated {
            return false;
        }
        let fps = self.config.background.animate_fps;
        if fps == 0 {
            return true;
        }
        self.render
            .background_last_animate
            .get(output_name)
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs_f64(1.0 / fps as f64))
    }

    /// Outputs whose animated background can actually render: active, canvas not
    /// concealed by a fullscreen window, not DPMS-off. Concealed and DPMS-off
    /// outputs stop rendering the background, so their `background_last_animate`
    /// stamps go stale and would otherwise read as permanently due. A
    /// fullscreen-entry transition keeps its canvas visible until the window
    /// covers it, so its background stays eligible for that short interval —
    /// unless the fullscreen it is growing into is being handed over by a window
    /// whose exit freeze is still hiding the output, in which case nothing was
    /// uncovered. A translucent fullscreen window never conceals, so its output
    /// stays eligible throughout, or the wallpaper it shows through would be
    /// drawn frozen. Shared by the idle due-check, the tick-timer arming wait,
    /// and the per-frame dirty-marking so all three agree on which outputs count.
    pub(crate) fn background_render_eligible_outputs(&self) -> impl Iterator<Item = &Output> {
        self.active_outputs
            .iter()
            .filter(|o| !self.fullscreen_conceals_canvas(o) && !self.dpms_off_outputs.contains(o))
    }

    /// Owned-name variant of [`Self::background_render_eligible_outputs`] for
    /// callers outside this module that need to filter a name-keyed map
    /// (e.g. `background_last_animate`) without holding a borrow of `self`.
    pub fn background_render_eligible_output_names(&self) -> impl Iterator<Item = String> + '_ {
        self.background_render_eligible_outputs().map(|o| o.name())
    }

    /// True when any eligible output's animated background is due (idle
    /// wake-up check). Restricted to outputs that actually render the
    /// background — an output that is DPMS-off, or whose fullscreen window
    /// conceals the canvas, stops stamping `background_last_animate`, so
    /// including it here would read as permanently due and defeat the idle fast
    /// path (see `background_render_eligible_outputs`).
    pub fn background_animation_due_any(&self) -> bool {
        self.background_render_eligible_outputs()
            .any(|o| self.background_animation_due(&o.name()))
    }

    pub fn has_active_animations(&self) -> bool {
        self.space
            .outputs()
            .any(|o| self.output_has_active_animations(o))
            || self.held_action.is_some()
            || self.cursor.exec_cursor_show_at.is_some()
            || self.cursor.exec_cursor_deadline.is_some()
            || self.cursor.is_animated()
            || self.window_animations.is_active()
            || !self.closing_snapshots.is_empty()
            || !self.standin_fades.is_empty()
            || !self.resize_crossfades.is_empty()
    }
}
