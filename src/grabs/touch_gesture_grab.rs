use std::collections::HashMap;

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
    utils::{Logical, Point, Serial, SERIAL_COUNTER},
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

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// 1–2 fingers with at least one on a window — forward to the app.
    Forward,
    /// 1–2 fingers on empty canvas, or 3 fingers anywhere — viewport pan + zoom.
    PanZoom,
    /// 4 fingers — global navigation (swipe-nearest, pinch overview/home).
    Navigate,
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

    fn apply_panzoom(&mut self, data: &mut DriftWm, centroid: Point<f64, Logical>, time: u32) {
        let zoom = output_state(&self.output).zoom;
        let centroid_delta = centroid - self.last_centroid;
        let pan = Point::from((
            -centroid_delta.x * data.config.touch_speed / zoom,
            -centroid_delta.y * data.config.touch_speed / zoom,
        ));
        data.drift_pan_on(pan, time, &self.output);

        if self.points.len() >= 2 && self.last_spread > 1.0 {
            let spread = self.spread(centroid);
            let scale = spread / self.last_spread;
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
        self.last_centroid = centroid;
    }

    fn apply_navigate(&mut self, data: &mut DriftWm, centroid: Point<f64, Logical>) {
        // Inverted, like the trackpad swipe: drag content right → reveal left.
        let centroid_delta = centroid - self.last_centroid;
        self.nav_cumulative += Point::from((-centroid_delta.x, -centroid_delta.y));

        let threshold = data.config.gesture_thresholds.swipe_distance;
        let cum_sq =
            self.nav_cumulative.x * self.nav_cumulative.x + self.nav_cumulative.y * self.nav_cumulative.y;
        if !self.nav_fired_swipe && cum_sq >= threshold * threshold {
            self.nav_fired_swipe = true;
            let dir = direction_from_vector(self.nav_cumulative);
            data.execute_action(&Action::CenterNearest(dir));
        }

        if !self.nav_fired_pinch && self.start_spread > 1.0 {
            let scale = self.spread(centroid) / self.start_spread;
            if scale < data.config.gesture_thresholds.pinch_in_scale {
                self.nav_fired_pinch = true;
                data.execute_action(&Action::ZoomToFit);
            } else if scale > data.config.gesture_thresholds.pinch_out_scale {
                self.nav_fired_pinch = true;
                data.execute_action(&Action::HomeToggle);
            }
        }
        self.last_centroid = centroid;
    }

    /// Double-tap-drag: hand off to a touch move grab on the window under the
    /// dragging finger. Returns false (and keeps panning) if there's no window.
    fn try_start_move(
        &mut self,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        event: &MotionEvent,
        seq: Serial,
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
        let start = TouchGrabStartData {
            focus: None,
            slot: event.slot,
            location: event.location,
        };
        // All current fingers are already down; seed the count so the move grab
        // stays alive until every one of them lifts.
        let slots = self.points.len();
        let grab = MoveSurfaceGrab::new_touch(start, window, initial, self.output.clone(), slots);
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
        if let Some((window, _)) = data.space.element_under(canvas).map(|(w, l)| (w.clone(), l)) {
            data.raise_and_focus(&window, serial);
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
            data.execute_action(&Action::CenterWindow);
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
                if self.app_owns && !self.system_cancelled {
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
            if dx * dx + dy * dy < DEAD_ZONE_PX * DEAD_ZONE_PX {
                return;
            }
            self.active = true;
            self.ever_active = true;
            self.last_centroid = centroid;
            self.last_spread = self.spread(centroid);
            self.start_spread = self.last_spread;
            self.nav_cumulative = Point::from((0.0, 0.0));

            if self.armed_for_move
                && self.max_fingers == 3
                && self.try_start_move(data, handle, event, seq)
            {
                return;
            }
            return;
        }

        match mode {
            Mode::PanZoom => self.apply_panzoom(data, centroid, event.time),
            Mode::Navigate => self.apply_navigate(data, centroid),
            Mode::Forward => {}
        }
    }

    fn frame(&mut self, data: &mut DriftWm, handle: &mut TouchInnerHandle<'_, DriftWm>, seq: Serial) {
        handle.frame(data, seq);
    }

    fn cancel(&mut self, data: &mut DriftWm, handle: &mut TouchInnerHandle<'_, DriftWm>, seq: Serial) {
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
