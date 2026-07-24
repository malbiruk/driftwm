use std::collections::HashMap;
use std::time::Duration;

use smithay::{
    backend::input::TouchSlot,
    input::{
        SeatHandler,
        touch::{
            DownEvent, GrabStartData as TouchGrabStartData, MotionEvent, OrientationEvent,
            ShapeEvent, TouchGrab, TouchInnerHandle, UpEvent,
        },
    },
    output::Output,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, SERIAL_COUNTER, Serial, Size},
};

use driftwm::canvas::{self, CanvasPos, ScreenPos, canvas_to_screen, screen_to_canvas};
use driftwm::config::ContinuousAction;

use driftwm::window_ext::WindowExt;

use crate::input::touch::HeldTouchEvent;
use crate::state::{DriftWm, FocusTarget, StageWindow, output_state};

use super::MoveGrab;
use super::touch_recognizer::{Decision, TapOutcome, TouchInput, TouchKind, TouchRecognizer};

/// Logical pixels per millimetre for `output`, used to convert physical gesture
/// thresholds (dead zone, nav swipe) into the panel's pixel space. Touch points
/// already arrive in logical px, so only the panel's physical size is needed.
///
/// Prefers the touch digitizer's own physical size (`device_mm`) over the output's
/// EDID: the finger travels across the digitizer, so its mm size is the right
/// denominator, and cheap USB touch monitors often report a bogus EDID size (e.g. a
/// cloned ~2× value) while the digitizer reports the truth. Falls back to EDID, then
/// to ~96 dpi (nested backend, or no usable size anywhere).
fn output_px_per_mm(output: &Output, device_mm: Option<(f64, f64)>) -> f64 {
    const FALLBACK: f64 = 4.0;
    let Some(mode) = output.current_mode() else {
        return FALLBACK;
    };
    if mode.size.w <= 0 || mode.size.h <= 0 {
        return FALLBACK;
    }
    let scale = output.current_scale().fractional_scale();
    let mode_area = mode.size.w as f64 * mode.size.h as f64;

    // Density via area (geometric mean of both axes) so axis orientation doesn't
    // matter — EDID axes align with the panel's native orientation, a digitizer's
    // may not. `None` on a missing size or an out-of-range (bogus) density.
    let density = |phys_w: f64, phys_h: f64| {
        (phys_w > 0.0 && phys_h > 0.0)
            .then(|| (mode_area / (phys_w * phys_h)).sqrt() / scale)
            .filter(|ppm| ppm.is_finite() && (1.5..20.0).contains(ppm))
    };

    let edid = output.physical_properties().size;
    device_mm
        .and_then(|(w, h)| density(w, h))
        .or_else(|| density(edid.w as f64, edid.h as f64))
        .unwrap_or(FALLBACK)
}

/// Map where the fingers landed within a window to a resize edge via a 3×3 grid
/// (`origin` is canvas-space, `loc`/`size` are the window's canvas rect). The
/// center cell — and any window too small for the fingers to land off-center —
/// falls back to the bottom-right corner.
fn edge_from_origin(
    origin: Point<f64, Logical>,
    loc: Point<i32, Logical>,
    size: Size<i32, Logical>,
) -> xdg_toplevel::ResizeEdge {
    use xdg_toplevel::ResizeEdge;
    let fx = if size.w > 0 {
        (origin.x - loc.x as f64) / size.w as f64
    } else {
        0.5
    };
    let fy = if size.h > 0 {
        (origin.y - loc.y as f64) / size.h as f64
    } else {
        0.5
    };
    let left = fx < 1.0 / 3.0;
    let right = fx > 2.0 / 3.0;
    let top = fy < 1.0 / 3.0;
    let bottom = fy > 2.0 / 3.0;
    match (left, right, top, bottom) {
        (true, _, true, _) => ResizeEdge::TopLeft,
        (_, true, true, _) => ResizeEdge::TopRight,
        (true, _, _, true) => ResizeEdge::BottomLeft,
        (_, true, _, true) => ResizeEdge::BottomRight,
        (_, _, true, _) => ResizeEdge::Top,
        (_, _, _, true) => ResizeEdge::Bottom,
        (true, _, _, _) => ResizeEdge::Left,
        (_, true, _, _) => ResizeEdge::Right,
        _ => ResizeEdge::BottomRight,
    }
}

