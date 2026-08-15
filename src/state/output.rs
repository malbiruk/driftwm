use std::collections::HashMap;

use smithay::desktop::Window;
use smithay::output::Output;
use smithay::utils::{Logical, Point};
use smithay::wayland::seat::WaylandFocus;

use driftwm::canvas::{ScreenPos, clamp_to_output};
use driftwm::config::OutputPosition;
use driftwm::stage::StageElement;

#[cfg(test)]
use super::OutputState;
use super::{
    CameraSeed, DriftWm, StageWindow, canvas_render_loc, init_output_state, output_logical_size,
    output_state,
};

impl DriftWm {
    /// Resolve an output name (the stage's fullscreen key) back to the live
    /// `Output`. `None` if the output was disconnected in the meantime.
    pub fn output_by_name(&self, name: &str) -> Option<Output> {
        self.space.outputs().find(|o| o.name() == name).cloned()
    }

    /// The output the pointer is currently on; falls back to the first output.
    pub fn active_output(&self) -> Option<Output> {
        self.focused_output
            .clone()
            .or_else(|| self.space.outputs().next().cloned())
    }

    /// Output whose viewport contains the element's center, or the active
    /// output if it isn't visible on any. Element-generic: a stand-in resolves
    /// by center containment exactly like a client (it never pins, so the
    /// pin short-circuit is simply inert for it).
    pub fn output_for_window<Q>(&self, window: &Q) -> Option<Output>
    where
        StageWindow: PartialEq<Q>,
        Q: StageElement,
    {
        // A pinned window is fixed to one output regardless of canvas geometry.
        if let Some(site) = self.stage.pin_of(window) {
            return self.output_by_name(&site.output);
        }
        let loc = self.stage.position_of(window)?;
        let size = window.size();
        let center: Point<f64, Logical> = Point::from((
            loc.x as f64 + size.w as f64 / 2.0,
            loc.y as f64 + size.h as f64 / 2.0,
        ));
        self.output_showing_canvas_point(center)
            .or_else(|| self.active_output())
    }

    /// True if `output`'s viewport currently shows the canvas point.
    pub fn output_shows_canvas_point(&self, output: &Output, point: Point<f64, Logical>) -> bool {
        let (camera, zoom) = {
            let os = output_state(output);
            (os.camera, os.zoom)
        };
        let visible = driftwm::canvas::visible_canvas_rect(
            camera.to_i32_round(),
            output_logical_size(output),
            zoom,
        );
        visible.contains(Point::from((point.x as i32, point.y as i32)))
    }

    /// First output whose viewport shows the canvas point. Callers that have a
    /// preferred output (the pointer's, say) should test it with
    /// `output_shows_canvas_point` first: viewports overlap by default — every
    /// output starts centered on the canvas origin — so the first match is
    /// registration order, not proximity.
    pub fn output_showing_canvas_point(&self, point: Point<f64, Logical>) -> Option<Output> {
        self.space
            .outputs()
            .find(|output| self.output_shows_canvas_point(output, point))
            .cloned()
    }

    /// Retire every virtual placeholder output (the ones held while all physical
    /// monitors were unplugged): exit any fullscreen entered on them, drain their
    /// enters, unmap them from the [`Space`], and drop their render state. Clears
    /// the placeholder set and focus so the next connected output bootstraps as a
    /// fresh first output.
    ///
    /// Empty set is a no-op — importantly it leaves `focused_output` untouched, so
    /// a second real monitor connecting doesn't reset focus.
    pub fn retire_placeholders(&mut self) {
        if self.disconnected_outputs.is_empty() {
            return;
        }
        let placeholders: Vec<Output> = self
            .space
            .outputs()
            .filter(|o| self.disconnected_outputs.contains(&o.name()))
            .cloned()
            .collect();
        for old in &placeholders {
            // A window can have entered fullscreen while headless (the
            // placeholder is a normal space output); exit it or the stage entry
            // outlives its output.
            self.exit_fullscreen_on(old);
            // Windows never enter placeholder outputs (membership refresh excludes
            // them), but a layer-shell surface created while headless still gets
            // entered on the placeholder by the layer map; drain those enters so
            // clients see the old output's leave before the new output's enter.
            old.leave_all();
            self.space.unmap_output(old);
            self.render.remove_output(&old.name());
        }
        self.disconnected_outputs.clear();
        self.focused_output = None;
    }

