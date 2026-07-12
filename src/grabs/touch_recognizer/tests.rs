use std::collections::HashSet;

use proptest::prelude::*;
use smithay::backend::input::TouchSlot;
use smithay::utils::{Logical, Point};

use driftwm::config::{Action, ContextBindings, Direction};

use super::{Config, Decision, TapOutcome, TouchInput, TouchKind, TouchRecognizer};

/// Nested-backend fallback density (~96 dpi). Fixed so thresholds are concrete:
/// dead zone 8px, nav-swipe 60px, min spread / min pinch delta 12px.
const PX_PER_MM: f64 = 4.0;

fn slot(id: u32) -> TouchSlot {
    TouchSlot::from(Some(id))
}

fn pt(x: f64, y: f64) -> Point<f64, Logical> {
    Point::from((x, y))
}

fn down(s: u32, x: f64, y: f64, app_owns: bool, t: u32) -> TouchInput {
    TouchInput {
        time_ms: t,
        slot: slot(s),
        kind: TouchKind::Down {
            location: pt(x, y),
            app_owns_hit: app_owns,
        },
    }
}

fn motion(s: u32, x: f64, y: f64, t: u32) -> TouchInput {
    TouchInput {
        time_ms: t,
        slot: slot(s),
        kind: TouchKind::Motion { location: pt(x, y) },
    }
}

fn up(s: u32, t: u32) -> TouchInput {
    TouchInput {
        time_ms: t,
        slot: slot(s),
        kind: TouchKind::Up,
    }
}

fn cfg_default() -> Config {
    Config::default()
}

/// All `[touch]` tiers unbound — every gesture is the app's.
fn cfg_empty() -> Config {
    let mut cfg = Config::default();
    cfg.touch_gestures = ContextBindings::empty();
    cfg
}

/// Test double of the thin adapter: owns the recognizer plus the holdback buffer
/// and last-3-finger-tap state the real adapter keeps in `touch_state`. Drives
/// inputs, mirrors those two pieces of state from the emitted decisions (exactly
/// as the adapter's `hold_touch_event` / `discard` / `flush` / tap paths do), and
/// records the full decision stream.
struct Harness<'a> {
    core: TouchRecognizer,
    cfg: &'a Config,
    holdback: Option<Vec<TouchInput>>,
    last_tap: Option<u32>,
    decisions: Vec<Decision>,
}

impl<'a> Harness<'a> {
    fn new(cfg: &'a Config) -> Self {
        Self {
            core: TouchRecognizer::new(PX_PER_MM),
            cfg,
            holdback: None,
            last_tap: None,
            decisions: Vec::new(),
        }
    }

    fn feed(&mut self, input: &TouchInput) -> Vec<Decision> {
        let cfg = self.cfg;
        let holdback_active = self.holdback.is_some();
        let last_tap = self.last_tap;
        let decs = self.core.process(
            cfg,
            &cfg.gesture_thresholds,
            input,
            last_tap,
            holdback_active,
        );
        for d in &decs {
            match d {
                Decision::Hold => self
                    .holdback
                    .get_or_insert_with(Vec::new)
                    .push(input.clone()),
                Decision::Discard | Decision::Flush => self.holdback = None,
                Decision::Tap { set_last_tap, .. } => self.last_tap = *set_last_tap,
                _ => {}
            }
        }
        self.decisions.extend(decs.iter().cloned());
        decs
    }

    fn run(&mut self, inputs: &[TouchInput]) {
        for i in inputs {
            self.feed(i);
        }
    }
}

fn run_all(cfg: &Config, inputs: &[TouchInput]) -> Vec<Decision> {
    let mut h = Harness::new(cfg);
    h.run(inputs);
    h.decisions
}

fn count<F: Fn(&Decision) -> bool>(decs: &[Decision], pred: F) -> usize {
    decs.iter().filter(|d| pred(d)).count()
}

fn is_pan(d: &Decision) -> bool {
    matches!(d, Decision::Pan(_))
}
fn is_zoom(d: &Decision) -> bool {
    matches!(d, Decision::Zoom { .. })
}
fn is_fire(d: &Decision) -> bool {
    matches!(d, Decision::FireThreshold(_))
}

/// The decisions that mean a system gesture claimed the sequence — forbidden once
/// the sequence is pinned to the app (`claims_blocked`) or when nothing is bound.
fn is_gesture_claim(d: &Decision) -> bool {
    matches!(
        d,
        Decision::CancelAppSequence
            | Decision::FireThreshold(_)
            | Decision::Pan(_)
            | Decision::Zoom { .. }
            | Decision::StartWindowGrab { .. }
            | Decision::PreExitFullscreen
            | Decision::Tap { .. }
            | Decision::Momentum
            | Decision::Discard
    )
}