/// Surface focus captured at a slot's touch-down (canvas-origin), for app
/// forwarding and the escalation replay.
type SlotFocus = Option<(FocusTarget, Point<f64, Logical>)>;

/// Per-slot state captured at touch-down.
struct SlotDown {
    focus: SlotFocus,
    /// (canvas - screen) at down, `Some` only for screen-space targets whose
    /// frozen focus offset makes smithay expect screen-basis locations.
    screen_delta: Option<Point<f64, Logical>>,
}

/// Touch grab that owns the whole multi-finger canvas-gesture lifecycle. The
/// classification — which action each recognized gesture drives — lives in the
/// clock-free, compositor-free [`TouchRecognizer`]; this adapter converts
/// canvas↔screen, applies the recognizer's [`Decision`]s against compositor state
/// (camera, actions, window-grab handoff, holdback delivery), and manages the
/// per-slot app focus for forwarding. The default bindings reproduce the classic
/// behavior: app forwarding (1–2 fingers on a window), simultaneous pan + pinch-zoom
/// (1–2 fingers on empty canvas or 3 fingers anywhere), one-shot 4-finger
/// navigation, and 3-finger tap / double-tap / double-tap-drag / hold-drag. Set on
/// the first touch-down; tracks all slots and unsets itself when the last finger
/// lifts. Parallel to `PanGrab`.
pub struct TouchGestureGrab {
    start_data: TouchGrabStartData<DriftWm>,
    output: Output,
    /// Keyed by slot; the recognizer owns geometry (screen positions), so this
    /// holds only the per-slot [`SlotDown`] state for app forwarding and the
    /// escalation replay.
    focus: HashMap<TouchSlot, SlotDown>,
    core: TouchRecognizer,
}

impl TouchGestureGrab {
    pub fn new(
        start_data: TouchGrabStartData<DriftWm>,
        output: Output,
        device_mm: Option<(f64, f64)>,
    ) -> Self {
        let px_per_mm = output_px_per_mm(&output, device_mm);
        Self {
            start_data,
            output,
            focus: HashMap::new(),
            core: TouchRecognizer::new(px_per_mm),
        }
    }

    fn camera_zoom(&self) -> (Point<f64, Logical>, f64) {
        let os = output_state(&self.output);
        (os.camera, os.zoom)
    }

    /// Hand a translation drag to a window move/resize grab per the resolved
    /// continuous action. Returns false (keep panning) if it isn't a window-grab
    /// action or there's no canvas window under the finger.
    fn start_window_grab(
        &mut self,
        action: ContinuousAction,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        event: &MotionEvent,
        seq: Serial,
    ) -> bool {
        match action {
            ContinuousAction::MoveWindow => self.try_start_move(data, handle, event, seq, false),
            ContinuousAction::MoveSnappedWindows => {
                self.try_start_move(data, handle, event, seq, true)
            }
            ContinuousAction::ResizeWindow => {
                self.try_start_resize(data, handle, event, seq, false)
            }
            ContinuousAction::ResizeWindowSnapped => {
                self.try_start_resize(data, handle, event, seq, true)
            }
            _ => false,
        }
    }

