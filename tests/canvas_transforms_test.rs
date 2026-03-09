use std::time::{Duration, Instant};

use driftwm::canvas::{
    CanvasPos, MomentumState, ScreenPos,
    canvas_to_screen, screen_to_canvas,
};
use smithay::utils::Point;

// --- Coordinate transform round-trip tests (zoom=1.0) ---

#[test]
fn screen_to_canvas_and_back_with_zero_camera() {
    let screen = ScreenPos(Point::from((100.0, 200.0)));
    let camera = Point::from((0.0, 0.0));
    let canvas = screen_to_canvas(screen, camera, 1.0);
    let back = canvas_to_screen(canvas, camera, 1.0);
    assert_eq!(back.0.x, screen.0.x);
    assert_eq!(back.0.y, screen.0.y);
}

#[test]
fn screen_to_canvas_and_back_with_positive_camera() {
    let screen = ScreenPos(Point::from((50.0, 75.0)));
    let camera = Point::from((300.0, 400.0));
    let canvas = screen_to_canvas(screen, camera, 1.0);
    let back = canvas_to_screen(canvas, camera, 1.0);
    assert!((back.0.x - screen.0.x).abs() < 1e-10);
    assert!((back.0.y - screen.0.y).abs() < 1e-10);
}

#[test]
fn screen_to_canvas_and_back_with_negative_camera() {
    let screen = ScreenPos(Point::from((10.0, 20.0)));
    let camera = Point::from((-150.0, -250.0));
    let canvas = screen_to_canvas(screen, camera, 1.0);
    let back = canvas_to_screen(canvas, camera, 1.0);
    assert!((back.0.x - screen.0.x).abs() < 1e-10);
    assert!((back.0.y - screen.0.y).abs() < 1e-10);
}

#[test]
fn canvas_to_screen_and_back_with_positive_camera() {
    let canvas = CanvasPos(Point::from((500.0, 600.0)));
    let camera = Point::from((100.0, 200.0));
    let screen = canvas_to_screen(canvas, camera, 1.0);
    let back = screen_to_canvas(screen, camera, 1.0);
    assert!((back.0.x - canvas.0.x).abs() < 1e-10);
    assert!((back.0.y - canvas.0.y).abs() < 1e-10);
}

#[test]
fn screen_to_canvas_adds_camera_offset() {
    let screen = ScreenPos(Point::from((10.0, 20.0)));
    let camera = Point::from((100.0, 200.0));
    let canvas = screen_to_canvas(screen, camera, 1.0);
    assert_eq!(canvas.0.x, 110.0);
    assert_eq!(canvas.0.y, 220.0);
}

#[test]
fn canvas_to_screen_subtracts_camera_offset() {
    let canvas = CanvasPos(Point::from((110.0, 220.0)));
    let camera = Point::from((100.0, 200.0));
    let screen = canvas_to_screen(canvas, camera, 1.0);
    assert_eq!(screen.0.x, 10.0);
    assert_eq!(screen.0.y, 20.0);
}

// --- Zoom coordinate transform tests ---

#[test]
fn screen_to_canvas_with_zoom_half() {
    // screen=100, camera=0, zoom=0.5 → canvas = 100/0.5 + 0 = 200
    let screen = ScreenPos(Point::from((100.0, 50.0)));
    let camera = Point::from((0.0, 0.0));
    let canvas = screen_to_canvas(screen, camera, 0.5);
    assert!((canvas.0.x - 200.0).abs() < 1e-10);
    assert!((canvas.0.y - 100.0).abs() < 1e-10);
}

#[test]
fn canvas_to_screen_with_zoom_half() {
    // canvas=200, camera=0, zoom=0.5 → screen = (200-0)*0.5 = 100
    let canvas = CanvasPos(Point::from((200.0, 100.0)));
    let camera = Point::from((0.0, 0.0));
    let screen = canvas_to_screen(canvas, camera, 0.5);
    assert!((screen.0.x - 100.0).abs() < 1e-10);
    assert!((screen.0.y - 50.0).abs() < 1e-10);
}

#[test]
fn zoom_round_trip_with_camera_and_zoom() {
    let screen = ScreenPos(Point::from((300.0, 200.0)));
    let camera = Point::from((100.0, 50.0));
    let zoom = 0.7;
    let canvas = screen_to_canvas(screen, camera, zoom);
    let back = canvas_to_screen(canvas, camera, zoom);
    assert!((back.0.x - screen.0.x).abs() < 1e-10);
    assert!((back.0.y - screen.0.y).abs() < 1e-10);
}

#[test]
fn screen_to_canvas_zoom_one_equals_no_zoom() {
    let screen = ScreenPos(Point::from((50.0, 75.0)));
    let camera = Point::from((300.0, 400.0));
    let with_zoom = screen_to_canvas(screen, camera, 1.0);
    // At zoom=1: canvas = screen/1 + camera = screen + camera
    assert!((with_zoom.0.x - 350.0).abs() < 1e-10);
    assert!((with_zoom.0.y - 475.0).abs() < 1e-10);
}

// --- MomentumState tests ---

const DT_16MS: Duration = Duration::from_millis(16);

