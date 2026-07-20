//! State file persistence under `$XDG_RUNTIME_DIR/driftwm/state`.
//!
//! External tools (launcher, status bars, scripts) read this file to learn
//! the current camera/zoom and window/layer inventory. Writes are throttled
//! to ~10/sec and only fire when something actually changed.
//!
//! The `windows=` line is a JSON array of objects with fields `id` (stable
//! per-session window handle), `app_id`, `title`, `position` ([x, y]), `size`
//! ([w, h]), and booleans `is_focused`/`is_widget`. Position/size match the
//! window-rules format in config.toml: position is the **window center** with
//! **Y-up** convention.
//!
//! `windows=` is canvas-space only. Screen-space windows are reported under
//! their owning output instead: `outputs.{name}.fullscreen` is a JSON
//! `{id, app_id, title}` object, and `outputs.{name}.pinned` a JSON array of
//! `{id, app_id, title, position, size}`. Like `windows=`, a pinned entry's
//! `position` is the window **center** in the rule convention (Y-up), but
//! relative to the **output center** rather than the canvas origin, so the
//! numbers paste straight into a `pinned_to_screen` rule's `position`.

use serde::Serialize;
use smithay::utils::{Logical, Point, Size};
use smithay::wayland::seat::WaylandFocus;
use std::collections::HashMap;
use std::time::Instant;

use driftwm::window_ext::WindowExt;

use crate::ipc::protocol::{CanvasLayerInfo, OutputFullscreen, OutputPinned, WindowInfo};

use super::{DriftWm, output_logical_size, output_state};

/// A fullscreen window in the state file's per-output section.
#[derive(Serialize)]
struct FullscreenInfo {
    id: u64,
    app_id: String,
    title: String,
}

/// A screen-pinned window in the state file's per-output section. `position` is
/// the window center in rule coordinates (output-center origin, Y-up) — the
/// numbers a `pinned_to_screen` rule's `position` takes; `size` in pixels.
#[derive(Serialize)]
struct PinnedInfo {
    id: u64,
    app_id: String,
    title: String,
    position: [i32; 2],
    size: [i32; 2],
}

/// `(app_id, title)` for a toplevel surface; empty strings when unavailable.
fn window_app_id_title(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> (String, String) {
    smithay::wayland::compositor::with_states(surface, |states| {
        states
            .data_map
            .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().ok())
            .map(|g| {
                (
                    g.app_id.clone().unwrap_or_default(),
                    g.title.clone().unwrap_or_default(),
                )
            })
            .unwrap_or_default()
    })
}

// Title is intentionally excluded from change detection: apps update their title
// on every keystroke / tab switch, and a write per title change would spam
// consumers. Title is still serialized when some other change triggers a write.
fn window_list_changed(a: &[WindowInfo], b: &[WindowInfo]) -> bool {
    a.len() != b.len()
        || a.iter().zip(b).any(|(x, y)| {
            x.id != y.id
                || x.app_id != y.app_id
                || x.position != y.position
                || x.size != y.size
                || x.is_focused != y.is_focused
                || x.is_widget != y.is_widget
                || x.suspended != y.suspended
        })
}

// Titles are excluded from file dirtiness (see above), but IPC subscribers want
// them, so a title-only change still pushes an event without rewriting the file.
// A length change is already covered by `window_list_changed`.
fn window_titles_changed(a: &[WindowInfo], b: &[WindowInfo]) -> bool {
    a.iter().zip(b).any(|(x, y)| x.title != y.title)
}