    /// Double-tap-drag: hand off to a touch move grab on the window under the
    /// dragging finger. `cluster` extends the move to the window's snap-cluster
    /// (the hold variant). Returns false (and keeps panning) if there's no window.
    fn try_start_move(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        event: &MotionEvent,
        seq: Serial,
        cluster: bool,
    ) -> bool {
        // Pinned windows sit above canvas content and drag in screen space;
        // cluster doesn't apply (they don't participate in snap), and widgets
        // stay grab-proof like on the canvas.
        let (camera, zoom) = self.camera_zoom();
        let finger_screen = canvas_to_screen(CanvasPos(event.location), camera, zoom).0;
        if let Some(window) = data.pinned_element_under(finger_screen) {
            if window.is_widget() {
                return false;
            }
            let start = TouchGrabStartData {
                focus: None,
                slot: event.slot,
                location: event.location,
            };
            let Some(grab) =
                data.build_touch_pinned_move_grab(&window, start, self.core.finger_count())
            else {
                return false;
            };
            let serial = SERIAL_COUNTER.next_serial();
            data.raise_and_focus(&window, serial);
            handle.set_grab(self, data, seq, grab);
            return true;
        }

        let Some((window, loc)) = data
            .element_under_raw(event.location)
            .map(|(w, l)| (w.clone(), l))
        else {
            return false;
        };
        if !data.is_canvas_window(&window) {
            return false;
        }
        let serial = SERIAL_COUNTER.next_serial();
        data.raise_and_focus(&window, serial);
        // Moving re-anchors the window, invalidating any fill restore point.
        data.stage.clear_fill(&window);
        let initial = data.stage.position_of(&window).unwrap_or(loc);
        let members = if cluster {
            data.cluster_snapshot_for_drag(&StageWindow::Client(window.clone()), initial)
        } else {
            Vec::new()
        };
        // Members ride along with the primary, so their fill restore points go
        // stale too.
        for (member, _) in &members {
            data.stage.clear_fill(member);
        }
        let start = TouchGrabStartData {
            focus: None,
            slot: event.slot,
            location: event.location,
        };
        // All current fingers are already down; seed the count so the move grab
        // stays alive until every one of them lifts.
        let slots = self.core.finger_count();
        data.arm_interactive_move(&window);
        let grab = MoveGrab::new_touch(start, window, initial, self.output.clone(), slots, members);
        handle.set_grab(self, data, seq, grab);
        true
    }

    /// Hold-then-drag resize: pick the edge from where the fingers landed (a 3×3
    /// grid over the window) and hand off to a touch resize grab. `snapped`
    /// extends the resize to the window's snap-cluster. Returns false (and keeps
    /// panning) if there's no canvas window under the landing point.
    fn try_start_resize(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        event: &MotionEvent,
        seq: Serial,
        snapped: bool,
    ) -> bool {
        // Use the live finger centroid with the live camera (not the landing
        // `start_centroid`, which is screen-space and goes stale if a momentum
        // coast moves the camera during the hold). It's within the dead zone of
        // the landing point, so the 3×3 cell is unchanged.
        let (camera, zoom) = self.camera_zoom();
        let screen_centroid = self.core.centroid();

        // Pinned windows resize in screen space; `build_touch_resize_grab`
        // resolves the pinned anchor itself (snapped is moot — no cluster).
        if let Some(window) = data.pinned_element_under(screen_centroid) {
            if window.is_widget() {
                return false;
            }
            let Some(site) = data.stage.pin_of(&window).cloned() else {
                return false;
            };
            let edges = edge_from_origin(screen_centroid, site.screen_pos, window.geometry().size);
            let start = TouchGrabStartData {
                focus: None,
                slot: event.slot,
                location: event.location,
            };
            let slots = self.core.finger_count();
            let Some(grab) = data.build_touch_resize_grab(
                &window,
                edges,
                start,
                self.output.clone(),
                slots,
                false,
            ) else {
                return false;
            };
            let serial = SERIAL_COUNTER.next_serial();
            data.raise_and_focus(&window, serial);
            handle.set_grab(self, data, seq, grab);
            return true;
        }

        let origin = screen_to_canvas(ScreenPos(screen_centroid), camera, zoom).0;
        let Some((window, _)) = data.element_under_raw(origin).map(|(w, l)| (w.clone(), l)) else {
            return false;
        };
        if !data.is_canvas_window(&window) {
            return false;
        }
        let Some(loc) = data.stage.position_of(&window) else {
            return false;
        };
        let edges = edge_from_origin(origin, loc, window.geometry().size);
        let start = TouchGrabStartData {
            focus: None,
            slot: event.slot,
            location: event.location,
        };
        let slots = self.core.finger_count();
        // Build before raising/focusing so a failed build leaves no stray focus
        // change (it falls through to pan).
        let Some(grab) = data.build_touch_resize_grab(
            &window,
            edges,
            start,
            self.output.clone(),
            slots,
            snapped,
        ) else {
            return false;
        };
        let serial = SERIAL_COUNTER.next_serial();
        data.raise_and_focus(&window, serial);
        handle.set_grab(self, data, seq, grab);
        true
    }

