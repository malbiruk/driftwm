mod device_config;
mod hold;
mod pinch;
mod swipe;

use smithay::{
    desktop::Window,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Size},
};

use driftwm::config::{Action, Direction, ThresholdAction};
use driftwm::layout::snap::SnapState;

use crate::state::DriftWm;

/// Active gesture — decided at Begin, locked for the gesture's duration.
pub enum GestureState {
    /// Continuous swipe → pan viewport (with momentum via drift_pan).
    SwipePan,
    /// Double-tap+drag → move window via MoveSurfaceGrab on the pointer.
    SwipeMove,
    /// Swipe → resize window (continuous).
    SwipeResize {
        window: Window,
        edges: xdg_toplevel::ResizeEdge,
        initial_location: Point<i32, Logical>,
        initial_size: Size<i32, Logical>,
        last_size: Size<i32, Logical>,
        cumulative: Point<f64, Logical>,
        snap: SnapState,
        constraints: crate::grabs::SizeConstraints,
        cluster_resize: crate::state::ClusterResizeSnapshot,
    },
    /// Threshold swipe — accumulate delta, detect direction, fire once.
    SwipeThreshold {
        cumulative: Point<f64, Logical>,
        fired: bool,
        /// Per-direction overrides (from SwipeUp/Down/Left/Right config entries).
        up: Option<ThresholdAction>,
        down: Option<ThresholdAction>,
        left: Option<ThresholdAction>,
        right: Option<ThresholdAction>,
        /// 8-direction fallback from the Swipe trigger's threshold action.
        directional: Option<ThresholdAction>,
    },
    /// Continuous pinch → cursor-anchored zoom.
    PinchZoom { initial_zoom: f64 },
    /// Pinch forwarded to client (unbound in this context).
    PinchForward,
    /// Threshold pinch — pinch-in/out fire discrete actions.
    PinchThreshold {
        fired_in: bool,
        fired_out: bool,
        action_in: Option<Action>,
        action_out: Option<Action>,
    },
    /// Hold gesture — fires action on release.
    HoldAction { action: Action },
}

pub(crate) const DOUBLE_TAP_WINDOW_MS: u64 = 300;

impl DriftWm {
    pub(super) fn exit_fullscreen_for_gesture(&mut self) {
        self.gesture_exited_fullscreen = self.active_fullscreen().map(|fs| fs.window.clone());
        let pointer = self.seat.get_pointer().unwrap();
        let pos = pointer.current_location();
        self.exit_fullscreen_remap_pointer(pos);
    }

    /// Read camera/zoom from the pinned gesture output, falling back to active output.
    pub(super) fn gesture_camera_zoom(&self) -> (f64, Point<f64, Logical>) {
        match self.gesture_output {
            Some(ref o) => {
                let os = crate::state::output_state(o);
                (os.zoom, os.camera)
            }
            None => (self.zoom(), self.camera()),
        }
    }

    pub(super) fn cancel_animations(&mut self) {
        self.with_output_state(|os| {
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_center = None;
            os.momentum.stop();
        });
    }
}

/// Map a 2D vector to the nearest of 8 directions (4 cardinal + 4 diagonal).
/// Uses 45° octants: tan(22.5°) ≈ 0.4142 as the minor/major axis ratio threshold.
pub(crate) fn direction_from_vector(v: Point<f64, Logical>) -> Direction {
    let ax = v.x.abs();
    let ay = v.y.abs();
    let minor = ax.min(ay);
    let major = ax.max(ay);

    // If the minor axis is > 41.4% of the major axis, the vector is diagonal
    if major > 0.0 && minor > major * 0.4142 {
        match (v.x > 0.0, v.y > 0.0) {
            (true, true) => Direction::DownRight,
            (true, false) => Direction::UpRight,
            (false, true) => Direction::DownLeft,
            (false, false) => Direction::UpLeft,
        }
    } else if ax > ay {
        if v.x > 0.0 {
            Direction::Right
        } else {
            Direction::Left
        }
    } else if v.y > 0.0 {
        Direction::Down
    } else {
        Direction::Up
    }
}