#[test]
fn momentum_tick_produces_delta_when_coasting() {
    let mut m = MomentumState::new(0.96);
    m.velocity = Point::from((600.0, 0.0)); // 600 px/sec
    m.coasting = true;
    let delta = m.tick(DT_16MS).expect("expected Some delta");
    // delta ≈ velocity * dt = 600 * 0.016 = 9.6
    assert!((delta.x - 600.0 * 0.016).abs() < 0.5, "delta ≈ v*dt");
    // velocity should have decayed
    assert!(m.velocity.x < 600.0, "velocity should decay after tick");
}

#[test]
fn momentum_tick_stops_below_threshold() {
    let mut m = MomentumState::new(0.96);
    // 10 px/sec is below the 15 px/sec stop threshold
    m.velocity = Point::from((7.0, 7.0));
    m.coasting = true;
    let result = m.tick(DT_16MS);
    assert!(result.is_none(), "tick should return None below threshold");
    assert_eq!(m.velocity.x, 0.0);
    assert_eq!(m.velocity.y, 0.0);
    assert!(!m.coasting);
}

#[test]
fn momentum_tick_returns_none_when_not_coasting() {
    let mut m = MomentumState::new(0.96);
    m.velocity = Point::from((1000.0, 0.0));
    // coasting is false by default
    let result = m.tick(DT_16MS);
    assert!(result.is_none());
}

#[test]
fn momentum_accumulate_prevents_tick() {
    let mut m = MomentumState::new(0.96);
    let now = Instant::now();
    m.accumulate(Point::from((5.0, 5.0)), now);
    // accumulate sets coasting=false, so tick returns None
    let result = m.tick(DT_16MS);
    assert!(result.is_none(), "tick during accumulation should return None");
}

#[test]
fn momentum_launch_enables_coasting() {
    let mut m = MomentumState::new(0.96);
    let now = Instant::now();
    // Simulate several scroll events over 40ms
    for i in 0..4 {
        let t = now + Duration::from_millis(i * 10);
        m.accumulate(Point::from((5.0, 0.0)), t);
    }
    m.launch();
    assert!(m.coasting);
    // Velocity should be non-zero (displacement/time)
    assert!(m.velocity.x > 0.0, "launch should produce positive velocity from accumulated deltas");
    let delta = m.tick(DT_16MS);
    assert!(delta.is_some(), "tick after launch should produce delta");
}

#[test]
fn momentum_decays_monotonically_and_stops() {
    let mut m = MomentumState::new(0.96);
    m.velocity = Point::from((1200.0, 0.0)); // 1200 px/sec
    m.coasting = true;
    let mut prev_speed = 1200.0_f64;
    let mut ticked = false;
    for _ in 0..500 {
        match m.tick(DT_16MS) {
            Some(_) => {
                ticked = true;
                let speed = (m.velocity.x.powi(2) + m.velocity.y.powi(2)).sqrt();
                assert!(speed <= prev_speed + 1e-10, "speed should decrease monotonically");
                prev_speed = speed;
            }
            None => {
                assert!(ticked, "momentum must tick at least once before stopping");
                break;
            }
        }
    }
}

#[test]
fn momentum_velocity_tracker_launch() {
    let mut m = MomentumState::new(0.96);
    let now = Instant::now();
    // Push 5 samples at 10ms intervals, each with 10px displacement
    for i in 0..5 {
        let t = now + Duration::from_millis(i * 10);
        m.accumulate(Point::from((10.0, 0.0)), t);
    }
    m.launch();
    // 5 samples over 40ms, total displacement = 50px → velocity ≈ 1250 px/sec
    assert!((m.velocity.x - 1250.0).abs() < 50.0,
        "expected ~1250 px/sec, got {}", m.velocity.x);
}

#[test]
fn momentum_stop_zeroes_velocity() {
    let mut m = MomentumState::new(0.96);
    m.velocity = Point::from((500.0, 500.0));
    m.coasting = true;
    m.stop();
    assert_eq!(m.velocity.x, 0.0);
    assert_eq!(m.velocity.y, 0.0);
    assert!(!m.coasting);
}

#[test]
fn momentum_stop_causes_tick_to_return_none() {
    let mut m = MomentumState::new(0.96);
    m.velocity = Point::from((500.0, 500.0));
    m.coasting = true;
    m.stop();
    let result = m.tick(DT_16MS);
    assert!(result.is_none());
}

#[test]
fn momentum_frame_rate_independence() {
    // Same velocity should produce similar total displacement regardless of tick rate
    let make = || {
        let mut m = MomentumState::new(0.96);
        m.velocity = Point::from((1000.0, 0.0));
        m.coasting = true;
        m
    };

    // 60 Hz: 500 ticks at ~16.67ms
    let mut m60 = make();
    let mut total_60 = 0.0;
    let dt_60 = Duration::from_micros(16667);
    for _ in 0..500 {
        if let Some(d) = m60.tick(dt_60) {
            total_60 += d.x;
        } else {
            break;
        }
    }

    // 144 Hz: 1200 ticks at ~6.94ms
    let mut m144 = make();
    let mut total_144 = 0.0;
    let dt_144 = Duration::from_micros(6944);
    for _ in 0..1200 {
        if let Some(d) = m144.tick(dt_144) {
            total_144 += d.x;
        } else {
            break;
        }
    }

    let ratio = total_60 / total_144;
    assert!(
        (ratio - 1.0).abs() < 0.05,
        "60Hz total ({total_60:.1}) vs 144Hz total ({total_144:.1}) should be within 5%, ratio={ratio:.3}"
    );
}
