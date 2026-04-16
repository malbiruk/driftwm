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

mod move_grab;
mod navigate_grab;
mod pan_grab;
mod resize_grab;

pub use move_grab::MoveSurfaceGrab;
pub use navigate_grab::NavigateGrab;
pub use pan_grab::PanGrab;
pub use resize_grab::{
    ResizeState, ResizeSurfaceGrab, SizeConstraints, has_bottom, has_left, has_right, has_top,
};
