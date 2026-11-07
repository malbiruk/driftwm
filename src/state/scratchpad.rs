//! Scratchpad windows: `toggle-scratchpad` stows a window by `app_id` without
//! unmapping it, and brings it back at the pin site it left from.
//!
//! The window stays on the stage with its client, size and pin site intact.
//! Stowing only adds it to `scratchpad_hidden`, the same off-screen,
//! out-of-every-relation state a first-commit deferred adopt uses
//! ([`DriftWm::root_hidden`]), so showing it again is one frame with no
//! animation.

use driftwm::window_ext::WindowExt;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::seat::WaylandFocus;

use super::{DriftWm, StageWindow};

impl DriftWm {
    pub(crate) fn toggle_scratchpad(&mut self, app_id: &str, command: &str) {
        let Some(window) = self.scratchpad_window(app_id) else {
            crate::state::spawn_command(command, &self.config.child_env);
            return;
        };
        let Some(surface) = window.wl_surface().map(|s| s.into_owned()) else {
            return;
        };
        if self.scratchpad_hidden.contains(&surface) {
            self.show_scratchpad(&window, &surface);
        } else {
            self.hide_scratchpad(&window, surface);
        }
    }

    fn scratchpad_window(&self, app_id: &str) -> Option<Window> {
        self.stage
            .windows()
            .filter_map(|w| w.client())
            .find(|w| !w.is_widget() && w.app_id_or_class().as_deref() == Some(app_id))
            .cloned()
    }

    fn show_scratchpad(&mut self, window: &Window, surface: &WlSurface) {
        self.scratchpad_hidden.retain(|s| s != surface);
        // Cancelled so a stale entry can't ease it in from where it last was.
        self.cancel_window_animation(window);
        self.raise_and_focus(window, SERIAL_COUNTER.next_serial());
    }

    fn hide_scratchpad(&mut self, window: &Window, surface: WlSurface) {
        let was_focused = self.focused_window().as_ref() == Some(window);
        self.scratchpad_hidden.push(surface);
        self.cancel_window_animation(window);
        self.stage
            .drop_from_focus_history(&StageWindow::from(window.clone()));
        if was_focused {
            self.focus_most_recent();
        }
    }

    /// Hand focus to the most recently focused visible window, or to nothing.
    fn focus_most_recent(&mut self) {
        let serial = SERIAL_COUNTER.next_serial();
        let next = self
            .stage
            .focus_history()
            .iter()
            .filter_map(|w| w.client())
            .find(|w| !self.hidden_by_deferred_adopt(*w))
            .cloned();
        match next {
            Some(w) => self.raise_and_focus(&w, serial),
            None => self.set_window_focus(None, serial),
        }
    }
}
