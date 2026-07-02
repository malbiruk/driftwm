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

/// Finger travel before a `PanZoom` gesture leaves the dead zone and starts to
/// pan, in millimetres (converted to px per panel via `px_per_mm` so the feel is
/// the same on any touchscreen). Below this — and below the zoom slop — it stays a
/// candidate tap.
const DEAD_ZONE_MM: f64 = 2.0;
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
/// Spread change that engages pinch-zoom with two fingers, as a fraction of the
/// current finger spread. A pinch is multiplicative, so a ratio is naturally
/// panel/scale/size-independent — no px or mm conversion needed. Pan and zoom run
/// simultaneously; the centroid always pans once active, this only gates zoom, so
/// a plain pan's finger jitter can't wobble it.
const ZOOM_SLOP_RATIO: f64 = 0.08;
/// Same slop for a 3-finger gesture. Three fingers can't translate uniformly
/// during a pan, so the spread metric is far noisier than with two; require a
/// larger fraction before zoom engages, or a pan wobbles into it.
const ZOOM_SLOP_RATIO_3F: f64 = 0.20;
/// Minimum finger spread (mm) for pinch-zoom to engage. The slop is a ratio, so
/// at a tiny spread a sliver of jitter is a large fraction; require a real
/// physical separation first — the floor the old absolute-px slop had implicitly.
const MIN_SPREAD_MM: f64 = 3.0;
/// Minimum *change* in finger spread (mm) before pinch engages, on top of the
/// ratio slop. The ratio alone is panel-size-fragile: on a small, cramped panel
/// the fingers sit close, so the baseline spread is small and ordinary jitter
/// during a pan/swipe is a large *fraction* of it — enough to wobble a 3-finger
/// pan into zoom or steal a 4-finger swipe as zoom-to-fit. A deliberate pinch
/// moves fingers well past this floor; jitter doesn't, so it separates intent on
/// any panel size. On a healthy panel (fingers far apart) the ratio change is
/// already many mm, so this floor never binds.
const PINCH_MIN_DELTA_MM: f64 = 3.0;
/// Centroid travel for a 4-finger directional navigation swipe, in millimetres
/// (converted to px per panel via `px_per_mm`). A muscle-memory command gesture
/// wants consistent physical travel across panels; a real mm-scale threshold also
/// keeps a pinch-in's small centroid drift from being misread as a swipe.
const NAV_SWIPE_MM: f64 = 15.0;
/// During 4-finger navigation, a swipe won't fire once pinch progress reaches
/// this fraction. A natural pinch-in drags the thumb a long way toward the other
/// fingers, drifting the centroid enough to read as a swipe, so the pinch has to
/// claim the gesture early (here, ~6% spread change) before the tiny swipe
/// threshold steals it. A clean directional swipe keeps its spread well below
/// this.
const SWIPE_BLOCK_PINCH: f64 = 0.4;

/// Logical pixels per millimetre for `output`, used to convert physical gesture
/// thresholds (dead zone, nav swipe) into the panel's pixel space. Touch points
/// already arrive in logical px, so only the panel's physical size is needed.
///
/// Prefers the touch digitizer's own physical size (`device_mm`) over the
/// output's EDID. The finger physically travels across the digitizer, so its mm
/// size is the right denominator — and a cheap USB touch monitor's EDID often
/// reports a bogus size (e.g. a cloned ~2× value) while the digitizer reports
/// the truth. Falls back to EDID, then to a ~96 dpi-equivalent density (nested
/// backend, or a panel with no usable size anywhere).
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

    // Density from a physical size via area (a geometric mean of both axes), so it
    // doesn't matter whether the digitizer reports its axes in the panel's native
    // orientation — EDID guarantees that alignment, a digitizer doesn't. `None`
    // when the size is missing, or yields an out-of-range (bogus) density.
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

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// 1–2 fingers with at least one on a window — forward to the app.
    Forward,
    /// 1–2 fingers on empty canvas, or 3 fingers anywhere — viewport pan and
    /// pinch-zoom, applied simultaneously (the centroid pans, the spread zooms).
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
    /// Pinch-zoom is live for the current `PanZoom` gesture (set once the spread
    /// clears the zoom slop). Pan runs regardless; this only gates zoom.
    zoom_engaged: bool,
    /// Logical px per mm for this grab's panel, for physical thresholds.
    px_per_mm: f64,
}

