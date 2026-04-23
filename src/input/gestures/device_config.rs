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

impl DriftWm {
    /// Configure a libinput device using settings from config.
    /// Trackpads get trackpad settings, mice get mouse settings.
    pub fn configure_libinput_device(&self, device: &mut smithay::reexports::input::Device) {
        if device.config_tap_finger_count() > 0 {
            self.configure_trackpad(device);
        } else if device.has_capability(smithay::reexports::input::DeviceCapability::Pointer) {
            self.configure_mouse(device);
        }
    }

    fn configure_trackpad(&self, device: &mut smithay::reexports::input::Device) {
        let cfg = &self.config.trackpad;
        tracing::info!(
            "Configuring trackpad: {} (tap={}, natural_scroll={}, accel={}, profile={:?}, click_method={:?})",
            device.name(),
            cfg.tap_to_click,
            cfg.natural_scroll,
            cfg.accel_speed,
            cfg.accel_profile,
            cfg.click_method,
        );

        if let Err(e) = device.config_tap_set_enabled(cfg.tap_to_click) {
            tracing::warn!("Failed to set tap_to_click: {e:?}");
        }
        if let Err(e) = device.config_tap_set_drag_enabled(cfg.tap_and_drag) {
            tracing::warn!("Failed to set tap_and_drag: {e:?}");
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
            "Configuring mouse: {} (accel={}, profile={:?}, natural_scroll={})",
            device.name(),
            cfg.accel_speed,
            cfg.accel_profile,
            cfg.natural_scroll,
        );

        if let Err(e) = device.config_accel_set_speed(cfg.accel_speed) {
            tracing::warn!("Failed to set mouse accel_speed: {e:?}");
        }
        set_accel_profile(device, cfg.accel_profile);
        if let Err(e) = device.config_scroll_set_natural_scroll_enabled(cfg.natural_scroll) {
            tracing::warn!("Failed to set mouse natural_scroll: {e:?}");
        }
    }
}
