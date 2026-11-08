//! Scratchpad windows: `toggle-scratchpad` stows a window by `app_id` without
//! unmapping it, and brings it back at the pin site it left from.
//!
//! The window stays on the stage with its client, size and pin site intact.
//! Stowing only adds it to `scratchpad_hidden`, the same off-screen,
//! out-of-every-relation state a first-commit deferred adopt uses
//! ([`DriftWm::root_hidden`]), so showing it again is one frame with no
//! animation. It shows on the output in use, not the one it was stowed from.

use std::time::{Duration, Instant};

use driftwm::window_ext::WindowExt;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::seat::WaylandFocus;

use super::{DriftWm, StageWindow};

/// How long a spawned scratchpad command is given to map its window before
/// another press may spawn it again. Covers slow starters (a mail client can
/// take seconds to show anything), where each extra press would otherwise
/// launch another instance.
const SPAWN_DEBOUNCE: Duration = Duration::from_secs(3);

impl DriftWm {
    #[cfg(test)]
    pub(crate) fn toggle_scratchpad(&mut self, app_id: &str, command: &str) {
        self.toggle_scratchpad_from(app_id, command, None);
    }

    /// `was_fullscreen` is the window that was fullscreen on the active output
    /// before the action guard ended it: if that is one of the windows being
    /// stowed, it is put back into fullscreen when shown.
    pub(crate) fn toggle_scratchpad_from(
        &mut self,
        app_id: &str,
        command: &str,
        was_fullscreen: Option<&Window>,
    ) {
        let windows = self.scratchpad_windows(app_id);
        if windows.is_empty() {
            let recent = self
                .scratchpad_spawns
                .get(app_id)
                .is_some_and(|at| at.elapsed() < SPAWN_DEBOUNCE);
            if !recent {
                self.scratchpad_spawns
                    .insert(app_id.to_owned(), Instant::now());
                crate::state::spawn_command(command, &self.config.child_env);
            }
            return;
        }
        self.scratchpad_spawns.remove(app_id);
        let surfaces: Vec<WlSurface> = windows
            .iter()
            .filter_map(|w| w.wl_surface().map(|s| s.into_owned()))
            .collect();
        // A window that hasn't had its first placement yet would be stowed
        // before its rules apply, and placement skips what is hidden.
        if surfaces.iter().any(|s| self.pending_center.contains(s)) {
            return;
        }
        if surfaces.iter().all(|s| self.scratchpad_hidden.contains(s)) {
            self.show_scratchpad(&windows, &surfaces);
        } else {
            self.hide_scratchpad(&windows, surfaces, was_fullscreen);
        }
    }

    /// Every window of the scratchpad: all non-widget windows with this exact
    /// `app_id` (a mail client's compose windows share its `app_id`), plus
    /// their transient children and dialogs. The toggle acts on them together.
    fn scratchpad_windows(&self, app_id: &str) -> Vec<Window> {
        let clients: Vec<Window> = self
            .stage
            .windows()
            .filter_map(|w| w.client())
            .cloned()
            .collect();
        let mut group: Vec<Window> = clients
            .iter()
            .filter(|w| !w.is_widget() && w.app_id_or_class().as_deref() == Some(app_id))
            .cloned()
            .collect();
        loop {
            let before = group.len();
            for w in &clients {
                if group.contains(w) {
                    continue;
                }
                let child_of_group = w
                    .parent_surface()
                    .is_some_and(|p| group.iter().any(|g| g.wl_surface().as_deref() == Some(&p)));
                if child_of_group {
                    group.push(w.clone());
                }
            }
            if group.len() == before {
                return group;
            }
        }
    }

    fn show_scratchpad(&mut self, windows: &[Window], surfaces: &[WlSurface]) {
        self.scratchpad_hidden.retain(|s| !surfaces.contains(s));
        // The scratchpad follows the display in use: a pin left on another
        // output is rebound to the active one, at the same relative position.
        if let Some(active) = self.active_output() {
            for window in windows {
                self.send_pinned_to_output_relative(window, &active);
            }
        }
        for (window, surface) in windows.iter().zip(surfaces) {
            // Cancelled so a stale entry can't ease it in from where it last was.
            self.cancel_window_animation(window);
            // A fullscreen / maximize the app asked for while stowed was queued.
            self.apply_queued_geometry_request(surface);
        }
        // Stage order, so a parent is raised before its children.
        let serial = SERIAL_COUNTER.next_serial();
        for window in windows {
            self.raise_and_focus(window, serial);
        }
    }

    fn hide_scratchpad(
        &mut self,
        windows: &[Window],
        surfaces: Vec<WlSurface>,
        was_fullscreen: Option<&Window>,
    ) {
        let was_focused = self.focused_window().is_some_and(|f| windows.contains(&f));
        for (window, surface) in windows.iter().zip(surfaces) {
            if !self.scratchpad_hidden.contains(&surface) {
                self.scratchpad_hidden.push(surface);
            }
            self.cancel_window_animation(window);
            // The guard has already ended its fullscreen so the desktop shows;
            // queue it to be entered again on show, as for a request made
            // while stowed.
            if was_fullscreen == Some(window)
                && let Some(s) = window.wl_surface()
            {
                self.pending_fullscreen.insert(s.into_owned(), None);
            }
            self.stage
                .drop_from_focus_history(&StageWindow::from(window.clone()));
        }
        if was_focused {
            self.focus_most_recent();
        }
    }

    /// Show again any stowed window the config no longer lets a binding reach
    /// (its binding was removed or points at another `app_id`). Run after a
    /// reload; a stowed window that is still bound is left alone.
    pub(crate) fn release_unreachable_scratchpads(&mut self) {
        let stranded: Vec<WlSurface> = self
            .scratchpad_hidden
            .iter()
            .filter(|s| {
                let Some(w) = self.window_for_surface(s) else {
                    return false;
                };
                // A child is reached through its parent's binding.
                let parent_stowed = w
                    .parent_surface()
                    .is_some_and(|p| self.scratchpad_hidden.contains(&p));
                let bound = w
                    .app_id_or_class()
                    .is_some_and(|a| self.config.is_scratchpad_app(&a));
                !parent_stowed && !bound
            })
            .cloned()
            .collect();
        if stranded.is_empty() {
            return;
        }
        let mut release = stranded;
        // Children of a released window go with it.
        for s in release.clone() {
            if let Some(w) = self.window_for_surface(&s) {
                for c in self.scratchpad_children(&w) {
                    if let Some(cs) = c.wl_surface().map(|x| x.into_owned()) {
                        release.push(cs);
                    }
                }
            }
        }
        self.scratchpad_hidden.retain(|s| !release.contains(s));
        for s in &release {
            self.apply_queued_geometry_request(s);
        }
    }

    fn scratchpad_children(&self, parent: &Window) -> Vec<Window> {
        let Some(ps) = parent.wl_surface().map(|s| s.into_owned()) else {
            return Vec::new();
        };
        self.stage
            .windows()
            .filter_map(|w| w.client())
            .filter(|w| w.parent_surface().as_ref() == Some(&ps))
            .cloned()
            .collect()
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
