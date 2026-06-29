use std::collections::{HashMap, HashSet};
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
use driftwm::config::Action;

use crate::input::gestures::direction_from_vector;
use crate::state::{DriftWm, FocusTarget, output_state};

use super::MoveSurfaceGrab;

/// Finger travel (screen px) before a touch commits to a viewport gesture.
/// Below this it stays a candidate tap; above it pan/zoom/navigate begin.
const DEAD_ZONE_PX: f64 = 8.0;
/// Max duration of a 3-finger tap (center / fit trigger).
const TAP_MAX_MS: u32 = 250;
/// Window for a second 3-finger tap to count as a double-tap.
const DOUBLE_TAP_MS: u32 = 300;
/// Dwell (ms) before a drag commits that turns a 3-finger drag into a hold
/// gesture: resize (no prior tap) or cluster move (after a double-tap). Long
/// enough that a normal pan, which drags promptly, never trips it.
const HOLD_MS: u32 = 350;
/// Per-frame pinch-zoom deadzone (on the spread ratio). The spread metric is
/// noisy, so a pure pan would wobble the zoom; ignore scale changes inside this
/// band. The baseline only advances on a committed zoom, so a deliberate pinch
/// still accumulates past it.
const ZOOM_DEADZONE: f64 = 0.02;
/// Spread-ratio change that signals a zoom (pinch) intent. A gesture stays a
/// candidate tap until the centroid crosses `DEAD_ZONE_PX` (pan) or the spread
/// crosses this (zoom); the winner locks the gesture touchpad-style for its
/// lifetime, so a pan can't zoom-wobble and a pinch can't pan-drift. Well above
/// resting finger jitter so a plain pan doesn't trip it.
const ZOOM_LOCK_FRAC: f64 = 0.12;
/// When both signals moved, a pinch must out-pace the pan by this factor to win.
/// Pan is the safe default: a mis-locked zoom strands the pan for the whole
/// gesture, and a leading-finger pan skews the spread without being a pinch.
const ZOOM_DOMINANCE: f64 = 1.5;
/// Centroid travel (in `DEAD_ZONE_PX` units) past which an undecided gesture
/// commits to pan, so an ambiguous pan+spread can't freeze waiting to resolve.
const PAN_LOCK_FORCE: f64 = 2.0;

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

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// 1–2 fingers with at least one on a window — forward to the app.
    Forward,
    /// 1–2 fingers on empty canvas, or 3 fingers anywhere — viewport pan or
    /// pinch-zoom (locked per gesture by `PanZoomLock`).
    PanZoom,
    /// 4 fingers — global navigation (swipe-nearest, pinch overview/home).
    Navigate,
}

/// Pan-vs-zoom decision for a `PanZoom` gesture, locked at the dead-zone
/// crossing and held for the gesture's lifetime (touchpad-style: a gesture is
/// either a pan or a pinch, never both, so neither can wobble into the other).
#[derive(Clone, Copy, PartialEq)]
enum PanZoomLock {
    Undecided,
    Pan,
    Zoom,
}

struct TouchPoint {
    /// Physical screen position. Stable across camera moves (recovered each
    /// motion from the canvas location via the current camera/zoom).
    last_screen: Point<f64, Logical>,
    /// Surface focus captured at down (canvas-origin), for app forwarding.
    focus: Option<(FocusTarget, Point<f64, Logical>)>,
}

/// Touch grab that owns the whole multi-finger canvas-gesture lifecycle: app
/// forwarding (1–2 fingers on a window), viewport pan + pinch-zoom (1–2 fingers
/// on empty canvas or 3 fingers anywhere), 4-finger navigation, and 3-finger
/// tap / double-tap / double-tap-drag. Set on the first touch-down; tracks all
/// slots and unsets itself when the last finger lifts. Parallel to `PanGrab`.
pub struct TouchGestureGrab {
    start_data: TouchGrabStartData<DriftWm>,
    output: Output,
    points: HashMap<TouchSlot, TouchPoint>,
    /// A finger landed on a window while still in 1–2 finger territory.
    app_owns: bool,
    /// High-water mark of simultaneous fingers — decides 3-finger vs 4-finger.
    max_fingers: usize,
    /// App touch sequence revoked once on escalation to a system gesture.
    system_cancelled: bool,
    /// Past the dead zone: viewport changes / navigation accumulation are live.
    active: bool,
    /// Ever passed the dead zone — disqualifies the gesture from being a tap.
    ever_active: bool,
    /// A recent 3-finger tap armed this gesture for double-tap-drag move.
    armed_for_move: bool,
    tap_start_time: u32,
    start_centroid: Point<f64, Logical>,
    last_centroid: Point<f64, Logical>,
    last_spread: f64,
    start_spread: f64,
    nav_cumulative: Point<f64, Logical>,
    nav_fired_swipe: bool,
    nav_fired_pinch: bool,
    /// Pan-vs-zoom lock for the current `PanZoom` gesture (`Undecided` until the
    /// dead-zone crossing picks one).
    panzoom_lock: PanZoomLock,
}