impl DriftWm {
    /// The canvas window inventory in the shared [`WindowInfo`] shape (position =
    /// window center, Y-up), focused window first. Single source of truth for
    /// both the state file and the IPC `state` response, so the two can't drift.
    pub fn window_inventory(&self) -> Vec<WindowInfo> {
        let focused = self.focused_window();
        let focused_suspended = self.gated_suspended_focus();
        let mut windows: Vec<WindowInfo> = Vec::new();
        for window in self.stage.windows() {
            // Suspended stand-ins have no surface; report them explicitly (they
            // live on the canvas like any window, are `msg`-selectable by id,
            // and a focused one is reported like a focused client).
            if let Some(s) = window.suspended() {
                let loc = self.stage.position_of(window).unwrap_or_default();
                let size = s.size.get();
                let (rx, ry) = driftwm::canvas::internal_to_rule(loc, size);
                windows.push(WindowInfo {
                    id: self
                        .stage
                        .id_of(window)
                        .expect("window from stage.windows() has an id")
                        .0,
                    app_id: s.identity.app_id.clone(),
                    title: s.last_title.clone(),
                    position: [rx, ry],
                    size: [size.w, size.h],
                    is_focused: focused_suspended == Some(s.id),
                    is_widget: false,
                    suspended: true,
                });
                continue;
            }
            let Some(surface) = window.wl_surface() else {
                continue;
            };
            // Pinned and fullscreen windows live in screen space, not on the
            // canvas — they're reported under `outputs.{name}.{pinned,fullscreen}`
            // instead. Omit them from this inventory, whose `position` is a
            // canvas coord (a fullscreen window's is the transient camera-origin).
            if self.is_pinned(window) || self.is_window_fullscreen(window) {
                continue;
            }
            let (app_id, title) = window_app_id_title(&surface);
            if app_id.is_empty() {
                continue;
            }
            let loc = self.stage.position_of(window).unwrap_or_default();
            // window.geometry().size can flicker for some Chromium-class clients
            // (see fit.rs), causing the occasional spurious write.
            let size = window.geometry().size;
            let (rx, ry) = driftwm::canvas::internal_to_rule(loc, size);
            windows.push(WindowInfo {
                id: self
                    .stage
                    .id_of(window)
                    .expect("window from stage.windows() has an id")
                    .0,
                app_id,
                title,
                position: [rx, ry],
                size: [size.w, size.h],
                is_focused: focused.as_ref().is_some_and(|f| window == f),
                is_widget: window.is_widget(),
                suspended: false,
            });
        }
        // Focused window first, so consumers can read windows[0] as the focused one.
        if let Some(idx) = windows.iter().position(|w| w.is_focused) {
            let w = windows.remove(idx);
            windows.insert(0, w);
        }
        windows
    }

    /// Layer-shell inventory, shared by the IPC `state` reply and the state
    /// file: namespaces of screen-space layer surfaces, plus canvas-positioned
    /// layers (which bypass the layer map) with their canvas rect in rule
    /// coordinates like `windows=`, sorted for deterministic output.
    pub fn layer_inventory(&self) -> (Vec<String>, Vec<CanvasLayerInfo>) {
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

        let mut canvas_layers: Vec<CanvasLayerInfo> = self
            .canvas_layers
            .iter()
            .filter_map(|cl| {
                let pos = cl.position?;
                let size = cl.surface.bbox().size;
                let (rx, ry) = driftwm::canvas::internal_to_rule(pos, size);
                Some(CanvasLayerInfo {
                    app_id: cl.namespace.clone(),
                    position: [rx, ry],
                    size: [size.w, size.h],
                })
            })
            .collect();
        canvas_layers.sort_by(|a, b| (&a.app_id, a.position).cmp(&(&b.app_id, b.position)));
        (layers, canvas_layers)
    }

