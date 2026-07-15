//! Compositor-side glue for the durable session store (session restore): build
//! an envelope from live state, write it through the [`driftwm::session`] IO,
//! and materialize it back into suspended windows at startup.
//!
//! Cadence: a create or dismiss writes immediately; a move or resize arms a
//! short debounce timer; graceful shutdown fsync's a final write. Suspended
//! windows are saved regardless of `restore_session`; live windows are saved
//! as `Quit` records only when it's on. `path == None` disables everything (a
//! winit dev session without `--session-file`, or a fixture without an
//! injected path).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use smithay::desktop::Window;
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::utils::{Logical, Point, Size};
use smithay::wayland::seat::WaylandFocus;

use driftwm::canvas::{ScreenPos, internal_to_rule, rule_to_internal, screen_to_canvas};
use driftwm::desktop_entry::AppIdentity;
use driftwm::session::{self, Origin, SessionEntry, SessionEnvelope, SessionOutput};
use driftwm::window_ext::WindowExt;

use super::{DriftWm, StageWindow, SuspendedId, SuspendedWindow, output_state};

/// How long a move/resize coalesces before the durable write lands.
const WRITE_DEBOUNCE: Duration = Duration::from_secs(1);

/// Runtime bookkeeping for the durable session store.
#[derive(Default)]
pub struct SessionStore {
    /// Durable file path. `None` disables all persistence.
    pub path: Option<PathBuf>,
    /// `Quit`-origin entries read at startup but not materialized (restore
    /// off), re-emitted on every write so a flag-off session never destroys
    /// the saved session.
    pub(crate) carried_forward: Vec<SessionEntry>,
    /// Per-output cameras read at startup, to seed outputs the runtime state
    /// file hasn't recorded yet (fresh boot).
    pub(crate) durable_cameras: HashMap<String, (Point<f64, Logical>, f64)>,
    /// A change is waiting for the debounce timer to write it.
    dirty: bool,
    /// The armed one-shot debounce timer, if any.
    timer: Option<RegistrationToken>,
}

impl DriftWm {
    /// Read the durable session at startup: stash per-output cameras for
    /// fresh-boot seeding, materialize the eligible entries as suspended
    /// windows (bottom→top), and hold the rest to carry forward.
    pub fn load_session(&mut self) {
        let Some(path) = self.session_store.path.clone() else {
            return;
        };
        let envelope = session::read(&path);
        self.session_store.durable_cameras = envelope
            .outputs
            .iter()
            .map(|(name, o)| {
                (
                    name.clone(),
                    (Point::from((o.camera[0], o.camera[1])), o.zoom),
                )
            })
            .collect();
        let (materialize, carried) =
            session::partition_for_restore(envelope.entries, self.config.restore_session);
        self.session_store.carried_forward = carried;
        for entry in materialize {
            self.materialize_entry(entry);
        }
    }

    /// Recreate one saved window as a dormant suspended stand-in at its canvas
    /// rect. A fresh per-process id is assigned — the durable record key is not
    /// reused across restarts, and nothing in this pass depends on it.
    /// `map_window` raises, so materializing bottom→top reproduces the z-order.
    fn materialize_entry(&mut self, entry: SessionEntry) {
        let size = Size::from((entry.size[0], entry.size[1]));
        let loc = rule_to_internal(entry.position[0], entry.position[1], size);
        let sid = SuspendedId(self.next_suspended_id);
        self.next_suspended_id += 1;
        let identity = AppIdentity {
            app_id: entry.app_id,
            desktop_id: entry.desktop_id,
            display_name: entry.display_name,
        };
        let s = Rc::new(SuspendedWindow::new(
            sid,
            size,
            identity,
            entry.title,
            entry.origin,
        ));
        self.map_window(StageWindow::Suspended(s), loc, false);
    }

    /// Per-output cameras to restore on connect: the durable fresh-boot seed
    /// with the runtime state file layered on top, so runtime wins within a
    /// login session and durable only fills gaps the runtime file lacks.
    pub fn saved_camera_state(&self) -> HashMap<String, (Point<f64, Logical>, f64)> {
        merge_saved_cameras(
            &self.session_store.durable_cameras,
            super::read_all_per_output_state(),
        )
    }

