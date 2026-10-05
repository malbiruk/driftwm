//! Temporary Binding-held framing; deliberate camera panning commits the current view.

use crate::state::{DriftWm, ZoomAnimationAnchor, output_state};
use driftwm::config::Action;
use smithay::utils::Point;

impl DriftWm {
    pub(crate) fn begin_held_overview(&mut self) {
        if self.held_overview {
            return;
        }
        let Some(output) = self.active_output() else {
            return;
        };
        self.cancel_animations_on(&output);
        self.set_overview_return(None);
        self.execute_action(&Action::ZoomToFit);
        self.held_overview = true;
        self.held_overview_output = Some(output);
    }

    pub(crate) fn finish_held_overview(&mut self) {
        self.held_overview = false;
        let Some(output) = self.held_overview_output.take() else {
            return;
        };
        // A later navigation clears the return, so release cannot undo it.
        let mut os = output_state(&output);
        let Some((camera, zoom)) = os.overview_return.take() else {
            return;
        };
        let usable = smithay::desktop::layer_map_for_output(&output).non_exclusive_zone();
        let screen = Point::from((
            usable.loc.x as f64 + usable.size.w as f64 / 2.0,
            usable.loc.y as f64 + usable.size.h as f64 / 2.0,
        ));
        os.zoom_animation_anchor = Some(ZoomAnimationAnchor {
            canvas: Point::from((camera.x + screen.x / zoom, camera.y + screen.y / zoom)),
            screen,
        });
        os.camera_target = Some(camera);
        os.zoom_target = Some(zoom);
    }

    pub(crate) fn cancel_held_overview_return(&mut self) {
        let Some(output) = self.held_overview_output.take() else {
            return;
        };
        self.cancel_animations_on(&output);
        output_state(&output).overview_return = None;
    }
}