    /// Backend-independent connect policy for a freshly created output. The
    /// backend has already set its mode and transform (and scale, where it has
    /// one) and created the `wl_output` global, but NOT its layout position —
    /// this owns that plus the per-output viewport state, focus bootstrap,
    /// [`Space`] mapping, and re-anchoring of orphaned pinned windows.
    ///
    /// `saved` holds any persisted `(viewport seed, zoom)` per output name to
    /// restore.
    pub fn output_connected(
        &mut self,
        output: &Output,
        saved: &HashMap<String, (CameraSeed, f64)>,
    ) {
        // Retire first: unmapping placeholders shrinks the auto-position sum below.
        self.retire_placeholders();

        let position: Point<i32, Logical> = match self
            .config
            .output_config(&output.name())
            .map(|c| &c.position)
        {
            Some(OutputPosition::Fixed(x, y)) => {
                tracing::info!(
                    "output {}: layout position ({x}, {y}) from config",
                    output.name()
                );
                (*x, *y).into()
            }
            _ => {
                // Auto: place left-to-right by connection order.
                let auto_x: i32 = self.space.outputs().map(|o| output_logical_size(o).w).sum();
                tracing::info!(
                    "output {}: auto layout position ({auto_x}, 0)",
                    output.name()
                );
                (auto_x, 0).into()
            }
        };
        output.change_current_state(None, None, None, Some(position));

        // Each new output gets its own camera centered on its viewport.
        let logical = output_logical_size(output);
        let camera = Point::from((-(logical.w as f64) / 2.0, -(logical.h as f64) / 2.0));
        init_output_state(output, camera, self.config.drift, position);

        // Restore per-output camera/zoom from the state file if available. The
        // seed is resolved to an internal camera first, so the bounds check
        // guards the value actually assigned: a seed outside sane bounds (a
        // hand-edit / corruption reaching here via the runtime file, which has
        // no validation of its own) is ignored so the output keeps its default
        // camera instead of an inf/NaN viewport.
        if let Some(&(seed, saved_zoom)) = saved.get(&output.name()) {
            let saved_cam = seed.resolve(saved_zoom, logical);
            if super::session_store::valid_camera_seed(saved_cam, saved_zoom) {
                let mut os = output_state(output);
                os.camera = saved_cam;
                os.zoom = saved_zoom;
                tracing::info!(
                    "output {}: restored camera ({:.1}, {:.1}) zoom {saved_zoom:.2}",
                    output.name(),
                    saved_cam.x,
                    saved_cam.y,
                );
            } else {
                tracing::warn!(
                    "output {}: ignoring out-of-range saved camera/zoom (zoom {saved_zoom})",
                    output.name()
                );
            }
        }

        // The first output created takes focus and the pointer.
        if self.focused_output.is_none() {
            self.focused_output = Some(output.clone());
            let size = output_logical_size(output);
            let (cam, zoom) = {
                let os = output_state(output);
                (os.camera, os.zoom)
            };
            let center = Point::from((
                cam.x + size.w as f64 / (2.0 * zoom),
                cam.y + size.h as f64 / (2.0 * zoom),
            ));
            self.warp_pointer(center);
        }

        // Map at the potentially-restored camera.
        let effective_camera = output_state(output).camera;
        self.space
            .map_output(output, effective_camera.to_i32_round());
        self.recompute_decoration_scale();

        // Both are no-ops when no windows exist, so this is safe at boot too.
        self.reassign_orphaned_pinned(output);
        driftwm::protocols::foreign_toplevel::send_output_enter_all(
            &mut self.foreign_toplevel_state,
            output,
        );
        // ext-workspace output_enter is reconciled per frame in `refresh` (the
        // client hasn't bound the new wl_output global yet at this point).
    }

