//! Keyboard event handling: VT-switch, session-lock forwarding, compositor
//! action lookup + execution, and key-repeat bookkeeping.

use smithay::{
    backend::{
        input::{Event, InputBackend, KeyState, KeyboardKeyEvent},
        session::Session,
    },
    input::keyboard::{FilterResult, keysyms},
    utils::SERIAL_COUNTER,
};

use driftwm::config::Modifiers;
use driftwm::window_ext::WindowExt;

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
                self,
                keycode,
                key_state,
                serial,
                time,
                |state, _modifiers, handle| {
                    if key_state == KeyState::Pressed {
                        let raw = handle.modified_sym().raw();
                        if (0x1008FE01..=0x1008FE0C).contains(&raw) {
                            let vt = (raw - 0x1008FE01 + 1) as i32;
                            // VT switch may not deliver releases; reset key/cycle state.
                            state.suppressed_keys.clear();
                            state.cycle_state = None;
                            state.tap.reset();
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
                let sym = handle.modified_sym();

                // Observe-only: never intercepts, so the modifier events still
                // forward to apps. The action fires on release (below).
                let mods = Modifiers::from_state(modifiers);
                if let Some(peak) =
                    state
                        .tap
                        .update(key_state, is_modifier_keysym(sym.raw()), &mods)
                    && let Some(action) = state.config.tap_lookup(&peak)
                {
                    state.pending_tap_action = Some(action.clone());
                }

                if state.cycle_state.is_some() && !state.config.cycle_hold.all_held(modifiers) {
                    state.end_cycle();
                    return FilterResult::Forward;
                }

                if key_state == KeyState::Released {
                    // Suppress the release of any key whose press we intercepted —
                    // otherwise the focused client sees a "release without press".
                    if state.suppressed_keys.remove(&keycode_u32) {
                        return FilterResult::Intercept(None);
                    }
                    return FilterResult::Forward;
                }

                // VT switching: Ctrl+Alt+F1..F12 produces XF86Switch_VT_1..12
                let raw = sym.raw();
                if (0x1008FE01..=0x1008FE0C).contains(&raw) {
                    let vt = (raw - 0x1008FE01 + 1) as i32;
                    // VT switch may not deliver releases; reset key/cycle state.
                    state.suppressed_keys.clear();
                    state.cycle_state = None;
                    state.tap.reset();
                    if let Some(ref mut session) = state.session
                        && let Err(e) = session.change_vt(vt)
                    {
                        tracing::warn!("Failed to switch to VT{vt}: {e}");
                    }
                    return FilterResult::Intercept(None);
                }

                // pass_keys: forward compositor keybindings to the focused window.
                // PassKeys::All  — forward everything (game-friendly).
                // PassKeys::Only — forward only the listed combos; rest stay active.
                // VT-switching above is always handled regardless.
                // Uses live config so a config-reload takes effect immediately.
                let focused_pass_keys = state.focused_window().and_then(|w| {
                    let app_id = w.app_id_or_class().unwrap_or_default();
                    let title = w.window_title().unwrap_or_default();
                    state
                        .config
                        .resolve_window_rules(&app_id, &title)
                        .map(|r| r.pass_keys)
                });
                if focused_pass_keys
                    .as_ref()
                    .is_some_and(|pk| pk.allows_raw(modifiers, sym))
                {
                    return FilterResult::Forward;
                }

                if let Some(action) = state.config.lookup(modifiers, sym) {
                    state.suppressed_keys.insert(keycode_u32);
                    return FilterResult::Intercept(Some(action.clone()));
                }

                if state.config.layout_independent
                    && let Some(raw_sym) = handle.raw_latin_sym_or_raw_current_sym()
                    && raw_sym != sym
                    && let Some(action) = state.config.lookup(modifiers, raw_sym)
                {
                    state.suppressed_keys.insert(keycode_u32);
                    return FilterResult::Intercept(Some(action.clone()));
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
                self.held_action = Some((
                    keycode_u32,
                    action.clone(),
                    std::time::Instant::now() + delay,
                ));
            } else {
                // Non-repeatable action pressed — cancel any active repeat
                self.held_action = None;
            }
            self.execute_action(action);
        }

        // Run the tap action after the modifier events were forwarded above.
        if let Some(tap_action) = self.pending_tap_action.take() {
            self.execute_action(&tap_action);
        }
    }
}

/// Tells a held chord modifier apart from a real key landing on top. The
/// modifier keysyms are the contiguous block Shift_L … Hyper_R, minus the two
/// lock keysyms in that range (Caps_Lock, Shift_Lock): those toggle rather than
/// hold, so pressing one should cancel a tap like any other key.
fn is_modifier_keysym(raw: u32) -> bool {
    (keysyms::KEY_Shift_L..=keysyms::KEY_Hyper_R).contains(&raw)
        && raw != keysyms::KEY_Caps_Lock
        && raw != keysyms::KEY_Shift_Lock
}

/// Tracks held modifier chords so tap-modifier bindings can fire — an action
/// bound to a bare modifier combo (e.g. `alt+shift`) that triggers when the
/// chord is pressed and released with no other key on top.
#[derive(Default)]
pub(crate) struct TapTracker {
    /// High-water mark of modifiers held since the chord began.
    peak: Modifiers,
    /// Set once a non-modifier key (or pointer button) lands on top, demoting
    /// the chord from a tap to an ordinary binding prefix.
    tainted: bool,
}

