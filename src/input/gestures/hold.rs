//! Hold gesture handlers — fires a threshold action on release.

use smithay::{
    backend::input::{Event, GestureBeginEvent, GestureEndEvent, InputBackend},
    input::pointer::{GestureHoldBeginEvent as WlHoldBegin, GestureHoldEndEvent as WlHoldEnd},
    utils::SERIAL_COUNTER,
};

use driftwm::config::{GestureConfigEntry, GestureTrigger, ThresholdAction};

use crate::state::DriftWm;

use super::GestureState;

impl DriftWm {
    pub fn on_gesture_hold_begin<I: InputBackend>(&mut self, event: I::GestureHoldBeginEvent) {
        let fingers = event.fingers();
        let time = Event::time_msec(&event);

        let keyboard = self.seat.get_keyboard().unwrap();
        let mods = keyboard.modifier_state();
        let pointer = self.seat.get_pointer().unwrap();
        let pos = pointer.current_location();
        let context = self.pointer_context(pos);

        let hold_trigger = GestureTrigger::Hold { fingers };
        if let Some(entry) = self.config.gesture_lookup(&mods, &hold_trigger, context) {
            let action = match entry {
                GestureConfigEntry::Threshold(ThresholdAction::Fixed(a)) => Some(a.clone()),
                _ => None,
            };
            if let Some(action) = action {
                self.gesture_state = Some(GestureState::HoldAction { action });
                return;
            }
        }
        self.forward_hold_begin(fingers, time);
    }

    pub fn on_gesture_hold_end<I: InputBackend>(&mut self, event: I::GestureHoldEndEvent) {
        let cancelled = event.cancelled();
        let time = Event::time_msec(&event);
        if let Some(GestureState::HoldAction { action }) = self.gesture_state.take() {
            if !cancelled {
                self.execute_action(&action);
            }
            return;
        }
        self.forward_hold_end(cancelled, time);
    }

    fn forward_hold_begin(&mut self, fingers: u32, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_hold_begin(
            self,
            &WlHoldBegin {
                serial,
                time,
                fingers,
            },
        );
        pointer.frame(self);
    }

    fn forward_hold_end(&mut self, cancelled: bool, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_hold_end(
            self,
            &WlHoldEnd {
                serial,
                time,
                cancelled,
            },
        );
        pointer.frame(self);
    }
}
