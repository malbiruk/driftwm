//! Grab-scoped contact layout snapshots, without retaining client objects.
use super::{ClusterMember, DriftWm, StageWindow};
use driftwm::layout::{contact_push, snap::SnapRect};
use smithay::utils::{Logical, Point};

pub(crate) struct ContactPushSnapshot {
    pub primary: SnapRect,
    members: Vec<(ClusterMember, Point<i32, Logical>, SnapRect)>,
}

impl DriftWm {
    pub(crate) fn capture_contact_push(
        &self,
        primary: &StageWindow,
    ) -> Option<ContactPushSnapshot> {
        let rect = self.snap_rect_for(primary)?;
        let members = self
            .stage
            .windows()
            .filter(|w| *w != primary && self.is_canvas_window(*w))
            .filter_map(|w| {
                Some((
                    ClusterMember::from_element(w),
                    self.stage.position_of(w)?,
                    self.snap_rect_for(w)?,
                ))
            })
            .collect();
        Some(ContactPushSnapshot {
            primary: rect,
            members,
        })
    }
}

impl ContactPushSnapshot {
    pub(crate) fn apply(&self, data: &mut DriftWm, driven: SnapRect) {
        let live: Vec<_> = self
            .members
            .iter()
            .filter_map(|(id, loc, rect)| {
                let element = id.resolve(&data.stage)?;
                (data.is_canvas_window(&element) && data.stage.position_of(&element).is_some())
                    .then_some((element, *loc, *rect))
            })
            .collect();
        let mut input = vec![self.primary];
        input.extend(live.iter().map(|(_, _, rect)| *rect));
        let output = contact_push::resolve(&input, driven, data.config.snap_gap);
        for ((element, loc, rect), next) in live.into_iter().zip(output.into_iter().skip(1)) {
            let pos = Point::from((
                loc.x + (next.x_low - rect.x_low).round() as i32,
                loc.y + (next.y_low - rect.y_low).round() as i32,
            ));
            if data.stage.position_of(&element) != Some(pos) {
                data.end_element_animation(&element);
                data.map_window(element.clone(), pos, false);
                data.refresh_stable_snap_rect(&element);
                data.render.blur_geometry_generation += 1;
            }
        }
    }
}
