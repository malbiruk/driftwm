//! `[input.trackpad] scroll_liftoff_damping`: finger scroll is capped against
//! the sliding average of the deltas before it, and a lift parts with a
//! deliberate fraction of that average ahead of the stop.
//! Lifting fingers shifts the contact centroid, and some pads report that as a
//! run of accelerating deltas; a client reads those as a fast flick and keeps
//! scrolling after the gesture ended. Each scenario asserts what the client
//! received, never the window that decided it.
//!
//! The window claims `mod+trackpad-scroll` so the scroll takes the ordinary
//! unbound path to the app instead of panning the canvas — the cap sits on
//! that path, and the claim is the shortest way to reach it.

use smithay::desktop::Window;
use smithay::utils::{Logical, Point};

use super::client::ClientId;
use super::input_backend::{
    FakeDevice, key_press, pointer_to, trackpad_scroll_axes_at, trackpad_scroll_lift_at,
    wheel_notch_down,
};
use super::{Fixture, config, map_window, window_by_app_id};

/// Super is the default `mod_key`, so it is what a `mod+…` claim needs held.
const KEY_LEFTMETA: u32 = 125;

/// Event-time spacing between the frames of one gesture, in ms — a trackpad
/// reports at roughly this rate, and it is well inside the idle threshold that
/// ends a gesture.
const FRAME_MS: u32 = 4;

/// Silence long enough to end a gesture: the cap treats a window older than
/// `MOMENTUM_LAUNCH_DELAY` (50 ms) as a gesture the user already gave up on.
const PAUSE_MS: u32 = 60;

/// Drag two fingers down the trackpad by `amount` px, `FRAME_MS` after the
/// previous frame. Timestamps are pinned throughout this file: `next_time` is
/// a process-global counter, so two frames issued back to back can land an
/// arbitrary distance apart once other tests run in parallel — past the idle
/// threshold that would read as the fingers having stopped.
fn scroll_v(f: &mut Fixture, clock: &mut u32, amount: f64) {
    *clock += FRAME_MS;
    trackpad_scroll_axes_at(f, &FakeDevice::touchpad(), (None, Some(amount)), *clock);
}

/// A sideways frame, on the horizontal axis alone — the shape libinput reports
/// when a gesture carries no vertical motion in that event.
fn scroll_h(f: &mut Fixture, clock: &mut u32, amount: f64) {
    *clock += FRAME_MS;
    trackpad_scroll_axes_at(f, &FakeDevice::touchpad(), (Some(amount), None), *clock);
}

/// Lift both fingers, `FRAME_MS` after the previous frame.
fn lift(f: &mut Fixture, clock: &mut u32) {
    *clock += FRAME_MS;
    trackpad_scroll_lift_at(f, &FakeDevice::touchpad(), *clock);
}

/// A gesture's own clock. Starts high enough that a `wrapping_sub` in the cap
/// is never measuring against zero.
fn gesture_clock() -> u32 {
    10_000
}

/// The horizontal scroll amounts the client has received so far.
fn client_axes_h(f: &mut Fixture, id: ClientId) -> Vec<f64> {
    f.double_roundtrip(id);
    f.client(id).state.pointer_axes_h.clone()
}

/// `trackpad_scroll` reports 15 px, so the cap lands on 16.5 and the parting
/// delta on 4.5 — both distinct from the frames themselves.
const LIFTOFF: &str = r#"
[input.trackpad]
scroll_liftoff_damping = true

[[window_rules]]
app_id = "app"
pass_mouse = ["mod+trackpad-scroll", "mod+wheel-scroll"]
"#;

/// The vertical scroll amounts the client has received so far.
fn client_axes(f: &mut Fixture, id: ClientId) -> Vec<f64> {
    f.double_roundtrip(id);
    f.client(id).state.pointer_axes.clone()
}

