//! Screen-pinned windows: pinning and unpinning without a visual jump, keeping
//! each pin's canvas location in step with the fixed screen position it renders
//! at, and rehoming pins whose output changes or is unplugged.
//!
//! The canvas location is bookkeeping — rendering and hit-testing read
//! `screen_pos` — but it has to be re-anchored whenever the camera moves, or
//! the window drifts off its output and the visibility culls freeze it.

use smithay::desktop::Window;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use super::window_animation::{AnimSpace, ContentPolicy, GeometryRole};
use super::{DriftWm, StageWindow, output_logical_size, output_state};

impl DriftWm {
    /// Pin `window` at the on-screen rect it is drawn at right now, on the
    /// output showing it. No-op when it is already pinned; `Err` when there is
    /// no output to pin to.
    pub(crate) fn pin_window(&mut self, window: &Window) -> Result<(), String> {
        if self.stage.pin_of(window).is_some() {
            return Ok(());
        }
        let output = self.output_for_window(window);
        // The on-screen rect the window is drawn at right now, read before the
        // mutation flips which space "on screen" is derived from. At zoom != 1
        // the flip is a `1/z` scale jump anchored at the content-box top-left,
        // and this is the picture the new entry grows out of.
        let pre_pin = output
            .as_ref()
            .and_then(|output| self.window_screen_rect_on(window, output));
        // Pinning flips the chase space (canvas → screen); an in-flight entry
        // would keep a stale-space visual, so drop it — along with any parked
        // pan and stashed capture belonging to the transition it supersedes.
        self.cancel_window_animation(window);
        // The pin decides where the window lives from here on.
        self.drop_owed_recenter(window);
        // The cancel and the drop sit above this guard because the pre-pin rect
        // has to be read before the cancel; the guard only fails with no output
        // at all, where nothing was on screen to disturb.
        let Some(output) = output else {
            return Err("no output to pin to".to_string());
        };
        let Some(loc) = self.stage.position_of(window) else {
            return Err("window has no position".to_string());
        };
        let (camera, zoom) = {
            let os = output_state(&output);
            (os.camera, os.zoom)
        };
        let screen = driftwm::canvas::canvas_to_screen(
            driftwm::canvas::CanvasPos(loc.to_f64()),
            camera,
            zoom,
        )
        .0;
        let screen_pos = Point::from((screen.x.round() as i32, screen.y.round() as i32));
        // Pinned windows are out of the focus cycle.
        self.stage.drop_from_focus_history(window);
        self.stage.set_pin(
            window,
            driftwm::stage::PinnedSite {
                output: output.name(),
                screen_pos,
            },
        );
        // The entry chases `screen_pos` at the window's real size under zoom
        // 1, so a capture taken at zoom 0.5 grows into it from half size.
        if let Some(seed) = pre_pin {
            self.begin_geometry_animation_seeded(
                window,
                seed,
                AnimSpace::Screen(output.name()),
                None,
                GeometryRole::Normal,
                ContentPolicy::Cap,
                None,
            );
        }
        // The hit-test path changed (pinned vs canvas); recompute pointer focus.
        self.refresh_pointer_focus();
        Ok(())
    }