    /// Screen-space windows (fullscreen + pinned) for the IPC `state` reply,
    /// each tagged with its output. Mirrors the state file's per-output
    /// `outputs.{name}.{fullscreen,pinned}` sections (same source fields), so a
    /// consumer reading either representation sees the same windows. Sorted for
    /// deterministic output (the underlying maps are unordered).
    pub fn screen_space_inventory(&self) -> (Vec<OutputFullscreen>, Vec<OutputPinned>) {
        let mut fullscreen: Vec<OutputFullscreen> = Vec::new();
        for (output, fs) in self.stage.fullscreen_entries() {
            // A dead fullscreen window may already be gone from the window list
            // (and thus have no id) until the reap pass runs — skip it.
            let Some(id) = self.stage.id_of(&fs.window) else {
                continue;
            };
            if let Some(surface) = fs.window.wl_surface() {
                let (app_id, title) = window_app_id_title(&surface);
                if !app_id.is_empty() {
                    fullscreen.push(OutputFullscreen {
                        id: id.0,
                        output: output.clone(),
                        app_id,
                        title,
                    });
                }
            }
        }
        fullscreen.sort_by(|a, b| (&a.output, &a.app_id).cmp(&(&b.output, &b.app_id)));

        let mut pinned: Vec<OutputPinned> = Vec::new();
        for window in self.stage.windows() {
            let Some(surface) = window.wl_surface() else {
                continue;
            };
            let Some(p) = self.stage.pin_of(window) else {
                continue;
            };
            let (app_id, title) = window_app_id_title(&surface);
            if app_id.is_empty() {
                continue;
            }
            let size = window.geometry().size;
            let Some((rx, ry)) = self.pinned_rule_coords(p, size) else {
                continue;
            };
            pinned.push(OutputPinned {
                id: self
                    .stage
                    .id_of(window)
                    .expect("window from stage.windows() has an id")
                    .0,
                output: p.output.clone(),
                app_id,
                title,
                position: [rx, ry],
                size: [size.w, size.h],
            });
        }
        pinned.sort_by(|a, b| (&a.output, a.position).cmp(&(&b.output, b.position)));

        (fullscreen, pinned)
    }

    /// A pin's rule coordinates (window center, output-center origin, Y-up) —
    /// the numbers a `pinned_to_screen` rule's `position` takes. `None` when the
    /// pin's output is disconnected: it can't be expressed relative to a monitor
    /// that's gone (and pins get reassigned off dead outputs anyway).
    fn pinned_rule_coords(
        &self,
        pin: &driftwm::stage::PinnedSite,
        size: Size<i32, Logical>,
    ) -> Option<(i32, i32)> {
        let output = self.output_by_name(&pin.output)?;
        let out_size = output_logical_size(&output);
        Some(driftwm::canvas::screen_top_left_to_rule(
            pin.screen_pos,
            size,
            out_size,
        ))
    }

