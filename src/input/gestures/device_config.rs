//! libinput device configuration — trackpad and mouse knobs applied from
//! the user's config on device hotplug or config reload.

use crate::state::DriftWm;

fn set_accel_profile(
    device: &mut smithay::reexports::input::Device,
    profile: driftwm::config::AccelProfile,
) {
    use driftwm::config::AccelProfile;
    let libinput_profile = match profile {
        AccelProfile::Flat => smithay::reexports::input::AccelProfile::Flat,
        AccelProfile::Adaptive => smithay::reexports::input::AccelProfile::Adaptive,
    };
    if let Err(e) = device.config_accel_set_profile(libinput_profile) {
        tracing::warn!("Failed to set accel_profile: {e:?}");
    }
}

fn set_send_events(
    device: &mut smithay::reexports::input::Device,
    mode: driftwm::config::SendEvents,
) {
    use driftwm::config::SendEvents;
    let libinput_mode = match mode {
        SendEvents::Enabled => smithay::reexports::input::SendEventsMode::ENABLED,
        SendEvents::Disabled => smithay::reexports::input::SendEventsMode::DISABLED,
        SendEvents::DisabledOnExternalMouse => {
            smithay::reexports::input::SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE
        }
    };
    if let Err(e) = device.config_send_events_set_mode(libinput_mode) {
        tracing::warn!("Failed to set send_events on {}: {e:?}", device.name());
    }
}

/// Tap support is what tells a trackpad from a mouse here. Both device loops
/// below must agree on it, or `set-trackpad` and `configure_trackpad` would
/// target different sets.
fn is_trackpad(device: &smithay::reexports::input::Device) -> bool {
    device.config_tap_finger_count() > 0
}

impl DriftWm {
    /// Configure a libinput device using settings from config.
    /// Trackpads get trackpad settings, mice get mouse settings.
    pub fn configure_libinput_device(&self, device: &mut smithay::reexports::input::Device) {
        if is_trackpad(device) {
            self.configure_trackpad(device);
        } else if device.has_capability(smithay::reexports::input::DeviceCapability::Pointer) {
            self.configure_mouse(device);
        }
    }

    /// The send-events mode in force: a runtime `set-trackpad` press if there
    /// has been one this session, otherwise the `[input.trackpad]` seed.
    pub fn effective_send_events(&self) -> driftwm::config::SendEvents {
        self.trackpad_send_events
            .unwrap_or(self.config.trackpad.send_events)
    }

    /// Push just the send-events mode to every connected trackpad.
    /// `set-trackpad` changes nothing else, so it doesn't need the full
    /// `configure_trackpad` pass.
    pub fn apply_trackpad_send_events(&self) {
        let mode = self.effective_send_events();
        let mut found = false;
        for mut device in self.input_devices.clone() {
            if is_trackpad(&device) {
                found = true;
                set_send_events(&mut device, mode);
            }
        }
        if !found {
            // Not a dead end — the mode is stored, so a trackpad plugged in
            // later comes up in it. Worth a breadcrumb all the same, since the
            // keypress otherwise does nothing visible.
            tracing::debug!("set-trackpad: no trackpad connected, holding {mode:?}");
        }
    }

    fn configure_trackpad(&self, device: &mut smithay::reexports::input::Device) {
        let cfg = &self.config.trackpad;
        // A runtime `set-trackpad` press outranks the config seed, so a
        // hotplug or VT switch re-applies what the user last chose rather than
        // reverting to the file.
        let send_events = self.effective_send_events();
        tracing::info!(
            "Configuring trackpad: {} (tap={}, natural_scroll={}, accel={}, profile={:?}, click_method={:?}, dwt={}, send_events={:?})",
            device.name(),
            cfg.tap_to_click,
            cfg.natural_scroll,
            cfg.accel_speed,
            cfg.accel_profile,
            cfg.click_method,
            cfg.disable_while_typing,
            send_events,
        );

        set_send_events(device, send_events);

        if let Err(e) = device.config_tap_set_enabled(cfg.tap_to_click) {
            tracing::warn!("Failed to set tap_to_click: {e:?}");
        }
        if let Err(e) = device.config_tap_set_drag_enabled(cfg.tap_and_drag) {
            tracing::warn!("Failed to set tap_and_drag: {e:?}");
        }
        if device.config_dwt_is_available()
            && let Err(e) = device.config_dwt_set_enabled(cfg.disable_while_typing)
        {
            tracing::warn!("Failed to set disable_while_typing: {e:?}");
        }
        if let Err(e) = device.config_scroll_set_natural_scroll_enabled(cfg.natural_scroll) {
            tracing::warn!("Failed to set natural_scroll: {e:?}");
        }
        // LRM: 1-finger=left, 2-finger=right, 3-finger=middle.
        // Hardcoded — the compositor uses BTN_MIDDLE from 3-finger tap
        // for double-tap+drag window move detection.
        if let Err(e) = device
            .config_tap_set_button_map(smithay::reexports::input::TapButtonMap::LeftRightMiddle)
        {
            tracing::warn!("Failed to set button_map: {e:?}");
        }
        if let Err(e) = device.config_accel_set_speed(cfg.accel_speed) {
            tracing::warn!("Failed to set accel_speed: {e:?}");
        }
        set_accel_profile(device, cfg.accel_profile);
        if let Some(ref method) = cfg.click_method {
            let click = match method.as_str() {
                "none" => None,
                "button_areas" => Some(smithay::reexports::input::ClickMethod::ButtonAreas),
                "clickfinger" => Some(smithay::reexports::input::ClickMethod::Clickfinger),
                other => {
                    tracing::warn!("Unknown click_method '{other}', ignoring");
                    None
                }
            };
            if let Some(click) = click
                && let Err(e) = device.config_click_set_method(click)
            {
                tracing::warn!("Failed to set click_method: {e:?}");
            }
        }
    }

    fn configure_mouse(&self, device: &mut smithay::reexports::input::Device) {
        let cfg = &self.config.mouse_device;
        tracing::info!(
            "Configuring mouse: {} (accel={}, profile={:?}, natural_scroll={}, left_handed={})",
            device.name(),
            cfg.accel_speed,
            cfg.accel_profile,
            cfg.natural_scroll,
            cfg.left_handed,
        );

        if let Err(e) = device.config_accel_set_speed(cfg.accel_speed) {
            tracing::warn!("Failed to set mouse accel_speed: {e:?}");
        }
        set_accel_profile(device, cfg.accel_profile);
        if let Err(e) = device.config_scroll_set_natural_scroll_enabled(cfg.natural_scroll) {
            tracing::warn!("Failed to set mouse natural_scroll: {e:?}");
        }
        if device.config_left_handed_is_available()
            && let Err(e) = device.config_left_handed_set(cfg.left_handed)
        {
            tracing::warn!("Failed to set mouse left_handed: {e:?}");
        }
    }
}
