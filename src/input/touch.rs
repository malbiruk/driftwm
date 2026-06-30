use std::any::Any;
use std::cell::RefCell;
use std::collections::HashSet;
use std::time::Duration;

use crate::decorations::DecorationHit;
use crate::grabs::{MoveSurfaceGrab, ResizeState, ResizeSurfaceGrab, TouchGestureGrab};
use crate::state::{DriftWm, FocusTarget, SessionLock, output_state};
use driftwm::canvas::{ScreenPos, screen_to_canvas};
use driftwm::window_ext::WindowExt;
use smithay::{
    backend::input::{AbsolutePositionEvent, Event, InputBackend, TouchEvent, TouchSlot},
    desktop::Window,
    input::touch::{DownEvent, GrabStartData as TouchGrabStartData, MotionEvent, UpEvent},
    output::Output,
    reexports::{
        calloop::{
            RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        input::Device as LibinputDevice,
        wayland_protocols::xdg::shell::server::xdg_toplevel,
    },
    utils::{IsAlive, Logical, Point, SERIAL_COUNTER},
    wayland::{compositor::with_states, seat::WaylandFocus},
};

/// A close-button press awaiting release. Fires only if the finger lifts while
/// still inside the button — touch's analogue of the pointer close path.
pub struct PendingClose {
    slot: TouchSlot,
    window: Window,
    last_canvas: Point<f64, Logical>,
}

/// Active touch window-move that is edge-panning the camera. The animation loop
/// re-drives the move from the finger's fixed screen position each frame (the
/// touch analogue of warping the pointer), so the window keeps following the
/// finger as the canvas scrolls under it.
#[derive(Clone)]
pub struct TouchEdgePan {
    pub slot: TouchSlot,
    pub screen_pos: Point<f64, Logical>,
    pub output: Output,
}

/// Coordinator-side touch state. Per-gesture state lives in `TouchGestureGrab`;
/// this only holds what must survive across grab lifetimes.
pub struct TouchState {
    /// Timestamp of the last clean 3-finger tap, for double-tap detection.
    pub last_three_finger_tap: Option<u32>,
    pub pending_close: Option<PendingClose>,
    /// Single-tap center deferred until the double-tap window elapses, so a
    /// follow-up double-tap (fit) / double-tap-drag (move) doesn't flash a
    /// center first. Cancelled when a second 3-finger gesture supersedes it.
    pub pending_center_timer: Option<RegistrationToken>,
    /// Set by an active touch move grab while the finger sits in an edge zone.
    /// Cleared when the grab ends or the finger leaves the zone.
    pub edge_pan: Option<TouchEdgePan>,
    /// Output the live touch interaction maps to, resolved once at touch-down
    /// and reused for the rest of the sequence. Motion reads this instead of
    /// re-resolving per event, so it can't diverge from the grab's output on a
    /// mid-gesture hotplug or `map_to_output` reload (and avoids per-event work).
    pub output: Option<Output>,
}

impl TouchState {
    pub fn new() -> Self {
        Self {
            last_three_finger_tap: None,
            pending_close: None,
            pending_center_timer: None,
            edge_pan: None,
            output: None,
        }
    }
}

impl DriftWm {
    /// Output a touch from `device` maps to. Resolved per-device so multiple
    /// touchscreens each drive their own monitor. Resolution order: explicit
    /// config first, then libinput's output tag, then a single-output shortcut,
    /// then physical-size match (a digitizer is the same physical size as the
    /// panel it overlays), then the internal panel, then the first output.
    ///
    /// The last two steps are best-effort guesses: a device that reports no
    /// output tag and no physical size on a multi-output system falls back to
    /// the internal panel even if it's an external touchscreen. Set
    /// `[touch] map_to_output` to pin such a device explicitly.
    pub(crate) fn touch_output_for_device<I: InputBackend>(
        &self,
        device: &I::Device,
    ) -> Option<Output>
    where
        I::Device: 'static,
    {
        if let Some(name) = self.config.touch.map_to_output.as_deref()
            && let Some(o) = self.output_by_name(name)
        {
            return Some(o);
        }

        let libinput_device = (device as &dyn Any).downcast_ref::<LibinputDevice>();

        if let Some(name) = libinput_device.and_then(LibinputDevice::output_name)
            && let Some(o) = self.output_by_name(&name)
        {
            return Some(o);
        }

        let mut outputs = self.space.outputs();
        let first = outputs.next().cloned();
        if outputs.next().is_none() {
            return first; // zero or one output: unambiguous
        }

        if let Some((dev_w, dev_h)) = libinput_device.and_then(LibinputDevice::size)
            && let Some(o) = self.space.outputs().find(|o| {
                let size = o.physical_properties().size;
                physical_size_matches(size.w as f64, size.h as f64, dev_w, dev_h)
            })
        {
            return Some(o.clone());
        }

        if let Some(o) = self.space.outputs().find(|o| is_internal_output(&o.name())) {
            return Some(o.clone());
        }

        first
    }

    fn output_by_name(&self, name: &str) -> Option<Output> {
        self.space.outputs().find(|o| o.name() == name).cloned()
    }

    /// Schedule a deferred single-tap center for `window` after `delay`. Any
    /// prior pending center is cancelled first.
    pub(crate) fn schedule_pending_center(&mut self, window: Window, delay: Duration) {
        self.cancel_pending_center();
        let timer = Timer::from_duration(delay);
        let token = self
            .loop_handle
            .insert_source(timer, move |_, _, data: &mut DriftWm| {
                data.touch_state.pending_center_timer = None;
                if window.alive() && data.is_canvas_window(&window) {
                    data.navigate_to_window(&window, true);
                }
                TimeoutAction::Drop
            })
            .ok();
        self.touch_state.pending_center_timer = token;
    }

    /// Cancel a pending deferred center, if any.
    pub(crate) fn cancel_pending_center(&mut self) {
        if let Some(token) = self.touch_state.pending_center_timer.take() {
            self.loop_handle.remove(token);
        }
    }

    /// Set up a touch resize grab on `window` for `edges`: clear fit state, mark
    /// the surface/toplevel resizing (for commit-time top/left repositioning),
    /// and build the grab. Single-window, canvas-space only (no pinned path).
    pub(crate) fn build_touch_resize_grab(
        &mut self,
        window: &Window,
        edges: xdg_toplevel::ResizeEdge,
        touch_start: TouchGrabStartData<DriftWm>,
        output: Output,
        slots: usize,
    ) -> Option<ResizeSurfaceGrab> {
        let initial_window_location = self.space.element_location(window)?;
        let initial_window_size = window.geometry().size;
        let wl_surface = window.wl_surface().map(|s| s.into_owned())?;

        crate::state::fit::clear_fit_state(&wl_surface);

        with_states(&wl_surface, |states| {
            states
                .data_map
                .get_or_insert(|| RefCell::new(ResizeState::Idle))
                .replace(ResizeState::Resizing {
                    edges,
                    initial_window_location,
                    initial_window_size,
                    initial_screen_pos: None,
                });
        });

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
                state.states.unset(xdg_toplevel::State::Maximized);
            });
        }

        let constraints = crate::grabs::SizeConstraints::for_window(window);
        Some(ResizeSurfaceGrab::new_touch(
            touch_start,
            window.clone(),
            edges,
            initial_window_location,
            initial_window_size,
            output,
            constraints,
            slots,
        ))
    }

    pub fn on_touch_down<I: InputBackend>(&mut self, event: I::TouchDownEvent)
    where
        I::Device: 'static,
    {
        if !self.config.touch.enable {
            return;
        }
        let Some(output) = self.touch_output_for_device::<I>(&event.device()) else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        // Touch acts on its own output and hides the pointer. Cache the output
        // for the rest of the sequence so motion reuses it (see `TouchState`).
        self.touch_state.output = Some(output.clone());
        self.focused_output = Some(output.clone());
        self.cursor.hidden_by_touch = true;

        let screen_pos = event.position_transformed(output_geo.size);
        let (camera, zoom) = {
            let os = output_state(&output);
            (os.camera, os.zoom)
        };
        let canvas_pos = screen_to_canvas(ScreenPos(screen_pos), camera, zoom).0;
        let slot = event.slot();
        let time = Event::time_msec(&event);
        let serial = SERIAL_COUNTER.next_serial();

        // Locked session: forward straight to the lock surface, no gestures.
        if !matches!(self.session_lock, SessionLock::Unlocked) {
            let Some(ls) = self.lock_surfaces.get(&output) else {
                return;
            };
            let focus = FocusTarget(ls.wl_surface().clone());
            let touch = self.seat.get_touch().unwrap();
            touch.down(
                self,
                Some((focus, Point::from((0.0, 0.0)))),
                &DownEvent {
                    slot,
                    location: screen_pos,
                    serial,
                    time,
                },
            );
            touch.frame(self);
            return;
        }

        // An active grab (canvas-gesture or move) owns routing — forward the
        // new finger into it and let it decide.
        let touch = self.seat.get_touch().unwrap();
        if touch.is_grabbed() {
            let under = self.pointer_focus_under(screen_pos, canvas_pos);
            self.seat.get_touch().unwrap().down(
                self,
                under,
                &DownEvent {
                    slot,
                    location: canvas_pos,
                    serial,
                    time,
                },
            );
            return;
        }

        // Any fresh touch supersedes a deferred single-tap center. A real
        // double-tap still re-resolves to fit in `detect_tap`, so this doesn't
        // break double-tap-to-fit.
        self.cancel_pending_center();

        // Fresh interaction. The first finger hit-tests SSD decorations.
        match self.decoration_under(canvas_pos) {
            Some((window, DecorationHit::TitleBar)) => {
                self.start_touch_move(&window, slot, canvas_pos, serial, output);
                return;
            }
            Some((window, DecorationHit::CloseButton)) => {
                self.touch_state.pending_close = Some(PendingClose {
                    slot,
                    window,
                    last_canvas: canvas_pos,
                });
                return;
            }
            // Resize borders aren't touch-draggable (8px ≪ a fingertip); fall
            // through to the canvas-gesture grab.
            _ => {}
        }

        // Otherwise start the canvas-gesture grab. A content touch focuses +
        // raises (same as click-to-focus); empty canvas stops any coast.
        let under = self.pointer_focus_under(screen_pos, canvas_pos);
        if let Some((ref target, _)) = under {
            if let Some(window) = self.window_for_surface(&target.0) {
                self.raise_and_focus(&window, serial);
            } else {
                self.set_window_focus(Some(target.clone()), serial);
            }
        } else {
            self.cancel_animations();
        }

        let start_data = TouchGrabStartData {
            focus: under.clone(),
            slot,
            location: canvas_pos,
        };
        let grab = TouchGestureGrab::new(start_data, output);
        let touch = self.seat.get_touch().unwrap();
        touch.set_grab(self, grab, serial);
        self.seat.get_touch().unwrap().down(
            self,
            under,
            &DownEvent {
                slot,
                location: canvas_pos,
                serial,
                time,
            },
        );
    }

    fn start_touch_move(
        &mut self,
        window: &Window,
        slot: TouchSlot,
        location: Point<f64, Logical>,
        serial: smithay::utils::Serial,
        output: Output,
    ) {
        let Some(initial) = self.space.element_location(window) else {
            return;
        };
        self.raise_and_focus(window, serial);
        let start = TouchGrabStartData {
            focus: None,
            slot,
            location,
        };
        // One finger down (the titlebar press); the grab intercepts its motion
        // and up directly, so no `down` forward is needed.
        let grab = MoveSurfaceGrab::new_touch(
            start,
            window.clone(),
            initial,
            output,
            1,
            Vec::new(),
            HashSet::new(),
        );
        self.seat.get_touch().unwrap().set_grab(self, grab, serial);
    }

    pub fn on_touch_motion<I: InputBackend>(&mut self, event: I::TouchMotionEvent) {
        if !self.config.touch.enable {
            return;
        }
        // Reuse the output resolved at touch-down; the down always precedes its
        // motion, so this is set for any live sequence.
        let Some(output) = self.touch_state.output.clone() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        self.cursor.hidden_by_touch = true;
        let screen_pos = event.position_transformed(output_geo.size);
        let (camera, zoom) = {
            let os = output_state(&output);
            (os.camera, os.zoom)
        };
        let canvas_pos = screen_to_canvas(ScreenPos(screen_pos), camera, zoom).0;
        let slot = event.slot();
        let time = Event::time_msec(&event);

        if !matches!(self.session_lock, SessionLock::Unlocked) {
            let touch = self.seat.get_touch().unwrap();
            touch.motion(
                self,
                None,
                &MotionEvent {
                    slot,
                    location: screen_pos,
                    time,
                },
            );
            touch.frame(self);
            return;
        }

        // A close-button press just tracks its finger so the up event knows
        // whether it's still inside.
        if let Some(pc) = self.touch_state.pending_close.as_mut()
            && pc.slot == slot
        {
            pc.last_canvas = canvas_pos;
            return;
        }

        let touch = self.seat.get_touch().unwrap();
        if touch.is_grabbed() {
            touch.motion(
                self,
                None,
                &MotionEvent {
                    slot,
                    location: canvas_pos,
                    time,
                },
            );
        }
    }

    pub fn on_touch_up<I: InputBackend>(&mut self, event: I::TouchUpEvent) {
        if !self.config.touch.enable {
            return;
        }
        let slot = event.slot();
        let time = Event::time_msec(&event);
        let serial = SERIAL_COUNTER.next_serial();

        if !matches!(self.session_lock, SessionLock::Unlocked) {
            let touch = self.seat.get_touch().unwrap();
            touch.up(self, &UpEvent { slot, serial, time });
            touch.frame(self);
            return;
        }

        if let Some(pc) = self.touch_state.pending_close.take() {
            if pc.slot == slot {
                let still_inside = matches!(
                    self.decoration_under(pc.last_canvas),
                    Some((ref w, DecorationHit::CloseButton)) if *w == pc.window
                );
                if still_inside {
                    pc.window.send_close();
                }
                return;
            }
            // Different slot — leave the pending close in place.
            self.touch_state.pending_close = Some(pc);
        }

        let touch = self.seat.get_touch().unwrap();
        if touch.is_grabbed() {
            touch.up(self, &UpEvent { slot, serial, time });
        }
    }

    pub fn on_touch_cancel<I: InputBackend>(&mut self, _event: I::TouchCancelEvent) {
        if let Some(touch) = self.seat.get_touch() {
            touch.cancel(self);
        }
        self.touch_state.pending_close = None;
    }

    pub fn on_touch_frame<I: InputBackend>(&mut self, _event: I::TouchFrameEvent) {
        if let Some(touch) = self.seat.get_touch() {
            touch.frame(self);
        }
    }
}