    /// Deliver the withheld events in order through the inner handle — inside
    /// grab dispatch the public `TouchHandle` would re-enter the grab and
    /// panic, and the inner handle forwards without re-processing.
    fn flush_holdback_inner(
        &self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        seq: Serial,
    ) {
        let Some(buffer) = data.touch_state.holdback.take() else {
            return;
        };
        if let Some(token) = buffer.timer {
            data.loop_handle.remove(token);
        }
        tracing::debug!(
            "touch holdback: flushing {} events (lift)",
            buffer.events.len()
        );
        for ev in buffer.events {
            match ev {
                HeldTouchEvent::Down {
                    slot,
                    focus,
                    location,
                    time,
                } => handle.down(
                    data,
                    focus,
                    &DownEvent {
                        slot,
                        location,
                        serial: SERIAL_COUNTER.next_serial(),
                        time,
                    },
                    seq,
                ),
                HeldTouchEvent::Motion {
                    slot,
                    location,
                    time,
                } => handle.motion(
                    data,
                    None,
                    &MotionEvent {
                        slot,
                        location,
                        time,
                    },
                    seq,
                ),
                HeldTouchEvent::Up { slot, time } => handle.up(
                    data,
                    &UpEvent {
                        slot,
                        serial: SERIAL_COUNTER.next_serial(),
                        time,
                    },
                    seq,
                ),
            }
        }
        handle.frame(data, seq);
    }

    /// Apply the recognizer's continuous-pan decision: scale the raw screen
    /// centroid delta by `touch_speed` and the live zoom (inverted, like the
    /// trackpad swipe), then drive the camera.
    fn apply_pan(&self, data: &mut DriftWm, delta: Point<f64, Logical>, time: u32) {
        let zoom = output_state(&self.output).zoom;
        let pan = Point::from((
            -delta.x * data.config.touch_speed / zoom,
            -delta.y * data.config.touch_speed / zoom,
        ));
        data.drift_pan_on(pan, time, &self.output);
    }

    /// Apply the recognizer's zoom decision: turn the spread ratio into a new zoom
    /// via `zoom_touch_speed`, clamp it, and re-anchor the camera at the screen
    /// anchor so the point under the fingers stays put.
    fn apply_zoom(&self, data: &mut DriftWm, scale: f64, anchor: Point<f64, Logical>) {
        let zoom = output_state(&self.output).zoom;
        let new_zoom = (zoom * (1.0 + (scale - 1.0) * data.config.zoom_touch_speed))
            .clamp(data.min_zoom(), canvas::MAX_ZOOM);
        let camera_after = output_state(&self.output).camera;
        let anchor_canvas = screen_to_canvas(ScreenPos(anchor), camera_after, zoom).0;
        let new_camera = canvas::zoom_anchor_camera(anchor_canvas, anchor, new_zoom);
        {
            let mut os = output_state(&self.output);
            os.camera = new_camera;
            os.zoom = new_zoom;
        }
        data.update_output_from_camera();
    }

    /// Apply a clean-tap decision: raise+focus the window under the tap, record the
    /// last 3-finger tap, then fire (or defer) the resolved action.
    fn apply_tap(
        &self,
        data: &mut DriftWm,
        focus_at: Point<f64, Logical>,
        set_last_tap: Option<u32>,
        outcome: TapOutcome,
    ) {
        let (camera, zoom) = self.camera_zoom();
        let canvas = screen_to_canvas(ScreenPos(focus_at), camera, zoom).0;
        let serial = SERIAL_COUNTER.next_serial();
        let under = data.element_under_raw(canvas).map(|(w, _)| w.clone());
        if let Some(window) = &under {
            data.raise_and_focus(window, serial);
        }
        data.touch_state.last_three_finger_tap = set_last_tap;
        match outcome {
            TapOutcome::Fire(action) => data.execute_action(&action),
            TapOutcome::DeferCenter { delay_ms } => {
                let target = under
                    .filter(|w| data.is_canvas_window(w))
                    .or_else(|| data.focused_window().filter(|w| data.is_canvas_window(w)));
                if let Some(window) = target {
                    data.schedule_pending_center(window, Duration::from_millis(delay_ms as u64));
                }
            }
            TapOutcome::None => {}
        }
    }
}