    /// Backend-independent disconnect policy for an output. Runs whether the
    /// output is the last surviving one or not: the "last output" path keeps the
    /// [`Output`] mapped as a virtual placeholder (so `active_output()` stays
    /// `Some` while a monitor is replugged) but still needs the grab/gesture/
    /// focus cleanup.
    ///
    /// Every client-facing leave (`wl_surface.leave`, foreign-toplevel
    /// `output_leave`) is sent here, i.e. before the caller disables the
    /// `wl_output` global — a leave sent after global removal arrives with a NULL
    /// `wl_output` and segfaults clients that don't null-check it. The caller owns
    /// the `wl_output` global teardown and must run it *after* this returns.
    /// `active_outputs` bookkeeping likewise stays with the caller, symmetric with
    /// where it was inserted.
    pub fn output_disconnected(&mut self, output: &Output, is_last: bool) {
        // Send wl_surface.leave while clients' wl_output proxies are still valid.
        // Once the global is disabled, clients destroy their proxy on
        // global_remove — a leave sent after that (normally by the next
        // Space::refresh) arrives in libwayland with a NULL wl_output argument and
        // segfaults clients that don't null-check it. leave_all also clears
        // smithay's enter tracking, so the later refresh-driven leave is a no-op.
        output.leave_all();

        driftwm::protocols::foreign_toplevel::send_output_leave_all(
            &mut self.foreign_toplevel_state,
            output,
        );
        driftwm::protocols::ext_workspace::send_output_leave(&mut self.ext_workspace_state, output);
        self.image_copy_capture_state.remove_output(output);
        self.screencopy_state.remove_output(output);
        self.gamma_control_manager_state.output_removed(output);

        // Fail + drop pending captures that can no longer render — a stranded entry
        // hangs the client and leaks its buffer fd. Toplevel captures drain on any
        // output's render path, but when this was the *last* output no CRTC remains
        // to run them (the virtual placeholder is never rendered), so they're dead.
        // Screencopy's Drop sends failed() itself; ext-image-copy frames must be
        // failed explicitly.
        self.pending_screencopies.retain(|s| s.output() != output);
        {
            use driftwm::protocols::image_copy_capture::PendingCaptureKind;
            use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason;
            let mut i = 0;
            while i < self.pending_captures.len() {
                let dead = match &self.pending_captures[i].kind {
                    PendingCaptureKind::Output(o) => o == output,
                    PendingCaptureKind::Toplevel(_) => is_last,
                };
                if dead {
                    self.pending_captures
                        .swap_remove(i)
                        .frame
                        .failed(FailureReason::Unknown);
                } else {
                    i += 1;
                }
            }
        }

        // Close layer surfaces hosted on this output. They'll re-anchor against
        // remaining outputs on their next configure round-trip.
        for layer in smithay::desktop::layer_map_for_output(output).layers() {
            layer.layer_surface().send_close();
        }

        // Grabs (move/resize/pan/navigate) clone the Output and keep mutating its
        // per-output state on every motion. Cancel before the output goes away.
        if let Some(pointer) = self.seat.get_pointer() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            pointer.unset_grab(self, serial, 0);
        }
        if self.gesture_output.as_ref().is_some_and(|go| go == output) {
            self.gesture_output = None;
            self.gesture_state = None;
        }

        self.exit_fullscreen_on(output);
        self.render.remove_output(&output.name());
        self.lock_surfaces.remove(output);
        self.redraws_needed.remove(output);
        self.stop_awaiting_lock_frame(output);