/// A mapped 400x300 window with the pointer resting on it and `mod` held.
fn window_under_pointer(f: &mut Fixture) -> (ClientId, Window) {
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(f, id, "app", (400, 300));
    let window = window_by_app_id(f, "app").unwrap();
    f.state().with_output_state(|os| {
        os.camera_target = None;
        os.zoom_target = None;
    });
    let loc = f.state().stage.position_of(&window).expect("staged");
    let center = Point::<f64, Logical>::from((
        loc.x as f64 + window.geometry().size.w as f64 / 2.0,
        loc.y as f64 + window.geometry().size.h as f64 / 2.0,
    ));
    key_press(f, KEY_LEFTMETA);
    pointer_to(f, &FakeDevice::mouse(), center);
    (id, window)
}

/// A frame far faster than the gesture around it reaches the app at the cap,
/// not at its own size: that is the parting spike a pad produces as contact
/// breaks, and a client would read it as a flick.
#[test]
fn a_spike_reaches_the_client_capped_to_the_gesture_speed() {
    let mut f = Fixture::with_config(config(LIFTOFF));
    let (id, _) = window_under_pointer(&mut f);

    let clock = &mut gesture_clock();
    for _ in 0..4 {
        scroll_v(&mut f, clock, 15.0);
    }
    scroll_v(&mut f, clock, 150.0);

    assert_eq!(
        client_axes(&mut f, id),
        vec![15.0, 15.0, 15.0, 15.0, 16.5],
        "the spike arrives at the cap, the rest untouched"
    );
}

/// The lift sends a delta of its own ahead of the stop — half the gesture's
/// average here — so the speed the client flings from is the gesture's, not
/// whatever the pad reported last.
#[test]
fn a_lift_parts_with_a_fraction_of_the_gesture_speed() {
    let mut f = Fixture::with_config(config(LIFTOFF));
    let (id, _) = window_under_pointer(&mut f);

    let clock = &mut gesture_clock();
    for _ in 0..4 {
        scroll_v(&mut f, clock, 15.0);
    }
    lift(&mut f, clock);

    assert_eq!(
        client_axes(&mut f, id),
        vec![15.0, 15.0, 15.0, 15.0, 4.5],
        "the parting delta is half the sliding average and precedes the stop"
    );
}

/// The cap needs a few samples to describe the gesture. Below that the frames
/// pass as they are, so the opening of a gesture is never squashed.
#[test]
fn the_opening_frames_of_a_gesture_pass_uncapped() {
    let mut f = Fixture::with_config(config(LIFTOFF));
    let (id, _) = window_under_pointer(&mut f);

    let clock = &mut gesture_clock();
    scroll_v(&mut f, clock, 15.0);
    scroll_v(&mut f, clock, 150.0);

    assert_eq!(
        client_axes(&mut f, id),
        vec![15.0, 150.0],
        "with two samples there is no average worth capping against"
    );
}

/// A wheel has no contact to break, so neither the cap nor the parting delta
/// applies to it even with the option on.
#[test]
fn a_wheel_notch_passes_untouched() {
    let mut f = Fixture::with_config(config(LIFTOFF));
    let (id, _) = window_under_pointer(&mut f);

    wheel_notch_down(&mut f, &FakeDevice::mouse());

    assert_eq!(
        client_axes(&mut f, id),
        vec![15.0],
        "wheel scroll reaches the app as it arrives"
    );
}

/// A pause with the fingers resting on the pad ends the gesture. libinput
/// reports nothing while they rest, so without an idle bound the window would
/// hold the pre-pause deltas for as long as the user cares to read the page —
/// and the lift would hand the client a fling it deliberately gave up.
#[test]
fn a_pause_before_the_lift_parts_with_nothing() {
    let mut f = Fixture::with_config(config(LIFTOFF));
    let (id, _) = window_under_pointer(&mut f);

    let clock = &mut gesture_clock();
    for _ in 0..4 {
        scroll_v(&mut f, clock, 15.0);
    }
    *clock += PAUSE_MS;
    lift(&mut f, clock);

    assert_eq!(
        client_axes(&mut f, id),
        vec![15.0, 15.0, 15.0, 15.0],
        "the stop arrives on its own — nothing to fling from after the pause"
    );
}