impl TouchGrab<DriftWm> for TouchGestureGrab {
    fn down(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        focus: Option<(<DriftWm as SeatHandler>::TouchFocus, Point<f64, Logical>)>,
        event: &DownEvent,
        seq: Serial,
    ) {
        if data.touch_state.replaying_holdback {
            handle.down(data, focus, event, seq);
            return;
        }
        let (camera, zoom) = self.camera_zoom();
        let screen = canvas_to_screen(CanvasPos(event.location), camera, zoom).0;
        // The hit-test that produced `focus` ran in this same dispatch, so the
        // flag still reflects this touch's target. Capture (canvas - screen) now
        // to rewrite later motion into the screen basis smithay freezes at down.
        let screen_delta =
            (focus.is_some() && data.pointer_over_screen_space).then(|| event.location - screen);
        self.focus.insert(
            event.slot,
            SlotDown {
                focus: focus.clone(),
                screen_delta,
            },
        );

        let input = TouchInput {
            time_ms: event.time,
            slot: event.slot,
            kind: TouchKind::Down {
                location: screen,
                app_owns_hit: focus.is_some(),
            },
        };
        let decisions = self.core.process(
            &data.config,
            &data.config.gesture_thresholds,
            &input,
            data.touch_state.last_three_finger_tap,
            data.touch_state.holdback.is_some(),
        );

        for decision in decisions {
            match decision {
                Decision::Forward => handle.down(data, focus.clone(), event, seq),
                Decision::Consume => handle.down(data, None, event, seq),
                Decision::Hold => data.hold_touch_event(HeldTouchEvent::Down {
                    slot: event.slot,
                    focus: focus.clone(),
                    location: event.location,
                    time: event.time,
                }),
                Decision::Discard => data.discard_touch_holdback(),
                // Escalation from app-forwarding to a system gesture: revoke the app's
                // in-flight touch sequence so it sees no dangling points. smithay's touch
                // cancel skips any slot already framed (current >= pending) — i.e. every
                // finger that landed in an earlier frame, the common case for a 3-finger
                // gesture. Replay a no-op motion on those slots first to bump pending past
                // current, so the cancel that follows actually revokes them.
                Decision::CancelAppSequence => {
                    let replays: Vec<(TouchSlot, Point<f64, Logical>)> = self
                        .focus
                        .iter()
                        .filter(|(slot, state)| **slot != event.slot && state.focus.is_some())
                        .filter_map(|(slot, state)| {
                            self.core.screen_pos(*slot).map(|sp| {
                                let location = match state.screen_delta {
                                    Some(delta) => sp + delta,
                                    None => screen_to_canvas(ScreenPos(sp), camera, zoom).0,
                                };
                                (*slot, location)
                            })
                        })
                        .collect();
                    for (slot, location) in replays {
                        handle.motion(
                            data,
                            None,
                            &MotionEvent {
                                slot,
                                location,
                                time: event.time,
                            },
                            seq,
                        );
                    }
                    handle.cancel(data, seq);
                }
                // Stash the exiting window so a nav firing right after can still
                // anchor to it. Uses the touch output, which may differ from the
                // active/pointer output.
                Decision::PreExitFullscreen => {
                    if let Some(window) = data.fullscreen_window_on(&self.output) {
                        data.pre_exited_fullscreen = Some(window);
                        data.exit_fullscreen_on(&self.output);
                    }
                }
                other => unreachable!("unexpected decision from down: {other:?}"),
            }
        }
    }

