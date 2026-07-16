//! Config hot-reload. On parse failure the old config is kept and an error
//! is logged — a bad edit never crashes the compositor.

use smithay::input::keyboard::XkbConfig;

use super::{DriftWm, ErrorSource, output_state};

impl DriftWm {
    pub fn reload_config(&mut self) {
        let config_path = driftwm::config::config_path();
        let contents = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    "Config reload: failed to read {}: {e}",
                    config_path.display()
                );
                self.set_error(
                    ErrorSource::Config,
                    format!("config: failed to read {}: {e}", config_path.display()),
                );
                return;
            }
        };
        self.reload_config_from_contents(&contents);
    }

    /// Apply a config from raw TOML text, bypassing disk I/O so tests can
    /// drive reload without a config file.
    pub fn reload_config_from_contents(&mut self, contents: &str) {
        let (mut new_config, config_errors) =
            match driftwm::config::Config::from_toml_collect(contents) {
                Ok((c, errs)) => (c, errs),
                Err(e) => {
                    tracing::error!("Config reload: parse error: {e}");
                    self.set_error(ErrorSource::Config, format!("config error: {e}"));
                    return;
                }
            };

        self.clear_error(ErrorSource::Keyboard);
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
                self.set_error(
                    ErrorSource::Keyboard,
                    "keyboard: invalid layout config — keeping previous".to_string(),
                );
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

        if new_config.repeat_rate != self.config.repeat_rate
            || new_config.repeat_delay != self.config.repeat_delay
        {
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.change_repeat_info(new_config.repeat_rate, new_config.repeat_delay);
        }

        if new_config.drift != self.config.drift {
            for output in self.space.outputs() {
                output_state(output).momentum.drift = new_config.drift;
            }
        }

        // Always clear background cache so shader-file edits take effect
        // after `touch`ing config, and type swaps (shader/tile/wallpaper)
        // are clean. Reset usage flags so stale `true`s from a prior shader
        // can't force per-frame redraws or push unused uniforms.
        self.render.background_shader = None;
        self.render.background_is_animated = false;
        self.render.background_uses_camera = false;
        self.render.background_uses_zoom = false;
        self.render.cached_bg.clear();
        // Shared animated-blur textures are only touched while `animate_blur`
        // is on; without this, disabling it would strand two full-output
        // textures (~66 MB at 4K) until exit.
        self.render.shared_blur.clear();
        self.render.tile_shader = None;
        self.render.tile_mirror_shader = None;
        self.render.wallpaper_shader = None;
        self.render.chunk_bg_shader = None;
        self.render.cached_tile_chunks.clear();
        self.render.cached_shader_chunks.clear();

        // Border + shadow caches key on phys-only — color and corner_radius
        // live in uniforms. Drop both so edits to those config fields apply.
        self.render.border_cache.clear();
        self.render.shadow_cache.clear();

        // Validate cursor theme before committing. XCURSOR_* reaches
        // children via child_env (rebuilt by `Config::from_raw`); cursor
        // loader reads `self.config` directly.
        let theme_changed = new_config.cursor_theme != self.config.cursor_theme;
        let size_changed = new_config.cursor_size != self.config.cursor_size;
        if theme_changed || size_changed {
            let theme_ok = if theme_changed {
                if let Some(ref theme_name) = new_config.cursor_theme {
                    let theme = xcursor::CursorTheme::load(theme_name);
                    if theme.load_icon("default").is_some() {
                        true
                    } else {
                        tracing::warn!(
                            "Cursor theme '{theme_name}' not found, keeping current theme"
                        );
                        new_config.cursor_theme = self.config.cursor_theme.clone();
                        match &self.config.cursor_theme {
                            Some(t) => {
                                new_config
                                    .child_env
                                    .insert("XCURSOR_THEME".into(), t.clone());
                            }
                            None => {
                                new_config.child_env.remove("XCURSOR_THEME");
                            }
                        }
                        false
                    }
                } else {
                    true
                }
            } else {
                false
            };

            if theme_ok || size_changed {
                self.cursor.cursor_buffers.clear();
            }
        }

        if new_config.trackpad != self.config.trackpad {
            self.config.trackpad = new_config.trackpad.clone();
            let devices = self.input_devices.clone();
            for mut device in devices {
                self.configure_libinput_device(&mut device);
            }
            tracing::info!("Config reload: trackpad settings applied to all devices");
        }

        // child_env auto-rebuilds via `Config::from_raw`; process env
        // untouched. Forward DISPLAY (set by xwayland::setup at startup).
        if let Some(display) = self.config.child_env.get("DISPLAY").cloned() {
            new_config.child_env.insert("DISPLAY".into(), display);
        }

        self.config = new_config;

        // Invalidate every SSD title bar's cached width so `update()`
        // rebuilds with the new font/height/colors.
        for deco in self.decorations.values_mut() {
            deco.width = -1;
        }

        self.apply_output_rules_after_reload();
        self.recompute_decoration_scale();

        if let Some(msg) = super::errors::summarize_config_errors(&config_errors) {
            self.set_error(ErrorSource::Config, msg);
        } else {
            self.clear_error(ErrorSource::Config);
        }
        self.mark_all_dirty();
        tracing::info!("Config reloaded");
    }

    /// Re-apply per-output rules (mode/scale/transform/position). Mode
    /// changes go through `pending_mode_changes`; everything else applies
    /// in-place via `Output::change_current_state`. Same lookup as
    /// `output_connected` so reload and startup compute identically.
    fn apply_output_rules_after_reload(&mut self) {
        use driftwm::config::{OutputMode as ConfigOutputMode, OutputPosition};
        use smithay::utils::Transform;

        let outputs: Vec<smithay::output::Output> = self.space.outputs().cloned().collect();
        // Cumulative width for auto-positioning, mirroring `output_connected`.
        // Widths read post-change_current_state so a scale change affects
        // subsequent outputs' auto positions.
        let mut auto_x: i32 = 0;
        for output in outputs {
            let name = output.name();
            let cfg = self.config.output_config(&name);

            let want_mode = cfg.map(|c| &c.mode).cloned().unwrap_or_default();
            if let Some(current) = output.current_mode() {
                let (cur_w, cur_h) = (current.size.w, current.size.h);
                let cur_hz_milli = current.refresh;
                let intent = match &want_mode {
                    ConfigOutputMode::Size(w, h) if (cur_w, cur_h) != (*w, *h) => {
                        Some(crate::state::ModeIntent::Custom {
                            w: *w,
                            h: *h,
                            refresh_mhz: cur_hz_milli,
                        })
                    }
                    ConfigOutputMode::SizeRefresh(w, h, hz)
                        if (cur_w, cur_h) != (*w, *h) || cur_hz_milli != *hz as i32 * 1000 =>
                    {
                        Some(crate::state::ModeIntent::Custom {
                            w: *w,
                            h: *h,
                            refresh_mhz: *hz as i32 * 1000,
                        })
                    }
                    // The state layer has no EDID/connector knowledge, so it
                    // can't tell whether the current mode already is the
                    // preferred one. Queue unconditionally; the backend skips
                    // the modeset when the resolved preferred mode is a no-op.
                    // Not on winit: nothing drains the queue there, and the
                    // default-rule intent would sit in debug counters forever.
                    ConfigOutputMode::Preferred
                        if !matches!(self.backend, Some(crate::backend::Backend::Winit(_))) =>
                    {
                        Some(crate::state::ModeIntent::Preferred)
                    }
                    _ => None,
                };
                if let Some(intent) = intent {
                    self.pending_mode_changes.insert(name.clone(), intent);
                }
            }

            // Missing field reverts to 1.0.
            let want_scale = cfg.and_then(|c| c.scale).unwrap_or(1.0);
            let cur_scale = output.current_scale().fractional_scale();
            let new_scale = if (cur_scale - want_scale).abs() > f64::EPSILON {
                Some(smithay::output::Scale::Fractional(want_scale))
            } else {
                None
            };

            // Missing field reverts to Normal.
            let want_transform = cfg.and_then(|c| c.transform).unwrap_or(Transform::Normal);
            let new_transform = if output.current_transform() != want_transform {
                Some(want_transform)
            } else {
                None
            };

            // Missing/`Auto` = auto-place by accumulated width.
            let want_position: smithay::utils::Point<i32, smithay::utils::Logical> =
                match cfg.map(|c| &c.position) {
                    Some(OutputPosition::Fixed(x, y)) => (*x, *y).into(),
                    _ => (auto_x, 0).into(),
                };
            let cur_position = crate::state::output_state(&output).layout_position;
            let new_position = if cur_position != want_position {
                let mut os = crate::state::output_state(&output);
                os.layout_position = want_position;
                Some(want_position)
            } else {
                None
            };

            if new_scale.is_some() || new_transform.is_some() || new_position.is_some() {
                output.change_current_state(None, new_transform, new_scale, new_position);
                {
                    let mut map = smithay::desktop::layer_map_for_output(&output);
                    map.arrange();
                }
                let size = crate::state::output_logical_size(&output);
                self.resize_fullscreen_for_output(&output, size);
                self.render.remove_output(&name);
                self.output_config_dirty = true;
            }

            // Use *post-change* width so a scale change cascades to next.
            auto_x += crate::state::output_logical_size(&output).w;
        }
    }
}
