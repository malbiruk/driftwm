//! Config hot-reload. On parse failure the old config is kept and an error
//! is logged — a bad edit never crashes the compositor.

use smithay::input::keyboard::XkbConfig;

use super::{DriftWm, output_state};

impl DriftWm {
    /// Hot-reload config from disk. On parse failure, logs an error and keeps the old config.
    pub fn reload_config(&mut self) {
        let config_path = driftwm::config::config_path();
        let contents = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    "Config reload: failed to read {}: {e}",
                    config_path.display()
                );
                return;
            }
        };
        let mut new_config = match driftwm::config::Config::from_toml(&contents) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Config reload: parse error: {e}");
                return;
            }
        };

        // Hot-reload keyboard layout
        if new_config.keyboard_layout != self.config.keyboard_layout {
            let kb = &new_config.keyboard_layout;
            let xkb = XkbConfig {
                layout: &kb.layout,
                variant: &kb.variant,
                options: if kb.options.is_empty() {
                    None
                } else {
                    Some(kb.options.clone())
                },
                model: &kb.model,
                ..Default::default()
            };
            let keyboard = self.seat.get_keyboard().unwrap();
            let num_lock = keyboard.modifier_state().num_lock;
            if let Err(err) = keyboard.set_xkb_config(self, xkb) {
                tracing::warn!("Config reload: error updating keyboard layout: {err:?}");
                new_config.keyboard_layout = self.config.keyboard_layout.clone();
            } else {
                tracing::info!("Config reload: keyboard layout updated");
                let mut mods = keyboard.modifier_state();
                if mods.num_lock != num_lock {
                    mods.num_lock = num_lock;
                    keyboard.set_modifier_state(mods);
                }
            }
        }
        if new_config.autostart != self.config.autostart {
            tracing::info!("Config reload: autostart changes only apply at startup");
        }

        // Keyboard repeat rate/delay
        if new_config.repeat_rate != self.config.repeat_rate
            || new_config.repeat_delay != self.config.repeat_delay
        {
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.change_repeat_info(new_config.repeat_rate, new_config.repeat_delay);
        }

        // Momentum friction — apply to all outputs
        if new_config.friction != self.config.friction {
            for output in self.space.outputs() {
                output_state(output).momentum.friction = new_config.friction;
            }
        }

        // Background shader/tile — always clear cached state so that editing
        // the shader file on disk takes effect after `touch`ing the config.
        self.render.background_shader = None;
        self.render.cached_bg_elements.clear();
        self.render.tile_shader = None;
        self.render.cached_tile_bg.clear();

        // Cursor theme/size — validate theme before committing
        let theme_changed = new_config.cursor_theme != self.config.cursor_theme;
        let size_changed = new_config.cursor_size != self.config.cursor_size;
        if theme_changed || size_changed {
            let theme_ok = if theme_changed {
                if let Some(ref theme_name) = new_config.cursor_theme {
                    let theme = xcursor::CursorTheme::load(theme_name);
                    if theme.load_icon("default").is_some() {
                        unsafe { std::env::set_var("XCURSOR_THEME", theme_name) };
                        true
                    } else {
                        tracing::warn!(
                            "Cursor theme '{theme_name}' not found, keeping current theme"
                        );
                        new_config.cursor_theme = self.config.cursor_theme.clone();
                        false
                    }
                } else {
                    unsafe { std::env::remove_var("XCURSOR_THEME") };
                    true
                }
            } else {
                false
            };

            if size_changed {
                if let Some(size) = new_config.cursor_size {
                    unsafe { std::env::set_var("XCURSOR_SIZE", size.to_string()) };
                } else {
                    unsafe { std::env::remove_var("XCURSOR_SIZE") };
                }
            }

            if theme_ok || size_changed {
                self.cursor.cursor_buffers.clear();
            }
        }

        // Trackpad settings — reconfigure all connected devices
        if new_config.trackpad != self.config.trackpad {
            self.config.trackpad = new_config.trackpad.clone();
            let devices = self.input_devices.clone();
            for mut device in devices {
                self.configure_libinput_device(&mut device);
            }
            tracing::info!("Config reload: trackpad settings applied to all devices");
        }

        // Env vars — diff old vs new, apply changes
        for (key, value) in &new_config.env {
            if self.config.env.get(key) != Some(value) {
                tracing::info!("Config reload: env {key}={value}");
                unsafe { std::env::set_var(key, value) };
            }
        }
        for key in self.config.env.keys() {
            if !new_config.env.contains_key(key) {
                tracing::info!("Config reload: env unset {key}");
                unsafe { std::env::remove_var(key) };
            }
        }

        self.config = new_config;
        self.mark_all_dirty();
        tracing::info!("Config reloaded");
    }
}
