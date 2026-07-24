use std::any::Any;
use std::cell::RefCell;
use std::time::Duration;

use crate::decorations::DecorationHit;
use crate::grabs::{MoveGrab, ResizeGrab, ResizeState, TouchGestureGrab};
use crate::input::DecoTarget;
use crate::state::{DriftWm, FocusTarget, SessionLock, StageWindow, output_state};
use driftwm::canvas::{CanvasPos, ScreenPos, canvas_to_screen, screen_to_canvas};
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
    wayland::{
        compositor::{get_parent, with_states},
        seat::WaylandFocus,
    },
};

/// How long touch events are withheld from the app after each finger lands,
/// giving the next finger of a forming multi-finger gesture time to register
/// before the app sees — and highlights, types, scrolls — anything. Each new
/// finger re-arms the window; a gesture claim discards the buffer, a lift or
/// the deadline flushes it. Tune against the stagger deltas logged at debug
/// level.
const HOLDBACK_MS: u64 = 40;

/// A touch event withheld from the app while a higher finger-count tier may
/// still claim the sequence (see [`DriftWm::hold_touch_event`]).
pub enum HeldTouchEvent {
    Down {
        slot: TouchSlot,
        focus: Option<(FocusTarget, Point<f64, Logical>)>,
        location: Point<f64, Logical>,
        time: u32,
    },
    Motion {
        slot: TouchSlot,
        location: Point<f64, Logical>,
        time: u32,
    },
    Up {
        slot: TouchSlot,
        time: u32,
    },
}

/// Withheld events of the live touch sequence plus the deadline timer that
/// flushes them if no gesture claims the sequence in time.
#[derive(Default)]
pub struct HoldbackBuffer {
    pub(crate) events: Vec<HeldTouchEvent>,
    pub(crate) timer: Option<RegistrationToken>,
}

/// A close-button press awaiting release. Fires only if the finger lifts while
/// still inside the button — touch's analogue of the pointer close path.
/// Pinned windows hit-test their decorations in screen space, canvas windows
/// in canvas space, so both positions are tracked.
pub struct PendingClose {
    slot: TouchSlot,
    window: Window,
    last_canvas: Point<f64, Logical>,
    last_screen: Point<f64, Logical>,
    pinned: bool,
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
    /// Withheld events of the live sequence; lives here rather than in the
    /// gesture grab so the deadline timer can reach it.
    pub holdback: Option<HoldbackBuffer>,
    /// A holdback flush is replaying events through the touch handle; the
    /// gesture grab passes replayed events straight through instead of
    /// re-processing them.
    pub replaying_holdback: bool,
}