    /// Return a pinned `window` to the canvas at the point its pin site maps to
    /// under the current camera, so nothing moves on screen. The re-map raises
    /// it; `activate` says whether it also takes the activated hint. No-op when
    /// the window isn't pinned.
    pub(crate) fn unpin_window(&mut self, window: &Window, activate: bool) {
        // Read rather than take: the pre-unpin rect below is the pinned picture.
        let Some(site) = self.stage.pin_of(window).cloned() else {
            return;
        };
        let output = self.output_by_name(&site.output);
        // The on-screen rect the window is drawn at right now, read before the
        // mutation flips which space "on screen" is derived from. At zoom != 1
        // the flip is a `1/z` scale jump anchored at the content-box top-left,
        // and this is the picture the new entry grows out of.
        let pre_unpin = output
            .as_ref()
            .and_then(|output| self.window_screen_rect_on(window, output));
        // Unpinning flips the chase space (screen → canvas); an in-flight entry
        // would keep a stale-space visual, so drop it — along with any parked
        // pan and stashed capture belonging to the transition it supersedes.
        self.cancel_window_animation(window);
        // The canvas location decides where the window lives from here on.
        self.drop_owed_recenter(window);
        self.stage.take_pin(window);
        if let Some(output) = output {
            // Convert the fixed screen position back to a canvas location at the
            // current camera/zoom — no visual jump.
            let (camera, zoom) = {
                let os = output_state(&output);
                (os.camera, os.zoom)
            };
            let canvas = driftwm::canvas::screen_to_canvas(
                driftwm::canvas::ScreenPos(site.screen_pos.to_f64()),
                camera,
                zoom,
            )
            .0
            .to_i32_round();
            self.map_window(window.clone(), canvas, activate);
            // Converting the pre-unpin screen rect back through the same camera
            // reproduces it exactly on the first frame; the chase then runs it
            // out to the canvas rect the camera magnifies by `1/z`. Inside the
            // output guard on purpose: without an output there is no camera to
            // convert with, and the window was never re-mapped.
            if let Some(screen) = pre_unpin {
                let seed = Rectangle::new(
                    Point::from((
                        camera.x + screen.loc.x / zoom,
                        camera.y + screen.loc.y / zoom,
                    )),
                    Size::from((screen.size.w / zoom, screen.size.h / zoom)),
                );
                self.begin_geometry_animation_seeded(
                    window,
                    seed,
                    AnimSpace::Canvas,
                    None,
                    GeometryRole::Normal,
                    ContentPolicy::Cap,
                    None,
                );
            }
        }
        // The hit-test path changed (pinned vs canvas); recompute pointer focus.
        self.refresh_pointer_focus();
    }

    /// Re-anchor each pinned window's canvas location to the point its fixed
    /// `screen_pos` currently maps to. Without this the loc freezes at placement
    /// and drifts off its output as the camera pans — triggering spurious
    /// `output_leave` and the visibility culls, which would freeze the pinned
    /// window at 0 FPS. Only the position is touched: this runs on every camera
    /// move, and a re-map would raise each pinned window to the top of the
    /// z-order every time, above windows the user put there — including one
    /// growing into the fullscreen a pinned window is on its way out of.
    /// Rendering and hit-testing still read `screen_pos`.
    pub(super) fn sync_pinned_locs(&mut self) {
        if !self.stage.has_pinned() {
            return;
        }
        let pinned: Vec<(StageWindow, driftwm::stage::PinnedSite)> = self
            .stage
            .pinned_windows()
            .map(|(w, site)| (w.clone(), site.clone()))
            .collect();
        for (window, site) in pinned {
            let Some(output) = self.output_by_name(&site.output) else {
                continue;
            };
            let (camera, zoom) = {
                let os = output_state(&output);
                (os.camera, os.zoom)
            };
            let canvas = driftwm::canvas::screen_to_canvas(
                driftwm::canvas::ScreenPos(site.screen_pos.to_f64()),
                camera,
                zoom,
            )
            .0
            .to_i32_round();
            self.stage.set_position(&window, canvas);
        }
    }

    /// Move a screen-pinned window to `target`, keeping its on-screen position
    /// (clamped into the target output's bounds) and rebinding the pin to it.
    /// No-op if the window isn't pinned or is already on `target`.
    pub(crate) fn send_pinned_to_output(&mut self, window: &Window, target: &Output) {
        let Some(mut site) = self.stage.pin_of(window).cloned() else {
            return;
        };
        if site.output == target.name() {
            return;
        }
        let target_size = output_logical_size(target);
        let chrome = self.element_chrome(window);
        site.output = target.name();
        site.screen_pos =
            clamp_pin_frame(site.screen_pos, window.geometry().size, target_size, chrome);
        self.stage.set_pin(window, site);
        // Re-anchor the Space loc to the new output now — `sync_pinned_locs`
        // only fires on camera changes, which this rebind doesn't trigger, so
        // without it the window keeps its stale (off the new output) canvas loc
        // and gets culled until the next pan.
        self.sync_pinned_locs();
    }