impl TouchGestureGrab {
    pub fn new(start_data: TouchGrabStartData<DriftWm>, output: Output) -> Self {
        Self {
            start_data,
            output,
            points: HashMap::new(),
            app_owns: false,
            max_fingers: 0,
            system_cancelled: false,
            active: false,
            ever_active: false,
            armed_for_move: false,
            tap_start_time: 0,
            start_centroid: Point::from((0.0, 0.0)),
            last_centroid: Point::from((0.0, 0.0)),
            last_spread: 0.0,
            start_spread: 0.0,
            nav_cumulative: Point::from((0.0, 0.0)),
            nav_fired_swipe: false,
            nav_fired_pinch: false,
            panzoom_lock: PanZoomLock::Undecided,
        }
    }

    fn mode(&self) -> Mode {
        if self.max_fingers >= 4 {
            Mode::Navigate
        } else if self.max_fingers >= 3 {
            Mode::PanZoom
        } else if self.app_owns {
            Mode::Forward
        } else {
            Mode::PanZoom
        }
    }

    fn camera_zoom(&self) -> (Point<f64, Logical>, f64) {
        let os = output_state(&self.output);
        (os.camera, os.zoom)
    }

    fn centroid(&self) -> Point<f64, Logical> {
        let n = self.points.len();
        if n == 0 {
            return Point::from((0.0, 0.0));
        }
        let sum = self
            .points
            .values()
            .fold(Point::from((0.0, 0.0)), |acc, p| acc + p.last_screen);
        Point::from((sum.x / n as f64, sum.y / n as f64))
    }

    fn spread(&self, centroid: Point<f64, Logical>) -> f64 {
        let n = self.points.len();
        if n < 2 {
            return 0.0;
        }
        let sum: f64 = self
            .points
            .values()
            .map(|p| {
                let dx = p.last_screen.x - centroid.x;
                let dy = p.last_screen.y - centroid.y;
                (dx * dx + dy * dy).sqrt()
            })
            .sum();
        sum / n as f64
    }

    /// Reset the per-frame baseline to the current finger configuration so a
    /// finger add/remove doesn't produce a pan/zoom jump.
    fn rebaseline(&mut self) {
        let c = self.centroid();
        self.last_centroid = c;
        self.last_spread = self.spread(c);
    }

    fn apply_pan(&mut self, data: &mut DriftWm, centroid: Point<f64, Logical>, time: u32) {
        let zoom = output_state(&self.output).zoom;
        let centroid_delta = centroid - self.last_centroid;
        let pan = Point::from((
            -centroid_delta.x * data.config.touch_speed / zoom,
            -centroid_delta.y * data.config.touch_speed / zoom,
        ));
        data.drift_pan_on(pan, time, &self.output);
        self.last_centroid = centroid;
    }

    fn apply_zoom(&mut self, data: &mut DriftWm, centroid: Point<f64, Logical>) {
        self.last_centroid = centroid;
        if self.points.len() < 2 || self.last_spread <= 1.0 {
            return;
        }
        let zoom = output_state(&self.output).zoom;
        let spread = self.spread(centroid);
        let scale = spread / self.last_spread;
        if (scale - 1.0).abs() > ZOOM_DEADZONE {
            let new_zoom = (zoom * (1.0 + (scale - 1.0) * data.config.zoom_touch_speed))
                .clamp(data.min_zoom(), canvas::MAX_ZOOM);
            let camera_after = output_state(&self.output).camera;
            let anchor = screen_to_canvas(ScreenPos(centroid), camera_after, zoom).0;
            let new_camera = canvas::zoom_anchor_camera(anchor, centroid, new_zoom);
            {
                let mut os = output_state(&self.output);
                os.camera = new_camera;
                os.zoom = new_zoom;
            }
            data.update_output_from_camera();
            self.last_spread = spread;
        }
    }

