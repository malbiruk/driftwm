//! Per-window output membership: sends clients `wl_surface.enter`/`leave` for
//! the outputs each window overlaps. Replaces the membership half of
//! `Space::refresh`, driven from `post_render` and the idle turn via
//! [`DriftWm::refresh_window_outputs`].
//!
//! Three behaviours differ from `Space`'s geometric-overlap default: a
//! fullscreen or pinned window belongs only to its single home output, virtual
//! placeholder outputs (dead `wl_output` global) are never entered, and
//! xwayland-satellite's client gets one sticky output per window. Satellite
//! stores a single output association per X11 window and derives that window's
//! fake X11 root coordinates from it, so reporting every overlapping output on
//! a canvas where viewports routinely co-observe the same window would strand
//! that association and offset X11 context menus.

use std::cell::RefCell;
use std::collections::HashMap;

use smithay::desktop::Window;
use smithay::desktop::space::SpaceElement;
use smithay::output::Output;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::seat::WaylandFocus;

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
        .filter_map(|(i, (_, geo))| overlap_in(bbox, *geo).map(|overlap| (i, overlap)))
        .collect()
}

/// Intersection of `geo` with `bbox`, rebased to the bbox origin.
fn overlap_in(
    bbox: Rectangle<i32, Logical>,
    geo: Rectangle<i32, Logical>,
) -> Option<Rectangle<i32, Logical>> {
    geo.intersection(bbox).map(|mut overlap| {
        overlap.loc -= bbox.loc;
        overlap
    })
}

