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
use driftwm::config::{
    Action, BindingContext, ContinuousAction, Direction, GestureConfigEntry, GestureTrigger,
    ThresholdAction,
};

use driftwm::window_ext::WindowExt;

use crate::input::gestures::direction_from_vector;
use crate::input::touch::HeldTouchEvent;
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
/// ratio slop. On a small panel the baseline spread is tiny, so jitter is a large
/// *fraction* of it and the ratio alone lets a pan wobble into zoom or a swipe
/// steal as zoom-to-fit. A deliberate pinch clears this floor; jitter doesn't. On
/// a roomy panel the ratio change is already many mm, so it never binds.
const PINCH_MIN_DELTA_MM: f64 = 3.0;
/// Consecutive frames the pinch floor must hold before zoom is trusted. A real
/// pinch sustains the spread change; a swipe or pan's finger splay only stabs
/// past the floor for lone frames — especially on a cramped panel, where four
/// fingers sit so close that a deliberate pinch-in barely out-travels the splay
/// in magnitude and only its *persistence* tells them apart. One frame over the
/// floor can't fire zoom.
const PINCH_CONFIRM_FRAMES: u32 = 2;
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

/// The config-resolved behavior for the current finger count and context, looked
/// up once per finger-count tier from `[touch]`. An all-`None` plan means the
/// gesture is unbound → forward it to the app.
#[derive(Clone, Default)]
struct Plan {
    /// Translation axis (centroid): `Continuous(PanViewport)` pans; `Threshold`
    /// accumulates and fires once (one-shot navigate).
    swipe: Option<GestureConfigEntry>,
    /// Per-direction swipe overrides (up, down, left, right), checked before the
    /// base swipe threshold fires.
    swipe_dirs: [Option<ThresholdAction>; 4],
    /// Move/resize preemptors on a translation drag: armed (recent tap) and held.
    doubletap_swipe: Option<ContinuousAction>,
    hold_swipe: Option<ContinuousAction>,
    /// Scale axis (spread): continuous zoom, or one-shot pinch-in/out.
    pinch: Option<ContinuousAction>,
    pinch_in: Option<ThresholdAction>,
    pinch_out: Option<ThresholdAction>,
    tap: Option<ThresholdAction>,
    doubletap: Option<ThresholdAction>,
}

impl Plan {
    fn translation_continuous(&self) -> bool {
        matches!(self.swipe, Some(GestureConfigEntry::Continuous(_)))
    }

    fn translation_threshold(&self) -> bool {
        matches!(self.swipe, Some(GestureConfigEntry::Threshold(_)))
            || self.swipe_dirs.iter().any(Option::is_some)
    }

    fn scale_continuous(&self) -> bool {
        self.pinch.is_some()
    }

    fn scale_threshold(&self) -> bool {
        self.pinch_in.is_some() || self.pinch_out.is_some()
    }

    /// A move/resize preemptor lives in the simultaneous engine's breakthrough, so
    /// its presence pulls the gesture there even without a continuous pan/zoom.
    fn has_preemptor(&self) -> bool {
        self.doubletap_swipe.is_some() || self.hold_swipe.is_some()
    }

    /// Run the simultaneous pan+zoom engine when translation pans continuously, when
    /// scale zooms continuously and translation isn't a one-shot swipe, or when a
    /// move/resize preemptor is bound. A one-shot swipe (threshold) instead claims
    /// the gesture for the arbitrated engine so it can fire, even if a continuous
    /// zoom is also bound (mixed — the zoom is dropped for that finger count; see the
    /// `[touch]` docs).
    fn simultaneous(&self) -> bool {
        self.translation_continuous()
            || (self.scale_continuous() && !self.translation_threshold())
            || self.has_preemptor()
    }

    fn is_empty(&self) -> bool {
        self.swipe.is_none()
            && self.swipe_dirs.iter().all(Option::is_none)
            && self.doubletap_swipe.is_none()
            && self.hold_swipe.is_none()
            && self.pinch.is_none()
            && self.pinch_in.is_none()
            && self.pinch_out.is_none()
            && self.tap.is_none()
            && self.doubletap.is_none()
    }