/// The same idle bound from the other side: a flick after a deliberate pause
/// is the gesture the user means, and it must not be squashed against the slow
/// average from before the pause.
#[test]
fn a_flick_after_a_pause_is_not_capped_against_it() {
    let mut f = Fixture::with_config(config(LIFTOFF));
    let (id, _) = window_under_pointer(&mut f);

    let clock = &mut gesture_clock();
    for _ in 0..4 {
        scroll_v(&mut f, clock, 15.0);
    }
    *clock += PAUSE_MS;
    scroll_v(&mut f, clock, 150.0);

    assert_eq!(
        client_axes(&mut f, id),
        vec![15.0, 15.0, 15.0, 15.0, 150.0],
        "the flick opens a fresh gesture with no average to cap it"
    );
}

/// A sideways spike during a vertical gesture is still a spike. The burst a pad
/// emits as contact breaks shifts the centroid in two dimensions, so part of it
/// lands on an axis the gesture never touched — and an axis with no samples of
/// its own measures against the speed the fingers did have.
#[test]
fn a_sideways_spike_is_capped_against_the_gesture_speed() {
    let mut f = Fixture::with_config(config(LIFTOFF));
    let (id, _) = window_under_pointer(&mut f);

    let clock = &mut gesture_clock();
    for _ in 0..4 {
        scroll_v(&mut f, clock, 15.0);
    }
    scroll_h(&mut f, clock, 150.0);

    assert_eq!(
        client_axes_h(&mut f, id),
        vec![16.5],
        "the sideways half of the burst is capped like the vertical half"
    );
}

/// Deliberate sideways motion is not. Each axis averages over the events that
/// actually carried it, so the vertical frames around a sideways scroll do not
/// drag its average toward zero and cap it against almost nothing.
#[test]
fn a_sideways_scroll_is_not_diluted_by_the_vertical_frames_around_it() {
    let mut f = Fixture::with_config(config(LIFTOFF));
    let (id, _) = window_under_pointer(&mut f);

    let clock = &mut gesture_clock();
    for _ in 0..4 {
        scroll_v(&mut f, clock, 15.0);
    }
    // Two sideways frames of the same size as the vertical ones: ordinary
    // motion, nothing like a spike. Counting the vertical frames as horizontal
    // zeros would put the average at 15/5 and cap the second one at 3.3.
    scroll_h(&mut f, clock, 15.0);
    scroll_h(&mut f, clock, 15.0);

    assert_eq!(
        client_axes_h(&mut f, id),
        vec![15.0, 15.0],
        "a sideways scroll arrives at its own size"
    );
}

/// driftwm's own fling is damped by the same cap. The canvas pan feeds
/// `drift_pan`'s velocity tracker, which is what the camera coasts from after a
/// lift, so a burst reaching it launches the canvas at a speed the fingers
/// never had. The cap sits above the fork between forwarding a scroll and
/// panning with it, so both halves see the same damped delta.
#[test]
fn the_canvas_flings_from_the_gesture_speed_not_the_burst() {
    let damped = canvas_launch_speed(true);
    let raw = canvas_launch_speed(false);

    assert!(
        damped < raw / 2.0,
        "the spike dominates the launch velocity undamped \
         ({raw:.0} px/s) and is capped out of it with the option on \
         ({damped:.0} px/s)"
    );
}

/// Pan the canvas with a finger scroll that ends in a spike, and report the
/// speed the camera would coast away at.
fn canvas_launch_speed(damping: bool) -> f64 {
    let cfg = format!(
        r#"
[input.trackpad]
scroll_liftoff_damping = {damping}
"#
    );
    let mut f = Fixture::with_config(config(&cfg));
    f.add_output(1, (1920, 1080));
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.zoom = 1.0;
        os.camera_target = None;
        os.zoom_target = None;
    });
    // Mid-viewport, and this fixture maps no windows: empty canvas under the
    // pointer with no modifier held, where plain `trackpad-scroll` is bound to
    // `pan-viewport` by default.
    pointer_to(&mut f, &FakeDevice::mouse(), Point::from((960.0, 540.0)));

    let clock = &mut gesture_clock();
    for _ in 0..4 {
        scroll_v(&mut f, clock, 15.0);
    }
    scroll_v(&mut f, clock, 150.0);
    lift(&mut f, clock);

    let v = f
        .state()
        .with_output_state(|os| os.momentum.velocity)
        .expect("an output to fling on");
    (v.x * v.x + v.y * v.y).sqrt()
}