#[test]
fn two_finger_drag_on_canvas_pans() {
    let cfg = cfg_default();
    let seq = vec![
        down(0, 500.0, 500.0, false, 0),
        down(1, 600.0, 500.0, false, 10),
        motion(0, 500.0, 560.0, 20),
        motion(1, 600.0, 560.0, 25),
        motion(0, 500.0, 620.0, 30),
        motion(1, 600.0, 620.0, 35),
        motion(0, 500.0, 680.0, 40),
        motion(1, 600.0, 680.0, 45),
        up(0, 60),
        up(1, 65),
    ];
    let decs = run_all(&cfg, &seq);
    assert!(count(&decs, is_pan) >= 1, "expected pans, got {decs:?}");
    assert_eq!(count(&decs, is_zoom), 0, "constant spread must not zoom");
    assert_eq!(count(&decs, is_fire), 0);
}

#[test]
fn two_finger_spread_on_canvas_zooms() {
    let cfg = cfg_default();
    let seq = vec![
        down(0, 500.0, 500.0, false, 0),
        down(1, 700.0, 500.0, false, 10),
        motion(0, 460.0, 500.0, 20),
        motion(1, 740.0, 500.0, 25),
        motion(0, 420.0, 500.0, 30),
        motion(1, 780.0, 500.0, 35),
        motion(0, 380.0, 500.0, 40),
        motion(1, 820.0, 500.0, 45),
        motion(0, 340.0, 500.0, 50),
        motion(1, 860.0, 500.0, 55),
    ];
    let decs = run_all(&cfg, &seq);
    assert!(count(&decs, is_zoom) >= 1, "expected zooms, got {decs:?}");
    assert_eq!(count(&decs, is_fire), 0);
}

#[test]
fn four_finger_swipe_fires_exactly_one_threshold() {
    // Default `swipe = 4` fingers is the one-shot navigate (`center-nearest`); the
    // 3-finger tier pans continuously. Fingers move left → inverted nav vector
    // points right → `CenterNearest(Right)`.
    let cfg = cfg_default();
    let mut seq = vec![
        down(0, 400.0, 500.0, false, 0),
        down(1, 500.0, 500.0, false, 5),
        down(2, 600.0, 500.0, false, 10),
        down(3, 700.0, 500.0, false, 15),
    ];
    let mut t = 20;
    for step in 1..=6 {
        let dx = -(step as f64) * 40.0;
        for f in 0..4 {
            seq.push(motion(f, 400.0 + f as f64 * 100.0 + dx, 500.0, t));
            t += 2;
        }
    }
    let decs = run_all(&cfg, &seq);
    assert_eq!(
        count(&decs, is_fire),
        1,
        "one-shot navigate must fire exactly once, got {decs:?}"
    );
    assert!(
        decs.iter().any(|d| matches!(
            d,
            Decision::FireThreshold(Action::CenterNearest(Direction::Right))
        )),
        "expected CenterNearest(Right), got {decs:?}"
    );
}

#[test]
fn three_finger_tap_within_window_taps() {
    let cfg = cfg_default();
    let seq = vec![
        down(0, 500.0, 500.0, false, 0),
        down(1, 550.0, 500.0, false, 10),
        down(2, 600.0, 500.0, false, 20),
        up(0, 40),
        up(1, 50),
        up(2, 60),
    ];
    let decs = run_all(&cfg, &seq);
    // Default 3-finger tap is `center-window` → the deferred-center outcome.
    assert!(
        decs.iter().any(|d| matches!(
            d,
            Decision::Tap {
                outcome: TapOutcome::DeferCenter { .. },
                set_last_tap: Some(_),
                ..
            }
        )),
        "expected a deferred-center tap, got {decs:?}"
    );
    assert_eq!(count(&decs, is_pan), 0);
}

#[test]
fn slow_tap_past_tap_window_does_not_tap() {
    let cfg = cfg_default();
    // Same clean 3-finger tap but the lift lands well past TAP_MAX_MS (250ms).
    let seq = vec![
        down(0, 500.0, 500.0, false, 0),
        down(1, 550.0, 500.0, false, 10),
        down(2, 600.0, 500.0, false, 20),
        up(0, 400),
        up(1, 410),
        up(2, 420),
    ];
    let decs = run_all(&cfg, &seq);
    assert!(
        !decs.iter().any(|d| matches!(d, Decision::Tap { .. })),
        "a tap past the window must not fire, got {decs:?}"
    );
}