    /// Reassign every pinned window whose output is no longer a live space
    /// output (it was unplugged) to `to`, clamping `screen_pos` into the new
    /// output's bounds. Covers both the multi-output unplug (output already
    /// unmapped) and the last-output reconnection (virtual placeholder swapped
    /// for the new monitor).
    pub fn reassign_orphaned_pinned(&mut self, to: &Output) {
        let live: Vec<String> = self.space.outputs().map(|o| o.name()).collect();
        let to_size = output_logical_size(to);
        let orphans: Vec<(StageWindow, driftwm::stage::PinnedSite)> = self
            .stage
            .pinned_windows()
            .filter(|(_, site)| !live.contains(&site.output))
            .map(|(w, site)| (w.clone(), site.clone()))
            .collect();
        let moved = !orphans.is_empty();
        for (window, mut site) in orphans {
            let chrome = self.element_chrome(&window);
            site.output = to.name();
            site.screen_pos =
                clamp_pin_frame(site.screen_pos, window.geometry().size, to_size, chrome);
            self.stage.set_pin(&window, site);
        }
        if moved {
            // Re-anchor the Space loc to the new output now — `sync_pinned_locs`
            // only fires on camera changes, which a hotplug doesn't guarantee, so
            // without this the reassigned window keeps its stale (off the new
            // output) canvas loc and gets culled until the next pan.
            self.sync_pinned_locs();
        }
        // A pin suspended by fullscreen (`fullscreen_return.pinned` on the
        // fullscreen output) is invisible to `stage.pinned_windows()`; rebind
        // it too, or fullscreen-exit restores the pin onto the dead output and
        // the window strands there. Clamp against the fullscreen entry's saved
        // size — the window's current geometry is the fullscreen viewport.
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            // A `fullscreen_return` without a stage entry is a divergence the
            // stage invariants assert against; don't paper over it here.
            let Some((saved_size, window)) = self
                .stage
                .fullscreen_on(&output.name())
                .map(|fs| (fs.saved_size, fs.window.clone()))
            else {
                continue;
            };
            // The chrome the window wears once the exit restores the pin: it has
            // none right now, which is why `element_chrome` is fullscreen-blind.
            let chrome = self.element_chrome(&window);
            let mut os = output_state(&output);
            if let Some(ret) = os.fullscreen_return.as_mut()
                && let Some(site) = ret.pinned.as_mut()
                && !live.contains(&site.output)
            {
                site.output = to.name();
                site.screen_pos = clamp_pin_frame(site.screen_pos, saved_size, to_size, chrome);
            }
        }
    }
}

/// Clamp a pin so its whole *visual frame* stays on an output of `output_size`.
/// Clamping the frame rather than the content is what keeps a rehome from
/// pushing a title bar off the top edge.
///
/// Takes and returns the content top-left, the form `PinnedSite::screen_pos`
/// stores, so no caller has to remember which space it is handing over — one
/// that inflated only the position and not the size would compile and silently
/// displace the pin by the whole chrome.
///
/// An output too small for the frame parks the frame's top-left at the origin and
/// lets the rest overflow, matching the placement clamp's `.max(0)`.
pub(crate) fn clamp_pin_frame(
    screen_pos: Point<i32, Logical>,
    content_size: Size<i32, Logical>,
    output_size: Size<i32, Logical>,
    chrome: driftwm::canvas::Chrome,
) -> Point<i32, Logical> {
    let frame_size = chrome.frame_size(content_size);
    let frame = chrome.frame_loc(screen_pos);
    chrome.content_loc(Point::from((
        frame.x.clamp(0, (output_size.w - frame_size.w).max(0)),
        frame.y.clamp(0, (output_size.h - frame_size.h).max(0)),
    )))
}