    /// Write viewport center + zoom to `$XDG_RUNTIME_DIR/driftwm/state` if changed.
    /// Atomic: writes to .tmp then renames.
    pub fn write_state_file_if_dirty(&mut self) {
        // Subscribers get an event per rendered frame while something changes —
        // a client animating from snapshots (a minimap) can't work from the
        // file's ~10 Hz. Only the file write itself stays on the 100ms
        // throttle: the tmp-write + rename is the expensive part here. With no
        // subscribers, keep the old cheap sub-throttle early-out before the
        // allocating window_inventory() + with_states locks.
        let throttle_elapsed =
            self.state_file_last_write.elapsed() >= std::time::Duration::from_millis(100);
        if !throttle_elapsed && self.ipc_subscribers.is_empty() {
            return;
        }

        // Retry any event bytes a stalled subscriber couldn't take, so it
        // converges after draining even if nothing else changes.
        crate::ipc::flush_subscriber_outboxes(self);

        let window_fps = self.window_inventory();

        let (layers, canvas_layer_infos) = self.layer_inventory();
        let canvas_sig: Vec<(String, [i32; 2], [i32; 2])> = canvas_layer_infos
            .iter()
            .map(|c| (c.app_id.clone(), c.position, c.size))
            .collect();

        // Screen-space windows (pinned + fullscreen) live outside `windows=`, so
        // they need their own change detection or the per-output sections go
        // stale — e.g. dragging a pinned window, or a window opening straight
        // into fullscreen on an untouched monitor (no camera move, never in the
        // canvas list).
        let mut pinned_by_output: HashMap<String, Vec<PinnedInfo>> = HashMap::new();
        for window in self.stage.windows() {
            let Some(surface) = window.wl_surface() else {
                continue;
            };
            let Some(p) = self.stage.pin_of(window) else {
                continue;
            };
            let (app_id, title) = window_app_id_title(&surface);
            if app_id.is_empty() {
                continue;
            }
            let size = window.geometry().size;
            let Some((rx, ry)) = self.pinned_rule_coords(p, size) else {
                continue;
            };
            let id = self
                .stage
                .id_of(window)
                .expect("window from stage.windows() has an id")
                .0;
            pinned_by_output
                .entry(p.output.clone())
                .or_default()
                .push(PinnedInfo {
                    id,
                    app_id,
                    title,
                    position: [rx, ry],
                    size: [size.w, size.h],
                });
        }

        let mut fullscreen_by_output: HashMap<String, FullscreenInfo> = HashMap::new();
        for (output, fs) in self.stage.fullscreen_entries() {
            // Dead fullscreen windows may lack an id until the reap pass runs —
            // skip them here too (see `screen_space_inventory`).
            let Some(id) = self.stage.id_of(&fs.window) else {
                continue;
            };
            if let Some(surface) = fs.window.wl_surface() {
                let (app_id, title) = window_app_id_title(&surface);
                if !app_id.is_empty() {
                    fullscreen_by_output.insert(
                        output.clone(),
                        FullscreenInfo {
                            id: id.0,
                            app_id,
                            title,
                        },
                    );
                }
            }
        }

        // Sorted signatures for change detection (HashMap order is
        // nondeterministic; title excluded, matching the windows= title policy).
        let mut pinned_sig: Vec<(u64, String, [i32; 2], [i32; 2])> = Vec::new();
        for (name, pins) in &pinned_by_output {
            for p in pins {
                pinned_sig.push((p.id, name.clone(), p.position, p.size));
            }
        }
        pinned_sig.sort();
        let mut fullscreen_sig: Vec<(String, u64, String)> = fullscreen_by_output
            .iter()
            .map(|(name, f)| (name.clone(), f.id, f.app_id.clone()))
            .collect();
        fullscreen_sig.sort();

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
        let windows_dirty = window_list_changed(&window_fps, &self.state_file_windows)
            || layers.len() != self.state_file_layer_count
            || canvas_sig != self.state_file_canvas_layers;
        let screen_space_dirty =
            pinned_sig != self.state_file_pinned || fullscreen_sig != self.state_file_fullscreen;

        let titles_dirty = window_titles_changed(&window_fps, &self.state_file_windows);
        // The file's top-level camera and the snapshot's `active` flags follow
        // the active output, so switching outputs dirties them even when no
        // camera moved.
        let active_name = self.active_output().map(|o| o.name());
        let active_dirty = active_name != self.state_file_active_output;

        if !layout_dirty
            && !any_output_dirty
            && !windows_dirty
            && !screen_space_dirty
            && !active_dirty
        {
            if titles_dirty {
                crate::ipc::broadcast_state_event(self);
                // Cache the new titles or this re-fires every tick; the file
                // itself deliberately stays stale on title-only changes.
                self.state_file_windows = window_fps;
            }
            return;
        }
        crate::ipc::broadcast_state_event(self);

        // The file and the caches that gate the dirty flags wait for the
        // throttle. Sub-throttle frames where the snapshot didn't actually
        // change are deduped by the event-bytes hash in the broadcast.
        if !throttle_elapsed {
            return;
        }
        self.state_file_last_write = Instant::now();

        let z = self.zoom();
        let vp = self.get_viewport_size();
        let (cx, cy) = driftwm::canvas::viewport_center(self.camera(), z, vp);

        let Some(dir) = state_file_dir() else { return };
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("state");
        let tmp = dir.join("state.tmp");
        // no separate dirty field: layout_short follows the active XKB group, so the
        // layout-dirty check covers it (except two layouts sharing a display name).
        let layout_short = crate::ipc::active_layout_short(self);
        let mut content = format!(
            "x={cx:.0}\ny={cy:.0}\nzoom={z:.3}\nlayout={}\nlayout_short={layout_short}\n",
            self.active_layout
        );

        if let Some(output) = self.active_output() {
            let home_return = output_state(&output).home_return.clone();
            if let Some(ref ret) = home_return {
                let sz = ret.zoom;
                let (sx, sy) = driftwm::canvas::viewport_center(ret.camera, sz, vp);
                content += &format!("saved_x={sx:.0}\nsaved_y={sy:.0}\nsaved_zoom={sz:.3}\n");
            }
        }

        if !window_fps.is_empty()
            && let Ok(json) = serde_json::to_string(&window_fps)
        {
            content += "windows=";
            content += &json;
            content.push('\n');
        }

        if !layers.is_empty() {
            content += &format!("layers={}\n", layers.join(","));
        }

        if !canvas_layer_infos.is_empty()
            && let Ok(json) = serde_json::to_string(&canvas_layer_infos)
        {
            content += &format!("canvas_layers={json}\n");
        }

        // Per-output camera/zoom + screen-space (fullscreen, pinned) inventory.
        for output in self.space.outputs() {
            let name = output.name();
            let (cam, z) = {
                let os = output_state(output);
                (os.camera, os.zoom)
            };
            content += &format!(
                "outputs.{name}.camera_x={:.1}\noutputs.{name}.camera_y={:.1}\noutputs.{name}.zoom={z:.3}\n",
                cam.x, cam.y
            );

            if let Some(info) = fullscreen_by_output.get(&name)
                && let Ok(json) = serde_json::to_string(info)
            {
                content += &format!("outputs.{name}.fullscreen={json}\n");
            }

            if let Some(pins) = pinned_by_output.get(&name)
                && let Ok(json) = serde_json::to_string(pins)
            {
                content += &format!("outputs.{name}.pinned={json}\n");
            }
        }

        // Update content caches only after a successful atomic rename, so a
        // transient FS error gets retried on the next call instead of being
        // silently swallowed.
        if std::fs::write(&tmp, content).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
            self.state_file_layer_count = layers.len();
            for output in self.space.outputs() {
                let os = output_state(output);
                self.state_file_cameras
                    .insert(output.name(), (os.camera, os.zoom));
            }
            self.state_file_layout = self.active_layout.clone();
            self.state_file_windows = window_fps;
            self.state_file_pinned = pinned_sig;
            self.state_file_fullscreen = fullscreen_sig;
            self.state_file_canvas_layers = canvas_sig;
            self.state_file_active_output = active_name;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u64, title: &str) -> WindowInfo {
        WindowInfo {
            id,
            app_id: "app".into(),
            title: title.into(),
            position: [0, 0],
            size: [100, 100],
            is_focused: false,
            is_widget: false,
            suspended: false,
        }
    }

    #[test]
    fn titles_changed_detects_title_only_diff() {
        let a = vec![win(1, "one"), win(2, "two")];
        let b = vec![win(1, "one"), win(2, "TWO")];
        assert!(window_titles_changed(&a, &b));
    }

    #[test]
    fn titles_changed_ignores_equal_titles() {
        let a = vec![win(1, "one"), win(2, "two")];
        let b = vec![win(1, "one"), win(2, "two")];
        assert!(!window_titles_changed(&a, &b));
    }

    #[test]
    fn list_changed_detects_suspend_transition() {
        // A suspend conversion is in-place: same id/app_id/rect/focus — only
        // `suspended` flips. Without it in the closure, subscribers and the
        // state file never see the transition.
        let a = vec![win(1, "one")];
        let mut b = a.clone();
        b[0].suspended = true;
        assert!(window_list_changed(&a, &b));
    }
}