#[test]
fn holdback_then_claim_discards_and_cancels() {
    let cfg = cfg_default();
    let mut h = Harness::new(&cfg);
    // 1 finger on a surface: unbound tier, but a 3-finger tier binds → withhold.
    let d0 = h.feed(&down(0, 500.0, 500.0, true, 0));
    assert_eq!(d0, vec![Decision::Hold]);
    let d1 = h.feed(&down(1, 550.0, 500.0, true, 10));
    assert_eq!(d1, vec![Decision::Hold]);
    // Third finger reaches a bound tier → claim: drop the buffer, revoke the app.
    let d2 = h.feed(&down(2, 600.0, 500.0, true, 20));
    assert_eq!(d2.first(), Some(&Decision::Discard), "claim discards first");
    assert!(
        d2.contains(&Decision::CancelAppSequence),
        "app sequence must be cancelled, got {d2:?}"
    );
    assert!(d2.contains(&Decision::Consume));
}

#[test]
fn holdback_flushes_on_lift() {
    let cfg = cfg_default();
    let mut h = Harness::new(&cfg);
    h.feed(&down(0, 500.0, 500.0, true, 0)); // Hold
    // The finger lifts before any higher tier claims → flush to the app.
    let d = h.feed(&up(0, 20));
    assert!(d.contains(&Decision::Hold), "lift is buffered, got {d:?}");
    assert!(d.contains(&Decision::Flush), "lift flushes, got {d:?}");
    assert!(d.contains(&Decision::UnsetGrab));
}

#[test]
fn deadline_models_a_flush() {
    let core = TouchRecognizer::new(PX_PER_MM);
    assert_eq!(core.deadline_elapsed(), vec![Decision::Flush]);
}

/// A raw op tuple: (kind 0=down/1=motion/2=up, slot 0..5, x, y, app_owns, dt_ms).
type RawOp = (u8, u32, f64, f64, bool, u32);

fn arb_raw_op() -> impl Strategy<Value = RawOp> {
    (
        0u8..3,
        0u32..5,
        0.0f64..2000.0,
        0.0f64..2000.0,
        any::<bool>(),
        0u32..300,
    )
}

/// Turn raw ops into a slot-valid input sequence with monotone times: a slot must
/// be down before it can move or lift, a down on an already-down slot is dropped.
fn normalize(raws: Vec<RawOp>) -> Vec<TouchInput> {
    let mut active: HashSet<u32> = HashSet::new();
    let mut time = 0u32;
    let mut out = Vec::new();
    for (kind, s, x, y, app, dt) in raws {
        time = time.saturating_add(dt);
        match kind {
            0 => {
                if active.insert(s) {
                    out.push(down(s, x, y, app, time));
                }
            }
            1 => {
                if active.contains(&s) {
                    out.push(motion(s, x, y, time));
                }
            }
            _ => {
                if active.remove(&s) {
                    out.push(up(s, time));
                }
            }
        }
    }
    out
}

fn arb_sequence() -> impl Strategy<Value = Vec<TouchInput>> {
    proptest::collection::vec(arb_raw_op(), 0..60).prop_map(normalize)
}

/// Plant `n` fingers before any motion (so no finger-count tier crossing re-arms
/// the navigate engine mid-drag), move them, then lift — a single navigate episode.
fn arb_single_episode() -> impl Strategy<Value = Vec<TouchInput>> {
    (
        1usize..=5,
        any::<bool>(),
        proptest::collection::vec((0u32..5, 0.0f64..2000.0, 0.0f64..2000.0, 1u32..120), 0..40),
    )
        .prop_map(|(n, app, moves)| {
            let mut out = Vec::new();
            let mut t = 0u32;
            for i in 0..n as u32 {
                t += 5;
                out.push(down(i, 100.0 + i as f64 * 60.0, 100.0, app, t));
            }
            for (s, x, y, dt) in moves {
                let s = s % n as u32;
                t += dt;
                out.push(motion(s, x, y, t));
            }
            for i in 0..n as u32 {
                t += 5;
                out.push(up(i, t));
            }
            out
        })
}