    /// Immediate write for a create/dismiss: cancel any pending debounce and
    /// flush now, so a user-visible change is durable at once.
    pub fn session_store_write_now(&mut self) {
        if self.session_store.path.is_none() {
            return;
        }
        if let Some(token) = self.session_store.timer.take() {
            self.loop_handle.remove(token);
        }
        self.session_store_flush();
    }

    /// Arm the debounced write for a move/resize: a one-shot ~1s timer coalesces
    /// a drag's stream of position/size updates into a single write.
    pub fn session_store_mark_dirty(&mut self) {
        if self.session_store.path.is_none() {
            return;
        }
        self.session_store.dirty = true;
        if self.session_store.timer.is_some() {
            return;
        }
        let timer = Timer::from_duration(WRITE_DEBOUNCE);
        self.session_store.timer = self
            .loop_handle
            .insert_source(timer, |_, _, data: &mut DriftWm| {
                data.session_store.timer = None;
                if data.session_store.dirty {
                    data.session_store_flush();
                }
                TimeoutAction::Drop
            })
            .ok();
    }

    /// Flush the durable session at graceful shutdown (keybind quit or
    /// SIGTERM/SIGHUP), fsync'd. Suspended windows are always saved; live
    /// windows are added as `Quit` records only when `restore_session` is on.
    pub fn serialize_session_on_shutdown(&mut self) {
        if self.session_store.path.is_none() {
            return;
        }
        self.write_session(self.config.restore_session, true);
    }

    /// Steady-state write: suspended windows + carried-forward + cameras, no
    /// live windows, no fsync. Clears the dirty flag.
    fn session_store_flush(&mut self) {
        self.session_store.dirty = false;
        self.write_session(false, false);
    }

    fn write_session(&mut self, include_live: bool, fsync: bool) {
        let Some(path) = self.session_store.path.clone() else {
            return;
        };
        let envelope = self.build_session_envelope(include_live);
        if let Err(err) = session::write(&path, &envelope, fsync) {
            tracing::warn!("failed to write durable session store: {err}");
        }
    }

    /// Serialize the current durable state. Suspended windows carry their own
    /// origin; live windows are appended as `Quit` records when `include_live`.
    /// Carried-forward entries lead so freshly-active windows restore on top.
    fn build_session_envelope(&mut self, include_live: bool) -> SessionEnvelope {
        let mut entries = self.session_store.carried_forward.clone();
        // The record id is informational (materialization assigns fresh
        // in-process ids); numbering live windows past the suspended ids just
        // keeps them distinct within this write.
        let mut next_live_id = self.next_suspended_id;
        let windows: Vec<StageWindow> = self.stage.windows().cloned().collect();
        for window in &windows {
            if let Some(s) = window.suspended() {
                let loc = self.stage.position_of(window).unwrap_or_default();
                entries.push(suspended_entry(s, loc));
            } else if include_live
                && let Some(entry) = self.live_window_entry(window, &mut next_live_id)
            {
                entries.push(entry);
            }
        }
        SessionEnvelope {
            version: session::VERSION,
            saved_at: now_unix(),
            entries,
            outputs: self.per_output_cameras(),
        }
    }

    /// A `Quit` record for one live client window, or `None` when it can't come
    /// back: a widget, a dialog (has a parent — dead or alive — or is modal,
    /// matching suspend eligibility), or an app that resolves to no `.desktop`
    /// entry.
    fn live_window_entry(
        &mut self,
        window: &StageWindow,
        next_id: &mut u64,
    ) -> Option<SessionEntry> {
        let client = window.client()?.clone();
        if window.is_widget() || window.parent_surface().is_some() || window.is_modal() {
            return None;
        }
        let app_id = window.app_id_or_class().unwrap_or_default();
        let identity = self.resolve_identity(&app_id)?;
        let title = window.window_title().unwrap_or_default();
        let (loc, size) = self.live_window_rect(&client);
        let (x, y) = internal_to_rule(loc, size);
        let id = *next_id;
        *next_id += 1;
        Some(SessionEntry {
            id,
            app_id: identity.app_id,
            desktop_id: identity.desktop_id,
            display_name: identity.display_name,
            title,
            position: [x, y],
            size: [size.w, size.h],
            origin: Origin::Quit,
        })
    }