        if is_last {
            // Keep the Output mapped as a virtual placeholder so active_output()
            // and other queries stay Some while no monitor is attached. The DRM
            // surface and wl_output global are already gone, so it's purely an
            // input-routing/coordinate-system anchor.
            tracing::warn!(
                "Last output disconnected — keeping virtual output '{}'",
                output.name()
            );
            self.disconnected_outputs.insert(output.name());
        } else {
            self.space.unmap_output(output);
            // Reassign screen-pinned windows on the gone output to a survivor.
            let pin_target = self.space.outputs().next().cloned();
            if let Some(target) = pin_target {
                self.reassign_orphaned_pinned(&target);
            }
            self.recompute_decoration_scale();
            output_state(output).fullscreen_return = None;
            self.stage.take_fullscreen(&output.name());
            self.dpms_off_outputs.remove(output);
            self.pending_dpms.remove(output);
            self.hot_corner_latch.take_if(|(o, _)| o == output);

            if self.focused_output.as_ref().is_some_and(|fo| fo == output) {
                self.focused_output = self.space.outputs().next().cloned();
                if let Some(ref new_out) = self.focused_output {
                    let (cam, zoom, size) = {
                        let os = output_state(new_out);
                        let sz = output_logical_size(new_out);
                        (os.camera, os.zoom, sz)
                    };
                    if self.session_lock.is_locked() {
                        // `warp_pointer` declines under a lock, and the stored
                        // location is screen-space against the output that just
                        // left — on a smaller survivor it would render outside
                        // the framebuffer until some relative motion clamped it
                        // back. Nothing to re-center on here: the lock surface
                        // is screen-fixed.
                        let pointer = self.seat.get_pointer().unwrap();
                        let clamped = clamp_to_output(ScreenPos(pointer.current_location()), size);
                        pointer.set_location(clamped.0);
                    } else {
                        let center = Point::from((
                            cam.x + size.w as f64 / (2.0 * zoom),
                            cam.y + size.h as f64 / (2.0 * zoom),
                        ));
                        self.warp_pointer(center);
                    }
                }
            }
        }
    }

    /// Effective render transform for `window` in one pass: the pre-zoom,
    /// output-relative logical origin of its surface tree (geometry top-left
    /// minus `geometry().loc`) and the scale to render at. The single
    /// canvas↔screen chokepoint — every render/capture consumer routes through
    /// it so a pinned window is decided once, not re-inlined per site.
    ///
    /// - Normal window: `loc - geom_loc - camera`, scaled by `zoom`.
    /// - Pinned window on its output: `screen_pos - geom_loc`, scale `1.0`
    ///   (identity — no camera, no zoom).
    /// - Pinned window on any other output: `None` (don't render here).
    /// - `output = None` (off-screen canvas capture): pinned → `None` by
    ///   construction, so captures never include screen-pinned windows.
    pub fn window_render_transform(
        &self,
        window: &Window,
        output: Option<&Output>,
        camera: Point<f64, Logical>,
        zoom: f64,
    ) -> Option<(Point<f64, Logical>, f64)> {
        let loc = self.stage.position_of(window)?;
        let geom_loc = window.geometry().loc;
        // A fullscreen window is visible only on its own output. For any other
        // output — and for the off-screen capture pass (`output == None`) — it
        // must not render: it keeps a real canvas coord at its output's
        // camera origin, so another monitor's camera would otherwise pan over
        // it. On its own output it falls through to the canvas branch below,
        // which yields (0,0) at zoom 1 thanks to the camera-park.
        if self.stage.has_fullscreen()
            && let Some(fs_output) = window
                .wl_surface()
                .and_then(|s| self.find_fullscreen_output_for_surface(&s))
            && output != Some(&fs_output)
        {
            return None;
        }
        match self.stage.pin_of(window) {
            Some(site) => match output {
                Some(o) if o.name() == site.output => Some((
                    Point::from((
                        site.screen_pos.x as f64 - geom_loc.x as f64,
                        site.screen_pos.y as f64 - geom_loc.y as f64,
                    )),
                    1.0,
                )),
                _ => None,
            },
            None => Some((canvas_render_loc(loc, geom_loc, camera), zoom)),
        }
    }

    pub fn output_in_direction(
        &self,
        from: &Output,
        dir: &driftwm::config::Direction,
    ) -> Option<Output> {
        let from_center: Point<f64, Logical> = {
            let os = output_state(from);
            let size = output_logical_size(from);
            Point::from((
                os.layout_position.x as f64 + size.w as f64 / 2.0,
                os.layout_position.y as f64 + size.h as f64 / 2.0,
            ))
        };
        let (dx, dy) = dir.to_unit_vec();

        self.space
            .outputs()
            .filter(|o| *o != from)
            .filter_map(|o| {
                let os = output_state(o);
                let size = output_logical_size(o);
                let center: Point<f64, Logical> = Point::from((
                    os.layout_position.x as f64 + size.w as f64 / 2.0,
                    os.layout_position.y as f64 + size.h as f64 / 2.0,
                ));
                drop(os);
                let to_x = center.x - from_center.x;
                let to_y = center.y - from_center.y;
                let dist = (to_x * to_x + to_y * to_y).sqrt();
                if dist < 1.0 {
                    return None;
                }
                // dot > 0.5 ≈ alignment within ~60° of `dir`.
                let dot = (to_x * dx + to_y * dy) / dist;
                if dot > 0.5 {
                    Some((o.clone(), dist))
                } else {
                    None
                }
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(o, _)| o)
    }

    /// Output whose layout rectangle contains `pos`. Uses `layout_position` +
    /// mode size (NOT `space.output_geometry()`, which is zoom-cached).
    pub fn output_at_layout_pos(&self, pos: Point<f64, Logical>) -> Option<Output> {
        self.space
            .outputs()
            .find(|output| {
                let os = output_state(output);
                let lp = os.layout_position;
                drop(os);
                let size = output_logical_size(output);
                pos.x >= lp.x as f64
                    && pos.x < (lp.x + size.w) as f64
                    && pos.y >= lp.y as f64
                    && pos.y < (lp.y + size.h) as f64
            })
            .cloned()
    }

    /// layout_pos = (canvas - camera) * zoom + layout_position.
    #[cfg(test)]
    pub fn canvas_to_layout_pos(
        canvas_pos: Point<f64, Logical>,
        os: &OutputState,
    ) -> Point<f64, Logical> {
        let screen = driftwm::canvas::canvas_to_screen(
            driftwm::canvas::CanvasPos(canvas_pos),
            os.camera,
            os.zoom,
        )
        .0;
        Point::from((
            screen.x + os.layout_position.x as f64,
            screen.y + os.layout_position.y as f64,
        ))
    }

    /// canvas = (layout_pos - layout_position) / zoom + camera.
    #[cfg(test)]
    pub fn layout_to_canvas_pos(
        layout_pos: Point<f64, Logical>,
        os: &OutputState,
    ) -> Point<f64, Logical> {
        let screen = Point::from((
            layout_pos.x - os.layout_position.x as f64,
            layout_pos.y - os.layout_position.y as f64,
        ));
        driftwm::canvas::screen_to_canvas(driftwm::canvas::ScreenPos(screen), os.camera, os.zoom).0
    }

    /// Sync each output's position to its camera so render_output
    /// applies the canvas→screen transform.
    pub fn update_output_from_camera(&mut self) {
        let mut changed = false;
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            let cam = output_state(&output).camera.to_i32_round();
            if self.space.output_geometry(&output).map(|g| g.loc) != Some(cam) {
                changed = true;
            }
            self.space.map_output(&output, cam);
        }
        if changed {
            self.sync_pinned_locs();
        }
    }

    /// Recompute `decoration_scale` from current outputs. Call after output
    /// add/remove/scale change so SSD buffers re-render at the right density.
    pub fn recompute_decoration_scale(&mut self) {
        let max_scale = self
            .space
            .outputs()
            .map(|o| o.current_scale().fractional_scale())
            .fold(1.0_f64, f64::max);
        self.decoration_scale = max_scale.ceil() as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use driftwm::canvas::MomentumState;
    use std::time::Instant;

    fn mock_output_state(
        camera: (f64, f64),
        zoom: f64,
        layout_position: (i32, i32),
    ) -> OutputState {
        OutputState {
            camera: Point::from(camera),
            zoom,
            zoom_target: None,
            zoom_animation_anchor: None,
            last_rendered_zoom: zoom,
            overview_return: None,
            camera_target: None,
            last_scroll_pan: None,
            momentum: MomentumState::new(0.96),
            panning: false,
            edge_pan_velocity: None,
            edge_pan_screen_pos: None,
            edge_pan_delay: None,
            last_rendered_camera: Point::from(camera),
            last_frame_instant: Instant::now(),
            layout_position: Point::from(layout_position),
            home_return: None,
            fullscreen_return: None,
            active_bookmark: None,
            backend_owned_mode: false,
        }
    }

    #[test]
    fn canvas_to_layout_round_trip_zoom_1() {
        let os = mock_output_state((100.0, 200.0), 1.0, (0, 0));
        let canvas = Point::from((150.0, 250.0));
        let layout = DriftWm::canvas_to_layout_pos(canvas, &os);
        let back = DriftWm::layout_to_canvas_pos(layout, &os);
        assert!((back.x - canvas.x).abs() < 0.001);
        assert!((back.y - canvas.y).abs() < 0.001);
    }

    #[test]
    fn canvas_to_layout_round_trip_with_zoom() {
        let os = mock_output_state((50.0, 75.0), 2.0, (1920, 0));
        let canvas = Point::from((80.0, 100.0));
        let layout = DriftWm::canvas_to_layout_pos(canvas, &os);
        let back = DriftWm::layout_to_canvas_pos(layout, &os);
        assert!((back.x - canvas.x).abs() < 0.001);
        assert!((back.y - canvas.y).abs() < 0.001);
    }

    #[test]
    fn canvas_to_layout_known_values() {
        // camera=(100,200), zoom=2, layout_position=(1920,0)
        // screen = (canvas - camera) * zoom = (50-100)*2 = -100, (50-200)*2 = -300
        // layout = screen + layout_position = -100+1920 = 1820, -300+0 = -300
        let os = mock_output_state((100.0, 200.0), 2.0, (1920, 0));
        let canvas = Point::from((50.0, 50.0));
        let layout = DriftWm::canvas_to_layout_pos(canvas, &os);
        assert!((layout.x - 1820.0).abs() < 0.001);
        assert!((layout.y - (-300.0)).abs() < 0.001);
    }

    #[test]
    fn layout_to_canvas_known_values() {
        // layout=(1920,0), layout_position=(1920,0), zoom=1, camera=(500,300)
        // screen = layout - layout_position = (0, 0)
        // canvas = screen / zoom + camera = 0 + 500 = 500, 0 + 300 = 300
        let os = mock_output_state((500.0, 300.0), 1.0, (1920, 0));
        let layout = Point::from((1920.0, 0.0));
        let canvas = DriftWm::layout_to_canvas_pos(layout, &os);
        assert!((canvas.x - 500.0).abs() < 0.001);
        assert!((canvas.y - 300.0).abs() < 0.001);
    }

    #[test]
    fn round_trip_two_outputs_different_cameras() {
        let os_a = mock_output_state((0.0, 0.0), 1.0, (0, 0));
        let os_b = mock_output_state((500.0, 200.0), 0.5, (1920, 0));

        let canvas = Point::from((600.0, 300.0));
        // Through output A
        let layout_a = DriftWm::canvas_to_layout_pos(canvas, &os_a);
        let back_a = DriftWm::layout_to_canvas_pos(layout_a, &os_a);
        assert!((back_a.x - canvas.x).abs() < 0.001);
        assert!((back_a.y - canvas.y).abs() < 0.001);

        // Through output B
        let layout_b = DriftWm::canvas_to_layout_pos(canvas, &os_b);
        let back_b = DriftWm::layout_to_canvas_pos(layout_b, &os_b);
        assert!((back_b.x - canvas.x).abs() < 0.001);
        assert!((back_b.y - canvas.y).abs() < 0.001);
    }
}
