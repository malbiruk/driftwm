//! Per-window output membership: sends clients `wl_surface.enter`/`leave` for
//! the outputs each window overlaps. Replaces the membership half of
//! `Space::refresh`, driven from `post_render` and the idle turn via
//! [`DriftWm::refresh_window_outputs`].
//!
//! Two behaviours differ from `Space`'s geometric-overlap default: a
//! fullscreen or pinned window belongs only to its single home output, and
//! virtual placeholder outputs (dead `wl_output` global) are never entered.

use std::cell::RefCell;
use std::collections::HashMap;

use smithay::desktop::Window;
use smithay::desktop::space::SpaceElement;
use smithay::output::Output;
use smithay::utils::{Logical, Rectangle};

use super::DriftWm;

/// Which outputs a window bbox belongs to: geometric overlap, restricted to a
/// single allowed output when the window is bound to one (fullscreen home /
/// pin target). Overlap rects are returned relative to the bbox origin.
fn desired_memberships(
    bbox: Rectangle<i32, Logical>,
    outputs: &[(String, Rectangle<i32, Logical>)],
    allowed: Option<&str>,
) -> Vec<(usize, Rectangle<i32, Logical>)> {
    outputs
        .iter()
        .enumerate()
        .filter(|(_, (name, _))| allowed.is_none_or(|a| a == name.as_str()))
        .filter_map(|(i, (_, geo))| {
            geo.intersection(bbox).map(|mut overlap| {
                overlap.loc -= bbox.loc;
                (i, overlap)
            })
        })
        .collect()
}

/// Per-window mirror of the overlap map smithay's `Space` keeps privately: it
/// records which outputs the window is currently entered on so a refresh only
/// re-sends enter/leave on an actual change. Distinct from the `Window`'s own
/// `WindowOutputUserData` (which holds the surface-level enter state).
#[derive(Default)]
struct WindowOutputs(RefCell<HashMap<Output, Rectangle<i32, Logical>>>);

impl DriftWm {
    /// Update every window's output membership, sending `wl_surface.enter`/
    /// `leave` as it changes.
    pub fn refresh_window_outputs(&self) {
        let candidates: Vec<(Output, Rectangle<i32, Logical>)> = self
            .space
            .outputs()
            .filter(|o| !self.disconnected_outputs.contains(&o.name()))
            .map(|o| (o.clone(), self.space.output_geometry(o).unwrap_or_default()))
            .collect();
        let named: Vec<(String, Rectangle<i32, Logical>)> =
            candidates.iter().map(|(o, geo)| (o.name(), *geo)).collect();

        // Suspended elements have no surface to enter/leave — clients only.
        let windows: Vec<Window> = self
            .stage
            .windows()
            .filter_map(|w| w.client())
            .cloned()
            .collect();
        for window in &windows {
            let Some(pos) = self.stage.position_of(window) else {
                continue;
            };
            // bbox_with_popups (not bbox): popup overhang past the toplevel must
            // still keep the window entered, matching Space's semantics.
            let mut bbox = window.bbox_with_popups();
            bbox.loc += pos - window.geometry().loc;

            // A window is never both fullscreen and pinned (stage invariant).
            let allowed = self
                .stage
                .fullscreen_output_of(window)
                .or_else(|| self.stage.pin_of(window).map(|s| s.output.as_str()));

            let desired: Vec<(Output, Rectangle<i32, Logical>)> =
                desired_memberships(bbox, &named, allowed)
                    .into_iter()
                    .map(|(i, overlap)| (candidates[i].0.clone(), overlap))
                    .collect();

            let tracker = window.user_data().get_or_insert(WindowOutputs::default);
            let mut map = tracker.0.borrow_mut();
            for (output, overlap) in &desired {
                if map.insert(output.clone(), *overlap) != Some(*overlap) {
                    SpaceElement::output_enter(window, output, *overlap);
                }
            }
            // A single retain covers every leave: window moved off an output,
            // fullscreen/pin restriction, output unplugged, or output became a
            // placeholder. A leave after teardown's `leave_all` is a no-op.
            map.retain(|output, _| {
                let keep = desired.iter().any(|(o, _)| o == output);
                if !keep {
                    SpaceElement::output_leave(window, output);
                }
                keep
            });
            drop(map);

            SpaceElement::refresh(window);
        }

        // Prune dead surfaces from every registry output's enter tracking,
        // including placeholders (which windows never enter but layer surfaces
        // still can).
        for o in self.space.outputs() {
            o.cleanup();
        }
    }
}

/// Send `output_leave` for every output the window is tracked on and clear the
/// tracker — the `Space::unmap_elem` contract, replicated for window unmap.
pub(crate) fn send_output_leaves(window: &Window) {
    let Some(tracker) = window.user_data().get::<WindowOutputs>() else {
        return;
    };
    let outputs: Vec<Output> = tracker.0.borrow_mut().drain().map(|(o, _)| o).collect();
    for output in outputs {
        SpaceElement::output_leave(window, &output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::{Point, Size};

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn window_spanning_two_outputs_enters_both() {
        let outputs = vec![
            ("A".to_string(), rect(0, 0, 100, 100)),
            ("B".to_string(), rect(100, 0, 100, 100)),
        ];
        // Spans x 50..150, y 10..60 — straddles the A|B seam at x=100.
        let bbox = rect(50, 10, 100, 50);
        let desired = desired_memberships(bbox, &outputs, None);
        assert_eq!(
            desired,
            vec![(0, rect(0, 0, 50, 50)), (1, rect(50, 0, 50, 50))]
        );
    }

    #[test]
    fn allowed_output_excludes_foreign_overlap() {
        let outputs = vec![
            ("A".to_string(), rect(0, 0, 100, 100)),
            ("B".to_string(), rect(100, 0, 100, 100)),
        ];
        let bbox = rect(50, 10, 100, 50);
        let desired = desired_memberships(bbox, &outputs, Some("A"));
        assert_eq!(desired, vec![(0, rect(0, 0, 50, 50))]);
    }

    #[test]
    fn allowed_output_without_overlap_is_empty() {
        let outputs = vec![
            ("A".to_string(), rect(0, 0, 100, 100)),
            ("B".to_string(), rect(100, 0, 100, 100)),
        ];
        // Window sits entirely over B, but only A is allowed.
        let bbox = rect(120, 0, 50, 50);
        assert!(desired_memberships(bbox, &outputs, Some("A")).is_empty());
    }

    #[test]
    fn zero_sized_output_is_excluded() {
        let outputs = vec![("A".to_string(), rect(0, 0, 0, 0))];
        let bbox = rect(0, 0, 50, 50);
        assert!(desired_memberships(bbox, &outputs, None).is_empty());
    }
}