    fn up(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        event: &UpEvent,
        seq: Serial,
    ) {
        if data.touch_state.replaying_holdback {
            handle.up(data, event, seq);
            return;
        }

        let input = TouchInput {
            time_ms: event.time,
            slot: event.slot,
            kind: TouchKind::Up,
        };
        let decisions = self.core.process(
            &data.config,
            &data.config.gesture_thresholds,
            &input,
            data.touch_state.last_three_finger_tap,
            data.touch_state.holdback.is_some(),
        );

        for decision in decisions {
            match decision {
                Decision::Forward => handle.up(data, event, seq),
                Decision::Hold => data.hold_touch_event(HeldTouchEvent::Up {
                    slot: event.slot,
                    time: event.time,
                }),
                Decision::Flush => self.flush_holdback_inner(data, handle, seq),
                Decision::Momentum => data.launch_momentum_on(&self.output),
                Decision::Tap {
                    focus_at,
                    set_last_tap,
                    outcome,
                } => self.apply_tap(data, focus_at, set_last_tap, outcome),
                Decision::UnsetGrab => handle.unset_grab(self, data),
                other => unreachable!("unexpected decision from up: {other:?}"),
            }
        }

        self.focus.remove(&event.slot);
    }

    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        _focus: Option<(<DriftWm as SeatHandler>::TouchFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
        seq: Serial,
    ) {
        if data.touch_state.replaying_holdback {
            handle.motion(data, None, event, seq);
            return;
        }
        let (camera, zoom) = self.camera_zoom();
        let screen = canvas_to_screen(CanvasPos(event.location), camera, zoom).0;
        let slot_down = self.focus.get(&event.slot);
        let stored_focus = slot_down.and_then(|s| s.focus.clone());
        let screen_delta = slot_down.and_then(|s| s.screen_delta);

        let input = TouchInput {
            time_ms: event.time,
            slot: event.slot,
            kind: TouchKind::Motion { location: screen },
        };
        let decisions = self.core.process(
            &data.config,
            &data.config.gesture_thresholds,
            &input,
            data.touch_state.last_three_finger_tap,
            data.touch_state.holdback.is_some(),
        );

        // smithay froze this slot's focus offset at down and ignores the focus
        // passed to motion, so a screen-space target only gets correct coords if
        // the delivered location is shifted into that same screen basis.
        let forward_location = match screen_delta {
            Some(delta) => screen + delta,
            None => event.location,
        };

        for decision in decisions {
            match decision {
                Decision::Forward => {
                    let ev = MotionEvent {
                        slot: event.slot,
                        location: forward_location,
                        time: event.time,
                    };
                    handle.motion(data, stored_focus.clone(), &ev, seq);
                }
                Decision::Consume => handle.motion(data, None, event, seq),
                Decision::Hold => data.hold_touch_event(HeldTouchEvent::Motion {
                    slot: event.slot,
                    location: forward_location,
                    time: event.time,
                }),
                Decision::Pan(delta) => self.apply_pan(data, delta, event.time),
                Decision::Zoom { scale, anchor } => self.apply_zoom(data, scale, anchor),
                Decision::FireThreshold(action) => data.execute_action(&action),
                Decision::StartWindowGrab { action } => {
                    self.start_window_grab(action, data, handle, event, seq);
                }
                other => unreachable!("unexpected decision from motion: {other:?}"),
            }
        }
    }

    fn frame(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        seq: Serial,
    ) {
        handle.frame(data, seq);
    }

    fn cancel(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        seq: Serial,
    ) {
        handle.cancel(data, seq);
        handle.unset_grab(self, data);
    }

    fn shape(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        event: &ShapeEvent,
        seq: Serial,
    ) {
        handle.shape(data, event, seq);
    }

    fn orientation(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        event: &OrientationEvent,
        seq: Serial,
    ) {
        handle.orientation(data, event, seq);
    }

    fn start_data(&self) -> &TouchGrabStartData<DriftWm> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut DriftWm) {
        // Normally the buffer is already flushed (lift) or discarded (claim)
        // by now; this catches a grab torn down mid-hold, e.g. on a dead
        // start-data surface, so a stale timer can't replay into the next
        // sequence.
        if let Some(buffer) = data.touch_state.holdback.take()
            && let Some(token) = buffer.timer
        {
            data.loop_handle.remove(token);
        }
        // unset runs on every teardown, including replacement by a move/resize
        // grab, and after any tap fired — so an unconsumed tier-crossing stash
        // can't leak into a later unrelated action.
        data.pre_exited_fullscreen = None;
    }
}