    /// The canvas rect a live window restores to. Fullscreen and pinned windows
    /// live in screen space, so use the geometry the stand-in would land at: the
    /// pre-fullscreen saved rect, or the unpin-to-canvas landing.
    fn live_window_rect(&self, window: &Window) -> (Point<i32, Logical>, Size<i32, Logical>) {
        if let Some(output) = window
            .wl_surface()
            .and_then(|s| self.find_fullscreen_output_for_surface(&s))
            && let Some(entry) = self.stage.fullscreen_on(&output.name())
        {
            return (entry.saved_location, entry.saved_size);
        }
        if let Some(site) = self.stage.pin_of(window).cloned()
            && let Some(output) = self.output_by_name(&site.output)
        {
            let (camera, zoom) = {
                let os = output_state(&output);
                (os.camera, os.zoom)
            };
            let canvas = screen_to_canvas(ScreenPos(site.screen_pos.to_f64()), camera, zoom)
                .0
                .to_i32_round();
            return (canvas, window.geometry().size);
        }
        let loc = self.stage.position_of(window).unwrap_or_default();
        (loc, window.geometry().size)
    }

    /// Current per-output cameras, plus stale entries for outputs that were
    /// present at boot but are gone now (an unplugged monitor's viewport isn't
    /// lost — matching the runtime file's behavior).
    fn per_output_cameras(&self) -> BTreeMap<String, SessionOutput> {
        let mut outputs = BTreeMap::new();
        for output in self.space.outputs() {
            let os = output_state(output);
            outputs.insert(
                output.name(),
                SessionOutput {
                    camera: [os.camera.x, os.camera.y],
                    zoom: os.zoom,
                },
            );
        }
        for (name, (cam, zoom)) in &self.session_store.durable_cameras {
            outputs.entry(name.clone()).or_insert(SessionOutput {
                camera: [cam.x, cam.y],
                zoom: *zoom,
            });
        }
        outputs
    }
}

/// A durable entry for a suspended window at canvas position `loc`.
fn suspended_entry(s: &SuspendedWindow, loc: Point<i32, Logical>) -> SessionEntry {
    let size = s.size.get();
    let (x, y) = internal_to_rule(loc, size);
    SessionEntry {
        id: s.id.0,
        app_id: s.identity.app_id.clone(),
        desktop_id: s.identity.desktop_id.clone(),
        display_name: s.identity.display_name.clone(),
        title: s.last_title.clone(),
        position: [x, y],
        size: [size.w, size.h],
        origin: s.origin,
    }
}

/// Merge the durable fresh-boot seed under the runtime file, which wins.
fn merge_saved_cameras(
    durable: &HashMap<String, (Point<f64, Logical>, f64)>,
    runtime: HashMap<String, (Point<f64, Logical>, f64)>,
) -> HashMap<String, (Point<f64, Logical>, f64)> {
    let mut merged = durable.clone();
    merged.extend(runtime);
    merged
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_camera_wins_over_durable_seed() {
        let mut durable = HashMap::new();
        durable.insert("only-durable".to_string(), (Point::from((1.0, 2.0)), 1.0));
        durable.insert("shared".to_string(), (Point::from((3.0, 4.0)), 1.5));

        let mut runtime = HashMap::new();
        runtime.insert("shared".to_string(), (Point::from((9.0, 9.0)), 2.0));
        runtime.insert("only-runtime".to_string(), (Point::from((5.0, 6.0)), 0.5));

        let merged = merge_saved_cameras(&durable, runtime);
        // A durable-only output is seeded on fresh boot.
        assert_eq!(merged["only-durable"], (Point::from((1.0, 2.0)), 1.0));
        // The runtime file wins within a login session.
        assert_eq!(merged["shared"], (Point::from((9.0, 9.0)), 2.0));
        assert_eq!(merged["only-runtime"], (Point::from((5.0, 6.0)), 0.5));
    }
}
