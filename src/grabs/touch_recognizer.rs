use std::collections::HashMap;

use smithay::backend::input::TouchSlot;
use smithay::utils::{Logical, Point};

use driftwm::config::{
    Action, BindingContext, Config, ContinuousAction, GestureConfigEntry, GestureThresholds,
    GestureTrigger, ThresholdAction,
};

use crate::input::gestures::direction_from_vector;

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

/// The config-resolved behavior for the current finger count and context, looked
/// up once per finger-count tier from `[touch]`. An all-`None` plan means the
/// gesture is unbound → forward it to the app.
#[derive(Clone, Default)]
pub struct Plan {
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

/// One touch event fed to the recognizer, in SCREEN-space coordinates. The
/// adapter converts canvas↔screen at the edge (via the live camera/zoom) before
/// handing an event to the core, and back again when applying decisions.
#[derive(Clone, Debug, PartialEq)]
pub struct TouchInput {
    pub time_ms: u32,
    pub slot: TouchSlot,
    pub kind: TouchKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TouchKind {
    Down {
        location: Point<f64, Logical>,
        /// The first finger landed on a client surface (`OnWindow` context).
        app_owns_hit: bool,
    },
    Motion {
        location: Point<f64, Logical>,
    },
    Up,
}

/// What the recognizer decided a single event (or the deadline) implies. The
/// adapter performs the compositor-bound half — coordinate conversion, camera
/// mutation, action dispatch, window-grab handoff, holdback delivery — the core
/// only decides *that* and *which kind*.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// Deliver the current event to the app with its captured focus
    /// (`handle.down`/`motion` with focus, `handle.up`).
    Forward,
    /// Deliver the current event to the handle with no app focus — the gesture
    /// engine consumes it (`handle.down`/`motion` with `None`).
    Consume,
    /// Withhold the current event in the holdback buffer (a `Down` also arms the
    /// flush deadline).
    Hold,
    /// Deliver the withheld buffer in order (the in-grab lift-flush path).
    Flush,
    /// Drop the withheld buffer unsent — a gesture claimed the sequence.
    Discard,
    /// Escalation: replay a no-op motion on every still-app-forwarded slot, then
    /// cancel the app's touch sequence so it sees no dangling points.
    CancelAppSequence,
    /// Exit fullscreen on the touch output before a system gesture that ends it
    /// (the adapter no-ops when nothing is fullscreen there).
    PreExitFullscreen,
    /// Pan the viewport by this screen-space centroid delta. The adapter scales it
    /// by `touch_speed` and the live zoom and calls `drift_pan_on`.
    Pan(Point<f64, Logical>),
    /// Zoom by this spread ratio anchored at this screen position. The adapter
    /// applies `zoom_touch_speed`, clamps to the zoom range, and moves the camera.
    Zoom {
        scale: f64,
        anchor: Point<f64, Logical>,
    },
    /// Fire a resolved one-shot threshold action (navigate swipe / pinch-in/out).
    FireThreshold(Action),
    /// Hand the translation drag to a window move/resize grab of this kind; the
    /// adapter resolves the window under the finger and the cluster snapshot.
    StartWindowGrab {
        action: ContinuousAction,
        cluster: bool,
    },
    /// Last-finger-up momentum coast.
    Momentum,
    /// A clean tap fired: raise+focus the window under `focus_at` (screen-space),
    /// record `set_last_tap` as the last 3-finger tap, then act on `outcome`.
    Tap {
        focus_at: Point<f64, Logical>,
        set_last_tap: Option<u32>,
        outcome: TapOutcome,
    },
    /// Tear down this grab (`handle.unset_grab`).
    UnsetGrab,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TapOutcome {
    /// Execute this resolved action now.
    Fire(Action),
    /// Defer a single-tap center until the double-tap window elapses.
    DeferCenter { delay_ms: u32 },
    /// Tap passed the gates but its tier binds nothing for this branch.
    None,
}

/// The clock-free, compositor-free multi-finger touch-gesture classifier. It sees
/// only screen-space positions, event times, and the resolved `[touch]` config,
/// and emits [`Decision`]s the adapter carries out. Every constant, comparison,
/// latch and ordering is preserved from the original in-grab recognizer.
pub struct TouchRecognizer {
    /// Screen positions of the currently-down slots.
    points: HashMap<TouchSlot, Point<f64, Logical>>,
    /// The first finger landed on a client surface — gates the escalation cancel
    /// that revokes the app's forwarded touches when a system gesture takes over.
    app_owns: bool,
    /// Binding context for config lookups, fixed from the first finger's focus.
    context: BindingContext,
    /// Config-resolved behavior for `max_fingers` + `context`, re-looked-up each
    /// time the finger count grows.
    plan: Plan,
    /// High-water mark of simultaneous fingers — the count the plan resolves for.
    max_fingers: usize,
    /// App touch sequence revoked once on escalation to a system gesture.
    system_cancelled: bool,
    /// A finger lifted while the sequence still belonged to the app (unbound
    /// tier). A real multi-finger gesture plants every finger before lifting any,
    /// so this marks the sequence typing-like: no tier may claim it anymore, and
    /// nothing is withheld — every event forwards.
    claims_blocked: bool,
    /// Some higher finger-count tier binds something, so a forming gesture could
    /// still claim this app-owned sequence — gates the holdback.
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

impl TouchRecognizer {
    pub fn new(px_per_mm: f64) -> Self {
        Self {
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

    pub fn finger_count(&self) -> usize {
        self.points.len()
    }

    /// Screen position last recorded for `slot`, for the adapter's escalation
    /// replay (canvas conversion happens adapter-side).
    pub fn screen_pos(&self, slot: TouchSlot) -> Option<Point<f64, Logical>> {
        self.points.get(&slot).copied()
    }

    /// Positions of the down slots in a deterministic (slot-sorted) order. The
    /// centroid/spread reductions sum over this, so their float result — and
    /// every gate that reads them — is reproducible; summing in `HashMap` order
    /// would vary per run at the ULP level.
    fn sorted_positions(&self) -> Vec<Point<f64, Logical>> {
        let mut entries: Vec<(i32, Point<f64, Logical>)> = self
            .points
            .iter()
            .map(|(slot, pos)| (i32::from(*slot), *pos))
            .collect();
        entries.sort_by_key(|(id, _)| *id);
        entries.into_iter().map(|(_, pos)| pos).collect()
    }

    pub fn centroid(&self) -> Point<f64, Logical> {
        let n = self.points.len();
        if n == 0 {
            return Point::from((0.0, 0.0));
        }
        let sum = self
            .sorted_positions()
            .into_iter()
            .fold(Point::from((0.0, 0.0)), |acc, p| acc + p);
        Point::from((sum.x / n as f64, sum.y / n as f64))
    }

    fn spread(&self, centroid: Point<f64, Logical>) -> f64 {
        let n = self.points.len();
        if n < 2 {
            return 0.0;
        }
        let sum: f64 = self
            .sorted_positions()
            .into_iter()
            .map(|p| {
                let dx = p.x - centroid.x;
                let dy = p.y - centroid.y;
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

    /// Resolve the `[touch]` bindings for the current finger count + context into a
    /// `Plan`. Re-run whenever `max_fingers` grows (a finger-count tier crossing).
    fn resolve_plan(&self, cfg: &Config) -> Plan {
        // Clamp the lookup tier to the max bindable finger count: a stray 6th+
        // contact resolves as the 5-finger tier (which navigates by default) rather
        // than an empty plan that would forward and abort the gesture. The true
        // `max_fingers` is left intact for tier-crossing and `all_fingers_down`.
        self.resolve_plan_at(cfg, (self.max_fingers as u32).min(5))
    }

    /// Whether a forming gesture at a higher finger-count tier could still claim
    /// this app-owned sequence; when nothing above can claim, events forward with
    /// zero holdback latency.
    fn any_higher_tier_bound(&self, cfg: &Config) -> bool {
        let cur = (self.max_fingers as u32).min(5);
        ((cur + 1)..=5).any(|n| !self.resolve_plan_at(cfg, n).is_empty())
    }

    fn resolve_plan_at(&self, cfg: &Config, n: u32) -> Plan {
        let cx = self.context;
        let continuous = |t: GestureTrigger| match cfg.touch_lookup(&t, cx) {
            Some(GestureConfigEntry::Continuous(a)) => Some(a.clone()),
            _ => None,
        };
        let threshold = |t: GestureTrigger| match cfg.touch_lookup(&t, cx) {
            Some(GestureConfigEntry::Threshold(a)) => Some(a.clone()),
            _ => None,
        };
        Plan {
            swipe: cfg
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

    /// `CenterNearest` derives its direction from the accumulated swipe vector; a
    /// `Fixed` action ignores it. Resolving here keeps the direction — and thus the
    /// whole navigate decision — in the core.
    fn resolve_threshold(&self, action: ThresholdAction) -> Action {
        match action {
            ThresholdAction::CenterNearest => {
                Action::CenterNearest(direction_from_vector(self.nav_cumulative))
            }
            ThresholdAction::Fixed(a) => a,
        }
    }

    /// The threshold action for a fired swipe: a per-direction override for the
    /// four cardinals if bound, else the base swipe binding.
    fn swipe_threshold_for(&self, dir: driftwm::config::Direction) -> Option<ThresholdAction> {
        use driftwm::config::Direction;
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

    /// The single entry point: dispatch one event to the per-kind handler and
    /// return the decisions it produced. Both the production adapter and the
    /// tests drive the state machine exclusively through here.
    pub fn process(
        &mut self,
        cfg: &Config,
        thresholds: &GestureThresholds,
        input: &TouchInput,
        last_three_finger_tap: Option<u32>,
        holdback_active: bool,
    ) -> Vec<Decision> {
        match input.kind {
            TouchKind::Down {
                location,
                app_owns_hit,
            } => self.down(
                cfg,
                input.slot,
                location,
                app_owns_hit,
                input.time_ms,
                last_three_finger_tap,
            ),
            TouchKind::Motion { location } => self.motion(
                thresholds,
                input.slot,
                location,
                input.time_ms,
                holdback_active,
            ),
            TouchKind::Up => self.up(
                input.slot,
                input.time_ms,
                holdback_active,
                last_three_finger_tap,
            ),
        }
    }

    fn down(
        &mut self,
        cfg: &Config,
        slot: TouchSlot,
        screen: Point<f64, Logical>,
        app_owns_hit: bool,
        time: u32,
        last_three_finger_tap: Option<u32>,
    ) -> Vec<Decision> {
        let mut out = Vec::new();
        let prev_max = self.max_fingers;
        self.points.insert(slot, screen);
        self.max_fingers = self.max_fingers.max(self.points.len());

        // The first finger fixes the gesture's binding context from its focus: on a
        // surface → app content (`OnWindow`, forwarded unless a system gesture is
        // bound), on empty canvas → viewport gesture (`OnCanvas`). `app_owns` comes
        // from the same focus so they can't disagree. A recent tap arms this touch
        // for a double-tap-drag move. Later fingers don't flip either, so a stray
        // contact can't strand an in-progress gesture.
        if self.points.len() == 1 {
            self.first_down_time = time;
            self.app_owns = app_owns_hit;
            self.context = if app_owns_hit {
                BindingContext::OnWindow
            } else {
                BindingContext::OnCanvas
            };
            self.armed_for_move =
                last_three_finger_tap.is_some_and(|t| time.saturating_sub(t) < DOUBLE_TAP_MS);
        } else {
            // Landing stagger, for tuning `HOLDBACK_MS` against real hardware.
            tracing::debug!(
                "touch stagger: finger {} at +{}ms",
                self.points.len(),
                time.saturating_sub(self.first_down_time)
            );
        }

        // Re-resolve the config plan whenever the finger count grows into a new tier.
        if self.max_fingers != prev_max {
            self.plan = self.resolve_plan(cfg);
            self.higher_tier_bound = self.any_higher_tier_bound(cfg);
        }

        // An early lift pinned the sequence to the app, whatever tier the finger
        // count reaches.
        if self.claims_blocked {
            out.push(Decision::Forward);
            return out;
        }

        if self.plan.is_empty() {
            // Unbound → the app's, but withhold events while a higher tier could
            // still claim the sequence.
            if self.higher_tier_bound {
                out.push(Decision::Hold);
            } else {
                out.push(Decision::Forward);
            }
        } else {
            // Anything still withheld is dropped unsent; fingers already delivered
            // are revoked by the escalation cancel below.
            out.push(Decision::Discard);
            if self.app_owns && !self.system_cancelled {
                out.push(Decision::CancelAppSequence);
                self.system_cancelled = true;
            }
            out.push(Decision::Consume);

            // Re-arm the gesture at start and at each finger-count tier crossing (into
            // 3-finger system gestures, into 4-finger navigation), so a clean tap stays
            // distinguishable from a drag and the navigation recognizer measures from a
            // fresh baseline.
            let crossed_system = prev_max < 3 && self.max_fingers >= 3;
            let crossed_nav = prev_max < 4 && self.max_fingers >= 4;
            // Exit fullscreen before a system gesture that ends it, so pan/zoom acts on
            // the restored canvas instead of sliding the parked window off its camera
            // origin. Gated on the tier actually binding something that ends fullscreen
            // (an unbound touch leaves it alone); both crossings are checked since a
            // 3-finger tier can preserve fullscreen while the 4-finger tier doesn't.
            if (crossed_system || crossed_nav) && self.plan.ends_fullscreen() {
                out.push(Decision::PreExitFullscreen);
            }
            if self.points.len() == 1 || crossed_system || crossed_nav {
                self.active = false;
                self.zoom_engaged = false;
                self.tap_start_time = time;
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
        out
    }

    fn up(
        &mut self,
        slot: TouchSlot,
        time: u32,
        holdback_active: bool,
        last_three_finger_tap: Option<u32>,
    ) -> Vec<Decision> {
        let mut out = Vec::new();
        let was_present = self.points.contains_key(&slot);

        // A lift while the sequence still belongs to the app pins it there.
        if was_present && self.plan.is_empty() {
            self.claims_blocked = true;
        }

        // A lift also flushes any withheld events right away — typing contacts are
        // short, so rolled keys deliver at first lift, not at the deadline.
        if was_present && holdback_active {
            out.push(Decision::Hold);
            out.push(Decision::Flush);
            self.points.remove(&slot);
            if self.points.is_empty() {
                out.push(Decision::UnsetGrab);
            } else {
                self.rebaseline();
            }
            return out;
        }

        out.push(Decision::Forward);
        self.points.remove(&slot);

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
                out.push(Decision::Momentum);
            }
            if was_present && !self.claims_blocked {
                self.detect_tap(time, last_three_finger_tap, &mut out);
            }
            out.push(Decision::UnsetGrab);
        } else {
            self.rebaseline();
        }
        out
    }

    fn motion(
        &mut self,
        thresholds: &GestureThresholds,
        slot: TouchSlot,
        screen: Point<f64, Logical>,
        time: u32,
        holdback_active: bool,
    ) -> Vec<Decision> {
        match self.points.get_mut(&slot) {
            Some(pos) => *pos = screen,
            None => return vec![Decision::Consume],
        }

        if holdback_active {
            return vec![Decision::Hold];
        }

        // Unbound gesture (or one an early lift pinned to the app) → forward.
        if self.claims_blocked || self.plan.is_empty() {
            return vec![Decision::Forward];
        }
        let mut out = vec![Decision::Consume];

        let centroid = self.centroid();

        // Arbitrated one-shot engine: a threshold swipe and/or pinch-in/out measured
        // from the gesture's rest baseline — fire the dominant one. No dead zone sits
        // in front of it (that double threshold made a deliberate pinch barely
        // register). A tap/doubletap-only plan tracks nothing here; it fires on up.
        if !self.plan.simultaneous() {
            if self.plan.translation_threshold() || self.plan.scale_threshold() {
                self.ever_active = true;
                self.apply_navigate(thresholds, centroid, &mut out);
            }
            return out;
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
                return out;
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
                let held = time.saturating_sub(self.tap_start_time) >= HOLD_MS;
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
                if let Some(action) = preempt {
                    out.push(Decision::StartWindowGrab { action, cluster });
                }
            }
            return out;
        }

        if matches!(
            self.plan.swipe,
            Some(GestureConfigEntry::Continuous(
                ContinuousAction::PanViewport
            ))
        ) {
            self.apply_pan(centroid, &mut out);
        }
        if self.plan.scale_continuous() && self.points.len() >= 2 {
            let cur_spread = self.spread(centroid);
            // Engage zoom once the spread clears both the ratio slop and the mm
            // floor for a couple of frames (so a pan's lone jitter spike can't latch
            // it), consuming the change so there's no jump on the first zoomed frame.
            // Needs the full finger set — a dropped contact collapses the spread past
            // both.
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
                self.apply_zoom(centroid, &mut out);
            }
        }
        out
    }

    fn apply_pan(&mut self, centroid: Point<f64, Logical>, out: &mut Vec<Decision>) {
        let centroid_delta = centroid - self.last_centroid;
        out.push(Decision::Pan(centroid_delta));
        self.last_centroid = centroid;
    }

    fn apply_zoom(&mut self, centroid: Point<f64, Logical>, out: &mut Vec<Decision>) {
        if self.points.len() < 2 || self.last_spread <= 1.0 {
            return;
        }
        let spread = self.spread(centroid);
        let scale = spread / self.last_spread;
        // The per-frame deadzone stays here because it gates the baseline advance;
        // the adapter applies `zoom_touch_speed`, clamping and the camera anchor.
        if (scale - 1.0).abs() > ZOOM_DEADZONE {
            out.push(Decision::Zoom {
                scale,
                anchor: centroid,
            });
            self.last_spread = spread;
        }
    }

    fn apply_navigate(
        &mut self,
        thresholds: &GestureThresholds,
        centroid: Point<f64, Logical>,
        out: &mut Vec<Decision>,
    ) {
        // Inverted, like the trackpad swipe: drag content right → reveal left.
        let centroid_delta = centroid - self.last_centroid;
        self.nav_cumulative += Point::from((-centroid_delta.x, -centroid_delta.y));
        self.last_centroid = centroid;

        if self.nav_fired_swipe || self.nav_fired_pinch {
            return;
        }

        let th = thresholds;
        let swipe_dist = (self.nav_cumulative.x.powi(2) + self.nav_cumulative.y.powi(2)).sqrt();
        let swipe_threshold = NAV_SWIPE_MM * self.px_per_mm;
        let swipe_progress = swipe_dist / swipe_threshold;

        // Pinch progress as a fraction of the in/out margin: a pure swipe's natural
        // splay stays well below 1.0, a deliberate pinch climbs past it.
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

        // Swipe and pinch are mutually exclusive; whichever is further past its own
        // threshold claims the gesture. Pinch wins ties, and a developing pinch
        // (past `SWIPE_BLOCK_PINCH`) blocks the swipe outright — a pinch-in contracts
        // slowly while the hand drifts the centroid, so otherwise the small swipe
        // threshold steals it before the pinch completes.
        if effective_pinch >= 1.0 && effective_pinch >= swipe_progress {
            let action = if scale < 1.0 {
                self.plan.pinch_in.clone()
            } else {
                self.plan.pinch_out.clone()
            };
            if let Some(a) = action {
                self.nav_fired_pinch = true;
                out.push(Decision::FireThreshold(self.resolve_threshold(a)));
            }
        } else if swipe_progress >= 1.0 && effective_pinch < SWIPE_BLOCK_PINCH {
            let dir = direction_from_vector(self.nav_cumulative);
            if let Some(a) = self.swipe_threshold_for(dir) {
                self.nav_fired_swipe = true;
                out.push(Decision::FireThreshold(self.resolve_threshold(a)));
            }
        }
    }

    /// On last-finger-up, decide the resolved tap (single) / doubletap (double)
    /// outcome for a clean tap. A tap is short, never passed the dead zone, and (via
    /// the escalation cancel) no longer belongs to an app, so it acts on the tapped
    /// window regardless of what's under it.
    fn detect_tap(
        &mut self,
        time: u32,
        last_three_finger_tap: Option<u32>,
        out: &mut Vec<Decision>,
    ) {
        if self.ever_active || (self.plan.tap.is_none() && self.plan.doubletap.is_none()) {
            return;
        }
        if time.saturating_sub(self.tap_start_time) > TAP_MAX_MS {
            return;
        }
        let double = last_three_finger_tap.is_some_and(|t| time.saturating_sub(t) < DOUBLE_TAP_MS);
        let focus_at = self.start_centroid;
        if double {
            let outcome = match self.plan.doubletap.clone() {
                Some(action) => TapOutcome::Fire(self.resolve_threshold(action)),
                None => TapOutcome::None,
            };
            out.push(Decision::Tap {
                focus_at,
                set_last_tap: None,
                outcome,
            });
        } else {
            let outcome = match self.plan.tap.clone() {
                // A deferred center avoids flashing a center before a follow-up
                // double-tap (fit) or double-tap-drag (move); a fresh interaction
                // cancels it. Specific to center — other tap actions fire now.
                Some(ThresholdAction::Fixed(Action::CenterWindow)) => TapOutcome::DeferCenter {
                    delay_ms: DOUBLE_TAP_MS,
                },
                Some(action) => TapOutcome::Fire(self.resolve_threshold(action)),
                None => TapOutcome::None,
            };
            out.push(Decision::Tap {
                focus_at,
                set_last_tap: Some(time),
                outcome,
            });
        }
    }

    /// Test-only stand-in for the calloop holdback deadline. Production flushes
    /// via `flush_touch_holdback`; its timer closure holds only `&mut DriftWm`
    /// and can't reach this grab without the reentry the design forbids, so the
    /// recognizer keeps no deadline state and just says to deliver the buffer.
    #[cfg(test)]
    pub fn deadline_elapsed(&self) -> Vec<Decision> {
        vec![Decision::Flush]
    }
}

#[cfg(test)]
mod tests;
