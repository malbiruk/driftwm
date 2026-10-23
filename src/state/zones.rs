//! Connected snap clusters ("zones") as a viewport navigation model.
//!
//! A zone is not a workspace: it is the connected component of snap-adjacent
//! canvas windows, recomputed from live geometry. Zones are exposed through
//! `ext-workspace-v1` when `navigation.workspace_source = "zones"`, and
//! activating one frames its whole bounding box exactly like
//! `zoom-to-fit-snapped`.

use smithay::output::Output;
use smithay::utils::{Logical, Rectangle};

use driftwm::config::WorkspaceSource;
use driftwm::layout::cluster::cluster_of;
use driftwm::protocols::ext_workspace::WorkspaceEntry;

use super::{DriftWm, StageWindow, output_logical_size, output_state};

/// Synthetic zone shown while the canvas has no windows.
pub(crate) const EMPTY_ZONE_ID: &str = "zone-empty";

/// Keep the active zone unless a rival is decisively larger. A rival has to
/// cover at least 1/0.9 (~11%) more viewport area to steal the pill.
const ACTIVE_ZONE_HYSTERESIS: f64 = 0.9;

/// One connected snap cluster.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Zone {
    /// Stable protocol id: `zone-<minimum Stage ElementId>`.
    pub id: String,
    /// Human-readable 1-based raster-order ordinal.
    pub name: String,
    /// Canvas-space bounding box of the whole cluster.
    pub bbox: Rectangle<i32, Logical>,
}

impl DriftWm {
    /// Enumerate connected snap clusters, sorted top-to-bottom then
    /// left-to-right. Display names are assigned after sorting, so they always
    /// read `1..N` in pill order. Empty canvas → no zones (the protocol layer
    /// adds the synthetic empty pill).
    #[allow(clippy::mutable_key_type)]
    pub(crate) fn zones(&self) -> Vec<Zone> {
        let rects = self.all_windows_with_snap_rects();
        if rects.is_empty() {
            return Vec::new();
        }

        let mut unvisited: std::collections::HashSet<StageWindow> =
            rects.iter().map(|(w, _)| w.clone()).collect();
        let mut zones = Vec::new();
        while let Some(root) = unvisited.iter().next().cloned() {
            let component = cluster_of(&root, &rects, self.config.snap_gap);
            for member in &component {
                unvisited.remove(member);
            }

            let min_id = component.iter().filter_map(|w| self.stage.id_of(w)).min();
            let Some(min_id) = min_id else {
                continue;
            };

            let bbox = driftwm::canvas::all_windows_bbox(component.iter().filter_map(|w| {
                let loc = self.stage.position_of(w)?;
                Some((loc, w.geometry().size))
            }));
            let Some(bbox) = bbox else {
                continue;
            };

            zones.push(Zone {
                id: format!("zone-{}", min_id.0),
                name: String::new(),
                bbox,
            });
        }

        zones.sort_by(|a, b| {
            (a.bbox.loc.y, a.bbox.loc.x, &a.id).cmp(&(b.bbox.loc.y, b.bbox.loc.x, &b.id))
        });
        for (index, zone) in zones.iter_mut().enumerate() {
            zone.name = (index + 1).to_string();
        }
        zones
    }

    /// Entries for the ext-workspace registry in the configured mode.
    /// Bookmarks keep their historical names and act as ids; zones use stable
    /// ids with ordinal display names. Both carry 1-D coordinates so bars keep
    /// the intended pill order. Takes the frame's zone snapshot so the active
    /// intersection and exported entries cannot see different geometry.
    pub(crate) fn workspace_entries(&self, zones: &[Zone]) -> Vec<WorkspaceEntry> {
        match self.config.workspace_source {
            WorkspaceSource::Bookmarks => self
                .bookmarks
                .keys()
                .enumerate()
                .map(|(index, name)| WorkspaceEntry::new(name.clone(), name.clone(), index as u32))
                .collect(),
            WorkspaceSource::Zones => {
                if zones.is_empty() {
                    vec![WorkspaceEntry::new(EMPTY_ZONE_ID, "1", 0)]
                } else {
                    zones
                        .iter()
                        .enumerate()
                        .map(|(index, zone)| {
                            WorkspaceEntry::new(zone.id.clone(), zone.name.clone(), index as u32)
                        })
                        .collect()
                }
            }
        }
    }

