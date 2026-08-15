/// Generates the 13 forwarding methods that every PointerGrab impl needs
/// but never customizes. Each grab only provides custom `motion()` and `button()`.
/// Assumes `self.start_data: GrabStartData<DriftWm>`.
macro_rules! forward_pointer_grab_methods {
    () => {
        fn relative_motion(
            &mut self,
            data: &mut crate::state::DriftWm,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, crate::state::DriftWm>,
            focus: Option<(
                <crate::state::DriftWm as smithay::input::SeatHandler>::PointerFocus,
                smithay::utils::Point<f64, smithay::utils::Logical>,
            )>,
            event: &smithay::input::pointer::RelativeMotionEvent,
        ) {
            handle.relative_motion(data, focus, event);
        }

        fn axis(
            &mut self,
            data: &mut crate::state::DriftWm,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, crate::state::DriftWm>,
            details: smithay::input::pointer::AxisFrame,
        ) {
            handle.axis(data, details);
        }

        fn frame(
            &mut self,
            data: &mut crate::state::DriftWm,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, crate::state::DriftWm>,
        ) {
            handle.frame(data);
        }

        fn gesture_swipe_begin(
            &mut self,
            data: &mut crate::state::DriftWm,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, crate::state::DriftWm>,
            event: &smithay::input::pointer::GestureSwipeBeginEvent,
        ) {
            handle.gesture_swipe_begin(data, event);
        }

        fn gesture_swipe_update(
            &mut self,
            data: &mut crate::state::DriftWm,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, crate::state::DriftWm>,
            event: &smithay::input::pointer::GestureSwipeUpdateEvent,
        ) {
            handle.gesture_swipe_update(data, event);
        }

        fn gesture_swipe_end(
            &mut self,
            data: &mut crate::state::DriftWm,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, crate::state::DriftWm>,
            event: &smithay::input::pointer::GestureSwipeEndEvent,
        ) {
            handle.gesture_swipe_end(data, event);
        }

        fn gesture_pinch_begin(
            &mut self,
            data: &mut crate::state::DriftWm,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, crate::state::DriftWm>,
            event: &smithay::input::pointer::GesturePinchBeginEvent,
        ) {
            handle.gesture_pinch_begin(data, event);
        }

        fn gesture_pinch_update(
            &mut self,
            data: &mut crate::state::DriftWm,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, crate::state::DriftWm>,
            event: &smithay::input::pointer::GesturePinchUpdateEvent,
        ) {
            handle.gesture_pinch_update(data, event);
        }

        fn gesture_pinch_end(
            &mut self,
            data: &mut crate::state::DriftWm,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, crate::state::DriftWm>,
            event: &smithay::input::pointer::GesturePinchEndEvent,
        ) {
            handle.gesture_pinch_end(data, event);
        }

        fn gesture_hold_begin(
            &mut self,
            data: &mut crate::state::DriftWm,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, crate::state::DriftWm>,
            event: &smithay::input::pointer::GestureHoldBeginEvent,
        ) {
            handle.gesture_hold_begin(data, event);
        }

        fn gesture_hold_end(
            &mut self,
            data: &mut crate::state::DriftWm,
            handle: &mut smithay::input::pointer::PointerInnerHandle<'_, crate::state::DriftWm>,
            event: &smithay::input::pointer::GestureHoldEndEvent,
        ) {
            handle.gesture_hold_end(data, event);
        }

        fn start_data(&self) -> &smithay::input::pointer::GrabStartData<crate::state::DriftWm> {
            &self.start_data
        }
    };
}
pub(crate) use forward_pointer_grab_methods;

/// Map `element` at `pos` on behalf of a drag, ending the animation entry the
/// drag is taking its geometry away from — but only when the map actually moves
/// it.
///
/// Neither at grab install nor on the first motion event: the SSD title bar
/// installs a `MoveGrab` on every left press, and `warp_pointer` delivers a
/// camera animation's compensating motion straight into a live grab, so either
/// would take down an entry that nothing has displaced yet. Waiting for the
/// geometry to actually change costs one frame of the drag and the entry
/// fighting each other, which beats popping a plain focus click's open fade.
pub(crate) fn drag_map_window(
    data: &mut crate::state::DriftWm,
    element: crate::state::StageWindow,
    pos: smithay::utils::Point<i32, smithay::utils::Logical>,
) {
    if data.stage.position_of(&element) != Some(pos) {
        data.end_element_animation(&element);
    }
    data.map_window(element, pos, false);
}

mod move_grab;
mod navigate_grab;
mod pan_grab;
mod resize_grab;
mod screen_space_click;
mod touch_gesture_grab;
mod touch_recognizer;

pub use move_grab::MoveGrab;
pub use navigate_grab::NavigateGrab;
pub use pan_grab::PanGrab;
pub use resize_grab::{
    ResizeGrab, ResizeState, SizeConstraints, has_bottom, has_left, has_right, has_top,
    locked_ratio_for, resize_screen_anchor,
};
pub use screen_space_click::ScreenSpaceClickGrab;
pub use touch_gesture_grab::TouchGestureGrab;
pub(crate) use touch_gesture_grab::edge_from_origin;
