//! XWayland-specific methods on `DriftWm`.
//!
//! Covers X11 activation/stacking sync, X11-root position re-anchoring for
//! cursor events, OR surface canvas positioning, and the Esc-synthesis path
//! for X11 popup dismissal. All of this is scoped to the `x11_*` fields on
//! `DriftWm` plus access to `space` and the active output.

use smithay::{
    desktop::Window,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle},
    xwayland::X11Surface,
};

use super::{DriftWm, output_logical_size, output_state};
use crate::focus::FocusTarget;

impl DriftWm {
    /// Update `_NET_WM_STATE_FOCUSED` on X11 windows when keyboard focus moves.
    /// Driven from `SeatHandler::focus_changed`, so every focus transition is
    /// covered regardless of which call site invoked `keyboard.set_focus`.
    pub fn sync_x11_activated(&mut self, new_focus: Option<&WlSurface>) {
        let new_x11 = new_focus.and_then(|s| self.find_x11_surface_by_wl(s));

        if let Some(prev) = self.last_x11_focused.take()
            && Some(&prev) != new_x11.as_ref()
            && !prev.is_override_redirect()
        {
            let _ = prev.set_activated(false);
        }

        if let Some(x11) = new_x11
            && !x11.is_override_redirect()
        {
            let _ = x11.set_activated(true);
            self.last_x11_focused = Some(x11);
        }
    }

