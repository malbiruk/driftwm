//! Runtime mosaic over a window's picture, for covering sensitive content
//! while screen recording. The flag lives on the surface's stored
//! `AppliedWindowRule`, like `msg opacity`: it is not saved with the session
//! and is gone when the window closes.

use smithay::wayland::compositor::with_states;
use smithay::wayland::seat::WaylandFocus;

use driftwm::config::AppliedWindowRule;

use crate::state::{DriftWm, StageWindow};

impl DriftWm {
    /// Flip the mosaic on the focused client window. A stand-in has no client
    /// picture to cover, so it is left alone.
    pub fn toggle_obscure_focused(&mut self) {
        let Some(StageWindow::Client(window)) = self.focused_element() else {
            return;
        };
        let Some(surface) = window.wl_surface() else {
            return;
        };
        with_states(&surface, |states| {
            states.data_map.insert_if_missing_threadsafe(|| {
                std::sync::Mutex::new(AppliedWindowRule::default())
            });
            let mut rule = states
                .data_map
                .get::<std::sync::Mutex<AppliedWindowRule>>()
                .unwrap()
                .lock()
                .unwrap();
            rule.obscured = !rule.obscured;
        });
        self.mark_all_dirty();
    }
}
