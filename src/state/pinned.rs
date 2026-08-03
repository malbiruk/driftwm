//! Screen-pinned windows: keeping each pin's canvas location in step with the
//! fixed screen position it renders at, and rehoming pins whose output changes
//! or is unplugged.
//!
//! The canvas location is bookkeeping — rendering and hit-testing read
//! `screen_pos` — but it has to be re-anchored whenever the camera moves, or
//! the window drifts off its output and the visibility culls freeze it.

use smithay::desktop::Window;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Size};

use super::{DriftWm, StageWindow, output_logical_size, output_state};

impl DriftWm {
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
            let Some(saved_size) = self
                .stage
                .fullscreen_on(&output.name())
                .map(|fs| fs.saved_size)
            else {
                continue;
            };
            let mut os = output_state(&output);
            if let Some(ret) = os.fullscreen_return.as_mut()
                && let Some(site) = ret.pinned.as_mut()
                && !live.contains(&site.output)
            {
                site.output = to.name();
                site.screen_pos.x = site
                    .screen_pos
                    .x
                    .clamp(0, (to_size.w - saved_size.w).max(0));
                site.screen_pos.y = site
                    .screen_pos
                    .y
                    .clamp(0, (to_size.h - saved_size.h).max(0));
            }
        }
    }
}

/// Clamp a pin's content top-left so its whole *visual frame* stays on an output
/// of `output_size` — the same rect a `pinned_to_screen` rule's `position` is
/// clamped into when the pin is first placed, so a rehome can't push a title bar
/// off the top edge that the original placement kept on screen.
///
/// An output too small for the frame pins the top-left corner and lets the rest
/// overflow, matching the placement clamp's `.max(0)`.
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