impl TouchGestureGrab {
    pub fn new(
        start_data: TouchGrabStartData<DriftWm>,
        output: Output,
        device_mm: Option<(f64, f64)>,
    ) -> Self {
        let px_per_mm = output_px_per_mm(&output, device_mm);
        let edid = output.physical_properties().size;
        tracing::debug!(
            target: "touch",
            output = %output.name(),
            ?device_mm,
            edid_mm = ?(edid.w, edid.h),
            mode = ?output.current_mode().map(|m| (m.size.w, m.size.h)),
            scale = output.current_scale().fractional_scale(),
            px_per_mm,
            "gesture grab: resolved density",
        );
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
            zoom_engaged: false,
            px_per_mm,
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

    /// Spread-change fraction required to engage zoom (larger with three fingers).
    fn zoom_slop_ratio(&self) -> f64 {
        if self.max_fingers >= 3 {
            ZOOM_SLOP_RATIO_3F
        } else {
            ZOOM_SLOP_RATIO
        }
    }

    /// A pinch reading is only trustworthy when every finger the gesture started
    /// with is currently down. Cheap digitizers intermittently drop a contact
    /// mid-gesture (fingers bunched on a small surface merge or vanish for many
    /// frames); with a finger missing, the remaining points' spread lurches like
    /// a large pinch — enough to wobble a 3-finger pan into zoom or make a
    /// 4-finger swipe read as zoom-to-fit. Suppress pinch detection until the
    /// full set is back. On healthy hardware this is always true, so it's inert.
    fn all_fingers_down(&self) -> bool {
        self.points.len() >= self.max_fingers
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
        let swipe_threshold = NAV_SWIPE_MM * self.px_per_mm;
        let swipe_progress = swipe_dist / swipe_threshold;

        // Pinch progress as a fraction of the in/out margin: a pure swipe's
        // natural splay stays well below 1.0, a deliberate pinch climbs past it.
        let cur_spread = self.spread(centroid);
        let scale = if self.start_spread > 1.0 {
            cur_spread / self.start_spread
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

        // The ratio alone isn't enough to call it a pinch: on a small, cramped
        // panel four fingers can't translate without their spread fluctuating
        // ~margin, so a swipe's jitter crosses `pinch_progress` and steals the
        // gesture as zoom-to-fit. Require a real physical spread change too, and
        // only trust the reading while all four fingers are down — a dropped
        // contact collapses the spread far past the floor and would fire zoom.
        // Until both hold the pinch reads as zero, so it neither fires nor blocks
        // the swipe.
        let effective_pinch = if self.all_fingers_down()
            && (cur_spread - self.start_spread).abs() >= PINCH_MIN_DELTA_MM * self.px_per_mm
        {
            pinch_progress
        } else {
            0.0
        };

        tracing::debug!(
            target: "touch",
            fingers = self.points.len(),
            max_fingers = self.max_fingers,
            swipe_progress,
            pinch_progress,
            effective_pinch,
            scale,
            spread_delta_mm = (cur_spread - self.start_spread).abs() / self.px_per_mm,
            swipe_dist_mm = swipe_dist / self.px_per_mm,
            floor_mm = PINCH_MIN_DELTA_MM,
            nav_swipe_mm = NAV_SWIPE_MM,
            px_per_mm = self.px_per_mm,
            "nav frame",
        );

        // Swipe and pinch are mutually exclusive; whichever is further past its
        // own threshold claims the gesture. Pinch wins ties, and a developing
        // pinch (past `SWIPE_BLOCK_PINCH`) blocks the swipe outright — a pinch-in
        // contracts slowly while the hand drifts the centroid, so otherwise the
        // small swipe threshold steals it before the pinch completes.
        if effective_pinch >= 1.0 && effective_pinch >= swipe_progress {
            self.nav_fired_pinch = true;
            if scale < 1.0 {
                data.execute_action(&Action::ZoomToFit);
            } else {
                data.execute_action(&Action::HomeToggle);
            }
        } else if swipe_progress >= 1.0 && effective_pinch < SWIPE_BLOCK_PINCH {
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
        let prev_max = self.max_fingers;
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

                // Re-arm the gesture at start and at each finger-count tier
                // crossing (into 3-finger system gestures, into 4-finger
                // navigation), so a clean tap stays distinguishable from a drag
                // and the navigation recognizer measures from a fresh baseline.
                let crossed_system = prev_max < 3 && self.max_fingers >= 3;
                let crossed_nav = prev_max < 4 && self.max_fingers >= 4;
                // Exit fullscreen before a 3+ finger system gesture so the pan/zoom
                // acts on the restored canvas instead of sliding the parked fullscreen
                // window off its camera origin. Stash the exited window so a 4-finger
                // nav firing right after can still anchor to it. Uses the touch output,
                // which may differ from the active/pointer output.
                if crossed_system
                    && let Some(window) = data
                        .fullscreen
                        .get(&self.output)
                        .map(|fs| fs.window.clone())
                {
                    data.gesture_exited_fullscreen = Some(window);
                    data.exit_fullscreen_on(&self.output);
                }
                if self.points.len() == 1 || crossed_system || crossed_nav {
                    self.active = false;
                    self.zoom_engaged = false;
                    self.tap_start_time = event.time;
                    let c = self.centroid();
                    self.start_centroid = c;
                    self.start_spread = self.spread(c);
                    self.nav_cumulative = Point::from((0.0, 0.0));
                    self.nav_fired_swipe = false;
                    self.nav_fired_pinch = false;
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
            // actions, so there's nothing to coast. A pinch must not fling the
            // canvas either — pan runs through a zoom in the simultaneous model,
            // so skip the coast for any gesture that engaged zoom.
            if was_present && mode == Mode::PanZoom && self.ever_active && !self.zoom_engaged {
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

        // 4-finger navigation is a one-shot recognizer measured from the
        // gesture's rest baseline — swipe vs pinch, fire the dominant one. No
        // pan/zoom dead zone sits in front of it (that double threshold made a
        // deliberate pinch barely register).
        if mode == Mode::Navigate {
            self.ever_active = true;
            self.apply_navigate(data, centroid);
            return;
        }

        // PanZoom: pan and zoom run simultaneously. The centroid pans, the finger
        // spread zooms; neither excludes the other.
        if !self.active {
            let dx = centroid.x - self.start_centroid.x;
            let dy = centroid.y - self.start_centroid.y;
            let centroid_disp = (dx * dx + dy * dy).sqrt();
            // A real pinch must clear both the ratio slop AND a physical spread
            // change (`PINCH_MIN_DELTA_MM`): the ratio gathers the fingers without
            // translating the centroid, so the spread can break the dead zone on
            // its own — but on a small panel ordinary jitter crosses the ratio at
            // a tiny absolute change, so the mm floor keeps a pan from wobbling
            // into zoom. (A pinch needs >= 2 fingers and a non-trivial baseline.)
            let has_two = self.points.len() >= 2;
            let cur_spread = if has_two { self.spread(centroid) } else { 0.0 };
            let span_ratio = if has_two && self.last_spread > MIN_SPREAD_MM * self.px_per_mm {
                (cur_spread / self.last_spread - 1.0).abs()
            } else {
                0.0
            };
            let slop = self.zoom_slop_ratio();
            let spread_pinch = has_two
                && span_ratio >= slop
                && (cur_spread - self.last_spread).abs() >= PINCH_MIN_DELTA_MM * self.px_per_mm;
            let dead_zone = DEAD_ZONE_MM * self.px_per_mm;
            // Break the dead zone on the spread change alone (ungated): a stale,
            // over-counted `max_fingers` must never leave a pure, non-translating
            // pinch with no way out. Zoom itself only *engages* with the full set
            // down, so a dropped-contact spread lurch still can't latch it.
            if centroid_disp < dead_zone && !spread_pinch {
                return;
            }
            self.ever_active = true;
            self.active = true;
            // Engage zoom right away only if the gesture broke the dead zone by a
            // real pinch; otherwise it engages later once the spread clears both.
            self.zoom_engaged = spread_pinch && self.all_fingers_down();
            self.last_centroid = centroid;
            self.last_spread = self.spread(centroid);

            // Hold variants belong to a translation gesture only: a held 3-finger
            // pan drag selects move (armed) / cluster-move (armed + held) / resize
            // (held). A pinch is a zoom, never a resize. A failed move/resize (no
            // window) falls through to pan.
            if self.max_fingers == 3 && !self.zoom_engaged {
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

        self.apply_pan(data, centroid, event.time);
        if self.points.len() >= 2 {
            let cur_spread = self.spread(centroid);
            tracing::debug!(
                target: "touch",
                fingers = self.points.len(),
                max_fingers = self.max_fingers,
                zoom_engaged = self.zoom_engaged,
                cur_spread,
                last_spread = self.last_spread,
                ratio = if self.last_spread > 0.0 { cur_spread / self.last_spread - 1.0 } else { 0.0 },
                delta_mm = (cur_spread - self.last_spread).abs() / self.px_per_mm,
                slop = self.zoom_slop_ratio(),
                floor_mm = PINCH_MIN_DELTA_MM,
                px_per_mm = self.px_per_mm,
                "panzoom frame",
            );
            // Engage zoom once the spread has changed past both the ratio slop and
            // the physical floor (so a pan's jitter can't latch it), consuming the
            // change so there's no jump on the first zoomed frame. Requires the
            // full finger set — a dropped contact collapses the spread past both.
            if !self.zoom_engaged
                && self.all_fingers_down()
                && self.last_spread > MIN_SPREAD_MM * self.px_per_mm
                && (cur_spread / self.last_spread - 1.0).abs() >= self.zoom_slop_ratio()
                && (cur_spread - self.last_spread).abs() >= PINCH_MIN_DELTA_MM * self.px_per_mm
            {
                self.zoom_engaged = true;
                self.last_spread = cur_spread;
            }
            if self.zoom_engaged {
                self.apply_zoom(data, centroid);
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

    fn unset(&mut self, _data: &mut DriftWm) {}
}