/// Single sticky output for xwayland-satellite's windows: the association
/// migrates only when the window genuinely leaves its current output, never
/// when a second viewport pans over it. `center` is the center of the window's
/// own area (popup-independent, so an open menu can't flip the output);
/// `current` is the presently-associated output and its last tracked overlap
/// rect, if any. Returns at most one entry, indexing `outputs`.
fn desired_satellite_membership(
    bbox: Rectangle<i32, Logical>,
    center: Point<i32, Logical>,
    outputs: &[(String, Rectangle<i32, Logical>)],
    current: Option<(&str, Rectangle<i32, Logical>)>,
) -> Vec<(usize, Rectangle<i32, Logical>)> {
    let overlap = |geo: Rectangle<i32, Logical>| overlap_in(bbox, geo);

    let current_live = current.and_then(|(name, tracked)| {
        outputs
            .iter()
            .position(|(n, _)| n == name)
            .map(|i| (i, tracked))
    });

    // Keep the current output while the window's center still sits on it.
    if let Some((i, tracked)) = current_live
        && outputs[i].1.contains(center)
    {
        return vec![(i, overlap(outputs[i].1).unwrap_or(tracked))];
    }
    // Center moved onto another output — migrate there.
    if let Some((i, geo)) = outputs
        .iter()
        .enumerate()
        .find(|(_, (_, geo))| geo.contains(center))
        .map(|(i, (_, geo))| (i, *geo))
    {
        return vec![(i, overlap(geo).unwrap_or_default())];
    }
    // Center left every output but the current one still overlaps — hold it.
    if let Some((i, _)) = current_live
        && let Some(o) = overlap(outputs[i].1)
    {
        return vec![(i, o)];
    }
    // Otherwise take the output with the largest overlap (first wins ties).
    if let Some(entry) = outputs
        .iter()
        .enumerate()
        .filter_map(|(i, (_, geo))| overlap(*geo).map(|o| (i, o)))
        .reduce(|acc, cur| {
            let area = |r: Rectangle<i32, Logical>| i64::from(r.size.w) * i64::from(r.size.h);
            if area(cur.1) > area(acc.1) { cur } else { acc }
        })
    {
        return vec![entry];
    }
    // Window fully off-screen: keep the current association untouched so
    // satellite's coordinate space survives (no event fires — nothing changed).
    // Empty only when the current output is gone and nothing overlaps.
    match current_live {
        Some((i, tracked)) => vec![(i, tracked)],
        None => Vec::new(),
    }
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

        let windows: Vec<Window> = self.stage.windows().cloned().collect();
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

            let tracker = window.user_data().get_or_insert(WindowOutputs::default);
            let mut map = tracker.0.borrow_mut();

            // Satellite windows report a single sticky output; the fullscreen/pin
            // restriction already yields one output, so it takes precedence.
            let use_satellite = allowed.is_none()
                && window
                    .wl_surface()
                    .and_then(|s| s.client())
                    .is_some_and(|c| self.client_is_satellite(&c));

            let desired: Vec<(Output, Rectangle<i32, Logical>)> = if use_satellite {
                let geo = window.geometry();
                let center = pos + Point::from((geo.size.w / 2, geo.size.h / 2));
                // The largest-rect live tracked output is the current association
                // (windows hold at most one; a transient several converges here,
                // deterministically rather than by hash order).
                let current = map
                    .iter()
                    .filter(|(o, _)| named.iter().any(|(n, _)| n == &o.name()))
                    .max_by_key(|(_, r)| i64::from(r.size.w) * i64::from(r.size.h))
                    .map(|(o, r)| (o.name(), *r));
                let current = current.as_ref().map(|(n, r)| (n.as_str(), *r));
                desired_satellite_membership(bbox, center, &named, current)
                    .into_iter()
                    .map(|(i, overlap)| (candidates[i].0.clone(), overlap))
                    .collect()
            } else {
                desired_memberships(bbox, &named, allowed)
                    .into_iter()
                    .map(|(i, overlap)| (candidates[i].0.clone(), overlap))
                    .collect()
            };
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

    fn center(x: i32, y: i32) -> Point<i32, Logical> {
        Point::from((x, y))
    }

    fn two_outputs() -> Vec<(String, Rectangle<i32, Logical>)> {
        vec![
            ("A".to_string(), rect(0, 0, 100, 100)),
            ("B".to_string(), rect(100, 0, 100, 100)),
        ]
    }

    #[test]
    fn satellite_overlap_of_both_keeps_current() {
        // Straddles the A|B seam but its center sits in A, which is current.
        let bbox = rect(40, 10, 80, 50);
        let got = desired_satellite_membership(
            bbox,
            center(80, 35),
            &two_outputs(),
            Some(("A", rect(0, 0, 60, 50))),
        );
        assert_eq!(got, vec![(0, rect(0, 0, 60, 50))]);
    }

    #[test]
    fn satellite_center_crossing_switches_output() {
        // Center now sits on B (x = 100 belongs to B under half-open bounds).
        let bbox = rect(60, 10, 80, 50);
        let got = desired_satellite_membership(
            bbox,
            center(100, 35),
            &two_outputs(),
            Some(("A", rect(0, 0, 40, 50))),
        );
        assert_eq!(got, vec![(1, rect(40, 0, 40, 50))]);
    }

    #[test]
    fn satellite_poison_sequence_never_enters_second_output() {
        let outputs = two_outputs();
        // Enter A.
        let step1 =
            desired_satellite_membership(rect(10, 10, 50, 50), center(35, 35), &outputs, None);
        assert_eq!(step1, vec![(0, rect(0, 0, 50, 50))]);
        // B now overlaps the window, but the center stays in A.
        let step2 = desired_satellite_membership(
            rect(10, 10, 120, 50),
            center(35, 35),
            &outputs,
            Some(("A", step1[0].1)),
        );
        assert_eq!(step2[0].0, 0);
        // B stops overlapping — still A, never B.
        let step3 = desired_satellite_membership(
            rect(10, 10, 50, 50),
            center(35, 35),
            &outputs,
            Some(("A", step2[0].1)),
        );
        assert_eq!(step3[0].0, 0);
    }

    #[test]
    fn satellite_fully_offscreen_keeps_current() {
        // Nothing overlaps: A is retained with its last tracked rect (no leave).
        let got = desired_satellite_membership(
            rect(500, 500, 50, 50),
            center(525, 525),
            &two_outputs(),
            Some(("A", rect(0, 0, 50, 50))),
        );
        assert_eq!(got, vec![(0, rect(0, 0, 50, 50))]);
    }

    #[test]
    fn satellite_current_unplugged_falls_through_to_largest_overlap() {
        // A was hot-unplugged; only B and C remain and the center lands in the
        // gap between them, so the larger overlap (C) wins.
        let outputs = vec![
            ("B".to_string(), rect(100, 0, 100, 100)),
            ("C".to_string(), rect(300, 0, 100, 100)),
        ];
        let got = desired_satellite_membership(
            rect(180, 10, 160, 50),
            center(260, 35),
            &outputs,
            Some(("A", rect(0, 0, 50, 50))),
        );
        assert_eq!(got, vec![(1, rect(120, 0, 40, 50))]);
    }
}
