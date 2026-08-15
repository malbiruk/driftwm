//! `[input.trackpad]` `enable` / `disable_on_external_mouse`, and the
//! `set-trackpad` action that overrides them for the rest of the session.
//!
//! The device side can't be exercised here — `input_devices` is only populated
//! by the udev backend — so these cover `effective_send_events()`, the value
//! `configure_trackpad` writes to each device.

use driftwm::config::{Action, SendEvents, TrackpadState};

use super::{Fixture, config};

#[test]
fn send_events_resolves_from_enable_and_external_mouse() {
    assert_eq!(config("").trackpad.send_events, SendEvents::Enabled);
    assert_eq!(
        config("[input.trackpad]\nenable = false\n")
            .trackpad
            .send_events,
        SendEvents::Disabled
    );
    assert_eq!(
        config("[input.trackpad]\ndisable_on_external_mouse = true\n")
            .trackpad
            .send_events,
        SendEvents::DisabledOnExternalMouse
    );
    assert_eq!(
        config("[input.trackpad]\nenable = false\ndisable_on_external_mouse = true\n")
            .trackpad
            .send_events,
        SendEvents::Disabled
    );
}

#[test]
fn with_no_press_the_effective_mode_is_the_config_seed() {
    let mut f = Fixture::with_config(config("[input.trackpad]\nenable = false\n"));

    assert_eq!(f.state().trackpad_send_events, None);
    assert_eq!(f.state().effective_send_events(), SendEvents::Disabled);
}

#[test]
fn a_press_outranks_the_config_seed() {
    let mut f = Fixture::with_config(config("[input.trackpad]\nenable = false\n"));

    f.state()
        .execute_action(&Action::SetTrackpad(TrackpadState::On));
    assert_eq!(f.state().trackpad_send_events, Some(SendEvents::Enabled));
    assert_eq!(f.state().effective_send_events(), SendEvents::Enabled);

    let mut f = Fixture::new();
    f.state()
        .execute_action(&Action::SetTrackpad(TrackpadState::Off));
    assert_eq!(f.state().effective_send_events(), SendEvents::Disabled);
}

#[test]
fn toggle_from_disable_on_external_mouse_goes_off() {
    let mut f = Fixture::with_config(config(
        "[input.trackpad]\ndisable_on_external_mouse = true\n",
    ));

    // The trackpad is live whenever no mouse is attached, so the first press
    // means "off" rather than "on".
    f.state()
        .execute_action(&Action::SetTrackpad(TrackpadState::Toggle));
    assert_eq!(f.state().trackpad_send_events, Some(SendEvents::Disabled));

    f.state()
        .execute_action(&Action::SetTrackpad(TrackpadState::Toggle));
    assert_eq!(f.state().trackpad_send_events, Some(SendEvents::Enabled));
}

#[test]
fn reload_keeps_the_override_across_an_unrelated_trackpad_edit() {
    let mut f = Fixture::new();
    f.state()
        .execute_action(&Action::SetTrackpad(TrackpadState::Off));

    // Editing a different knob re-applies every device, but the press stands:
    // saving an unrelated line must not hand the trackpad back.
    f.state()
        .reload_config_from_contents("[input.trackpad]\naccel_speed = 0.5\n");

    assert_eq!(f.state().trackpad_send_events, Some(SendEvents::Disabled));
    assert_eq!(f.state().effective_send_events(), SendEvents::Disabled);
}

#[test]
fn reload_that_resolves_to_the_same_mode_leaves_the_override_alone() {
    let mut f = Fixture::new();
    f.state()
        .execute_action(&Action::SetTrackpad(TrackpadState::Off));

    // Spelling out the default the user is already on isn't a change, so it
    // doesn't hand the trackpad back — pressing the key again does.
    f.state()
        .reload_config_from_contents("[input.trackpad]\nenable = true\n");

    assert_eq!(f.state().effective_send_events(), SendEvents::Disabled);
}

#[test]
fn reload_lets_an_edited_enable_field_reassert() {
    let mut f = Fixture::new();
    f.state()
        .execute_action(&Action::SetTrackpad(TrackpadState::Off));

    f.state()
        .reload_config_from_contents("[input.trackpad]\ndisable_on_external_mouse = true\n");

    assert_eq!(f.state().trackpad_send_events, None);
    assert_eq!(
        f.state().effective_send_events(),
        SendEvents::DisabledOnExternalMouse
    );
}
