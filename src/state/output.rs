use std::collections::HashMap;

use smithay::output::Output;
use smithay::utils::{Logical, Point};

use driftwm::config::OutputPosition;

use super::{DriftWm, init_output_state, output_logical_size, output_state};

impl DriftWm {
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
    /// `saved` holds any persisted `(camera, zoom)` per output name to restore.
    pub fn output_connected(
        &mut self,
        output: &Output,
        saved: &HashMap<String, (Point<f64, Logical>, f64)>,
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

        // Restore per-output camera/zoom from the state file if available. A
        // seed outside sane bounds (a hand-edit / corruption reaching here via
        // the runtime file, which has no validation of its own) is ignored so
        // the output keeps its default camera instead of an inf/NaN viewport.
        if let Some(&(saved_cam, saved_zoom)) = saved.get(&output.name()) {
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
