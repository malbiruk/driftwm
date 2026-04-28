//! State file persistence under `$XDG_RUNTIME_DIR/driftwm/state`.
//!
//! External tools (launcher, status bars, scripts) read this file to learn
//! the current camera/zoom and window/layer inventory. Writes are throttled
//! to ~10/sec and only fire when something actually changed.

use smithay::utils::{Logical, Point};
use smithay::wayland::seat::WaylandFocus;
use std::collections::HashMap;
use std::time::Instant;

use super::{DriftWm, output_state};

impl DriftWm {
    /// Write viewport center + zoom to `$XDG_RUNTIME_DIR/driftwm/state` if changed.
    /// Atomic: writes to .tmp then renames.
    pub fn write_state_file_if_dirty(&mut self) {
        // Check if any output's camera/zoom changed (not just active output)
        let layout_dirty = self.state_file_layout != self.active_layout;
        let mut any_output_dirty = false;
        for output in self.space.outputs() {
            let os = output_state(output);
            let name = output.name();
            let (cam, z) = (os.camera, os.zoom);
            drop(os);
            if let Some(&(cached_cam, cached_z)) = self.state_file_cameras.get(&name) {
                if (cam.x - cached_cam.x).abs() >= 0.5
                    || (cam.y - cached_cam.y).abs() >= 0.5
                    || (z - cached_z).abs() >= 0.001
                {
                    any_output_dirty = true;
                    break;
                }
            } else {
                any_output_dirty = true;
                break;
            }
        }
        let window_count = self.space.elements().count();
        let layer_count: usize = self
            .space
            .outputs()
            .map(|o| smithay::desktop::layer_map_for_output(o).layers().count())
            .sum();
        let windows_dirty = window_count != self.state_file_window_count
            || layer_count != self.state_file_layer_count;

        if !layout_dirty && !any_output_dirty && !windows_dirty {
            return;
        }
        // Throttle writes to ~10/sec max (100ms between writes)
        if self.state_file_last_write.elapsed() < std::time::Duration::from_millis(100) {
            return;
        }
        // Update cached state
        self.state_file_window_count = window_count;
        self.state_file_layer_count = layer_count;
        for output in self.space.outputs() {
            let os = output_state(output);
            self.state_file_cameras
                .insert(output.name(), (os.camera, os.zoom));
        }
        self.state_file_layout = self.active_layout.clone();
        self.state_file_last_write = Instant::now();

        // Convert active output's camera to viewport center in canvas coords.
        // Negate Y so positive = above origin (user-facing Y-up convention).
        let cam = self.camera();
        let z = self.zoom();
        let vp = self.get_viewport_size();
        let cx = cam.x + vp.w as f64 / (2.0 * z);
        let cy = -(cam.y + vp.h as f64 / (2.0 * z));

        let Some(dir) = state_file_dir() else { return };
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("state");
        let tmp = dir.join("state.tmp");
        let mut content = format!(
            "x={cx:.0}\ny={cy:.0}\nzoom={z:.3}\nlayout={}\n",
            self.active_layout
        );

        {
            let home_return = output_state(&self.active_output().unwrap())
                .home_return
                .clone();
            if let Some(ref ret) = home_return {
                let sz = ret.zoom;
                let sx = ret.camera.x + vp.w as f64 / (2.0 * sz);
                let sy = -(ret.camera.y + vp.h as f64 / (2.0 * sz));
                content += &format!("saved_x={sx:.0}\nsaved_y={sy:.0}\nsaved_zoom={sz:.3}\n");
            }
        }

        // Window list: app_id of each toplevel (focused window first).
        let focused_surface = self.seat.get_keyboard().and_then(|kb| kb.current_focus());
        let mut app_ids: Vec<String> = Vec::new();
        for window in self.space.elements() {
            let Some(surface) = window.wl_surface() else {
                continue;
            };
            let app_id = smithay::wayland::compositor::with_states(&surface, |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .and_then(|d| d.lock().ok())
                    .and_then(|guard| guard.app_id.clone())
            })
            .unwrap_or_default();
            if !app_id.is_empty() {
                let is_focused = focused_surface.as_ref().is_some_and(|f| f.0 == *surface);
                if is_focused {
                    app_ids.insert(0, app_id);
                } else {
                    app_ids.push(app_id);
                }
            }
        }
        if !app_ids.is_empty() {
            content += &format!("windows={}\n", app_ids.join(","));
        }

        // Layer shell surfaces (waybar, notifications, etc.)
        let mut layers: Vec<String> = Vec::new();
        for output in self.space.outputs() {
            let layer_map = smithay::desktop::layer_map_for_output(output);
            for layer in layer_map.layers() {
                let ns = layer.namespace().to_string();
                if !ns.is_empty() && !layers.contains(&ns) {
                    layers.push(ns);
                }
            }
        }
        if !layers.is_empty() {
            content += &format!("layers={}\n", layers.join(","));
        }

        // Per-output camera/zoom state
        for output in self.space.outputs() {
            let os = output_state(output);
            let name = output.name();
            content += &format!(
                "outputs.{name}.camera_x={:.1}\noutputs.{name}.camera_y={:.1}\noutputs.{name}.zoom={:.3}\n",
                os.camera.x, os.camera.y, os.zoom
            );
        }

        if std::fs::write(&tmp, content).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn state_file_dir() -> Option<std::path::PathBuf> {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(|d| std::path::PathBuf::from(d).join("driftwm"))
}

/// Remove the state file on compositor exit.
pub fn remove_state_file() {
    if let Some(dir) = state_file_dir() {
        let _ = std::fs::remove_file(dir.join("state"));
        let _ = std::fs::remove_file(dir.join("state.tmp"));
    }
}

/// Read all per-output camera/zoom entries from the state file.
/// Returns a map from output name to `(camera, zoom)`.
pub fn read_all_per_output_state() -> HashMap<String, (Point<f64, Logical>, f64)> {
    let mut result = HashMap::new();
    let Some(dir) = state_file_dir() else {
        return result;
    };
    let Ok(content) = std::fs::read_to_string(dir.join("state")) else {
        return result;
    };

    // Parse lines like "outputs.eDP-1.camera_x=123.4"
    type Partial = (Option<f64>, Option<f64>, Option<f64>);
    let mut entries: HashMap<String, Partial> = HashMap::new();
    for line in content.lines() {
        let Some(rest) = line.strip_prefix("outputs.") else {
            continue;
        };
        // rest = "eDP-1.camera_x=123.4"
        let Some((name_and_key, val_str)) = rest.split_once('=') else {
            continue;
        };
        let Ok(val) = val_str.parse::<f64>() else {
            continue;
        };
        if let Some(name) = name_and_key.strip_suffix(".camera_x") {
            entries.entry(name.to_string()).or_default().0 = Some(val);
        } else if let Some(name) = name_and_key.strip_suffix(".camera_y") {
            entries.entry(name.to_string()).or_default().1 = Some(val);
        } else if let Some(name) = name_and_key.strip_suffix(".zoom") {
            entries.entry(name.to_string()).or_default().2 = Some(val);
        }
    }
    for (name, (cx, cy, z)) in entries {
        if let (Some(x), Some(y), Some(zoom)) = (cx, cy, z) {
            result.insert(name, (Point::from((x, y)), zoom));
        }
    }
    result
}