/// Whether a touch digitizer's physical size (mm) matches a panel's, within 5%
/// (mutter's `MAX_SIZE_MATCH_DIFF`). Tries both orientations so a digitizer that
/// reports its width/height swapped still matches. Zero/unknown sizes never
/// match.
fn physical_size_matches(out_w: f64, out_h: f64, dev_w: f64, dev_h: f64) -> bool {
    const TOLERANCE: f64 = 0.05;
    let close = |a: f64, b: f64| b > 0.0 && a > 0.0 && (a - b).abs() / b <= TOLERANCE;
    (close(dev_w, out_w) && close(dev_h, out_h)) || (close(dev_w, out_h) && close(dev_h, out_w))
}

/// Whether `name` is an internal-panel connector (laptop built-in display).
fn is_internal_output(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name.starts_with("EDP") || name.starts_with("LVDS") || name.starts_with("DSI")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_match_exact_and_within_tolerance() {
        // A 13" panel and its bonded digitizer report ~the same mm.
        assert!(physical_size_matches(294.0, 165.0, 294.0, 165.0));
        // EDID rounding / digitizer slop within 5%.
        assert!(physical_size_matches(294.0, 165.0, 300.0, 168.0));
    }

    #[test]
    fn size_match_rotated_panel() {
        // Output reported portrait, digitizer landscape (or vice versa).
        assert!(physical_size_matches(165.0, 294.0, 294.0, 165.0));
    }

    #[test]
    fn size_mismatch_rejects_other_monitor_and_touchpad() {
        // A different-sized external monitor must not match.
        assert!(!physical_size_matches(531.0, 299.0, 294.0, 165.0));
        // A touchpad (~100x70mm) must never match a display.
        assert!(!physical_size_matches(294.0, 165.0, 100.0, 70.0));
    }

    #[test]
    fn size_match_rejects_unknown_dimensions() {
        // Outputs with no EDID physical size (0x0) never match.
        assert!(!physical_size_matches(0.0, 0.0, 294.0, 165.0));
        assert!(!physical_size_matches(294.0, 165.0, 0.0, 0.0));
    }

    #[test]
    fn internal_output_detection() {
        assert!(is_internal_output("eDP-1"));
        assert!(is_internal_output("LVDS-1"));
        assert!(is_internal_output("DSI-1"));
        assert!(!is_internal_output("DP-2"));
        assert!(!is_internal_output("HDMI-A-1"));
    }
}