/// Plant `n` fingers on one point and jitter each within ±1px for a bounded number
/// of frames: total centroid travel and spread change stay well under every gate
/// (dead zone 8px, min spread / delta 12px, nav-swipe 60px).
fn arb_dead_zone() -> impl Strategy<Value = Vec<TouchInput>> {
    (
        1usize..=5,
        any::<bool>(),
        200.0f64..1800.0,
        200.0f64..1800.0,
        proptest::collection::vec((0u32..5, -1.0f64..1.0, -1.0f64..1.0, 1u32..120), 0..8),
    )
        .prop_map(|(n, app, bx, by, moves)| {
            let mut out = Vec::new();
            let mut t = 0u32;
            for i in 0..n as u32 {
                t += 5;
                out.push(down(i, bx, by, app, t));
            }
            for (s, jx, jy, dt) in moves {
                let s = s % n as u32;
                t += dt;
                out.push(motion(s, bx + jx, by + jy, t));
            }
            for i in 0..n as u32 {
                t += 5;
                out.push(up(i, t));
            }
            out
        })
}

proptest! {
    /// 1. The arbitrated navigate engine fires at most one threshold per episode,
    ///    and its one-shot latches forbid firing both a swipe and a pinch.
    #[test]
    fn prop_one_shot_exclusivity(inputs in arb_single_episode()) {
        let cfg = cfg_default();
        let decs = run_all(&cfg, &inputs);
        prop_assert!(count(&decs, is_fire) <= 1, "fired more than once: {decs:?}");
    }

    /// 2. Once a finger lifts while the plan is empty (`claims_blocked`), no later
    ///    event in the sequence produces a gesture claim — only app forwarding.
    #[test]
    fn prop_claims_blocked_absorbs(tail in arb_sequence()) {
        let cfg = cfg_default();
        let mut h = Harness::new(&cfg);
        // Prefix guarantees claims_blocked on a state production actually
        // reaches: two fingers land on a surface (unbound tier, higher tier
        // bound → held), one lifts while still unbound, and one stays down so
        // the grab — and the blocked flag — persists into the tail. Prefix
        // slots 7/8 are outside the generator's 0..5 range, so the tail stays
        // slot-valid with slot 7 still held.
        h.feed(&down(7, 500.0, 500.0, true, 0));
        h.feed(&down(8, 520.0, 500.0, true, 5));
        h.feed(&up(8, 10));
        let boundary = h.decisions.len();
        let shifted: Vec<TouchInput> = tail
            .into_iter()
            .map(|mut i| { i.time_ms = i.time_ms.saturating_add(20); i })
            .collect();
        h.run(&shifted);
        let after = &h.decisions[boundary..];
        prop_assert!(
            !after.iter().any(is_gesture_claim),
            "gesture claim after claims_blocked: {after:?}"
        );
    }

    /// 3. A claim (Discard) makes the withheld buffer invisible: a Flush only ever
    ///    delivers a buffer that has held events since the last clear.
    #[test]
    fn prop_claimed_never_flushed(inputs in arb_sequence()) {
        let cfg = cfg_default();
        let mut h = Harness::new(&cfg);
        let mut pending = 0i32;
        for input in &inputs {
            for d in h.feed(input) {
                match d {
                    Decision::Hold => pending += 1,
                    Decision::Discard => pending = 0,
                    Decision::Flush => {
                        prop_assert!(pending > 0, "flushed a claimed/empty buffer");
                        pending = 0;
                    }
                    _ => {}
                }
            }
        }
    }

    /// 4. Below the gates: no viewport change and no navigate fire.
    #[test]
    fn prop_dead_zone_is_inert(inputs in arb_dead_zone()) {
        let cfg = cfg_default();
        let decs = run_all(&cfg, &inputs);
        prop_assert_eq!(count(&decs, is_pan), 0, "pan inside dead zone: {:?}", decs);
        prop_assert_eq!(count(&decs, is_zoom), 0, "zoom inside dead zone: {:?}", decs);
        prop_assert_eq!(count(&decs, is_fire), 0, "fire inside dead zone: {:?}", decs);
    }

    /// 5. With every tier unbound, every event forwards (or holds-then-flushes) —
    ///    nothing is ever claimed.
    #[test]
    fn prop_empty_plans_only_forward(inputs in arb_sequence()) {
        let cfg = cfg_empty();
        let decs = run_all(&cfg, &inputs);
        prop_assert!(
            !decs.iter().any(is_gesture_claim),
            "empty config produced a claim: {decs:?}"
        );
    }

    /// 6. The recognizer is deterministic: identical inputs → identical decisions.
    #[test]
    fn prop_deterministic(inputs in arb_sequence()) {
        let cfg = cfg_default();
        let a = run_all(&cfg, &inputs);
        let b = run_all(&cfg, &inputs);
        prop_assert_eq!(a, b);
    }
}