impl TapTracker {
    /// Advance tracking for one key event. Returns the completed chord (the peak
    /// modifier set) when this event is the release that finishes an untainted
    /// modifier-only chord — the caller looks that up in the tap bindings.
    ///
    /// Fires at most once per chord: `peak` is a high-water mark that only clears
    /// once every modifier lifts, so the chord must fully release before it can
    /// fire again (holding one modifier and re-tapping another won't re-fire).
    pub(crate) fn update(
        &mut self,
        key_state: KeyState,
        is_modifier: bool,
        mods: &Modifiers,
    ) -> Option<Modifiers> {
        match key_state {
            KeyState::Pressed => {
                if is_modifier {
                    self.peak = self.peak.union(mods);
                } else {
                    // A real key on top: this chord is a binding prefix, not a tap.
                    self.tainted = true;
                }
                None
            }
            KeyState::Released => {
                let completed = (is_modifier && !self.tainted && !self.peak.is_empty())
                    .then(|| self.peak.clone());
                // Once the chord starts releasing, don't let later releases refire it.
                if completed.is_some() {
                    self.tainted = true;
                }
                if mods.is_empty() {
                    self.reset();
                }
                completed
            }
        }
    }

    /// Cancel any in-progress chord (a pointer button landed on top of it).
    pub(crate) fn taint(&mut self) {
        if !self.peak.is_empty() {
            self.tainted = true;
        }
    }

    /// Drop all chord state — used when key delivery may have been interrupted
    /// (VT switch), so a stale half-chord can't fire later.
    pub(crate) fn reset(&mut self) {
        self.peak = Modifiers::EMPTY;
        self.tainted = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(ctrl: bool, alt: bool, shift: bool, logo: bool) -> Modifiers {
        Modifiers {
            ctrl,
            alt,
            shift,
            logo,
        }
    }

    const NONE: Modifiers = Modifiers::EMPTY;

    #[test]
    fn clean_alt_shift_tap_fires_on_release() {
        let mut t = TapTracker::default();
        let alt = m(false, true, false, false);
        let alt_shift = m(false, true, true, false);
        // Press alt, then shift.
        assert_eq!(t.update(KeyState::Pressed, true, &alt), None);
        assert_eq!(t.update(KeyState::Pressed, true, &alt_shift), None);
        // Release shift → completes the {alt,shift} chord.
        assert_eq!(
            t.update(KeyState::Released, true, &alt),
            Some(alt_shift.clone())
        );
        // Releasing alt must not refire.
        assert_eq!(t.update(KeyState::Released, true, &NONE), None);
    }

    #[test]
    fn key_on_top_taints_the_chord() {
        let mut t = TapTracker::default();
        let alt = m(false, true, false, false);
        let alt_shift = m(false, true, true, false);
        t.update(KeyState::Pressed, true, &alt);
        t.update(KeyState::Pressed, true, &alt_shift);
        // A real key (q) is pressed while the chord is held.
        t.update(KeyState::Pressed, false, &alt_shift);
        t.update(KeyState::Released, false, &alt_shift); // q up
        // Neither modifier release fires now.
        assert_eq!(t.update(KeyState::Released, true, &alt), None);
        assert_eq!(t.update(KeyState::Released, true, &NONE), None);
    }

    #[test]
    fn pointer_button_taints_the_chord() {
        let mut t = TapTracker::default();
        let alt = m(false, true, false, false);
        let alt_shift = m(false, true, true, false);
        t.update(KeyState::Pressed, true, &alt);
        t.update(KeyState::Pressed, true, &alt_shift);
        t.taint(); // alt+shift+click
        assert_eq!(t.update(KeyState::Released, true, &alt), None);
    }

    #[test]
    fn single_modifier_tap_fires() {
        let mut t = TapTracker::default();
        let alt = m(false, true, false, false);
        t.update(KeyState::Pressed, true, &alt);
        assert_eq!(t.update(KeyState::Released, true, &NONE), Some(alt));
    }

    #[test]
    fn adding_a_modifier_changes_the_completed_chord() {
        // Tapping alt+shift reports {alt,shift}, never the sub-chord {alt}.
        let mut t = TapTracker::default();
        let alt = m(false, true, false, false);
        let alt_shift = m(false, true, true, false);
        t.update(KeyState::Pressed, true, &alt);
        t.update(KeyState::Pressed, true, &alt_shift);
        assert_eq!(
            t.update(KeyState::Released, true, &alt),
            Some(alt_shift),
            "peak chord, not the partial {{alt}}"
        );
    }

    #[test]
    fn reset_clears_a_stale_half_chord() {
        // A chord held when key delivery is interrupted (lock / VT switch) must
        // not survive the reset and fire on the next unrelated tap.
        let mut t = TapTracker::default();
        let alt = m(false, true, false, false);
        let alt_shift = m(false, true, true, false);
        t.update(KeyState::Pressed, true, &alt);
        t.update(KeyState::Pressed, true, &alt_shift); // releases never observed
        t.reset();
        // A fresh alt tap fires {alt} — not the stale {alt,shift}.
        t.update(KeyState::Pressed, true, &alt);
        assert_eq!(t.update(KeyState::Released, true, &NONE), Some(alt));
    }

    #[test]
    fn plain_typing_leaves_no_residual_state() {
        let mut t = TapTracker::default();
        let q = NONE;
        t.update(KeyState::Pressed, false, &q);
        t.update(KeyState::Released, false, &q);
        // A subsequent clean alt tap still fires.
        let alt = m(false, true, false, false);
        t.update(KeyState::Pressed, true, &alt);
        assert_eq!(t.update(KeyState::Released, true, &NONE), Some(alt));
    }
}