impl TouchState {
    pub fn new() -> Self {
        Self {
            last_three_finger_tap: None,
            pending_close: None,
            pending_center_timer: None,
            edge_pan: None,
            output: None,
            holdback: None,
            replaying_holdback: false,
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

        let libinput_device = as_libinput_device::<I>(device);

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

    /// Withhold a touch event from the app. A `Down` (re-)arms the flush
    /// deadline: each landing finger buys the next one `HOLDBACK_MS` to
    /// register before the sequence is handed to the app.
    pub(crate) fn hold_touch_event(&mut self, ev: HeldTouchEvent) {
        let arm = matches!(ev, HeldTouchEvent::Down { .. });
        let buffer = self.touch_state.holdback.get_or_insert_default();
        buffer.events.push(ev);
        if arm {
            if let Some(token) = buffer.timer.take() {
                self.loop_handle.remove(token);
            }
            let timer = Timer::from_duration(Duration::from_millis(HOLDBACK_MS));
            buffer.timer = self
                .loop_handle
                .insert_source(timer, |_, _, data: &mut DriftWm| {
                    data.flush_touch_holdback();
                    TimeoutAction::Drop
                })
                .ok();
            // No deadline means events could sit withheld until the finger
            // lifts; degrade to eager forwarding instead.
            if buffer.timer.is_none() {
                self.flush_touch_holdback();
            }
        }
    }

    /// Drop the withheld events unsent — a gesture claimed the sequence, so
    /// the app must never see them.
    pub(crate) fn discard_touch_holdback(&mut self) {
        let Some(buffer) = self.touch_state.holdback.take() else {
            return;
        };
        if let Some(token) = buffer.timer {
            self.loop_handle.remove(token);
        }
        tracing::debug!(
            "touch holdback: discarded {} events (gesture claim)",
            buffer.events.len()
        );
    }

    /// Deliver every withheld event to the app, in order. Runs outside grab
    /// dispatch (the deadline timer), so it replays through the public touch
    /// handle with `replaying_holdback` set; in-grab flushes (a finger lift)
    /// go through the grab's inner handle instead, which doesn't re-enter.
    pub(crate) fn flush_touch_holdback(&mut self) {
        let Some(buffer) = self.touch_state.holdback.take() else {
            return;
        };
        if let Some(token) = buffer.timer {
            self.loop_handle.remove(token);
        }
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        tracing::debug!(
            "touch holdback: flushing {} events (deadline)",
            buffer.events.len()
        );
        self.touch_state.replaying_holdback = true;
        for ev in buffer.events {
            match ev {
                HeldTouchEvent::Down {
                    slot,
                    focus,
                    location,
                    time,
                } => touch.down(
                    self,
                    focus,
                    &DownEvent {
                        slot,
                        location,
                        serial: SERIAL_COUNTER.next_serial(),
                        time,
                    },
                ),
                HeldTouchEvent::Motion {
                    slot,
                    location,
                    time,
                } => touch.motion(
                    self,
                    None,
                    &MotionEvent {
                        slot,
                        location,
                        time,
                    },
                ),
                // Unreachable from the deadline timer — a buffered Up triggers
                // an immediate in-grab flush.
                HeldTouchEvent::Up { slot, time } => touch.up(
                    self,
                    &UpEvent {
                        slot,
                        serial: SERIAL_COUNTER.next_serial(),
                        time,
                    },
                ),
            }
        }
        touch.frame(self);
        self.touch_state.replaying_holdback = false;
    }

    /// Set up a touch resize grab on `window` for `edges`: clear fit state, mark
    /// the surface/toplevel resizing (for commit-time top/left repositioning),
    /// and build the grab. `snapped` extends the resize to the window's
    /// snap-cluster; a screen-pinned window resizes in screen space instead
    /// (single-window, anchored to its pin site).
    pub(crate) fn build_touch_resize_grab(
        &mut self,
        window: &Window,
        edges: xdg_toplevel::ResizeEdge,
        touch_start: TouchGrabStartData<DriftWm>,
        output: Output,
        slots: usize,
        snapped: bool,
    ) -> Option<ResizeGrab> {
        let initial_window_location = self.stage.position_of(window)?;
        let initial_window_size = window.geometry().size;
        let wl_surface = window.wl_surface().map(|s| s.into_owned())?;

        let pinned_site = self.stage.pin_of(window).cloned();
        let pinned_initial_screen_pos = pinned_site.as_ref().map(|s| s.screen_pos);
        let output = pinned_site
            .as_ref()
            .and_then(|s| self.output_by_name(&s.output))
            .unwrap_or(output);

        self.stage.clear_fit(window);
        self.stage.clear_fill(window);

        with_states(&wl_surface, |states| {
            states
                .data_map
                .get_or_insert(|| RefCell::new(ResizeState::Idle))
                .replace(ResizeState::Resizing {
                    edges,
                    initial_window_location,
                    initial_window_size,
                    initial_screen_pos: pinned_initial_screen_pos,
                    last_committed_size: initial_window_size,
                });
        });

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
                state.states.unset(xdg_toplevel::State::Maximized);
            });
        }

