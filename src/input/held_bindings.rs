//! Binding lifecycle independent of any particular action or modifier.
use crate::state::DriftWm;
use driftwm::config::{HeldKeyBinding, HoldUntil};
use smithay::input::keyboard::ModifiersState;

pub(crate) struct ActiveHeldBinding {
    keycode: u32,
    pub(crate) binding: HeldKeyBinding,
    key_down: bool,
    cancelled: bool,
}

impl DriftWm {
    pub(crate) fn start_held_binding(&mut self, keycode: u32, binding: HeldKeyBinding) {
        self.held_bindings.push(ActiveHeldBinding {
            keycode,
            binding,
            key_down: true,
            cancelled: false,
        });
    }
    pub(crate) fn held_trigger_latched(&self, keycode: u32) -> bool {
        self.held_bindings.iter().any(|b| b.keycode == keycode)
    }
    pub(crate) fn has_active_held_bindings(&self) -> bool {
        self.held_bindings.iter().any(|b| !b.cancelled)
    }
    pub(crate) fn held_key_released(&mut self, keycode: u32) {
        for b in &mut self.held_bindings {
            if b.keycode == keycode {
                b.key_down = false;
            }
        }
    }
    pub(crate) fn cancel_held_bindings(&mut self) {
        for b in &mut self.held_bindings {
            if !b.cancelled {
                b.cancelled = true;
                if let Some(action) = &b.binding.cancel {
                    self.held_cancel_actions.push(action.clone());
                }
            }
        }
    }
    pub(crate) fn clear_held_bindings(&mut self) {
        self.held_bindings.clear();
        self.held_cancel_actions.clear();
    }
    // Execute after keyboard.input releases its mutex: actions can change keyboard focus.
    pub(crate) fn finish_released_bindings(&mut self, mods: &ModifiersState) {
        let mut actions = std::mem::take(&mut self.held_cancel_actions);
        self.held_bindings.retain(|b| {
            let held = match b.binding.hold {
                HoldUntil::Key => b.key_down,
                HoldUntil::Modifiers => b.binding.modifiers.all_held(mods),
            };
            if !held && !b.cancelled {
                actions.push(b.binding.release.clone());
            }
            held
        });
        for action in actions {
            self.execute_action(&action);
        }
    }
}