    /// Whether any binding in this tier would end fullscreen if it engaged —
    /// continuous entries always do; threshold entries per action.
    fn ends_fullscreen(&self) -> bool {
        self.translation_continuous()
            || self.has_preemptor()
            || self.pinch.is_some()
            || matches!(&self.swipe, Some(GestureConfigEntry::Threshold(a)) if a.ends_fullscreen())
            || self
                .swipe_dirs
                .iter()
                .flatten()
                .any(|a| a.ends_fullscreen())
            || [&self.pinch_in, &self.pinch_out, &self.tap, &self.doubletap]
                .into_iter()
                .flatten()
                .any(|a| a.ends_fullscreen())
    }
}

struct TouchPoint {
    /// Physical screen position. Stable across camera moves (recovered each
    /// motion from the canvas location via the current camera/zoom).
    last_screen: Point<f64, Logical>,
    /// Surface focus captured at down (canvas-origin), for app forwarding.
    focus: Option<(FocusTarget, Point<f64, Logical>)>,
}

/// Touch grab that owns the whole multi-finger canvas-gesture lifecycle. Which
/// action each recognized gesture drives comes from the `[touch]` config, resolved
/// per finger-count tier and context into a [`Plan`]; an unbound gesture forwards
/// to the app. The default bindings reproduce the classic behavior: app forwarding
/// (1–2 fingers on a window), simultaneous pan + pinch-zoom (1–2 fingers on empty
/// canvas or 3 fingers anywhere), one-shot 4-finger navigation, and 3-finger tap /
/// double-tap / double-tap-drag / hold-drag. Set on the first touch-down; tracks
/// all slots and unsets itself when the last finger lifts. Parallel to `PanGrab`.
pub struct TouchGestureGrab {
    start_data: TouchGrabStartData<DriftWm>,
    output: Output,
    points: HashMap<TouchSlot, TouchPoint>,
    /// The first finger landed on a client surface — gates the escalation cancel
    /// that revokes the app's forwarded touches when a system gesture takes over.
    app_owns: bool,
    /// Binding context for config lookups, fixed from the first finger's focus
    /// (`OnWindow` if it landed on a surface, else `OnCanvas`). Derived from the
    /// same `focus` as `app_owns` so the two can't disagree.
    context: BindingContext,
    /// Config-resolved behavior for `max_fingers` + `context`, re-looked-up each
    /// time the finger count grows.
    plan: Plan,
    /// High-water mark of simultaneous fingers — the count the plan resolves for.
    max_fingers: usize,
    /// App touch sequence revoked once on escalation to a system gesture.
    system_cancelled: bool,
    /// A finger lifted while the sequence still belonged to the app (unbound
    /// tier). A real multi-finger gesture plants every finger before lifting
    /// any, so this marks the sequence typing-like: no tier may claim it
    /// anymore, and nothing is withheld — every event forwards.
    claims_blocked: bool,
    /// Some higher finger-count tier binds something, so a forming gesture
    /// could still claim this app-owned sequence — gates the holdback.
    /// Re-resolved with the plan at each tier crossing.
    higher_tier_bound: bool,
    /// Time of the sequence's first touch-down, for stagger logging. Unlike
    /// `tap_start_time` it is never re-armed at tier crossings.
    first_down_time: u32,
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
    /// Consecutive frames the pinch floor has held, for the confirm debounce.
    pinch_streak: u32,
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
        Self {
            start_data,
            output,
            points: HashMap::new(),
            app_owns: false,
            context: BindingContext::OnCanvas,
            plan: Plan::default(),
            max_fingers: 0,
            system_cancelled: false,
            claims_blocked: false,
            higher_tier_bound: false,
            first_down_time: 0,
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
            pinch_streak: 0,
            zoom_engaged: false,
            px_per_mm,
        }
    }

    /// Resolve the `[touch]` bindings for the current finger count + context into a
    /// `Plan`. Re-run whenever `max_fingers` grows (a finger-count tier crossing).
    fn resolve_plan(&self, data: &DriftWm) -> Plan {
        // Clamp the lookup tier to the max bindable finger count: a stray 6th+
        // contact resolves as the 5-finger tier (which navigates by default) rather
        // than an empty plan that would forward and abort the gesture. The true
        // `max_fingers` is left intact for tier-crossing and `all_fingers_down`.
        self.resolve_plan_at(data, (self.max_fingers as u32).min(5))
    }

    /// Whether a forming gesture at a higher finger-count tier could still
    /// claim this app-owned sequence; when nothing above can claim, events
    /// forward with zero holdback latency.
    fn any_higher_tier_bound(&self, data: &DriftWm) -> bool {
        let cur = (self.max_fingers as u32).min(5);
        ((cur + 1)..=5).any(|n| !self.resolve_plan_at(data, n).is_empty())
    }

    fn resolve_plan_at(&self, data: &DriftWm, n: u32) -> Plan {
        let cx = self.context;
        let continuous = |t: GestureTrigger| match data.config.touch_lookup(&t, cx) {
            Some(GestureConfigEntry::Continuous(a)) => Some(a.clone()),
            _ => None,
        };
        let threshold = |t: GestureTrigger| match data.config.touch_lookup(&t, cx) {
            Some(GestureConfigEntry::Threshold(a)) => Some(a.clone()),
            _ => None,
        };
        Plan {
            swipe: data
                .config
                .touch_lookup(&GestureTrigger::Swipe { fingers: n }, cx)
                .cloned(),
            swipe_dirs: [
                threshold(GestureTrigger::SwipeUp { fingers: n }),
                threshold(GestureTrigger::SwipeDown { fingers: n }),
                threshold(GestureTrigger::SwipeLeft { fingers: n }),
                threshold(GestureTrigger::SwipeRight { fingers: n }),
            ],
            doubletap_swipe: continuous(GestureTrigger::DoubletapSwipe { fingers: n }),
            hold_swipe: continuous(GestureTrigger::HoldSwipe { fingers: n }),
            pinch: continuous(GestureTrigger::Pinch { fingers: n }),
            pinch_in: threshold(GestureTrigger::PinchIn { fingers: n }),
            pinch_out: threshold(GestureTrigger::PinchOut { fingers: n }),
            tap: threshold(GestureTrigger::Tap { fingers: n }),
            doubletap: threshold(GestureTrigger::Doubletap { fingers: n }),
        }
    }

    /// Hand a translation drag to a window move/resize grab per the resolved
    /// continuous action. `cluster` extends a move to the focused window's
    /// snap-cluster. Returns false (keep panning) if it isn't a window-grab action
    /// or there's no canvas window under the finger.
    fn start_window_grab(
        &mut self,
        action: ContinuousAction,
        data: &mut DriftWm,
        handle: &mut TouchInnerHandle<'_, DriftWm>,
        event: &MotionEvent,
        seq: Serial,
        cluster: bool,
    ) -> bool {
        match action {
            ContinuousAction::MoveWindow => self.try_start_move(data, handle, event, seq, cluster),
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

    /// A pinch reading is only trustworthy with every starting finger down. Cheap
    /// digitizers drop a contact mid-gesture (fingers bunched on a small surface
    /// merge or vanish for frames); with one missing, the remaining points' spread
    /// lurches like a big pinch and can fire zoom. Inert on healthy hardware.
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

        // Ratio alone isn't a pinch: on a cramped panel four fingers can't translate
        // without their spread fluctuating ~margin, so a swipe's jitter crosses
        // `pinch_progress` and steals zoom-to-fit. Require a real physical spread
        // change too, held for a couple of frames, and only while all fingers are
        // down — a dropped contact collapses the spread past the floor, and a
        // swipe's splay stabs past it for lone frames. Until confirmed it reads
        // zero, so it neither fires nor blocks the swipe.
        let qualified = self.all_fingers_down()
            && (cur_spread - self.start_spread).abs() >= PINCH_MIN_DELTA_MM * self.px_per_mm;
        self.pinch_streak = if qualified { self.pinch_streak + 1 } else { 0 };
        let effective_pinch = if self.pinch_streak >= PINCH_CONFIRM_FRAMES {
            pinch_progress
        } else {
            0.0
        };

        // Swipe and pinch are mutually exclusive; whichever is further past its
        // own threshold claims the gesture. Pinch wins ties, and a developing
        // pinch (past `SWIPE_BLOCK_PINCH`) blocks the swipe outright — a pinch-in
        // contracts slowly while the hand drifts the centroid, so otherwise the
        // small swipe threshold steals it before the pinch completes.
        if effective_pinch >= 1.0 && effective_pinch >= swipe_progress {
            let action = if scale < 1.0 {
                self.plan.pinch_in.clone()
            } else {
                self.plan.pinch_out.clone()
            };
            if let Some(a) = action {
                self.nav_fired_pinch = true;
                self.execute_threshold(data, a);
            }
        } else if swipe_progress >= 1.0 && effective_pinch < SWIPE_BLOCK_PINCH {
            let dir = direction_from_vector(self.nav_cumulative);
            if let Some(a) = self.swipe_threshold_for(dir) {
                self.nav_fired_swipe = true;
                self.execute_threshold(data, a);
            }
        }
    }

    /// Fire a resolved threshold action. `CenterNearest` derives its direction from
    /// the accumulated swipe vector; a `Fixed` action ignores it.
    fn execute_threshold(&self, data: &mut DriftWm, action: ThresholdAction) {
        match action {
            ThresholdAction::CenterNearest => {
                let dir = direction_from_vector(self.nav_cumulative);
                data.execute_action(&Action::CenterNearest(dir));
            }
            ThresholdAction::Fixed(a) => data.execute_action(&a),
        }
    }

    /// The threshold action for a fired swipe: a per-direction override for the
    /// four cardinals if bound, else the base swipe binding.
    fn swipe_threshold_for(&self, dir: Direction) -> Option<ThresholdAction> {
        let cardinal = match dir {
            Direction::Up => Some(0),
            Direction::Down => Some(1),
            Direction::Left => Some(2),
            Direction::Right => Some(3),
            _ => None,
        };
        if let Some(i) = cardinal
            && let Some(a) = &self.plan.swipe_dirs[i]
        {
            return Some(a.clone());
        }
        match &self.plan.swipe {
            Some(GestureConfigEntry::Threshold(a)) => Some(a.clone()),
            _ => None,
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
            let Some(grab) = data.build_touch_pinned_move_grab(&window, start, self.points.len())
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
        let initial = data.stage.position_of(&window).unwrap_or(loc);
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
        let screen_centroid = self.centroid();

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
            let slots = self.points.len();
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
        let slots = self.points.len();
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

    /// On last-finger-up, fire the resolved tap (single) / doubletap (double)
    /// action for a clean tap. A tap is short, never passed the dead zone, and (via
    /// the escalation cancel) no longer belongs to an app, so it acts on the tapped
    /// window regardless of what's under it.
    fn detect_tap(&mut self, data: &mut DriftWm, time: u32) {
        if self.ever_active || (self.plan.tap.is_none() && self.plan.doubletap.is_none()) {
            return;
        }
        if time.saturating_sub(self.tap_start_time) > TAP_MAX_MS {
            return;
        }
        let (camera, zoom) = self.camera_zoom();
        let canvas = screen_to_canvas(ScreenPos(self.start_centroid), camera, zoom).0;
        let serial = SERIAL_COUNTER.next_serial();
        let under = data.element_under_raw(canvas).map(|(w, _)| w.clone());
        if let Some(window) = &under {
            data.raise_and_focus(window, serial);
        }
        let double = data
            .touch_state
            .last_three_finger_tap
            .is_some_and(|t| time.saturating_sub(t) < DOUBLE_TAP_MS);
        if double {
            data.touch_state.last_three_finger_tap = None;
            if let Some(action) = self.plan.doubletap.clone() {
                self.execute_threshold(data, action);
            }
        } else {
            data.touch_state.last_three_finger_tap = Some(time);
            match self.plan.tap.clone() {
                // A deferred center avoids flashing a center before a follow-up
                // double-tap (fit) or double-tap-drag (move); a fresh interaction
                // cancels it. Specific to center — other tap actions fire now.
                Some(ThresholdAction::Fixed(Action::CenterWindow)) => {
                    let target = under
                        .filter(|w| data.is_canvas_window(w))
                        .or_else(|| data.focused_window().filter(|w| data.is_canvas_window(w)));
                    if let Some(window) = target {
                        data.schedule_pending_center(
                            window,
                            Duration::from_millis(DOUBLE_TAP_MS as u64),
                        );
                    }
                }
                Some(action) => self.execute_threshold(data, action),
                None => {}
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
        if data.touch_state.replaying_holdback {
            handle.down(data, focus, event, seq);
            return;
        }
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

        // The first finger fixes the gesture's binding context from its focus: on a
        // surface → app content (`OnWindow`, forwarded unless a system gesture is
        // bound), on empty canvas → viewport gesture (`OnCanvas`). `app_owns` comes
        // from the same `focus` so they can't disagree. A recent tap arms this touch
        // for a double-tap-drag move. Later fingers don't flip either, so a stray
        // contact can't strand an in-progress gesture.
        if self.points.len() == 1 {
            self.first_down_time = event.time;
            self.app_owns = focus.is_some();
            self.context = if focus.is_some() {
                BindingContext::OnWindow
            } else {
                BindingContext::OnCanvas
            };
            self.armed_for_move = data
                .touch_state
                .last_three_finger_tap
                .is_some_and(|t| event.time.saturating_sub(t) < DOUBLE_TAP_MS);
        } else {
            // Landing stagger, for tuning `HOLDBACK_MS` against real hardware.
            tracing::debug!(
                "touch stagger: finger {} at +{}ms",
                self.points.len(),
                event.time.saturating_sub(self.first_down_time)
            );
        }

        // Re-resolve the config plan whenever the finger count grows into a new tier.
        if self.max_fingers != prev_max {
            self.plan = self.resolve_plan(data);
            self.higher_tier_bound = self.any_higher_tier_bound(data);
        }

        // An early lift pinned the sequence to the app, whatever tier the
        // finger count reaches.
        if self.claims_blocked {
            handle.down(data, focus, event, seq);
            return;
        }

        if self.plan.is_empty() {
            // Unbound → the app's, but withhold events while a higher tier
            // could still claim the sequence (see `hold_touch_event`).
            if self.higher_tier_bound {
                data.hold_touch_event(HeldTouchEvent::Down {
                    slot: event.slot,
                    focus,
                    location: event.location,
                    time: event.time,
                });
            } else {
                handle.down(data, focus, event, seq);
            }
        } else {
            // Anything still withheld is dropped unsent; fingers already
            // delivered are revoked by the escalation cancel below.
            data.discard_touch_holdback();
            // Escalation from app-forwarding to a system gesture: revoke the app's
            // in-flight touch sequence so it sees no dangling points. smithay's touch
            // cancel skips any slot already framed (current >= pending) — i.e. every
            // finger that landed in an earlier frame, the common case for a 3-finger
            // gesture. Replay a no-op motion on those slots first to bump pending past
            // current, so the cancel that follows actually revokes them.
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

            // Re-arm the gesture at start and at each finger-count tier crossing (into
            // 3-finger system gestures, into 4-finger navigation), so a clean tap stays
            // distinguishable from a drag and the navigation recognizer measures from a
            // fresh baseline.
            let crossed_system = prev_max < 3 && self.max_fingers >= 3;
            let crossed_nav = prev_max < 4 && self.max_fingers >= 4;
            // Exit fullscreen before a system gesture that ends it, so pan/zoom acts
            // on the restored canvas instead of sliding the parked window off its
            // camera origin. Gated on the tier actually binding something that ends
            // fullscreen (an unbound touch leaves it alone); both crossings are
            // checked since a 3-finger tier can preserve fullscreen while the
            // 4-finger tier doesn't. Stash the window so a nav firing right after can
            // still anchor to it. Uses the touch output, which may differ from the
            // active/pointer output.
            if (crossed_system || crossed_nav)
                && self.plan.ends_fullscreen()
                && let Some(window) = data.fullscreen_window_on(&self.output)
            {
                data.pre_exited_fullscreen = Some(window);
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
                self.pinch_streak = 0;
            }
            self.rebaseline();
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
        let was_present = self.points.contains_key(&event.slot);

        // A lift while the sequence still belongs to the app pins it there
        // (see `claims_blocked`).
        if was_present && self.plan.is_empty() {
            self.claims_blocked = true;
        }

        // A lift also flushes any withheld events right away — typing contacts
        // are short, so rolled keys deliver at first lift, not at the deadline.
        if was_present && data.touch_state.holdback.is_some() {
            data.hold_touch_event(HeldTouchEvent::Up {
                slot: event.slot,
                time: event.time,
            });
            self.flush_holdback_inner(data, handle, seq);
            self.points.remove(&event.slot);
            if self.points.is_empty() {
                handle.unset_grab(self, data);
            } else {
                self.rebaseline();
            }
            return;
        }

        handle.up(data, event, seq);
        self.points.remove(&event.slot);

        if self.points.is_empty() {
            // Only a continuous pan accumulates velocity to coast. A one-shot
            // navigate fires discrete actions (nothing to coast), and a pinch must
            // not fling the canvas — pan runs through a zoom in the simultaneous
            // model, so skip the coast for any gesture that engaged zoom.
            let panned = matches!(
                self.plan.swipe,
                Some(GestureConfigEntry::Continuous(
                    ContinuousAction::PanViewport
                ))
            );
            if was_present && panned && self.ever_active && !self.zoom_engaged {
                data.launch_momentum_on(&self.output);
            }
            if was_present && !self.claims_blocked {
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
        if data.touch_state.replaying_holdback {
            handle.motion(data, None, event, seq);
            return;
        }
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

        if data.touch_state.holdback.is_some() {
            data.hold_touch_event(HeldTouchEvent::Motion {
                slot: event.slot,
                location: event.location,
                time: event.time,
            });
            return;
        }

        // Unbound gesture (or one an early lift pinned to the app) → forward.
        if self.claims_blocked || self.plan.is_empty() {
            handle.motion(data, stored_focus, event, seq);
            return;
        }
        handle.motion(data, None, event, seq);

        let centroid = self.centroid();

        // Arbitrated one-shot engine: a threshold swipe and/or pinch-in/out measured
        // from the gesture's rest baseline — fire the dominant one. No dead zone sits
        // in front of it (that double threshold made a deliberate pinch barely
        // register). A tap/doubletap-only plan tracks nothing here; it fires on up.
        if !self.plan.simultaneous() {
            if self.plan.translation_threshold() || self.plan.scale_threshold() {
                self.ever_active = true;
                self.apply_navigate(data, centroid);
            }
            return;
        }

        // Simultaneous engine: continuous pan and/or zoom (whichever is bound) plus
        // the move/resize preemptors. The centroid pans, the finger spread zooms;
        // neither excludes the other.
        if !self.active {
            let dx = centroid.x - self.start_centroid.x;
            let dy = centroid.y - self.start_centroid.y;
            let centroid_disp = (dx * dx + dy * dy).sqrt();
            // A real pinch clears both the ratio slop and the mm floor: a pinch
            // gathers the fingers without moving the centroid, so the spread must
            // break the dead zone on its own — but on a small panel jitter crosses
            // the ratio at a tiny absolute change, so the mm floor stops a pan
            // wobbling into zoom. Only considered when a continuous zoom is bound.
            let has_two = self.plan.scale_continuous() && self.points.len() >= 2;
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
            // Break the dead zone on the spread change alone, ungated by finger
            // count: a stale, over-counted `max_fingers` must never trap a pure,
            // non-translating pinch. Safe because zoom only *engages* with the full
            // set down, so a dropped-contact spread lurch still can't latch it.
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

            // Hold variants belong to a translation gesture only: a held drag selects
            // move (armed doubletap-swipe) / cluster-move (armed + held) / resize
            // (held hold-swipe). A pinch is a zoom, never a grab. A failed grab (no
            // window) falls through to pan.
            if !self.zoom_engaged {
                let held = event.time.saturating_sub(self.tap_start_time) >= HOLD_MS;
                // The held→cluster upgrade is a doubletap-swipe affordance
                // ("hold to move the cluster"). A hold-swipe binding is already
                // held by definition, so upgrading it would make `move-window`
                // indistinguishable from `move-snapped-windows` there.
                let (preempt, cluster) = if self.armed_for_move {
                    (self.plan.doubletap_swipe.clone(), held)
                } else if held {
                    (self.plan.hold_swipe.clone(), false)
                } else {
                    (None, false)
                };
                if let Some(action) = preempt
                    && self.start_window_grab(action, data, handle, event, seq, cluster)
                {
                    return;
                }
            }
            return;
        }

        if matches!(
            self.plan.swipe,
            Some(GestureConfigEntry::Continuous(
                ContinuousAction::PanViewport
            ))
        ) {
            self.apply_pan(data, centroid, event.time);
        }
        if self.plan.scale_continuous() && self.points.len() >= 2 {
            let cur_spread = self.spread(centroid);
            // Engage zoom once the spread clears both the ratio slop and the mm
            // floor for a couple of frames (so a pan's lone jitter spike can't
            // latch it), consuming the change so there's no jump on the first
            // zoomed frame. Needs the full finger set — a dropped contact collapses
            // the spread past both.
            let qualified = self.all_fingers_down()
                && self.last_spread > MIN_SPREAD_MM * self.px_per_mm
                && (cur_spread / self.last_spread - 1.0).abs() >= self.zoom_slop_ratio()
                && (cur_spread - self.last_spread).abs() >= PINCH_MIN_DELTA_MM * self.px_per_mm;
            self.pinch_streak = if qualified { self.pinch_streak + 1 } else { 0 };
            if !self.zoom_engaged && self.pinch_streak >= PINCH_CONFIRM_FRAMES {
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
