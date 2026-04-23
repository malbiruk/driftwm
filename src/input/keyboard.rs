//! Keyboard event handling: VT-switch, session-lock forwarding, compositor
//! action lookup + execution, and key-repeat bookkeeping.

use smithay::{
    backend::{
        input::{Event, InputBackend, KeyState, KeyboardKeyEvent},
        session::Session,
    },
    input::keyboard::FilterResult,
    utils::SERIAL_COUNTER,
};

use crate::state::DriftWm;

impl DriftWm {
    pub(super) fn on_keyboard<I: InputBackend>(&mut self, event: I::KeyboardKeyEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let key_state = event.state();
        let keycode = event.key_code();
        let keycode_u32: u32 = keycode.into();

        // When session is locked, only allow VT switching — forward everything else
        if !matches!(self.session_lock, crate::state::SessionLock::Unlocked) {
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.input::<(), _>(
                self, keycode, key_state, serial, time,
                |state, _modifiers, handle| {
                    if key_state == KeyState::Pressed {
                        let raw = handle.modified_sym().raw();
                        if (0x1008FE01..=0x1008FE0C).contains(&raw) {
                            let vt = (raw - 0x1008FE01 + 1) as i32;
                            if let Some(ref mut session) = state.session
                                && let Err(e) = session.change_vt(vt)
                            {
                                tracing::warn!("Failed to switch to VT{vt}: {e}");
                            }
                        }
                    }
                    FilterResult::Forward
                },
            );
            return;
        }

        // Clear key repeat on release of the held key
        if key_state == KeyState::Released
            && let Some((held_keycode, _, _)) = &self.held_action
            && *held_keycode == keycode_u32
        {
            self.held_action = None;
        }

        let keyboard = self.seat.get_keyboard().unwrap();

        let action = keyboard.input(
            self,
            keycode,
            key_state,
            serial,
            time,
            |state, modifiers, handle| {
                // If cycling is active and the cycle modifier was released, end cycle
                if state.cycle_state.is_some()
                    && !state.config.cycle_modifier.is_pressed(modifiers)
                {
                    state.end_cycle();
                    return FilterResult::Forward;
                }

                if key_state == KeyState::Pressed {
                    let sym = handle.modified_sym();

                    // VT switching: Ctrl+Alt+F1..F12 produces XF86Switch_VT_1..12
                    let raw = sym.raw();
                    if (0x1008FE01..=0x1008FE0C).contains(&raw) {
                        let vt = (raw - 0x1008FE01 + 1) as i32;
                        if let Some(ref mut session) = state.session
                            && let Err(e) = session.change_vt(vt)
                        {
                            tracing::warn!("Failed to switch to VT{vt}: {e}");
                        }
                        return FilterResult::Intercept(None);
                    }

                    if let Some(action) = state.config.lookup(modifiers, sym) {
                        return FilterResult::Intercept(Some(action.clone()));
                    }

                    if state.config.layout_independent
                        && let Some(raw_sym) = handle.raw_latin_sym_or_raw_current_sym()
                        && raw_sym != sym
                        && let Some(action) = state.config.lookup(modifiers, raw_sym)
                    {
                        return FilterResult::Intercept(Some(action.clone()));
                    }
                }
                FilterResult::Forward
            },
        );

        // Update active layout name (may have changed via XKB group switch)
        let layout_name = keyboard.with_xkb_state(self, |ctx| {
            let xkb = ctx.xkb().lock().unwrap();
            let layout = xkb.active_layout();
            xkb.layout_name(layout).to_owned()
        });
        if self.active_layout != layout_name {
            self.active_layout = layout_name;
        }

        if let Some(ref action) = action.flatten() {
            // Set up key repeat for repeatable actions
            if action.is_repeatable() {
                let delay = std::time::Duration::from_millis(self.config.repeat_delay as u64);
                self.held_action = Some((keycode_u32, action.clone(), std::time::Instant::now() + delay));
            } else {
                // Non-repeatable action pressed — cancel any active repeat
                self.held_action = None;
            }
            self.execute_action(action);
        }
    }
}