    /// Pick this output's active zone: the visible cluster with the largest
    /// viewport intersection, keeping the incumbent through near-ties. The
    /// synthetic empty zone is active while the canvas has no windows.
    pub(crate) fn active_zone_for_output(
        &self,
        output: &Output,
        zones: &[Zone],
        incumbent: Option<&str>,
    ) -> Option<String> {
        if zones.is_empty() {
            return Some(EMPTY_ZONE_ID.to_string());
        }

        let (camera, zoom) = {
            let os = output_state(output);
            (os.camera, os.zoom)
        };
        let visible = driftwm::canvas::visible_canvas_rect(
            camera.to_i32_round(),
            output_logical_size(output),
            zoom,
        );

        let mut best: Option<(&str, i64)> = None;
        let mut incumbent_area = None;
        for zone in zones {
            let area = intersection_area(zone.bbox, visible);
            if area <= 0 {
                continue;
            }
            if incumbent == Some(zone.id.as_str()) {
                incumbent_area = Some(area);
            }
            if best.is_none_or(|(_, best_area)| area > best_area) {
                best = Some((zone.id.as_str(), area));
            }
        }

        let (best_id, best_area) = best?;
        if let (Some(incumbent), Some(incumbent_area)) = (incumbent, incumbent_area)
            && best_id != incumbent
            && incumbent_area as f64 >= ACTIVE_ZONE_HYSTERESIS * best_area as f64
        {
            return Some(incumbent.to_string());
        }
        Some(best_id.to_string())
    }

    /// Frame the zone with `id`. The synthetic empty zone has nothing to frame,
    /// and a stale id (the cluster changed since the protocol snapshot) is a
    /// no-op.
    pub(crate) fn activate_zone(&mut self, id: &str) -> bool {
        if id == EMPTY_ZONE_ID {
            return false;
        }
        let Some(bbox) = self
            .zones()
            .into_iter()
            .find(|zone| zone.id == id)
            .map(|zone| zone.bbox)
        else {
            return false;
        };
        self.fit_to_bbox(bbox);
        true
    }

    /// Frame the zone at a 1-based raster-order ordinal, for keyboard bindings.
    pub(crate) fn activate_zone_ordinal(&mut self, ordinal: usize) -> bool {
        if ordinal == 0 {
            return false;
        }
        let Some(bbox) = self.zones().get(ordinal - 1).map(|zone| zone.bbox) else {
            return false;
        };
        self.fit_to_bbox(bbox);
        true
    }
}

fn intersection_area(a: Rectangle<i32, Logical>, b: Rectangle<i32, Logical>) -> i64 {
    let left = a.loc.x.max(b.loc.x) as i64;
    let top = a.loc.y.max(b.loc.y) as i64;
    let right = (a.loc.x + a.size.w).min(b.loc.x + b.size.w) as i64;
    let bottom = (a.loc.y + a.size.h).min(b.loc.y + b.size.h) as i64;
    if right <= left || bottom <= top {
        0
    } else {
        (right - left) * (bottom - top)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn intersection_area_clips_and_rejects_disjoint_rects() {
        assert_eq!(
            intersection_area(rect(0, 0, 10, 10), rect(5, 5, 10, 10)),
            25
        );
        assert_eq!(
            intersection_area(rect(0, 0, 10, 10), rect(10, 0, 10, 10)),
            0
        );
        assert_eq!(
            intersection_area(rect(0, 0, 10, 10), rect(100, 100, 10, 10)),
            0
        );
    }
}