    fn apply_navigate(&mut self, data: &mut DriftWm, centroid: Point<f64, Logical>) {
        // Inverted, like the trackpad swipe: drag content right → reveal left.
        let centroid_delta = centroid - self.last_centroid;
        self.nav_cumulative += Point::from((-centroid_delta.x, -centroid_delta.y));
        self.last_centroid = centroid;

        if self.nav_fired_swipe || self.nav_fired_pinch {
            return;
        }

        let th = &data.config.gesture_thresholds;
        let swipe_dist = (self.nav_cumulative.x.powi(2) + self.nav_cumulative.y.powi(2)).sqrt();
        let swipe_progress = if th.swipe_distance > 0.0 {
            swipe_dist / th.swipe_distance
        } else {
            f64::INFINITY
        };

        // Pinch progress as a fraction of the in/out margin: a pure swipe's
        // natural splay stays well below 1.0, a deliberate pinch climbs past it.
        let scale = if self.start_spread > 1.0 {
            self.spread(centroid) / self.start_spread
        } else {
            1.0
        };
        let pinch_progress = if scale < 1.0 {
            let margin = 1.0 - th.pinch_in_scale;
            if margin > 0.0 {
                (1.0 - scale) / margin
            } else {
                0.0
            }
        } else {
            let margin = th.pinch_out_scale - 1.0;
            if margin > 0.0 {
                (scale - 1.0) / margin
            } else {
                0.0
            }
        };

        // Swipe and pinch are mutually exclusive; whichever is further past its
        // own threshold claims the gesture. Pinch wins ties so a deliberate
        // pinch-in isn't stolen by the small swipe threshold (a pinch always
        // drifts the centroid a little).
        if pinch_progress >= 1.0 && pinch_progress >= swipe_progress {
            self.nav_fired_pinch = true;
            if scale < 1.0 {
                data.execute_action(&Action::ZoomToFit);
            } else {
                data.execute_action(&Action::HomeToggle);
            }
        } else if swipe_progress >= 1.0 {
            self.nav_fired_swipe = true;
            let dir = direction_from_vector(self.nav_cumulative);
            data.execute_action(&Action::CenterNearest(dir));
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
        let Some((window, loc)) = data
            .space
            .element_under(event.location)
            .map(|(w, l)| (w.clone(), l))
        else {
            return false;
        };
        if !data.is_canvas_window(&window) {
            return false;
        }
        let serial = SERIAL_COUNTER.next_serial();
        data.raise_and_focus(&window, serial);
        let initial = data.space.element_location(&window).unwrap_or(loc);
        let (members, surfaces) = if cluster {
            data.cluster_snapshot_for_drag(&window, initial)
        } else {
            (Vec::new(), HashSet::new())
        };
        let start = TouchGrabStartData {
            focus: None,
            slot: event.slot,
            location: event.location,
        };
        // All current fingers are already down; seed the count so the move grab
        // stays alive until every one of them lifts.
        let slots = self.points.len();
        let grab = MoveSurfaceGrab::new_touch(
            start,
            window,
            initial,
            self.output.clone(),
            slots,
            members,
            surfaces,
        );
        handle.set_grab(self, data, seq, grab);
        true
    }

    /// Hold-then-drag resize: pick the edge from where the fingers landed (a 3×3
    /// grid over the window) and hand off to a touch resize grab. Returns false
    /// (and keeps panning) if there's no canvas window under the landing point.
    fn try_start_resize(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        event: &MotionEvent,
        seq: Serial,
    ) -> bool {
        // Use the live finger centroid with the live camera (not the landing
        // `start_centroid`, which is screen-space and goes stale if a momentum
        // coast moves the camera during the hold). It's within the dead zone of
        // the landing point, so the 3×3 cell is unchanged.
        let (camera, zoom) = self.camera_zoom();
        let origin = screen_to_canvas(ScreenPos(self.centroid()), camera, zoom).0;
        let Some((window, _)) = data
            .space
            .element_under(origin)
            .map(|(w, l)| (w.clone(), l))
        else {
            return false;
        };
        if !data.is_canvas_window(&window) {
            return false;
        }
        let Some(loc) = data.space.element_location(&window) else {
            return false;
        };
        let edges = edge_from_origin(origin, loc, window.geometry().size);
        let start = TouchGrabStartData {
            focus: None,
            slot: event.slot,
            location: event.location,
        };
        let slots = self.points.len();
        // Build before raising/focusing so a failed build leaves no stray focus
        // change (it falls through to pan).
        let Some(grab) =
            data.build_touch_resize_grab(&window, edges, start, self.output.clone(), slots)
        else {
            return false;
        };
        let serial = SERIAL_COUNTER.next_serial();
        data.raise_and_focus(&window, serial);
        handle.set_grab(self, data, seq, grab);
        true
    }

    /// On last-finger-up, fire center (single) / fit (double) for a clean
    /// 3-finger tap. A tap is short, never passed the dead zone, and never
    /// belonged to an app.
    fn detect_tap(&mut self, data: &mut DriftWm, time: u32) {
        // A 3-finger tap is a system gesture regardless of what's under it — the
        // escalation already cancelled any app touches, so center/fit the tapped
        // window even when the first finger happened to land on one.
        if self.ever_active || self.max_fingers != 3 {
            return;
        }
        if time.saturating_sub(self.tap_start_time) > TAP_MAX_MS {
            return;
        }
        let (camera, zoom) = self.camera_zoom();
        let canvas = screen_to_canvas(ScreenPos(self.start_centroid), camera, zoom).0;
        let serial = SERIAL_COUNTER.next_serial();
        let under = data.space.element_under(canvas).map(|(w, _)| w.clone());
        if let Some(window) = &under {
            data.raise_and_focus(window, serial);
        }
        let double = data
            .touch_state
            .last_three_finger_tap
            .is_some_and(|t| time.saturating_sub(t) < DOUBLE_TAP_MS);
        if double {
            data.touch_state.last_three_finger_tap = None;
            data.execute_action(&Action::FitWindow);
        } else {
            data.touch_state.last_three_finger_tap = Some(time);
            // Defer the center so a follow-up double-tap (fit) or double-tap-drag
            // (move) doesn't flash a center first; a fresh interaction cancels it.
            let target = under
                .filter(|w| data.is_canvas_window(w))
                .or_else(|| data.focused_window().filter(|w| data.is_canvas_window(w)));
            if let Some(window) = target {
                data.schedule_pending_center(window, Duration::from_millis(DOUBLE_TAP_MS as u64));
            }
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
        let (camera, zoom) = self.camera_zoom();
        let screen = canvas_to_screen(CanvasPos(event.location), camera, zoom).0;
        let was_system = self.max_fingers >= 3;
        self.points.insert(
            event.slot,
            TouchPoint {
                last_screen: screen,
                focus: focus.clone(),
            },
        );
        self.max_fingers = self.max_fingers.max(self.points.len());

        // The first finger sets the gesture's nature — on a window → app content
        // (forward), on empty canvas → viewport gesture — and a recent 3-finger
        // tap arms this touch for a double-tap-drag move. Later fingers don't
        // flip either, so a stray contact can't strand an in-progress pan.
        if self.points.len() == 1 {
            if focus.is_some() {
                self.app_owns = true;
            }
            self.armed_for_move = data
                .touch_state
                .last_three_finger_tap
                .is_some_and(|t| event.time.saturating_sub(t) < DOUBLE_TAP_MS);
        }

        match self.mode() {
            Mode::Forward => {
                handle.down(data, focus, event, seq);
            }
            Mode::PanZoom | Mode::Navigate => {
                // Escalation from app-forwarding to a system gesture: revoke the
                // app's in-flight touch sequence so it sees no dangling points.
                // smithay's touch cancel skips any slot already framed
                // (current >= pending) — i.e. every finger that landed in an
                // earlier frame, the common case for a 3-finger gesture. Replay a
                // no-op motion on those slots first to bump pending past current,
                // so the cancel that follows actually revokes them.
                if self.app_owns && !self.system_cancelled {
                    let replays: Vec<(TouchSlot, Point<f64, Logical>)> = self
                        .points
                        .iter()
                        .filter(|(slot, p)| **slot != event.slot && p.focus.is_some())
                        .map(|(slot, p)| {
                            (
                                *slot,
                                screen_to_canvas(ScreenPos(p.last_screen), camera, zoom).0,
                            )
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
                    self.system_cancelled = true;
                }
                handle.down(data, None, event, seq);

                let now_system = self.max_fingers >= 3;
                // Arm the dead zone at gesture start and again when crossing into
                // 3-finger territory, so a clean 3-finger tap is distinguishable
                // from a 3-finger drag.
                if self.points.len() == 1 || (now_system && !was_system) {
                    self.active = false;
                    self.panzoom_lock = PanZoomLock::Undecided;
                    self.tap_start_time = event.time;
                    self.start_centroid = self.centroid();
                }
                self.rebaseline();
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
        let mode = self.mode();
        let was_present = self.points.contains_key(&event.slot);
        handle.up(data, event, seq);
        self.points.remove(&event.slot);

        if self.points.is_empty() {
            // Only PanZoom accumulates pan velocity; Navigate fires discrete
            // actions, so there's nothing to coast.
            if was_present && mode == Mode::PanZoom && self.ever_active {
                data.launch_momentum_on(&self.output);
            }
            if was_present {
                self.detect_tap(data, event.time);
            }
            handle.unset_grab(self, data);
        } else {
            self.rebaseline();
        }
    }

    fn motion(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        _focus: Option<(<DriftWm as SeatHandler>::TouchFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
        seq: Serial,
    ) {
        let mode = self.mode();
        let (camera, zoom) = self.camera_zoom();
        let screen = canvas_to_screen(CanvasPos(event.location), camera, zoom).0;
        let stored_focus = match self.points.get_mut(&event.slot) {
            Some(p) => {
                p.last_screen = screen;
                p.focus.clone()
            }
            None => {
                handle.motion(data, None, event, seq);
                return;
            }
        };

        if mode == Mode::Forward {
            handle.motion(data, stored_focus, event, seq);
            return;
        }
        handle.motion(data, None, event, seq);

        let centroid = self.centroid();
        if !self.active {
            let dx = centroid.x - self.start_centroid.x;
            let dy = centroid.y - self.start_centroid.y;
            let centroid_disp = (dx * dx + dy * dy).sqrt();
            let pan_progress = centroid_disp / DEAD_ZONE_PX;
            // Spread deviation from the settle baseline (`last_spread`, untouched
            // while inactive). A pinch gathers the fingers without translating the
            // centroid, so zoom must be able to break the dead zone on its own.
            let spread_dev = if self.last_spread > 1.0 {
                (self.spread(centroid) / self.last_spread - 1.0).abs()
            } else {
                0.0
            };
            let zoom_progress = spread_dev / ZOOM_LOCK_FRAC;
            if pan_progress < 1.0 && zoom_progress < 1.0 {
                return;
            }
            // Past the dead zone — no longer a candidate tap, even if the pan/zoom
            // decision needs another frame to resolve.
            self.ever_active = true;

            // Touchpad-style lock, held for the gesture's lifetime. Pan is the
            // default; a pinch must out-pace it by `ZOOM_DOMINANCE` to win. While
            // neither is decisive, wait a frame — bounded by `PAN_LOCK_FORCE` so
            // an ambiguous pan+spread can't freeze. Navigate ignores the lock and
            // runs its own swipe/pinch arbitration.
            let lock = if mode == Mode::Navigate {
                PanZoomLock::Undecided
            } else if zoom_progress >= 1.0 && zoom_progress >= pan_progress * ZOOM_DOMINANCE {
                PanZoomLock::Zoom
            } else if pan_progress >= 1.0
                && (pan_progress >= zoom_progress || pan_progress >= PAN_LOCK_FORCE)
            {
                PanZoomLock::Pan
            } else {
                return;
            };

            self.active = true;
            let cur_spread = self.spread(centroid);
            // Measure pinch scale from the settle-time spread, not the activation
            // spread — an already-progressing pinch compares against rest.
            self.start_spread = if self.last_spread > 1.0 {
                self.last_spread
            } else {
                cur_spread
            };
            self.last_centroid = centroid;
            self.last_spread = cur_spread;
            self.nav_cumulative = Point::from((0.0, 0.0));
            self.panzoom_lock = lock;

            // Hold variants belong to a translation gesture only: a held 3-finger
            // pan drag selects move (armed) / cluster-move (armed + held) / resize
            // (held). A pinch is a zoom, never a resize. A failed move/resize (no
            // window) falls through to pan.
            if self.max_fingers == 3 && self.panzoom_lock == PanZoomLock::Pan {
                let held = event.time.saturating_sub(self.tap_start_time) >= HOLD_MS;
                if self.armed_for_move {
                    if self.try_start_move(data, handle, event, seq, held) {
                        return;
                    }
                } else if held && self.try_start_resize(data, handle, event, seq) {
                    return;
                }
            }
            return;
        }

        match mode {
            Mode::PanZoom => match self.panzoom_lock {
                PanZoomLock::Zoom if self.points.len() >= 2 => self.apply_zoom(data, centroid),
                // A zoom dropped to one finger keeps panning rather than freezing;
                // the lock is always decided (Pan/Zoom) while active.
                _ => self.apply_pan(data, centroid, event.time),
            },
            Mode::Navigate => self.apply_navigate(data, centroid),
            Mode::Forward => {}
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

    fn unset(&mut self, _data: &mut DriftWm) {}
}