    /// Find a mapped window wrapping the given X11 surface.
    pub fn find_x11_window(&self, x11: &X11Surface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.x11_surface() == Some(x11))
            .cloned()
    }

    /// Sync an X11 toplevel's X11-root position to its compositor screen
    /// position (`canvas_loc - camera`). Without this, X11 windows larger
    /// than the X11 root (= wl_output bounding box) get pointer events
    /// clamped to root edges, producing dead zones in the surface periphery.
    /// XWayland computes the absolute X11 pointer position as
    /// `drawable_x + event_x` and the X server clamps to root bounds; making
    /// `drawable_x = window_screen_x` cancels out the clamp at zoom = 1.0.
    /// No-op for OR surfaces, unmapped windows, or unchanged positions.
    pub fn sync_x11_position(&self, window: &Window) {
        let Some(x11) = window.x11_surface() else { return };
        if x11.is_override_redirect() {
            return;
        }
        let Some(canvas_loc) = self.space.element_location(window) else { return };
        let Some(output) = self.active_output() else { return };
        let camera = output_state(&output).camera;
        let new_loc: Point<i32, Logical> = (
            canvas_loc.x - camera.x.round() as i32,
            canvas_loc.y - camera.y.round() as i32,
        )
            .into();
        let geo = x11.geometry();
        if geo.loc == new_loc {
            return;
        }
        let _ = x11.configure(Rectangle::new(new_loc, geo.size));
    }

    /// Sync X11 root positions for every X11 toplevel in `space`. Call after
    /// the camera moves so visible X11 windows keep cursor events unclamped.
    pub fn sync_all_x11_positions(&self) {
        let windows: Vec<Window> = self
            .space
            .elements()
            .filter(|w| w.x11_surface().is_some())
            .cloned()
            .collect();
        for w in &windows {
            self.sync_x11_position(w);
        }
    }

    /// Re-anchor an X11 window's X11-root position so the cursor's surface
    /// coord lands near the root center, only when the cursor would
    /// otherwise be clamped (its computed X11 absolute coord is outside
    /// root bounds with a margin). Complements `sync_x11_position` for
    /// zoom levels where cursor canvas reach exceeds the X11 root size.
    /// Called from pointer-focus dispatch with the live cursor canvas pos.
    pub fn nudge_x11_root_for_cursor(
        &self,
        wl_surface: &WlSurface,
        cursor_canvas: Point<f64, Logical>,
    ) {
        let Some(x11) = self.find_x11_surface_by_wl(wl_surface) else { return };
        if x11.is_override_redirect() {
            return;
        }
        let Some(window) = self.find_x11_window(&x11) else { return };
        let Some(canvas_loc) = self.space.element_location(&window) else { return };
        let Some(output) = self.active_output() else { return };
        let root_size = output_logical_size(&output);
        let surface_x = cursor_canvas.x - canvas_loc.x as f64;
        let surface_y = cursor_canvas.y - canvas_loc.y as f64;
        let geo = x11.geometry();
        let cursor_root_x = (geo.loc.x as f64 + surface_x).round() as i32;
        let cursor_root_y = (geo.loc.y as f64 + surface_y).round() as i32;
        // Margin keeps a buffer before clamping kicks in, so we don't
        // re-configure on every pixel of motion near the edges.
        const MARGIN: i32 = 64;
        let in_bounds = cursor_root_x >= MARGIN
            && cursor_root_x < root_size.w - MARGIN
            && cursor_root_y >= MARGIN
            && cursor_root_y < root_size.h - MARGIN;
        if in_bounds {
            return;
        }
        let new_loc: Point<i32, Logical> = (
            root_size.w / 2 - surface_x.round() as i32,
            root_size.h / 2 - surface_y.round() as i32,
        )
            .into();
        if new_loc == geo.loc {
            return;
        }
        let _ = x11.configure(Rectangle::new(new_loc, geo.size));
    }

    /// Raise the X11 window owning `wl_surface` to the top of XWayland's
    /// internal stack — *only* on the X11 side, never touches compositor
    /// visual stacking. Called from pointer-focus dispatch so hover events
    /// route to the visually-topmost X11 client when overlapping windows
    /// share canvas coords. No-op if the surface isn't an X11 window or is
    /// already top, and skipped for override-redirect surfaces.
    pub fn raise_x11_for_hover(&mut self, wl_surface: &WlSurface) {
        let Some(x11) = self.find_x11_surface_by_wl(wl_surface) else { return };
        if x11.is_override_redirect() {
            return;
        }
        if self.last_x11_hover_raised.as_ref() == Some(&x11) {
            return;
        }
        let Some(xwm) = self.x11_wm.as_mut() else { return };
        if let Err(err) = xwm.raise_window(&x11) {
            tracing::warn!(?err, "Failed to raise X11 window for hover");
            return;
        }
        self.last_x11_hover_raised = Some(x11);
    }

    /// Push the compositor's current bottom→top window order into XWayland
    /// so the X server's stacking matches the visual stacking. Required for
    /// correct pointer-event routing between overlapping X11 clients.
    /// Skips the call for ≤1 X11 windows — nothing to reorder, and the
    /// underlying `grab_server` round-trip can delay input events.
    pub fn update_x11_stacking_order(&mut self) {
        let Some(xwm) = self.x11_wm.as_mut() else { return };
        let order: Vec<X11Surface> = self
            .space
            .elements()
            .filter_map(|w| w.x11_surface().cloned())
            .collect();
        if order.len() < 2 {
            return;
        }
        if let Err(err) = xwm.update_stacking_order_upwards(order.iter()) {
            tracing::warn!(?err, "Failed to sync X11 stacking order");
        }
    }

    /// Briefly redirect keyboard focus to `or_wl`, deliver Esc press+release,
    /// then restore the prior focus. Used for X11 popup-menu dismissal where
    /// the popup app (jgmenu, dmenu) has an X11 KeyboardGrab and dismisses
    /// on Esc — but doesn't install a Wayland-side xwayland-keyboard-grab,
    /// so Esc only reaches it when its wl_surface holds keyboard focus.
    pub fn synth_esc_to_or(&mut self, or_wl: WlSurface) {
        use smithay::backend::input::KeyState;
        use smithay::input::keyboard::{FilterResult, Keycode};
        let keyboard = self.seat.get_keyboard().unwrap();
        let prev = keyboard.current_focus();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, Some(FocusTarget(or_wl)), serial);

        let time = self.start_time.elapsed().as_millis() as u32;
        let esc = Keycode::new(9); // evdev KEY_ESC (1) + 8
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.input::<(), _>(self, esc, KeyState::Pressed,
            smithay::utils::SERIAL_COUNTER.next_serial(), time,
            |_, _, _| FilterResult::Forward);
        keyboard.input::<(), _>(self, esc, KeyState::Released,
            smithay::utils::SERIAL_COUNTER.next_serial(), time,
            |_, _, _| FilterResult::Forward);

        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, prev, smithay::utils::SERIAL_COUNTER.next_serial());
    }

    /// Find the X11Surface whose underlying wl_surface matches the given one.
    /// Searches both managed windows (in `space`) and override-redirect popups
    /// — the latter matters for the xwayland-keyboard-grab protocol, which
    /// hands us an OR's wl_surface when the X11 client calls XGrabKeyboard.
    pub fn find_x11_surface_by_wl(&self, wl: &WlSurface) -> Option<X11Surface> {
        self.space
            .elements()
            .filter_map(|w| w.x11_surface().cloned())
            .find(|x11| x11.wl_surface().as_ref() == Some(wl))
            .or_else(|| {
                self.x11_override_redirect
                    .iter()
                    .find(|x11| x11.wl_surface().as_ref() == Some(wl))
                    .cloned()
            })
    }

    /// Compute the canvas position of an override-redirect X11 surface.
    /// OR windows use absolute X11 root coords; we map them relative to
    /// their parent's canvas position, or center them if no parent exists.
    pub fn or_canvas_position(&self, or_surface: &X11Surface) -> Point<i32, Logical> {
        let or_geo = or_surface.geometry();

        if let Some(parent_id) = or_surface.is_transient_for() {
            // Search managed windows in Space for parent
            let parent_in_space = self
                .space
                .elements()
                .find(|w| w.x11_surface().is_some_and(|x| x.window_id() == parent_id));
            if let Some(parent_win) = parent_in_space {
                let parent_canvas = self.space.element_location(parent_win).unwrap_or_default();
                let parent_x11_loc = parent_win.x11_surface().unwrap().geometry().loc;
                return parent_canvas + (or_geo.loc - parent_x11_loc);
            }

            // Search other OR windows (nested menus) with depth limit
            fn find_or_parent(
                or_list: &[X11Surface],
                space: &smithay::desktop::Space<smithay::desktop::Window>,
                target_id: u32,
                depth: u32,
            ) -> Option<Point<i32, Logical>> {
                if depth == 0 {
                    return None;
                }
                let parent_or = or_list.iter().find(|w| w.window_id() == target_id)?;
                let parent_geo = parent_or.geometry();
                if let Some(grandparent_id) = parent_or.is_transient_for() {
                    // Check Space first
                    let gp_in_space = space.elements().find(|w| {
                        w.x11_surface()
                            .is_some_and(|x| x.window_id() == grandparent_id)
                    });
                    if let Some(gp_win) = gp_in_space {
                        let gp_canvas = space.element_location(gp_win).unwrap_or_default();
                        let gp_x11_loc = gp_win.x11_surface().unwrap().geometry().loc;
                        return Some(gp_canvas + (parent_geo.loc - gp_x11_loc));
                    }
                    // Recurse into OR list
                    let gp_canvas = find_or_parent(or_list, space, grandparent_id, depth - 1)?;
                    return Some(
                        gp_canvas
                            + (parent_geo.loc
                                - or_list
                                    .iter()
                                    .find(|w| w.window_id() == grandparent_id)
                                    .map(|w| w.geometry().loc)
                                    .unwrap_or_default()),
                    );
                }
                None
            }

            if let Some(parent_canvas) =
                find_or_parent(&self.x11_override_redirect, &self.space, parent_id, 10)
            {
                let parent_or = self
                    .x11_override_redirect
                    .iter()
                    .find(|w| w.window_id() == parent_id);
                let parent_x11_loc = parent_or.map(|w| w.geometry().loc).unwrap_or_default();
                return parent_canvas + (or_geo.loc - parent_x11_loc);
            }
        }

        // No transient_for: decide between cursor-pinned and managed-window
        // anchoring by whether the OR's WM_CLASS matches any managed X11
        // window. No match → OR-only app (jgmenu / dmenu-style), use the
        // cursor-pinned `or_root_anchor`. Match → managed-app OR (Steam
        // hover menus etc.), use `pick_anchor` with that window as anchor.
        // All X11 clients share one wl_client so class is the best signal.
        let or_class = or_surface.class();
        let has_class_match = !or_class.is_empty()
            && self.space.elements().any(|w| {
                w.x11_surface().is_some_and(|x| x.class() == or_class)
            });

        if !has_class_match
            && let Some(anchor) = self.or_root_anchor
        {
            return anchor + or_geo.loc;
        }

        // Managed-app unparented OR, or fallback when no cursor anchor
        // is pinned yet. Prefer the X11 window raised on hover (most
        // likely the creator), then last-focused, then topmost.
        let pick_anchor = |target: Option<&X11Surface>| {
            self.space.elements().rev().find_map(|w| {
                let x11 = w.x11_surface()?;
                if let Some(t) = target
                    && x11 != t
                {
                    return None;
                }
                let canvas_loc = self.space.element_location(w)?;
                Some((canvas_loc, x11.geometry().loc))
            })
        };
        let anchor = pick_anchor(self.last_x11_hover_raised.as_ref())
            .or_else(|| pick_anchor(self.last_x11_focused.as_ref()))
            .or_else(|| pick_anchor(None));
        if let Some((anchor_canvas, anchor_x11)) = anchor {
            return anchor_canvas + (or_geo.loc - anchor_x11);
        }

        // Last resort: cursor-pinned anchor, regardless of class-match.
        if let Some(anchor) = self.or_root_anchor {
            return anchor + or_geo.loc;
        }

        // No X11 windows at all: center in viewport
        self.active_output()
            .and_then(|o| self.space.output_geometry(&o))
            .map(|viewport| {
                let cam = self.camera();
                let z = self.zoom();
                Point::from((
                    (cam.x + viewport.size.w as f64 / (2.0 * z)) as i32 - or_geo.size.w / 2,
                    (cam.y + viewport.size.h as f64 / (2.0 * z)) as i32 - or_geo.size.h / 2,
                ))
            })
            .unwrap_or_default()
    }
}