        // Pinned resize is screen-space and single-window — no snap or cluster.
        let cluster_resize = if snapped && pinned_site.is_none() {
            self.cluster_snapshot_for_resize(&StageWindow::Client(window.clone()), edges)
        } else {
            crate::state::ClusterResizeSnapshot::empty()
        };
        let constraints = crate::grabs::SizeConstraints::for_window(window);
        Some(ResizeGrab::new_touch(
            touch_start,
            window.clone(),
            edges,
            initial_window_location,
            initial_window_size,
            output,
            constraints,
            slots,
            cluster_resize,
            pinned_initial_screen_pos,
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
            // No hit-test runs on this path; clear the stale flag so a live
            // gesture grab can't capture a bogus screen delta for this slot.
            self.pointer_over_screen_space = false;
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

        // Pinned windows render above canvas content and hit-test their SSD in
        // screen space — the canvas-space check below can't see them.
        match self.pinned_decoration_under(screen_pos) {
            Some((window, DecorationHit::TitleBar)) => {
                self.start_touch_pinned_move(&window, slot, canvas_pos, serial);
                return;
            }
            Some((window, DecorationHit::CloseButton)) => {
                self.touch_state.pending_close = Some(PendingClose {
                    slot,
                    window,
                    last_canvas: canvas_pos,
                    last_screen: screen_pos,
                    pinned: true,
                });
                return;
            }
            _ => {}
        }

        // Fresh interaction. The first finger hit-tests SSD decorations.
        match self.decoration_under(canvas_pos) {
            Some((DecoTarget::Client(window), DecorationHit::TitleBar)) => {
                self.start_touch_move(&window, slot, canvas_pos, serial, output);
                return;
            }
            Some((DecoTarget::Client(window), DecorationHit::CloseButton)) => {
                self.touch_state.pending_close = Some(PendingClose {
                    slot,
                    window,
                    last_canvas: canvas_pos,
                    last_screen: screen_pos,
                    pinned: false,
                });
                return;
            }
            // Suspended windows are opaque: a tap focuses + raises, the label
            // relaunches, the close button dismisses. Touch move/resize of a
            // suspended window is not wired in pass 1 — those taps focus + raise.
            // But an Overlay/Top layer or pinned window renders above the
            // stand-in; dispatch its tap only when the stand-in is the real
            // cascade winner (`pointer_focus_under` returns None over it),
            // matching the pointer path's layers > pinned > suspended ordering.
            Some((DecoTarget::Suspended(s), hit))
                if self.pointer_focus_under(screen_pos, canvas_pos).is_none() =>
            {
                let id = s.id;
                match hit {
                    DecorationHit::CloseButton => self.dismiss_suspended(id),
                    DecorationHit::Label => {
                        self.focus_and_raise_suspended(id);
                        self.relaunch_suspended(id);
                    }
                    _ => self.focus_and_raise_suspended(id),
                }
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
            // The hit may be a subsurface; windows are keyed by their root.
            let mut root = target.0.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.window_for_surface(&root) {
                // Mirror the pointer click branch: a widget takes keyboard
                // focus without a raise (or MRU entry).
                if window.is_widget() {
                    self.set_window_focus(
                        window.wl_surface().map(|s| FocusTarget(s.into_owned())),
                        serial,
                    );
                } else {
                    self.raise_and_focus(&window, serial);
                }
            } else {
                // Layer surface: mirror the pointer path — keyboard focus only
                // if it asks (on-demand). A `none` layer (e.g. an OSK) must not
                // steal focus from the window it types into. Popups also land
                // here (get_parent only walks subsurfaces) and keep the current
                // focus — a grabbing popup already holds the keyboard grab.
                self.focus_layer_if_on_demand(Some(target.0.clone()), serial);
            }
        } else {
            self.cancel_animations();
        }

        let start_data = TouchGrabStartData {
            focus: under.clone(),
            slot,
            location: canvas_pos,
        };
        let device_mm = touch_device_size_mm::<I>(&event.device());
        let grab = TouchGestureGrab::new(start_data, output, device_mm);
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

    /// Build a screen-space move grab for a pinned `window`, anchored by the
    /// finger's offset to the pin site — converted with the site output's own
    /// camera, so every pinned-touch-move entry point shares one offset
    /// convention. `None` if the window lost its pin or output.
    pub(crate) fn build_touch_pinned_move_grab(
        &self,
        window: &Window,
        touch_start: TouchGrabStartData<DriftWm>,
        slots: usize,
    ) -> Option<MoveGrab> {
        let site = self.stage.pin_of(window).cloned()?;
        let output = self.output_by_name(&site.output)?;
        let (camera, zoom) = {
            let os = output_state(&output);
            (os.camera, os.zoom)
        };
        let finger_screen = canvas_to_screen(CanvasPos(touch_start.location), camera, zoom).0;
        let grab_offset = site.screen_pos.to_f64() - finger_screen;
        Some(MoveGrab::new_pinned_touch(
            touch_start,
            window.clone(),
            output,
            grab_offset,
            slots,
        ))
    }

    /// Touch analogue of `start_pinned_move`: drag a screen-pinned window by a
    /// fixed screen-space offset from the finger.
    fn start_touch_pinned_move(
        &mut self,
        window: &Window,
        slot: TouchSlot,
        location: Point<f64, Logical>,
        serial: smithay::utils::Serial,
    ) {
        let start = TouchGrabStartData {
            focus: None,
            slot,
            location,
        };
        let Some(grab) = self.build_touch_pinned_move_grab(window, start, 1) else {
            return;
        };
        self.raise_and_focus(window, serial);
        self.arm_interactive_move(window);
        self.seat.get_touch().unwrap().set_grab(self, grab, serial);
    }

    fn start_touch_move(
        &mut self,
        window: &Window,
        slot: TouchSlot,
        location: Point<f64, Logical>,
        serial: smithay::utils::Serial,
        output: Output,
    ) {
        let Some(initial) = self.stage.position_of(window) else {
            return;
        };
        self.raise_and_focus(window, serial);
        // Moving re-anchors the window, invalidating any fill restore point.
        self.stage.clear_fill(window);
        let start = TouchGrabStartData {
            focus: None,
            slot,
            location,
        };
        // One finger down (the titlebar press); the grab intercepts its motion
        // and up directly, so no `down` forward is needed.
        self.arm_interactive_move(window);
        let grab = MoveGrab::new_touch(start, window.clone(), initial, output, 1, Vec::new());
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
            pc.last_screen = screen_pos;
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
                let still_inside = if pc.pinned {
                    matches!(
                        self.pinned_decoration_under(pc.last_screen),
                        Some((ref w, DecorationHit::CloseButton)) if *w == pc.window
                    )
                } else {
                    matches!(
                        self.decoration_under(pc.last_canvas),
                        Some((DecoTarget::Client(ref w), DecorationHit::CloseButton)) if *w == pc.window
                    )
                };
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

/// Downcast a backend input device to the libinput device behind it, if any (the
/// udev backend); `None` for the winit virtual device.
fn as_libinput_device<I: InputBackend>(device: &I::Device) -> Option<&LibinputDevice>
where
    I::Device: 'static,
{
    (device as &dyn Any).downcast_ref::<LibinputDevice>()
}

/// Touch digitizer's physical size in mm, if the backend device reports one
/// (libinput touchscreens do; the winit virtual device doesn't).
fn touch_device_size_mm<I: InputBackend>(device: &I::Device) -> Option<(f64, f64)>
where
    I::Device: 'static,
{
    as_libinput_device::<I>(device).and_then(LibinputDevice::size)
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
